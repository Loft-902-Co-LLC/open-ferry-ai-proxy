//! The gjson, sjson and Go JSON behaviour that upstream's Claude code relies
//! on, over `serde_json` values.
//!
//! These repeat a few crate-private helpers of `open_ferry_translate`, so
//! that this module doesn't change that crate.
//!
//! Paths are dotted, as gjson's: each part is an object key or, on an array,
//! an index.

use std::fmt::Write as _;

use serde_json::{Map, Value};

/// A request body as upstream holds it, in bytes: empty, not JSON, or JSON.
pub(crate) enum Body {
    Empty,
    Invalid,
    Json(Value),
}

impl Body {
    pub(crate) fn parse(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self::Empty;
        }
        serde_json::from_slice(bytes).map_or(Self::Invalid, Self::Json)
    }

    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    pub(crate) fn json(&self) -> Option<&Value> {
        match self {
            Self::Json(value) => Some(value),
            _ => None,
        }
    }
}

/// gjson `Get` for a dotted path of object keys and array indexes.
pub(crate) fn get<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(object) => object.get(key),
        Value::Array(items) => key.parse::<usize>().ok().and_then(|index| items.get(index)),
        _ => None,
    })
}

/// [`get`] for editing.
pub(crate) fn get_mut<'v>(value: &'v mut Value, path: &str) -> Option<&'v mut Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(object) => object.get_mut(key),
        Value::Array(items) => key
            .parse::<usize>()
            .ok()
            .and_then(|index| items.get_mut(index)),
        _ => None,
    })
}

/// gjson `Exists`: true for a `null` too.
pub(crate) fn exists(value: &Value, path: &str) -> bool {
    get(value, path).is_some()
}

/// gjson `String()`: a string as it is, missing or null as `""`, other
/// scalars as text, and objects and arrays as compact JSON.
pub(crate) fn str_of(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => {
            // An integer literal comes back as written; anything else as a
            // plain decimal float.
            let raw = number.to_string();
            let digits = raw.strip_prefix('-').unwrap_or(&raw);
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return raw;
            }
            match raw.parse::<f64>() {
                Ok(f) if f == f64::INFINITY => "+Inf".to_owned(),
                Ok(f) if f == f64::NEG_INFINITY => "-Inf".to_owned(),
                Ok(f) => f.to_string(),
                Err(_) => raw,
            }
        }
        Some(other) => other.to_string(),
    }
}

/// [`str_of`] at `path`.
pub(crate) fn str_at(value: &Value, path: &str) -> String {
    str_of(get(value, path))
}

/// The string at `path`, if it is one (gjson's `Type == String`).
pub(crate) fn string_at<'v>(value: &'v Value, path: &str) -> Option<&'v str> {
    get(value, path).and_then(Value::as_str)
}

/// gjson `Int()`. A float out of `i64`'s range saturates.
pub(crate) fn int_of(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Bool(true)) => 1,
        Some(Value::String(text)) => parse_int(text).unwrap_or(0),
        Some(Value::Number(number)) => {
            let text = number.to_string();
            let float = text.parse::<f64>().unwrap_or(0.0);
            if float.abs() <= 9_007_199_254_740_991.0 {
                float as i64
            } else {
                parse_int(&text).unwrap_or(float as i64)
            }
        }
        _ => 0,
    }
}

/// [`int_of`] at `path`.
pub(crate) fn int_at(value: &Value, path: &str) -> i64 {
    int_of(get(value, path))
}

/// gjson `Bool()`.
pub(crate) fn bool_of(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => matches!(
            open_ferry_translate::go::to_lower(text).as_str(),
            "1" | "t" | "true"
        ),
        Some(Value::Number(number)) => number
            .to_string()
            .parse::<f64>()
            .is_ok_and(|float| float != 0.0),
        _ => false,
    }
}

/// gjson's strict integer parser: an optional `-`, then ASCII digits.
fn parse_int(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if digits.is_empty() {
        return None;
    }
    let mut number: i64 = 0;
    for byte in digits.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        number = number.wrapping_mul(10).wrapping_add(i64::from(byte - b'0'));
    }
    Some(if negative {
        number.wrapping_neg()
    } else {
        number
    })
}

/// sjson `Set` for a dotted path of object keys and existing array
/// indexes. A value on the way that is neither an object nor an array is
/// replaced by an object; a new key goes last, and an existing one keeps its
/// place. Returns `false`, changing nothing, when an index isn't there.
pub(crate) fn set(value: &mut Value, path: &str, new: Value) -> bool {
    let mut value = value;
    for key in path.split('.') {
        if let Value::Array(items) = value {
            match key
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get_mut(index))
            {
                Some(item) => {
                    value = item;
                    continue;
                }
                None => return false,
            }
        }
        if !value.is_object() {
            *value = Value::Object(Map::new());
        }
        let Value::Object(object) = value else {
            return false;
        };
        value = object.entry(key).or_insert(Value::Null);
    }
    *value = new;
    true
}

/// sjson `Delete` for a dotted path. Returns whether a value was removed;
/// the remaining keys keep their order.
pub(crate) fn delete(value: &mut Value, path: &str) -> bool {
    let (parent, key) = match path.rsplit_once('.') {
        Some((parent, key)) => match get_mut(value, parent) {
            Some(parent) => (parent, key),
            None => return false,
        },
        None => (value, path),
    };
    parent
        .as_object_mut()
        .is_some_and(|object| object.shift_remove(key).is_some())
}

/// Go's `json.Marshal` of a string, with its HTML escapes.
pub(crate) fn go_json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' || matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Go's `strings.EqualFold`.
pub(crate) fn eq_fold(a: &str, b: &str) -> bool {
    a.chars().count() == b.chars().count()
        && a.chars().zip(b.chars()).all(|(x, y)| {
            x == y || x.to_lowercase().eq(y.to_lowercase()) || x.to_uppercase().eq(y.to_uppercase())
        })
}

/// Go's `strings.ToLower(strings.TrimSpace(text))`.
pub(crate) fn lower_trim(text: &str) -> String {
    open_ferry_translate::go::to_lower(text.trim())
}

/// What Go's `json.Unmarshal` into a struct needs: the struct's object, or
/// `None` for `null`, or Go's error for anything else.
pub(crate) fn object_or_null<'v>(
    value: &'v Value,
    field: &str,
) -> Result<Option<&'v Map<String, Value>>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Object(object) => Ok(Some(object)),
        _ => Err(type_error(value, field, "struct")),
    }
}

/// The struct field a JSON key fills: an exact match first, then one equal
/// under case folding, as Go's decoder matches.
pub(crate) fn key_of(key: &str, fields: &[&'static str]) -> Option<&'static str> {
    fields
        .iter()
        .find(|field| **field == key)
        .or_else(|| fields.iter().find(|field| eq_fold(field, key)))
        .copied()
}

/// Sets a Go `string` field: `null` leaves it.
pub(crate) fn set_string(target: &mut String, value: &Value, field: &str) -> Result<(), String> {
    match value {
        Value::Null => Ok(()),
        Value::String(text) => {
            text.clone_into(target);
            Ok(())
        }
        _ => Err(type_error(value, field, "string")),
    }
}

/// Sets a Go `int` field: only an integer literal in range fits.
pub(crate) fn set_int(target: &mut i64, value: &Value, field: &str) -> Result<(), String> {
    match value {
        Value::Null => Ok(()),
        Value::Number(number) => {
            *target = number
                .to_string()
                .parse()
                .map_err(|_| type_error(value, field, "int"))?;
            Ok(())
        }
        _ => Err(type_error(value, field, "int")),
    }
}

fn type_error(value: &Value, field: &str, go_type: &str) -> String {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    format!("json: cannot unmarshal {kind} into Go struct field {field} of type {go_type}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_like_gjson() {
        let value = json!({"a": {"b": [1, {"c": "x"}]}, "n": 1.50, "i": 7, "t": "TRUE", "z": null});
        assert_eq!(str_at(&value, "a.b.1.c"), "x");
        assert_eq!(str_at(&value, "n"), "1.5");
        assert_eq!(str_at(&value, "i"), "7");
        assert_eq!(str_at(&value, "z"), "");
        assert_eq!(str_at(&value, "a.b.1"), r#"{"c":"x"}"#);
        assert!(exists(&value, "z"));
        assert!(!exists(&value, "a.b.2"));
        assert!(!exists(&value, "i.x"));
        assert_eq!(int_at(&value, "n"), 1);
        assert_eq!(int_of(Some(&json!("12"))), 12);
        assert_eq!(int_of(Some(&json!("1.2"))), 0);
        assert!(bool_of(get(&value, "t")));
        assert!(!bool_of(Some(&json!("yes"))));
        assert_eq!(string_at(&value, "a.b.1.c"), Some("x"));
        assert_eq!(string_at(&value, "i"), None);
    }

    #[test]
    fn writes_like_sjson() {
        let mut value = json!({"a": 1, "list": [{"id": null}]});
        assert!(set(&mut value, "a", json!(2)));
        assert!(set(&mut value, "b.c", json!(true)));
        assert!(set(&mut value, "list.0.id", json!("x")));
        assert!(!set(&mut value, "list.3.id", json!("y")));
        assert_eq!(
            value.to_string(),
            r#"{"a":2,"list":[{"id":"x"}],"b":{"c":true}}"#
        );
        assert!(delete(&mut value, "a"));
        assert!(!delete(&mut value, "a"));
        assert!(delete(&mut value, "b.c"));
        assert!(delete(&mut value, "list.0.id"));
        assert_eq!(value.to_string(), r#"{"list":[{}],"b":{}}"#);
    }

    #[test]
    fn decodes_fields_like_go() {
        assert_eq!(
            key_of("ACCESS_TOKEN", &["access_token"]),
            Some("access_token")
        );
        let mut text = String::from("keep");
        set_string(&mut text, &Value::Null, "t.f").unwrap();
        assert_eq!(text, "keep");
        assert_eq!(
            set_string(&mut text, &json!(1), "t.f").unwrap_err(),
            "json: cannot unmarshal number into Go struct field t.f of type string"
        );
        let mut number = 0;
        set_int(&mut number, &json!(3600), "t.n").unwrap();
        assert_eq!(number, 3600);
        assert!(set_int(&mut number, &json!(1.5), "t.n").is_err());
        assert!(object_or_null(&json!([]), "t").is_err());
    }

    #[test]
    fn marshals_strings_like_go() {
        assert_eq!(go_json_string("a<b>&\"\n"), r#""a\u003cb\u003e\u0026\"\n""#);
    }
}
