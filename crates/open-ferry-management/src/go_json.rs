// Ported from Go's encoding/json decode.go and fold.go (Unmarshal,
// Decoder.Decode, and how they fill a struct, a slice, a map, a pointer
// and a json.RawMessage; foldName) and fmt's scan.go (Sscanf with %d into
// an int) (go1.27, BSD-3-Clause), as CLIProxyAPI
// internal/api/handlers/management/config_lists.go and config_basic.go
// read request bodies and the `index` query (v8.0.15, MIT).
// https://github.com/golang/go
// https://github.com/router-for-me/CLIProxyAPI

//! Request bodies read into the config's types as Go's JSON decoder reads
//! them into upstream's.
//!
//! [`whole`] reads a body as `json.Unmarshal` does, all of it one value,
//! and [`first`] as gin's `ShouldBindJSON` does, the first value only.
//! [`decode`] then reads a value into a config type: an object's keys
//! match a field's JSON name exactly, else regardless of case as Go folds
//! names, and other keys are skipped; `null` reads as the zero value, or as
//! `None` for an optional field; an integer must be written as one, within
//! range; and a value of the wrong type fails. [`fields`] picks a request's
//! own fields out of an object the same way. Each byte of a body that isn't
//! part of valid UTF-8 reads as U+FFFD.
//!
//! [`sscanf_int`] reads an `index` query as `fmt.Sscanf(s, "%d", &n)` does.
//!
//! The JSON names are the config's YAML names, as upstream gives both,
//! apart from a model's `thinking`, whose names use `_` where the YAML
//! uses `-`.
//!
//! Deviations from upstream:
//! - A key given more than once takes its last value. Go decodes each in
//!   turn, so there a later `null` leaves a plain field as it was, and a
//!   later object or list is merged into the earlier one.
//! - Values nest at most 128 deep (serde_json's limit); Go allows 10000.
//! - A string holding an unpaired UTF-16 surrogate escape fails the read;
//!   Go reads the surrogate as U+FFFD.

use std::borrow::Cow;
use std::fmt;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, IntoDeserializer as _, MapAccess, SeqAccess, Visitor,
};
use serde::{Deserialize as _, Deserializer};
use serde_json::{Map, Number, Value};

use crate::bind::field_index;
use crate::go::lossy;

/// The serde name of upstream's `registry.ThinkingSupport`, whose JSON
/// names differ from its YAML names.
const THINKING: &str = "registry.ThinkingSupport";

/// `body` as one JSON value, as `json.Unmarshal` reads it: anything but
/// white space after the value fails.
pub(crate) fn whole(body: &[u8]) -> Option<Value> {
    serde_json::from_str(&lossy(body)).ok()
}

/// The first JSON value in `body`, as gin's `ShouldBindJSON` reads it with
/// `json.Decoder.Decode`: what follows is ignored, and an empty body fails.
pub(crate) fn first(body: &[u8]) -> Option<Value> {
    let text = lossy(body);
    let mut deserializer = serde_json::Deserializer::from_str(&text);
    Value::deserialize(&mut deserializer).ok()
}

/// `value` read into a `T` as Go's decoder reads it into upstream's type;
/// `None` where Go's decode fails.
pub(crate) fn decode<T: DeserializeOwned>(value: &Value) -> Option<T> {
    T::deserialize(GoValue(value)).ok()
}

/// A field of a request body that doesn't read as its type: the request
/// answers 400 `invalid body`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Invalid;

/// A pointer field's value (`*T` in upstream's request): `None` when the
/// key is missing or `null`, else the value read as a `T`.
pub(crate) fn pointer<T: DeserializeOwned>(value: Option<&Value>) -> Result<Option<T>, Invalid> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => decode(value).map(Some).ok_or(Invalid),
    }
}

/// A plain field's value (`T` in upstream's request): the zero value when
/// the key is missing or `null`.
pub(crate) fn plain<T: DeserializeOwned + Default>(value: Option<&Value>) -> Result<T, Invalid> {
    pointer(value).map(Option::unwrap_or_default)
}

/// The values of `names` in `value`, an object or `null`, as Go fills a
/// struct with those JSON names: each key goes to the name it matches
/// exactly, else to the first it matches as Go folds names, and the last
/// key for a name wins. `None` when `value` is neither, which Go fails to
/// decode into a struct.
pub(crate) fn fields<'a, const N: usize>(
    value: &'a Value,
    names: [&str; N],
) -> Option<[Option<&'a Value>; N]> {
    let mut out = [None; N];
    for (slot, found) in out.iter_mut().zip(field_values(value, &names)?) {
        *slot = found;
    }
    Some(out)
}

/// [`fields`], for names known only at run time.
pub(crate) fn field_values<'a>(value: &'a Value, names: &[&str]) -> Option<Vec<Option<&'a Value>>> {
    let mut out = vec![None; names.len()];
    match value {
        Value::Null => Some(out),
        Value::Object(map) => {
            for (key, value) in map {
                if let Some(slot) = field_index(names, key).and_then(|index| out.get_mut(index)) {
                    *slot = Some(value);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// An integer, as Go reads a JSON number into an `int`: the number as
/// written, which must have no fraction or exponent and fit 64 bits.
pub(crate) fn int(number: &Number) -> Option<i64> {
    number.to_string().parse().ok()
}

/// The integer at the start of `s`, as Go's `fmt.Sscanf(s, "%d", &n)` reads
/// one into an `int`: after any spaces but a newline, an optional sign and
/// at least one ASCII digit; whatever follows the digits is ignored.
pub(crate) fn sscanf_int(s: &[u8]) -> Option<i64> {
    let text = lossy(s);
    let rest = text.trim_start_matches(|c: char| c != '\n' && is_scan_space(c));
    let (sign, rest) = match rest.as_bytes().first() {
        Some(b'+') => ("", rest.get(1..)?),
        Some(b'-') => ("-", rest.get(1..)?),
        _ => ("", rest),
    };
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    format!("{sign}{}", rest.get(..digits)?).parse().ok()
}

/// The space `fmt`'s scanner skips (its `space` table).
fn is_scan_space(c: char) -> bool {
    matches!(
        c,
        '\t'..='\r'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// Why a value didn't read as its type.
#[derive(Debug)]
pub(crate) struct GoError(String);

impl fmt::Display for GoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GoError {}

impl de::Error for GoError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }
}

/// A JSON value, read as Go's decoder reads it into a typed field.
#[derive(Clone, Copy)]
struct GoValue<'a>(&'a Value);

impl GoValue<'_> {
    fn mismatch(self, want: &str) -> GoError {
        let got = match self.0 {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        GoError(format!("cannot unmarshal {got} into {want}"))
    }

    /// The integer of an integer field; `null` reads as 0.
    fn int(self) -> Result<i64, GoError> {
        match self.0 {
            Value::Null => Ok(0),
            Value::Number(number) => int(number).ok_or_else(|| self.mismatch("int")),
            _ => Err(self.mismatch("int")),
        }
    }

    /// The integer of an unsigned field; `null` reads as 0.
    fn uint(self) -> Result<u64, GoError> {
        match self.0 {
            Value::Null => Ok(0),
            Value::Number(number) => number
                .to_string()
                .parse()
                .map_err(|_| self.mismatch("uint")),
            _ => Err(self.mismatch("uint")),
        }
    }
}

macro_rules! signed {
    ($($method:ident),*) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
            visitor.visit_i64(self.int()?)
        }
    )*};
}

macro_rules! unsigned {
    ($($method:ident),*) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
            visitor.visit_u64(self.uint()?)
        }
    )*};
}

impl<'de> Deserializer<'de> for GoValue<'_> {
    type Error = GoError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Bool(value) => visitor.visit_bool(*value),
            Value::Number(number) => match int(number) {
                Some(value) => visitor.visit_i64(value),
                None => visitor.visit_str(&number.to_string()),
            },
            Value::String(value) => visitor.visit_str(value),
            Value::Array(items) => visitor.visit_seq(Items(items.iter())),
            Value::Object(map) => visitor.visit_map(Entries::new(
                map.iter().map(|(key, value)| (key.as_str(), value)),
            )),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        match self.0 {
            Value::Null => visitor.visit_bool(false),
            Value::Bool(value) => visitor.visit_bool(*value),
            _ => Err(self.mismatch("bool")),
        }
    }

    signed!(
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64
    );
    unsigned!(
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64
    );

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        self.deserialize_f64(visitor)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        match self.0 {
            Value::Null => visitor.visit_f64(0.0),
            Value::Number(number) => match number.to_string().parse::<f64>() {
                Ok(value) if value.is_finite() => visitor.visit_f64(value),
                _ => Err(self.mismatch("float64")),
            },
            _ => Err(self.mismatch("float64")),
        }
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        match self.0 {
            Value::Null => visitor.visit_str(""),
            Value::String(value) => visitor.visit_str(value),
            _ => Err(self.mismatch("string")),
        }
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, GoError> {
        Err(self.mismatch("[]byte"))
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, GoError> {
        Err(self.mismatch("[]byte"))
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, GoError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, GoError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        match self.0 {
            Value::Null => visitor.visit_seq(Items([].iter())),
            Value::Array(items) => visitor.visit_seq(Items(items.iter())),
            _ => Err(self.mismatch("slice")),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, GoError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, GoError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        match self.0 {
            Value::Null => visitor.visit_map(Entries::new(std::iter::empty())),
            Value::Object(map) => visitor.visit_map(Entries::new(
                map.iter().map(|(key, value)| (key.as_str(), value)),
            )),
            _ => Err(self.mismatch("map")),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, GoError> {
        match self.0 {
            Value::Null => visitor.visit_map(Entries::new(std::iter::empty())),
            Value::Object(map) => {
                visitor.visit_map(Entries::new(struct_entries(name, fields, map).into_iter()))
            }
            _ => Err(self.mismatch("struct")),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        _variants: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, GoError> {
        Err(self.mismatch(name))
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, GoError> {
        visitor.visit_unit()
    }
}

/// An object's entries as a struct's fields: each key goes to the field
/// whose JSON name it matches, the last key for a field winning, and the
/// fields come out in order under their serde names.
fn struct_entries<'a>(
    name: &str,
    fields: &'static [&'static str],
    map: &'a Map<String, Value>,
) -> Vec<(&'static str, &'a Value)> {
    let json_names: Vec<Cow<'_, str>> = fields
        .iter()
        .map(|field| {
            if name == THINKING {
                Cow::Owned(field.replace('-', "_"))
            } else {
                Cow::Borrowed(*field)
            }
        })
        .collect();
    let json_names: Vec<&str> = json_names.iter().map(AsRef::as_ref).collect();
    let mut slots: Vec<Option<&'a Value>> = vec![None; fields.len()];
    for (key, value) in map {
        if let Some(slot) = field_index(&json_names, key).and_then(|index| slots.get_mut(index)) {
            *slot = Some(value);
        }
    }
    fields
        .iter()
        .zip(slots)
        .filter_map(|(field, value)| value.map(|value| (*field, value)))
        .collect()
}

/// An array's items.
struct Items<'a>(std::slice::Iter<'a, Value>);

impl<'de> SeqAccess<'de> for Items<'_> {
    type Error = GoError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, GoError> {
        self.0
            .next()
            .map(|value| seed.deserialize(GoValue(value)))
            .transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

/// An object's entries, or a struct's fields.
struct Entries<'a, I> {
    entries: I,
    value: Option<&'a Value>,
}

impl<'a, I: Iterator<Item = (&'a str, &'a Value)>> Entries<'a, I> {
    fn new(entries: I) -> Self {
        Self {
            entries,
            value: None,
        }
    }
}

impl<'de, 'a, I: Iterator<Item = (&'a str, &'a Value)>> MapAccess<'de> for Entries<'a, I> {
    type Error = GoError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, GoError> {
        match self.entries.next() {
            Some((key, value)) => {
                self.value = Some(value);
                seed.deserialize(key.into_deserializer()).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, GoError> {
        match self.value.take() {
            Some(value) => seed.deserialize(GoValue(value)),
            None => Err(GoError("a value without a key".to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use open_ferry_core::config::{
        ClaudeModel, CodexKey, OAuthModelAlias, RequestScopedErrorRule, ThinkingSupport,
    };
    use serde_json::json;

    use super::*;

    // Not upstream's: Go's json.Unmarshal and Decoder.Decode, as probed.
    #[test]
    fn whole_reads_one_value_and_first_the_first() {
        assert_eq!(whole(b" [1] "), Some(json!([1])));
        assert_eq!(whole(b"[1] x"), None);
        assert_eq!(whole(b""), None);
        assert_eq!(first(b"{\"a\":1} trailing {"), Some(json!({"a": 1})));
        assert_eq!(first(b""), None);
        assert_eq!(first(b"  "), None);
        assert_eq!(first(b"nul"), None);
        assert_eq!(
            first(b"\"a\xffb\""),
            Some(Value::String("a\u{fffd}b".into()))
        );
    }

    // Not upstream's: how Go's decoder fills config.CodexKey and its models.
    #[test]
    fn structs_read_as_go_reads_them() {
        let value = json!({
            "API-KEY": "k",
            "base-url": "b",
            "weight": 3,
            "disable-cooling": null,
            "unknown": {"x": [1, 2.5]},
            "models": [{"name": "n", "thinking": {"zero_allowed": true, "max": 9}}, null],
            "headers": {"A": "1", "B": null},
            "excluded-models": null,
            "request-scoped-errors": [{"status": 429, "match": ["m"], "action": "stop"}],
        });
        let key: CodexKey = decode(&value).unwrap();
        assert_eq!(key.api_key, "k");
        assert_eq!(key.base_url, "b");
        assert_eq!(key.weight, Some(3));
        assert_eq!(key.disable_cooling, None);
        assert_eq!(key.models.len(), 2);
        assert_eq!(key.models[0].name, "n");
        assert_eq!(
            key.models[0].thinking,
            Some(ThinkingSupport {
                max: 9,
                zero_allowed: true,
                ..ThinkingSupport::default()
            })
        );
        assert_eq!(key.models[1].name, "");
        assert_eq!(
            key.headers,
            BTreeMap::from([("A".into(), "1".into()), ("B".into(), String::new())])
        );
        assert!(key.excluded_models.is_empty());
        assert_eq!(
            key.request_scoped_errors,
            vec![RequestScopedErrorRule {
                status: 429,
                matches: vec!["m".into()],
                action: "stop".into(),
                ..RequestScopedErrorRule::default()
            }]
        );
        // The YAML names of `thinking` aren't its JSON names.
        let model: ClaudeModel = decode(&json!({"thinking": {"zero-allowed": true}})).unwrap();
        assert_eq!(model.thinking, Some(ThinkingSupport::default()));
        // The last key for a field wins, matched exactly or folded.
        let alias: OAuthModelAlias = decode(&json!({"name": "a", "NAME": "b"})).unwrap();
        assert_eq!(alias.name, "b");
        let alias: OAuthModelAlias = decode(&json!({"Fork": true, "fork": false})).unwrap();
        assert!(!alias.fork);
        assert_eq!(decode::<CodexKey>(&Value::Null), Some(CodexKey::default()));
    }

    // Not upstream's: Go's decoder refuses these.
    #[test]
    fn wrong_types_fail() {
        for value in [
            json!({"api-key": 1}),
            json!({"priority": "1"}),
            json!({"priority": 1.0}),
            json!({"priority": 1e2}),
            json!({"priority": 9_223_372_036_854_775_808_u64}),
            json!({"weight": true}),
            json!({"websockets": "true"}),
            json!({"headers": []}),
            json!({"headers": {"A": 1}}),
            json!({"models": {}}),
            json!({"models": [1]}),
            json!({"excluded-models": [1]}),
            json!([]),
            json!("x"),
        ] {
            assert_eq!(decode::<CodexKey>(&value), None, "{value}");
        }
        assert_eq!(
            decode::<CodexKey>(&json!({"priority": -0})).map(|key| key.priority),
            Some(0)
        );
    }

    // Not upstream's: a request's own fields, as Go fills its struct.
    #[test]
    fn fields_pick_a_request_apart() {
        let value = json!({"Index": 1, "match": "m", "MATCH": "x", "other": 2});
        let [index, matched, missing] = fields(&value, ["index", "match", "value"]).unwrap();
        assert_eq!(index, Some(&json!(1)));
        assert_eq!(matched, Some(&json!("x")));
        assert_eq!(missing, None);
        assert_eq!(fields(&Value::Null, ["index"]), Some([None]));
        assert_eq!(fields(&json!([]), ["index"]), None);
        assert_eq!(pointer::<i64>(Some(&json!(null))), Ok(None));
        assert_eq!(pointer::<i64>(Some(&json!(4))), Ok(Some(4)));
        assert_eq!(pointer::<i64>(Some(&json!("4"))), Err(Invalid));
        assert_eq!(plain::<String>(None), Ok(String::new()));
    }

    // Not upstream's: fmt.Sscanf(s, "%d", &n), as probed with Go 1.26.
    #[test]
    fn sscanf_reads_as_go_does() {
        for (input, want) in [
            ("1", Some(1)),
            (" 1", Some(1)),
            ("\t\u{3000}2", Some(2)),
            ("\r3", Some(3)),
            ("+4", Some(4)),
            ("-5", Some(-5)),
            ("1abc", Some(1)),
            ("1_0", Some(1)),
            ("0x1", Some(0)),
            ("1.5", Some(1)),
            ("9223372036854775807", Some(i64::MAX)),
            ("9223372036854775808", None),
            ("", None),
            ("  ", None),
            ("+", None),
            ("-", None),
            ("+-1", None),
            ("\n1", None),
            ("\r\n1", None),
            ("a1", None),
            ("\u{ff11}", None),
        ] {
            assert_eq!(sscanf_int(input.as_bytes()), want, "{input:?}");
        }
        assert_eq!(sscanf_int(b"\xff1"), None);
    }
}
