//! The gjson, sjson and Go JSON behaviour that upstream's Codex code relies
//! on, over `serde_json` values.
//!
//! These repeat a few crate-private helpers of `open_ferry_translate`
//! (`json::str_of`, `int_of`, `bool_of`, `go::json_string`), so that this
//! module doesn't change that crate.
//!
//! Paths are dotted, as gjson's: each part is an object key or, on an array,
//! an index.

use std::fmt::Write as _;

use serde_json::{Map, Value};

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

/// gjson `Exists`.
pub(crate) fn exists(value: &Value, path: &str) -> bool {
    get(value, path).is_some()
}

/// gjson `String()`: a string as it is, missing or null as `""`, other
/// scalars as text, and objects and arrays as compact JSON (gjson gives the
/// raw text).
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

/// sjson `Delete` for a dotted path of object keys. Returns whether a value
/// was removed; the remaining keys keep their order.
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
pub(crate) fn json_string(text: &str) -> String {
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

/// Go's `strings.EqualFold` of `a`, trimmed, and `b`.
pub(crate) fn eq_fold_trim(a: &str, b: &str) -> bool {
    let a = a.trim();
    a.chars().count() == b.chars().count()
        && a.chars().zip(b.chars()).all(|(x, y)| {
            x == y || x.to_lowercase().eq(y.to_lowercase()) || x.to_uppercase().eq(y.to_uppercase())
        })
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
        assert_eq!(str_at(&value, "a.b.0"), "1");
        assert_eq!(str_at(&value, "a.b.1"), r#"{"c":"x"}"#);
        assert!(exists(&value, "z"));
        assert!(!exists(&value, "a.b.2"));
        assert_eq!(int_at(&value, "n"), 1);
        assert_eq!(int_of(Some(&json!("12"))), 12);
        assert_eq!(int_of(Some(&json!("1.2"))), 0);
        assert!(bool_of(get(&value, "t")));
        assert!(bool_of(Some(&json!(2))));
        assert!(!bool_of(Some(&json!("yes"))));
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
        assert_eq!(value.to_string(), r#"{"list":[{"id":"x"}],"b":{}}"#);
    }

    #[test]
    fn marshals_strings_like_go() {
        assert_eq!(json_string("a<b>&\"\n"), r#""a\u003cb\u003e\u0026\"\n""#);
    }

    #[test]
    fn folds_case_like_go() {
        assert!(eq_fold_trim(" TRUE ", "true"));
        assert!(eq_fold_trim("Codex", "codex"));
        assert!(!eq_fold_trim("codexx", "codex"));
    }
}
