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
//! Deviations from upstream:
//! - Type errors leave out yaml.v3's excerpt of the offending value.
//! - Only the types this crate decodes are supported; there is no general
//!   `interface{}` decoding.

use std::cell::RefCell;
use std::fmt;

use serde::de::value::StringDeserializer;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor,
};

use super::yaml::{
    Kind, Node, Scalar, YamlError, duplicate_key_errors, is_merge, push_type_error, resolve_node,
    scalar_string, type_error,
};

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
    let value = T::deserialize(NodeDe {
        node,
        hint: "",
        errors: &errors,
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
        _ => "",
    }
}

/// A node being decoded, with the Go type a collection would have.
#[derive(Clone, Copy)]
struct NodeDe<'a> {
    node: &'a Node,
    hint: &'static str,
    errors: &'a RefCell<Vec<String>>,
}

impl<'a> NodeDe<'a> {
    fn child(&self, node: &'a Node, hint: &'static str) -> Self {
        Self {
            node,
            hint,
            errors: self.errors,
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
            Kind::Poison => Err(Fatal(self.node.value.clone())),
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
            Kind::Poison => Err(Fatal(self.node.value.clone())),
            Kind::Scalar => Ok(scalar_string(self.node).map_err(fatal)?.unwrap_or_default()),
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
            return Err(Fatal(self.node.value.clone()));
        }
        scalar_string(self.node).map_err(fatal)
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

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Fatal> {
        // yaml.v3 never looks at the values of unknown fields.
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        i8 i16 i32 i128 u8 u16 u32 u64 u128 f32 f64 char bytes byte_buf unit
        unit_struct newtype_struct tuple tuple_struct enum identifier
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
