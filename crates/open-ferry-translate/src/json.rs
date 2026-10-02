//! Loose value coercions matching gjson's `Result.String()`, `.Int()` and `.Bool()`.
//!
//! Upstream reads client JSON through gjson, which coerces instead of failing:
//! a numeric tool name becomes `"123"`, a missing field becomes `""`. These
//! helpers reproduce that so translated output matches upstream for sloppy input.

use std::borrow::Cow;

use serde_json::Value;

use crate::go;

/// gjson `String()`: strings as-is, missing/null as `""`, other scalars as text,
/// objects and arrays as JSON (compact here; gjson returns the client's raw bytes).
pub(crate) fn str_of(value: Option<&Value>) -> Cow<'_, str> {
    match value {
        None | Some(Value::Null) => Cow::Borrowed(""),
        Some(Value::String(s)) => Cow::Borrowed(s),
        Some(Value::Bool(true)) => Cow::Borrowed("true"),
        Some(Value::Bool(false)) => Cow::Borrowed("false"),
        Some(Value::Number(n)) => {
            // gjson returns integer literals verbatim and reformats anything else
            // as a plain decimal float ("1.50" -> "1.5", "1e3" -> "1000"). Go
            // writes a float too large for f64 as "+Inf" or "-Inf".
            let raw = n.to_string();
            let digits = raw.strip_prefix('-').unwrap_or(&raw);
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return Cow::Owned(raw);
            }
            Cow::Owned(match raw.parse::<f64>() {
                Ok(f) if f == f64::INFINITY => "+Inf".into(),
                Ok(f) if f == f64::NEG_INFINITY => "-Inf".into(),
                Ok(f) => f.to_string(),
                Err(_) => raw,
            })
        }
        Some(other) => Cow::Owned(other.to_string()),
    }
}

/// gjson `Get` for a dotted path of object keys, such as `usage.input_tokens`.
pub(crate) fn path<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| value.get(key))
}

/// gjson `Int()`.
pub(crate) fn int_of(value: &Value) -> i64 {
    match value {
        Value::Bool(true) => 1,
        Value::String(s) => parse_int(s).unwrap_or(0),
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f.abs() <= 9_007_199_254_740_991.0 {
                f as i64
            } else {
                parse_int(&n.to_string()).unwrap_or(f as i64)
            }
        }
        _ => 0,
    }
}

/// gjson `Bool()`.
pub(crate) fn bool_of(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(go::to_lower(s).as_str(), "1" | "t" | "true"),
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => false,
    }
}

/// Builds a JSON object, keeping field order. Unlike `json!`, which serializes
/// from a reference, this moves the values in, so large payloads such as base64
/// images are not copied again.
pub(crate) fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// gjson's strict integer parser: optional `-`, then ASCII digits only.
fn parse_int(s: &str) -> Option<i64> {
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for b in digits.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.wrapping_mul(10).wrapping_add(i64::from(b - b'0'));
    }
    Some(if negative { n.wrapping_neg() } else { n })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn str_of_matches_gjson() {
        assert_eq!(str_of(None), "");
        assert_eq!(str_of(Some(&json!(null))), "");
        assert_eq!(str_of(Some(&json!("x"))), "x");
        assert_eq!(str_of(Some(&json!(123))), "123");
        assert_eq!(str_of(Some(&json!(false))), "false");
        let big: Value = serde_json::from_str("123456789012345678901234567890").unwrap();
        assert_eq!(str_of(Some(&big)), "123456789012345678901234567890");
        let float: Value = serde_json::from_str("1.50").unwrap();
        assert_eq!(str_of(Some(&float)), "1.5");
        let exp: Value = serde_json::from_str("1e3").unwrap();
        assert_eq!(str_of(Some(&exp)), "1000");
        let huge: Value = serde_json::from_str("-1e400").unwrap();
        assert_eq!(str_of(Some(&huge)), "-Inf");
        assert_eq!(str_of(Some(&json!([1, 2]))), "[1,2]");
    }

    #[test]
    fn path_follows_object_keys() {
        let value = json!({"a": {"b": [1], "c": null}});
        assert_eq!(path(&value, "a.c"), Some(&Value::Null));
        assert_eq!(path(&value, "a.b"), Some(&json!([1])));
        assert_eq!(path(&value, "a.b.0"), None);
        assert_eq!(path(&value, "a.x"), None);
    }

    #[test]
    fn int_of_matches_gjson() {
        assert_eq!(int_of(&json!(2048)), 2048);
        assert_eq!(int_of(&json!(2048.9)), 2048);
        assert_eq!(int_of(&json!(-1)), -1);
        assert_eq!(int_of(&json!("512")), 512);
        assert_eq!(int_of(&json!("+512")), 0);
        assert_eq!(int_of(&json!("5x")), 0);
        assert_eq!(int_of(&json!(true)), 1);
        assert_eq!(int_of(&json!(null)), 0);
    }

    #[test]
    fn bool_of_matches_gjson() {
        assert!(bool_of(&json!(true)));
        assert!(bool_of(&json!("TRUE")));
        assert!(bool_of(&json!("1")));
        assert!(bool_of(&json!(2)));
        assert!(!bool_of(&json!("yes")));
        assert!(!bool_of(&json!(0)));
        assert!(!bool_of(&json!(null)));
    }
}
