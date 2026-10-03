// Ported from CLIProxyAPI sdk/auth/filestore.go (jsonEqual) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Credential JSON as upstream reads, writes and compares it: Go's
//! `json.Unmarshal` into a map or a struct field, `json.Marshal` of a
//! decoded map, and `jsonEqual` from `sdk/auth/filestore.go`.
//!
//! Deviations from upstream:
//! - Numbers keep their text when written back, where Go re-encodes the
//!   float64 (`1.50` stays `1.50`, not `1.5`).
//! - Invalid UTF-8 becomes one U+FFFD per invalid sequence, where Go has one
//!   per byte.
//! - Nesting deeper than 128 levels doesn't decode (serde_json's limit; Go
//!   allows 10000).

use std::fmt;

use open_ferry_translate::go::json_string;
use serde_json::{Map, Value};

use super::go::equal_fold;

/// Why a credential file didn't decode. Holds no file content.
#[derive(Debug)]
pub(crate) struct DecodeError(String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Go's `json.Unmarshal` of a document into `map[string]any`: the object,
/// or `None` for `null`. Anything else, and a number a float64 can't hold,
/// is an error.
///
/// Invalid UTF-8 becomes U+FFFD, as Go does, though one per invalid sequence
/// where Go has one per byte.
pub(crate) fn unmarshal_object(data: &[u8]) -> Result<Option<Map<String, Value>>, DecodeError> {
    let text = String::from_utf8_lossy(data);
    let value: Value = serde_json::from_str(&text).map_err(|err| DecodeError(err.to_string()))?;
    if !decodable(&value) {
        return Err(DecodeError("number out of float64 range".to_owned()));
    }
    match value {
        Value::Object(map) => Ok(Some(map)),
        Value::Null => Ok(None),
        _ => Err(DecodeError(
            "json: cannot unmarshal non-object into Go value of type map[string]interface {}"
                .to_owned(),
        )),
    }
}

/// Whether Go could decode every number in `value` into a float64.
pub(crate) fn decodable(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.as_f64().is_some(),
        Value::Array(items) => items.iter().all(decodable),
        Value::Object(map) => map.values().all(decodable),
        _ => true,
    }
}

/// The value Go's `json.Unmarshal` puts in a struct field named `name`: that
/// of the last key matching it without regard to case.
///
/// A key repeated exactly keeps its first position here, so where it is
/// interleaved with a case variant the variant may win where Go would take
/// the repeat.
pub(crate) fn fold_field<'a>(map: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    map.iter()
        .rev()
        .find(|(key, _)| equal_fold(key, name))
        .map(|(_, value)| value)
}

/// The values Go's `json.Unmarshal` meets for a struct field named `name`:
/// those of the keys matching it without regard to case, in order. A key
/// repeated exactly counts once, with its last value, at its first position.
pub(crate) fn fold_values<'a>(map: &'a Map<String, Value>, name: &str) -> Vec<&'a Value> {
    map.iter()
        .filter(|(key, _)| equal_fold(key, name))
        .map(|(_, value)| value)
        .collect()
}

/// [`fold_values`] for an object Go writes back out before decoding it into
/// a struct, as when it remarshals a decoded `map[string]any`: `json.Marshal`
/// sorts the keys, so they come in sorted order, wherever they were in the
/// file.
pub(crate) fn remarshaled_fold_values<'a>(
    map: &'a Map<String, Value>,
    name: &str,
) -> Vec<&'a Value> {
    let mut matches: Vec<_> = map
        .iter()
        .filter(|(key, _)| equal_fold(key, name))
        .collect();
    matches.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
    matches.into_iter().map(|(_, value)| value).collect()
}

/// Go's `json.Unmarshal` into a string, number or bool struct field, from
/// the `values` it meets for it: each decoded in turn by `decode`, a `null`
/// leaving the field as it was. `None` when one has the wrong type, which
/// fails Go's whole decode.
pub(crate) fn decode_field<'a, T>(
    values: impl IntoIterator<Item = &'a Value>,
    unset: T,
    decode: impl Fn(&'a Value) -> Option<T>,
) -> Option<T> {
    let mut field = unset;
    for value in values {
        if !value.is_null() {
            field = decode(value)?;
        }
    }
    Some(field)
}

/// Go's `json.Marshal` of a decoded JSON object: compact, keys sorted, and
/// strings escaped for HTML as Go escapes them. Numbers keep their text.
pub(crate) fn marshal_map(map: &Map<String, Value>) -> String {
    let mut out = String::new();
    write_map(&mut out, map);
    out
}

/// [`marshal_map`] for any JSON value.
#[cfg(test)]
pub(crate) fn marshal(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&json_string(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => write_map(out, map),
    }
}

fn write_map(out: &mut String, map: &Map<String, Value>) {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    out.push('{');
    for (i, (key, value)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(key));
        out.push(':');
        write_value(out, value);
    }
    out.push('}');
}

/// Upstream's `jsonEqual`: whether two JSON documents hold the same value,
/// comparing numbers as float64. A document that doesn't parse equals
/// nothing.
pub(crate) fn json_equal(a: &[u8], b: &[u8]) -> bool {
    match (
        serde_json::from_str::<Value>(&String::from_utf8_lossy(a)),
        serde_json::from_str::<Value>(&String::from_utf8_lossy(b)),
    ) {
        (Ok(a), Ok(b)) => deep_equal(&a, &b),
        _ => false,
    }
}

fn deep_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| deep_equal(a, b)))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| deep_equal(a, b))
        }
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        },
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marshal_sorts_keys_and_escapes_like_go() {
        let value: Value = serde_json::from_str(
            r#"{"b":1.0,"a":{"z":[true,null],"y":"<&>"},"c":12345678901234567890}"#,
        )
        .unwrap();
        assert_eq!(
            marshal(&value),
            r#"{"a":{"y":"\u003c\u0026\u003e","z":[true,null]},"b":1.0,"c":12345678901234567890}"#
        );
    }

    #[test]
    fn unmarshal_object_matches_go() {
        assert!(unmarshal_object(br#"{"a":1}"#).unwrap().is_some());
        assert!(unmarshal_object(b" null ").unwrap().is_none());
        assert!(unmarshal_object(b"[1]").is_err());
        assert!(unmarshal_object(b"not json").is_err());
        assert!(unmarshal_object(br#"{"a":[1e400]}"#).is_err());
        let mut data = br#"{"a":"x"#.to_vec();
        data.push(0xff);
        data.extend_from_slice(br#"y"}"#);
        let map = unmarshal_object(&data).unwrap().unwrap();
        assert_eq!(map["a"], Value::from("x\u{fffd}y"));
    }

    #[test]
    fn fold_field_takes_the_last_match() {
        let map: Map<String, Value> =
            serde_json::from_str(r#"{"exp":1,"EXP":2,"other":3}"#).unwrap();
        assert_eq!(fold_field(&map, "exp"), Some(&Value::from(2)));
        assert_eq!(fold_field(&map, "Other"), Some(&Value::from(3)));
        assert_eq!(fold_field(&map, "missing"), None);
    }

    #[test]
    fn fields_decode_every_matching_key_as_go_does() {
        let text = |object: &str, remarshaled: bool| {
            let map: Map<String, Value> = serde_json::from_str(object).unwrap();
            let values = if remarshaled {
                remarshaled_fold_values(&map, "name")
            } else {
                fold_values(&map, "name")
            };
            decode_field(values, String::new(), |value| {
                value.as_str().map(str::to_owned)
            })
        };
        // json.Marshal sorts the keys, so "name" comes after "Name"; read
        // directly, they come as written.
        for object in [
            r#"{"name":"upstream","Name":"other"}"#,
            r#"{"Name":"other","name":"upstream"}"#,
        ] {
            assert_eq!(text(object, true).as_deref(), Some("upstream"), "{object}");
        }
        assert_eq!(
            text(r#"{"name":"upstream","Name":"other"}"#, false).as_deref(),
            Some("other")
        );
        // A null leaves what an earlier key set.
        for object in [
            r#"{"Name":"upstream","name":null}"#,
            r#"{"name":null,"Name":"upstream"}"#,
        ] {
            assert_eq!(text(object, true).as_deref(), Some("upstream"), "{object}");
        }
        assert_eq!(
            text(r#"{"name":"a","NAME":"b","Name":null}"#, true).as_deref(),
            Some("a")
        );
        assert_eq!(
            text(r#"{"name":"a","NAME":"b","Name":null}"#, false).as_deref(),
            Some("b")
        );
        // A value of the wrong type under any matching key fails.
        for object in [
            r#"{"Name":42,"name":"upstream"}"#,
            r#"{"name":"upstream","Name":42}"#,
        ] {
            assert_eq!(text(object, true), None, "{object}");
            assert_eq!(text(object, false), None, "{object}");
        }
        assert_eq!(text(r#"{"alias":"x"}"#, true).as_deref(), Some(""));
    }

    #[test]
    fn json_equal_compares_numbers_as_floats() {
        assert!(json_equal(
            br#"{"a":1,"b":[1.0]}"#,
            br#"{"b":[1],"a":1.00}"#
        ));
        assert!(!json_equal(br#"{"a":1}"#, br#"{"a":"1"}"#));
        assert!(!json_equal(br#"{"a":1}"#, br#"{"a":1,"b":null}"#));
        assert!(!json_equal(b"not json", b"not json"));
        assert!(json_equal(b"null", b" null "));
    }
}
