// Ported from gopkg.in/yaml.v3 v3.0.1 decode.go (Apache-2.0), the decoder
// CLIProxyAPI internal/config/config_v8.go (v8.0.10, MIT) decodes its
// config with.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml

//! Decodes a [`Node`] tree into config types as yaml.v3 decodes into Go
//! structs.
//!
//! The types derive `serde::Deserialize`; this module is the deserializer.
//! It follows yaml.v3's rules rather than serde's usual ones:
//! - Values of the wrong type are collected as type errors and decoding
//!   goes on, so one load reports every bad value. Unresolvable scalars
//!   (`!!int abc`) stop it.
//! - Duplicate keys in any mapping are errors, and a field set twice through
//!   keys that differ in the source (`!!binary` and plain) is too.
//! - A null value leaves a field at its default. Null list items are
//!   dropped; a null map value is the zero value.
//! - Values of fields a type doesn't have are skipped without being read.
//! - Booleans also accept `yes`, `no`, `on`, `off`, `y` and `n`; integers
//!   accept floats, which are truncated.
//!
//! Type errors name Go's types (`config.TLSConfig`, `[]string`), as upstream
//! reports them: struct names come from each type's serde name and
//! collection types from [`field_hint`].
//!
//! An [`AnyValue`] field decodes its node as yaml.v3 decodes into Go's
//! `any`; it asks for that through serde's newtype-struct hook with a name
//! of its own.
//!
//! Deviations from upstream:
//! - Type errors leave out yaml.v3's excerpt of the offending value.
//! - Only the types this crate decodes are supported.
//! - A decode stops with `document contains excessive aliasing` once the
//!   strings that aliases expanded to add up to more than 64 MiB; yaml.v3
//!   decodes them.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;

use serde::de::value::StringDeserializer;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, IntoDeserializer, MapAccess,
    SeqAccess, VariantAccess, Visitor,
};
use serde::{Deserialize, forward_to_deserialize_any};

use super::layout::AnyValue;
use super::yaml::{
    AliasBudget, Kind, Node, Scalar, YamlError, duplicate_key_errors, is_merge, push_type_error,
    resolve_node, scalar_string, timestamp_json_text, type_error,
};

/// The newtype name [`AnyValue`] deserializes with, which [`NodeDe`]
/// answers with the node decoded into `any`.
const ANY_VALUE: &str = "config.anyValue";

/// A decode that stopped; yaml.v3's `failf`, without the `yaml: ` prefix.
#[derive(Debug)]
pub(crate) struct Fatal(String);

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

impl de::Error for Fatal {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }
}

fn fatal(error: YamlError) -> Fatal {
    match error {
        YamlError::Fatal(message) => Fatal(message),
        other => Fatal(other.message()),
    }
}

/// Decodes `node` into `T` as yaml.v3's `Node.Decode` would.
pub(crate) fn decode<T: DeserializeOwned>(node: &Node) -> Result<T, YamlError> {
    let errors = RefCell::new(Vec::new());
    let budget = AliasBudget::default();
    let value = T::deserialize(NodeDe {
        node,
        hint: "",
        errors: &errors,
        budget: &budget,
    })
    .map_err(|Fatal(message)| YamlError::Fatal(message))?;
    let errors = errors.into_inner();
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(YamlError::Type(errors))
    }
}

/// The Go type of a collection field, for type errors.
fn field_hint(owner: &str, key: &str) -> &'static str {
    match (owner, key) {
        ("config.legacyConfig", "api-keys" | "trusted-proxies") => "[]string",
        ("config.legacyConfig", "codex-api-key") => "[]config.CodexKey",
        ("config.legacyConfig", "claude-api-key") => "[]config.ClaudeKey",
        ("config.legacyConfig", "oauth-excluded-models") => "map[string][]string",
        ("config.legacyConfig", "oauth-model-alias") => "map[string][]config.OAuthModelAlias",
        ("config.legacyConfig", "oauth-request-scoped-errors") => {
            "map[string][]config.RequestScopedErrorRule"
        }
        ("config.legacyConfig", "oauth-settings") => "map[string][]config.OAuthModelSetting",
        ("config.CodexKey", "models") => "[]config.CodexModel",
        ("config.ClaudeKey", "models") => "[]config.ClaudeModel",
        (_, "headers") => "map[string]string",
        (_, "excluded-models") => "[]string",
        (_, "request-scoped-errors") => "[]config.RequestScopedErrorRule",
        ("registry.ThinkingSupport", "levels") => "[]string",
        ("config.RequestScopedErrorRule", "match" | "match-regexr") => "[]string",
        ("config.PayloadConfig", "default" | "default-raw" | "override" | "override-raw") => {
            "[]config.PayloadRule"
        }
        ("config.PayloadConfig", "filter") => "[]config.PayloadFilterRule",
        ("config.PayloadRule" | "config.PayloadFilterRule", "models") => {
            "[]config.PayloadModelRule"
        }
        ("config.PayloadRule", "params") => "map[string]interface {}",
        ("config.PayloadFilterRule", "params") => "[]string",
        ("config.PayloadModelRule", "match" | "not-match") => "[]map[string]interface {}",
        ("config.PayloadModelRule", "exist" | "not-exist") => "[]string",
        _ => "",
    }
}

/// A node being decoded, with the Go type a collection would have.
#[derive(Clone, Copy)]
struct NodeDe<'a> {
    node: &'a Node,
    hint: &'static str,
    errors: &'a RefCell<Vec<String>>,
    /// The text aliases have produced so far.
    budget: &'a AliasBudget,
}

impl<'a> NodeDe<'a> {
    fn child(&self, node: &'a Node, hint: &'static str) -> Self {
        Self {
            node,
            hint,
            errors: self.errors,
            budget: self.budget,
        }
    }

    fn push(&self, error: String) {
        push_type_error(&mut self.errors.borrow_mut(), error);
    }

    /// Records the node's duplicate keys; true when it has any. yaml.v3
    /// checks before looking at the target type.
    fn duplicates(&self) -> bool {
        if self.node.kind != Kind::Mapping {
            return false;
        }
        let duplicates = duplicate_key_errors(self.node);
        if duplicates.is_empty() {
            return false;
        }
        let mut errors = self.errors.borrow_mut();
        duplicates
            .into_iter()
            .for_each(|error| push_type_error(&mut errors, error));
        true
    }

    /// Records that the node can't go into `type_name`.
    fn mismatch(&self, type_name: &str) {
        if !self.duplicates() {
            self.push(type_error(self.node, type_name));
        }
    }

    /// The resolved scalar, or `None` for a collection.
    fn scalar(&self) -> Result<Option<Scalar>, Fatal> {
        match self.node.kind {
            Kind::Poison => Err(Fatal(self.node.value.to_string())),
            Kind::Scalar => resolve_node(self.node)
                .map(|resolved| Some(resolved.value))
                .map_err(fatal),
            Kind::Sequence | Kind::Mapping => Ok(None),
        }
    }

    fn is_null(&self) -> Result<bool, Fatal> {
        Ok(matches!(self.scalar()?, Some(Scalar::Null)))
    }

    fn int(&self) -> Result<i64, Fatal> {
        Ok(match self.scalar()? {
            Some(Scalar::Null) => 0,
            Some(Scalar::Int(value)) => value,
            Some(Scalar::Uint(value)) if i64::try_from(value).is_ok() => value as i64,
            // Go converts with int64(f) after checking f <= MaxInt64; NaN fails.
            Some(Scalar::Float(value)) if value <= i64::MAX as f64 => value as i64,
            _ => {
                self.mismatch("int");
                0
            }
        })
    }

    fn bool(&self) -> Result<bool, Fatal> {
        Ok(match self.scalar()? {
            Some(Scalar::Null) => false,
            Some(Scalar::Bool(value)) => value,
            Some(Scalar::Str(text)) => match text.as_str() {
                "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => true,
                "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => false,
                _ => {
                    self.mismatch("bool");
                    false
                }
            },
            _ => {
                self.mismatch("bool");
                false
            }
        })
    }

    fn string(&self) -> Result<String, Fatal> {
        match self.node.kind {
            Kind::Poison => Err(Fatal(self.node.value.to_string())),
            Kind::Scalar => {
                self.budget.charge(self.node).map_err(fatal)?;
                Ok(scalar_string(self.node).map_err(fatal)?.unwrap_or_default())
            }
            Kind::Sequence | Kind::Mapping => {
                self.mismatch("string");
                Ok(String::new())
            }
        }
    }

    /// Decodes a mapping key into a Go string; `None` when yaml.v3 skips it.
    fn key(&self, key: &Node) -> Result<Option<String>, Fatal> {
        let key = self.child(key, "string");
        match key.node.kind {
            Kind::Scalar | Kind::Poison => Ok(key.string_or_null()?),
            Kind::Sequence | Kind::Mapping => {
                key.mismatch("string");
                Ok(None)
            }
        }
    }

    fn string_or_null(&self) -> Result<Option<String>, Fatal> {
        if self.node.kind == Kind::Poison {
            return Err(Fatal(self.node.value.to_string()));
        }
        self.budget.charge(self.node).map_err(fatal)?;
        scalar_string(self.node).map_err(fatal)
    }

    /// The node as yaml.v3 decodes it into `any` (`decoder.scalar`,
    /// `decoder.sequence` and `decoder.mapping` with an `interface{}`
    /// target): a mapping with a repeated key records the error and decodes
    /// to nil, and sequences keep their null items.
    fn any(&self) -> Result<AnyValue, Fatal> {
        match self.node.kind {
            Kind::Poison => Err(Fatal(self.node.value.to_string())),
            Kind::Scalar => {
                self.budget.charge(self.node).map_err(fatal)?;
                Ok(match resolve_node(self.node).map_err(fatal)?.value {
                    Scalar::Null => AnyValue::Null,
                    Scalar::Bool(value) => AnyValue::Bool(value),
                    Scalar::Int(value) => AnyValue::Int(value),
                    Scalar::Uint(value) => AnyValue::Uint(value),
                    Scalar::Float(value) => AnyValue::Float(value),
                    Scalar::Timestamp => AnyValue::Time(timestamp_json_text(&self.node.value)),
                    Scalar::Str(value) => AnyValue::Str(value.to_string()),
                })
            }
            Kind::Sequence => self
                .node
                .content
                .iter()
                .map(|item| self.child(item, "").any())
                .collect::<Result<_, _>>()
                .map(AnyValue::Seq),
            Kind::Mapping => {
                if self.duplicates() {
                    return Ok(AnyValue::Null);
                }
                let string_keys = self
                    .node
                    .pairs()
                    .all(|(key, _)| key.tag == "!!str" || key.tag == "!!merge");
                if string_keys {
                    let mut map = BTreeMap::new();
                    for (key, value) in self.node.pairs() {
                        self.budget.charge(key).map_err(fatal)?;
                        map.insert(key.value.to_string(), self.child(value, "").any()?);
                    }
                    return Ok(AnyValue::Map(map));
                }
                for (key, value) in self.node.pairs() {
                    self.child(key, "").any()?;
                    self.child(value, "").any()?;
                }
                Ok(AnyValue::AnyMap)
            }
        }
    }
}

impl<'de> de::Deserializer<'de> for NodeDe<'_> {
    type Error = Fatal;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        match self.scalar()? {
            None if self.node.kind == Kind::Sequence => self.deserialize_seq(visitor),
            None => self.deserialize_map(visitor),
            Some(Scalar::Null) => visitor.visit_unit(),
            Some(Scalar::Bool(value)) => visitor.visit_bool(value),
            Some(Scalar::Int(value)) => visitor.visit_i64(value),
            Some(Scalar::Uint(value)) => visitor.visit_u64(value),
            Some(Scalar::Float(value)) => visitor.visit_f64(value),
            Some(Scalar::Timestamp | Scalar::Str(_)) => visitor.visit_string(self.string()?),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        visitor.visit_bool(self.bool()?)
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        visitor.visit_i64(self.int()?)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        visitor.visit_string(self.string()?)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        visitor.visit_string(self.string()?)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        if self.is_null()? {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        if self.node.kind == Kind::Sequence {
            let hint = self.hint.strip_prefix("[]").unwrap_or_default();
            return visitor.visit_seq(Items {
                de: self,
                items: self.node.content.iter(),
                hint,
            });
        }
        if !self.is_null()? {
            self.mismatch(self.hint);
        }
        visitor.visit_seq(Items {
            de: self,
            items: [].iter(),
            hint: "",
        })
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        let hint = self.hint.strip_prefix("map[string]").unwrap_or_default();
        let empty = Entries {
            de: self,
            pairs: [].iter(),
            owner: "",
            fields: None,
            done: Vec::new(),
            hint,
            value: None,
        };
        if self.node.kind != Kind::Mapping {
            if !self.is_null()? {
                self.mismatch(self.hint);
            }
            return visitor.visit_map(empty);
        }
        if self.duplicates() {
            return visitor.visit_map(empty);
        }
        visitor.visit_map(Entries {
            pairs: self.node.content.as_chunks::<2>().0.iter(),
            ..empty
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Fatal> {
        let empty = Entries {
            de: self,
            pairs: [].iter(),
            owner: name,
            fields: Some(fields),
            done: Vec::new(),
            hint: "",
            value: None,
        };
        if self.node.kind != Kind::Mapping {
            if !self.is_null()? {
                self.mismatch(name);
            }
            return visitor.visit_map(empty);
        }
        if self.duplicates() {
            return visitor.visit_map(empty);
        }
        visitor.visit_map(Entries {
            pairs: self.node.content.as_chunks::<2>().0.iter(),
            ..empty
        })
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Fatal> {
        if name == ANY_VALUE {
            return AnyDe(self.any()?).deserialize_any(visitor);
        }
        self.deserialize_any(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        // yaml.v3 never looks at the values of unknown fields.
        visitor.visit_unit()
    }

    forward_to_deserialize_any! {
        i8 i16 i32 i128 u8 u16 u32 u64 u128 f32 f64 char bytes byte_buf unit
        unit_struct tuple tuple_struct enum identifier
    }
}

impl<'de> Deserialize<'de> for AnyValue {
    /// Only the config decoder gives every kind of value; another
    /// deserializer gives what its data model holds.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_newtype_struct(ANY_VALUE, AnyVisitor)
    }
}

/// Rebuilds an [`AnyValue`] from what [`AnyDe`] hands over.
struct AnyVisitor;

impl<'de> Visitor<'de> for AnyVisitor {
    type Value = AnyValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any value")
    }

    fn visit_unit<E>(self) -> Result<AnyValue, E> {
        Ok(AnyValue::Null)
    }

    fn visit_none<E>(self) -> Result<AnyValue, E> {
        Ok(AnyValue::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<AnyValue, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<AnyValue, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_bool<E>(self, value: bool) -> Result<AnyValue, E> {
        Ok(AnyValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<AnyValue, E> {
        Ok(AnyValue::Int(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<AnyValue, E> {
        Ok(i64::try_from(value).map_or(AnyValue::Uint(value), AnyValue::Int))
    }

    fn visit_f64<E>(self, value: f64) -> Result<AnyValue, E> {
        Ok(AnyValue::Float(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<AnyValue, E> {
        Ok(AnyValue::Str(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<AnyValue, E> {
        Ok(AnyValue::Str(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<AnyValue, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(AnyValue::Seq(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<AnyValue, A::Error> {
        let mut entries = BTreeMap::new();
        while let Some((key, value)) = map.next_entry()? {
            entries.insert(key, value);
        }
        Ok(AnyValue::Map(entries))
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<AnyValue, A::Error> {
        let (variant, access): (String, _) = data.variant()?;
        match variant.as_str() {
            TIME => access
                .newtype_variant()
                .map(|text| AnyValue::Time(Some(text))),
            BAD_TIME => access.unit_variant().map(|()| AnyValue::Time(None)),
            _ => access.unit_variant().map(|()| AnyValue::AnyMap),
        }
    }
}

/// The enum variants [`AnyDe`] hands over the values serde has no kind
/// for as: a time's JSON text, a time Go's encoder refuses, and a mapping
/// with keys that aren't strings.
const TIME: &str = "time";
const BAD_TIME: &str = "bad-time";
const ANY_MAP: &str = "any-map";

/// Hands a decoded [`AnyValue`] to a visitor.
struct AnyDe(AnyValue);

impl<'de> Deserializer<'de> for AnyDe {
    type Error = Fatal;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        match self.0 {
            AnyValue::Null => visitor.visit_unit(),
            AnyValue::Bool(value) => visitor.visit_bool(value),
            AnyValue::Int(value) => visitor.visit_i64(value),
            AnyValue::Uint(value) => visitor.visit_u64(value),
            AnyValue::Float(value) => visitor.visit_f64(value),
            AnyValue::Str(value) => visitor.visit_string(value),
            AnyValue::Time(Some(text)) => visitor.visit_enum(AnyVariant(TIME, text)),
            AnyValue::Time(None) => visitor.visit_enum(AnyVariant(BAD_TIME, String::new())),
            AnyValue::AnyMap => visitor.visit_enum(AnyVariant(ANY_MAP, String::new())),
            AnyValue::Seq(items) => visitor.visit_seq(AnyItems(items.into_iter())),
            AnyValue::Map(entries) => visitor.visit_map(AnyEntries {
                entries: entries.into_iter(),
                value: None,
            }),
        }
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// A sequence's items, handed to a visitor.
struct AnyItems(std::vec::IntoIter<AnyValue>);

impl<'de> SeqAccess<'de> for AnyItems {
    type Error = Fatal;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Fatal> {
        self.0
            .next()
            .map(|item| seed.deserialize(AnyDe(item)))
            .transpose()
    }
}

/// A mapping's entries, handed to a visitor.
struct AnyEntries {
    entries: std::collections::btree_map::IntoIter<String, AnyValue>,
    value: Option<AnyValue>,
}

impl<'de> MapAccess<'de> for AnyEntries {
    type Error = Fatal;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Fatal> {
        let Some((key, value)) = self.entries.next() else {
            return Ok(None);
        };
        self.value = Some(value);
        let key: StringDeserializer<Fatal> = key.into_deserializer();
        seed.deserialize(key).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Fatal> {
        let Some(value) = self.value.take() else {
            return Err(Fatal("internal error: value before key".to_owned()));
        };
        seed.deserialize(AnyDe(value))
    }
}

/// One of the variants above, with a time's text.
struct AnyVariant(&'static str, String);

impl<'de> EnumAccess<'de> for AnyVariant {
    type Error = Fatal;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self), Fatal> {
        let name: StringDeserializer<Fatal> = self.0.to_owned().into_deserializer();
        Ok((seed.deserialize(name)?, self))
    }
}

impl<'de> VariantAccess<'de> for AnyVariant {
    type Error = Fatal;

    fn unit_variant(self) -> Result<(), Fatal> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Fatal> {
        let text: StringDeserializer<Fatal> = self.1.into_deserializer();
        seed.deserialize(text)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Fatal> {
        visitor.visit_unit()
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Fatal> {
        visitor.visit_unit()
    }
}

/// Sequence items, skipping nulls as yaml.v3 drops them.
struct Items<'a, I> {
    de: NodeDe<'a>,
    items: I,
    hint: &'static str,
}

impl<'de, 'a, I: Iterator<Item = &'a Node>> SeqAccess<'de> for Items<'a, I> {
    type Error = Fatal;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Fatal> {
        for item in self.items.by_ref() {
            let item = self.de.child(item, self.hint);
            if item.is_null()? {
                continue;
            }
            return seed.deserialize(item).map(Some);
        }
        Ok(None)
    }
}

/// Mapping entries, for a map (`fields` is `None`) or a struct.
struct Entries<'a> {
    de: NodeDe<'a>,
    pairs: std::slice::Iter<'a, [Node; 2]>,
    owner: &'static str,
    fields: Option<&'static [&'static str]>,
    /// Struct fields already set.
    done: Vec<String>,
    /// The Go type of map values.
    hint: &'static str,
    /// The value of the key last returned.
    value: Option<(&'a Node, &'static str)>,
}

impl<'de> MapAccess<'de> for Entries<'_> {
    type Error = Fatal;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Fatal> {
        for [key, value] in self.pairs.by_ref() {
            if is_merge(key) {
                continue;
            }
            let Some(name) = self.de.key(key)? else {
                continue;
            };
            let hint = match self.fields {
                None => self.hint,
                Some(fields) => {
                    if !fields.contains(&name.as_str()) {
                        continue;
                    }
                    if self.done.contains(&name) {
                        let owner = self.owner;
                        self.de.push(format!(
                            "line {}: field {name} already set in type {owner}",
                            key.line
                        ));
                        continue;
                    }
                    self.done.push(name.clone());
                    // A null leaves the field as it was.
                    if self.de.child(value, "").is_null()? {
                        continue;
                    }
                    field_hint(self.owner, &name)
                }
            };
            self.value = Some((value, hint));
            let key: StringDeserializer<Fatal> = name.into_deserializer();
            return seed.deserialize(key).map(Some);
        }
        Ok(None)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Fatal> {
        let Some((value, hint)) = self.value.take() else {
            return Err(Fatal("internal error: value before key".to_owned()));
        };
        seed.deserialize(self.de.child(value, hint))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::Deserialize;

    use super::*;
    use crate::config::yaml::parse_document;

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default, rename = "config.Sample", rename_all = "kebab-case")]
    struct Sample {
        name: String,
        count: i64,
        flag: bool,
        maybe: Option<i64>,
        headers: BTreeMap<String, String>,
        excluded_models: Vec<String>,
    }

    fn run(text: &str) -> Result<Sample, String> {
        let root = parse_document(text)
            .map_err(|e| e.message())?
            .unwrap_or_default();
        decode::<Sample>(&root).map_err(|e| e.message())
    }

    #[test]
    fn scalars_follow_yaml_v3() {
        let sample = run("name: 5\ncount: 1.9\nflag: on\nmaybe: 0x10\n").unwrap_or_default();
        assert_eq!(sample.name, "5");
        assert_eq!(sample.count, 1);
        assert!(sample.flag);
        assert_eq!(sample.maybe, Some(16));
        assert_eq!(run("flag: 'off'\n").map(|s| s.flag), Ok(false));
        assert_eq!(
            run("name: !!binary aGVsbG8=\n").map(|s| s.name),
            Ok("hello".to_owned())
        );
        assert_eq!(run("count: -1.5\n").map(|s| s.count), Ok(-1));
        assert_eq!(
            run("count: ~\nmaybe: ~\n").map(|s| (s.count, s.maybe)),
            Ok((0, None))
        );
    }

    #[test]
    fn type_errors_are_collected() {
        assert_eq!(
            run("count: abc\nflag: 'true'\nname: [a]\nheaders: 5\nexcluded-models: {a: 1}\n"),
            Err("yaml: unmarshal errors:\n  line 1: cannot unmarshal !!str into int\n  \
                 line 2: cannot unmarshal !!str into bool\n  line 3: cannot unmarshal !!seq into string\n  \
                 line 4: cannot unmarshal !!int into map[string]string\n  \
                 line 5: cannot unmarshal !!map into []string"
                .to_owned())
        );
        assert_eq!(
            run("count: .nan\n"),
            Err("yaml: unmarshal errors:\n  line 1: cannot unmarshal !!float into int".to_owned())
        );
        assert_eq!(
            run("count: 9999999999999999999\n"),
            Err("yaml: unmarshal errors:\n  line 1: cannot unmarshal !!int into int".to_owned())
        );
        assert_eq!(
            run("count: !!int abc\n"),
            Err("yaml: cannot decode !!str as a !!int".to_owned())
        );
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(default, rename = "config.AnySample")]
    struct AnySample {
        value: Option<AnyValue>,
        list: Vec<AnyValue>,
    }

    fn any(text: &str) -> Result<AnySample, String> {
        let root = parse_document(text)
            .map_err(|e| e.message())?
            .unwrap_or_default();
        decode::<AnySample>(&root).map_err(|e| e.message())
    }

    // Not upstream's: yaml.v3 decoding into `any`, as upstream's payload
    // params decode.
    #[test]
    fn any_values_decode_as_yaml_v3_decodes_into_any() {
        let value = |text: &str| any(text).map(|sample| sample.value.unwrap_or(AnyValue::Null));
        assert_eq!(
            value(
                "value: 0x10
"
            ),
            Ok(AnyValue::Int(16))
        );
        assert_eq!(
            value(
                "value: 18446744073709551615
"
            ),
            Ok(AnyValue::Uint(u64::MAX))
        );
        assert_eq!(
            value(
                "value: 1.5
"
            ),
            Ok(AnyValue::Float(1.5))
        );
        assert_eq!(
            value(
                "value: on
"
            ),
            Ok(AnyValue::Str("on".into()))
        );
        assert_eq!(
            value(
                "value: {a: ~}
"
            ),
            Ok(AnyValue::Map(BTreeMap::from([(
                "a".into(),
                AnyValue::Null
            )])))
        );
        assert_eq!(
            value(
                "value: !!binary aGVsbG8=
"
            ),
            Ok(AnyValue::Str("hello".into()))
        );
        assert_eq!(
            value(
                "value: 2001-12-14
"
            ),
            Ok(AnyValue::Time(Some("2001-12-14T00:00:00Z".into())))
        );
        assert_eq!(
            value(
                "value: [1, ~, {a: [x]}]
"
            ),
            Ok(AnyValue::Seq(vec![
                AnyValue::Int(1),
                AnyValue::Null,
                AnyValue::Map(BTreeMap::from([(
                    "a".into(),
                    AnyValue::Seq(vec![AnyValue::Str("x".into())])
                )])),
            ]))
        );
        assert_eq!(
            value(
                "value: {1: a}
"
            ),
            Ok(AnyValue::AnyMap)
        );
        assert_eq!(
            any("list: [a, ~, 1]
")
            .map(|sample| sample.list),
            Ok(vec![AnyValue::Str("a".into()), AnyValue::Int(1)])
        );
        assert_eq!(
            value(
                "value: {a: {b: 1, b: 2}}
"
            ),
            Err(
                "yaml: unmarshal errors:\n  line 1: mapping key \"b\" already defined at line 1"
                    .to_owned()
            )
        );
        assert_eq!(
            value(
                "value: !!int abc
"
            ),
            Err("yaml: cannot decode !!str as a !!int".to_owned())
        );
    }

    #[test]
    fn duplicates_and_nulls() {
        assert_eq!(
            run("name: a\nname: b\n"),
            Err(
                "yaml: unmarshal errors:\n  line 2: mapping key \"name\" already defined at line 1"
                    .to_owned()
            )
        );
        assert_eq!(
            run("name: a\n!!binary bmFtZQ==: b\n"),
            Err(
                "yaml: unmarshal errors:\n  line 2: field name already set in type config.Sample"
                    .to_owned()
            )
        );
        assert_eq!(
            run("headers: {a: 1, a: 2}\n"),
            Err(
                "yaml: unmarshal errors:\n  line 1: mapping key \"a\" already defined at line 1"
                    .to_owned()
            )
        );
        let sample = run(
            "headers: {a: ~, b: x}\nexcluded-models: [a, ~, b]\n~: 5\nunknown: [1, {x: !!int abc}]\n",
        );
        let sample = sample.unwrap_or_default();
        assert_eq!(sample.headers.get("a").map(String::as_str), Some(""));
        assert_eq!(sample.excluded_models, ["a", "b"]);
    }
}
