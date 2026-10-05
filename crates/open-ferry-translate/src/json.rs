//! Loose value coercions matching gjson's `Result.String()`, `.Int()` and `.Bool()`.
//!
//! Upstream reads client JSON through gjson, which coerces instead of failing:
//! a numeric tool name becomes `"123"`, a missing field becomes `""`. These
//! helpers reproduce that so translated output matches upstream for sloppy input.
//!
//! [`exact`] reads JSON keeping each number's text as written, as upstream
//! keeps the client's text where it copies a value.

use std::borrow::Cow;

use serde_json::{Map, Number, Value};

use crate::go;

pub mod exact;
pub(crate) mod lenient;
pub(crate) mod raw;

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
                Ok(f) => go::format_float(f),
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

/// [`path`] for editing.
pub(crate) fn path_mut<'v>(value: &'v mut Value, path: &str) -> Option<&'v mut Value> {
    path.split('.')
        .try_fold(value, |value, key| value.get_mut(key))
}

/// sjson `Set` for a dotted path of object keys. A value on the way that isn't
/// an object is replaced by one, except an array: sjson can't set a key in an
/// array, so then nothing changes and this returns `false`. A new key goes
/// last.
pub(crate) fn set_path(value: &mut Value, path: &str, new: Value) -> bool {
    let mut value = value;
    for key in path.split('.') {
        if value.is_array() {
            // Only keys that were already there lead here, so nothing has
            // changed yet.
            return false;
        }
        if !value.is_object() {
            *value = Value::Object(Map::new());
        }
        let Value::Object(object) = value else {
            unreachable!("made an object above");
        };
        value = object.entry(key).or_insert(Value::Null);
    }
    *value = new;
    true
}

/// sjson `Delete` for a dotted path of object keys. Returns whether a value was
/// removed. The remaining keys keep their order.
pub(crate) fn delete_path(value: &mut Value, path: &str) -> bool {
    let (parent, key) = match path.rsplit_once('.') {
        Some((parent, key)) => match path_mut(value, parent) {
            Some(parent) => (parent, key),
            None => return false,
        },
        None => (value, path),
    };
    parent
        .as_object_mut()
        .is_some_and(|object| object.shift_remove(key).is_some())
}

/// gjson `Int()`. A float out of `i64`'s range saturates, where Go's result
/// depends on the CPU (amd64 gives the minimum `i64`).
pub(crate) fn int_of(value: &Value) -> i64 {
    match value {
        Value::Bool(true) => 1,
        Value::String(s) => parse_int(s).unwrap_or(0),
        Value::Number(n) => {
            let text = n.to_string();
            // Like Go's ParseFloat, a number too large for f64 reads as infinite.
            let f = text.parse::<f64>().unwrap_or(0.0);
            if f.abs() <= 9_007_199_254_740_991.0 {
                f as i64
            } else {
                parse_int(&text).unwrap_or(f as i64)
            }
        }
        _ => 0,
    }
}

/// gjson `Float()`, written as sjson writes a `float64`. `None` if it isn't
/// finite, which sjson writes as `+Inf`, `-Inf` or `NaN`: not JSON.
pub(crate) fn float_of(value: &Value) -> Option<Value> {
    let float: f64 = match value {
        Value::Number(number) => number.to_string().parse().unwrap_or(0.0),
        Value::String(text) => go::parse_float(text),
        Value::Bool(true) => 1.0,
        _ => 0.0,
    };
    if !float.is_finite() {
        return None;
    }
    serde_json::from_str(&go::format_float(float)).ok()
}

/// gjson `Bool()`.
pub(crate) fn bool_of(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(go::to_lower(s).as_str(), "1" | "t" | "true"),
        // A number too large for f64 reads as infinite, which isn't zero.
        Value::Number(n) => n.to_string().parse::<f64>().is_ok_and(|f| f != 0.0),
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

/// gjson `Value()` as sjson then writes it. Numbers become `float64`, which
/// sjson writes as plain decimals. Objects and arrays go through `json.Marshal`,
/// which sorts object keys and writes numbers in exponent form below 1e-6 and
/// from 1e21. A number beyond `f64`'s range is kept as written; Go can't write it.
/// Negative zero comes out as `0` where Go writes `-0`: serde_json reads `-0`
/// as the integer 0.
pub(crate) fn go_value(value: &Value) -> Value {
    match value {
        Value::Number(number) => go_float(number, false),
        _ => go_marshaled(value),
    }
}

/// gjson `Value()` inside a value `json.Marshal` writes, such as a field of a
/// Go map: as [`go_value`], but a number is written as `json.Marshal` writes
/// it too.
pub(crate) fn go_marshaled(value: &Value) -> Value {
    match value {
        Value::Number(number) => go_float(number, true),
        Value::Array(items) => Value::Array(items.iter().map(go_marshaled).collect()),
        Value::Object(fields) => {
            let mut fields: Vec<(&String, &Value)> = fields.iter().collect();
            fields.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key.clone(), go_marshaled(value)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// A number as Go writes a `float64`: `strconv.FormatFloat(f, 'f', -1, 64)`,
/// or as `json.Marshal` writes it when `marshaled`.
fn go_float(number: &Number, marshaled: bool) -> Value {
    let f = match number.to_string().parse::<f64>() {
        Ok(f) if f.is_finite() => f,
        _ => return Value::Number(number.clone()),
    };
    let text = if marshaled {
        go::json_float(f)
    } else {
        go::format_float(f)
    };
    serde_json::from_str(&text).unwrap_or_else(|_| Value::Number(number.clone()))
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
        // Halfway between two shortest decimals, which Go 1.26.4's gjson
        // rounds to even.
        for (text, want) in [
            ("2156163594508435.25", "2156163594508435.2"),
            ("-628643006909686.25", "-628643006909686.2"),
            ("2.98023223876953125e-8", "0.000000029802322387695312"),
            ("1e21", "1000000000000000000000"),
        ] {
            let number: Value = serde_json::from_str(text).unwrap();
            assert_eq!(str_of(Some(&number)), want, "{text}");
        }
    }

    // Not upstream's: sjson's `Set` of gjson's `Float()`, as Go 1.26.4
    // writes it.
    #[test]
    fn float_of_writes_as_sjson_does() {
        let parse = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        for (value, want) in [
            (parse("1.50"), "1.5"),
            (parse("1e21"), "1000000000000000000000"),
            (parse("1e-7"), "0.0000001"),
            (parse("2156163594508435.25"), "2156163594508435.2"),
            (json!("-191224687729131.625"), "-191224687729131.62"),
            (
                json!("2.98023223876953125e-8"),
                "0.000000029802322387695312",
            ),
            (json!(true), "1"),
        ] {
            let written = float_of(&value).map(|value| value.to_string());
            assert_eq!(written.as_deref(), Some(want), "{value}");
        }
        assert_eq!(float_of(&parse("1e400")), None);
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
    fn set_path_matches_sjson() {
        let set = |text: &str| {
            let mut value: Value = serde_json::from_str(text).unwrap();
            let changed = set_path(&mut value, "a.b.c", json!(true));
            (value.to_string(), changed)
        };
        let made = r#"{"a":{"b":{"c":true}}}"#;
        for text in [
            r#"{}"#,
            r#"{"a":"s"}"#,
            r#"{"a":null}"#,
            r#"{"a":{"b":1}}"#,
            r#""s""#,
            "null",
            "1",
        ] {
            assert_eq!(set(text), (made.to_owned(), true), "{text}");
        }
        assert_eq!(
            set(r#"{"x":1,"a":{"b":{"d":2,"c":1}}}"#),
            (r#"{"x":1,"a":{"b":{"d":2,"c":true}}}"#.to_owned(), true)
        );
        assert_eq!(
            set(r#"{"a":{"y":1},"z":2}"#),
            (r#"{"a":{"y":1,"b":{"c":true}},"z":2}"#.to_owned(), true)
        );
        for text in ["[1]", r#"{"a":[1]}"#, r#"{"a":{"b":[]}}"#] {
            assert_eq!(set(text), (text.to_owned(), false), "{text}");
        }
    }

    #[test]
    fn delete_path_keeps_key_order() {
        let mut value = json!({"a": {"x": 1, "y": 2, "z": 3}, "b": [1]});
        assert!(delete_path(&mut value, "a.y"));
        assert_eq!(value.to_string(), r#"{"a":{"x":1,"z":3},"b":[1]}"#);
        assert!(!delete_path(&mut value, "a.y"));
        assert!(!delete_path(&mut value, "b.0"));
        assert!(!delete_path(&mut value, "c.d"));
        assert!(delete_path(&mut value, "a"));
        assert_eq!(value.to_string(), r#"{"b":[1]}"#);
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
        let parse = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        assert_eq!(int_of(&parse("1e30")), i64::MAX);
        assert_eq!(int_of(&parse("1e400")), i64::MAX);
        assert_eq!(int_of(&parse("-1e400")), i64::MIN);
        assert_eq!(int_of(&parse("9223372036854775808")), i64::MIN);
    }

    #[test]
    fn go_value_matches_sjson() {
        let parse = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        let written = |text: &str| go_value(&parse(text)).to_string();
        assert_eq!(written("1.50"), "1.5");
        assert_eq!(written("1E+2"), "100");
        assert_eq!(written("-0.0"), "0");
        assert_eq!(written("9007199254740993"), "9007199254740992");
        assert_eq!(written("1e21"), "1000000000000000000000");
        // Kept as serde_json read it, which adds the exponent's sign.
        assert_eq!(written("1e400"), "1e+400");
        assert_eq!(written(r#""x""#), r#""x""#);
        assert_eq!(written("null"), "null");
        assert_eq!(
            written(r#"{"b":[1.50,1e21,1.5e-7,1e-6],"a":{"d":2,"c":0}}"#),
            r#"{"a":{"c":0,"d":2},"b":[1.5,1e+21,1.5e-7,0.000001]}"#
        );
        // Halfway between two shortest decimals, which Go 1.26.4 rounds to
        // even, with `FormatFloat` and with `json.Marshal`.
        assert_eq!(written("2156163594508435.25"), "2156163594508435.2");
        assert_eq!(
            written("2.98023223876953125e-8"),
            "0.000000029802322387695312"
        );
        assert_eq!(
            written("[-191224687729131.625,2.98023223876953125e-8]"),
            "[-191224687729131.62,2.9802322387695312e-8]"
        );
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
