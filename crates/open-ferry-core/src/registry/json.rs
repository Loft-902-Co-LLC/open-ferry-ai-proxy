//! Go's JSON decoding rules, for the few shapes the registry reads.
//!
//! Upstream decodes the model catalog, per-credential model aliases and
//! token claims into Go structs. So a key sets a field whatever its case,
//! unknown keys are skipped, `null` leaves a string, number or bool alone
//! and empties a list, and a value of the wrong type fails the whole decode.
//! These helpers follow those rules over a parsed [`Value`].

use serde_json::{Map, Value};

use super::equal_fold;

/// The field among `fields` that the key `key` sets, matched as Go matches
/// keys to struct fields: ignoring case. Go decodes an object's keys in
/// order, so a later key for the same field wins.
pub(super) fn field<'a>(key: &str, fields: &[&'a str]) -> Option<&'a str> {
    fields.iter().copied().find(|field| equal_fold(key, field))
}

/// Decodes a string into `target`.
pub(super) fn string(path: &str, value: &Value, target: &mut String) -> Result<(), String> {
    match value {
        Value::Null => {}
        Value::String(text) => target.clone_from(text),
        other => return Err(mismatch(path, other, "a string")),
    }
    Ok(())
}

/// Decodes an integer into `target`. Like Go's, it takes no fraction or
/// exponent, and nothing outside 64 bits.
pub(super) fn int(path: &str, value: &Value, target: &mut i64) -> Result<(), String> {
    match value {
        Value::Null => {}
        Value::Number(number) => match number.as_i64() {
            Some(number) => *target = number,
            None => return Err(format!("{path}: {number} is not a 64-bit integer")),
        },
        other => return Err(mismatch(path, other, "an integer")),
    }
    Ok(())
}

/// Decodes a bool into `target`.
pub(super) fn boolean(path: &str, value: &Value, target: &mut bool) -> Result<(), String> {
    match value {
        Value::Null => {}
        Value::Bool(flag) => *target = *flag,
        other => return Err(mismatch(path, other, "a bool")),
    }
    Ok(())
}

/// Decodes a list of strings into `target`. A `null` list empties it, and a
/// `null` item is an empty string.
pub(super) fn strings(path: &str, value: &Value, target: &mut Vec<String>) -> Result<(), String> {
    target.clear();
    let Some(items) = array(path, value)? else {
        return Ok(());
    };
    for (index, item) in items.iter().enumerate() {
        let mut text = String::new();
        string(&format!("{path}[{index}]"), item, &mut text)?;
        target.push(text);
    }
    Ok(())
}

/// An object, or `None` for `null`.
pub(super) fn object<'a>(
    path: &str,
    value: &'a Value,
) -> Result<Option<&'a Map<String, Value>>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Object(object) => Ok(Some(object)),
        other => Err(mismatch(path, other, "an object")),
    }
}

/// An array, or `None` for `null`.
pub(super) fn array<'a>(path: &str, value: &'a Value) -> Result<Option<&'a Vec<Value>>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Array(items) => Ok(Some(items)),
        other => Err(mismatch(path, other, "an array")),
    }
}

fn mismatch(path: &str, value: &Value, want: &str) -> String {
    let found = match value {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    };
    format!("{path}: want {want}, found {found}")
}
