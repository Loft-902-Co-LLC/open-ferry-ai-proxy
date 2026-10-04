// Ported from tidwall/gjson v1.18.0 gjson.go (Parse, Get, Result.Exists,
// Result.IsObject, Result.IsArray, Result.String, Result.Int, Result.Array,
// parseInt, safeInt) (MIT), as CLIProxyAPI's usage helpers read upstream
// responses with it (v8.0.10, MIT).
// https://github.com/tidwall/gjson
// https://github.com/router-for-me/CLIProxyAPI

//! The few gjson reads the usage statistics make of an upstream's answers:
//! a value at a dotted path, whether it exists and what kind it is, and its
//! text or integer as gjson coerces them.
//!
//! [`Doc::scan`] reads as gjson's `GetBytes` does, from the first `{` or
//! `[`; [`Doc::parse`] as `ParseBytes` then `Get`, which finds nothing
//! unless the first byte past the white space opens an object or array.
//! A path's parts are object keys, or indexes into an array, as
//! `candidates.0.finishReason`.
//!
//! Deviations from upstream:
//! - The value found is parsed whole, and one that doesn't parse, such as
//!   one cut short, nested more than 128 deep or holding a string that
//!   isn't UTF-8, has nothing in it. gjson reads what it can of broken
//!   JSON.
//! - A key given twice reads as its last value; gjson reads the first.
//! - The text of an object or array is its compact JSON, where gjson gives
//!   the text as it was written.

use std::borrow::Cow;

use open_ferry_translate::go;
use serde_json::Value;

/// A parsed answer, or nothing when there was no JSON to read.
#[derive(Debug)]
pub(crate) struct Doc(Option<Value>);

impl Doc {
    /// The first JSON object or array in `bytes`, found as gjson's
    /// `GetBytes` finds it: from the first `{` or `[`.
    pub(crate) fn scan(bytes: &[u8]) -> Self {
        let start = bytes.iter().position(|&b| b == b'{' || b == b'[');
        Self(start.and_then(|start| first_value(bytes.get(start..)?)))
    }

    /// The object or array `bytes` starts with past any white space or
    /// control bytes, as gjson's `ParseBytes` reads it before a `Get`.
    pub(crate) fn parse(bytes: &[u8]) -> Self {
        let start = bytes.iter().position(|&b| b > b' ');
        let opens = start
            .and_then(|start| bytes.get(start))
            .is_some_and(|&b| b == b'{' || b == b'[');
        if !opens {
            return Self(None);
        }
        Self(start.and_then(|start| first_value(bytes.get(start..)?)))
    }

    /// The whole value.
    pub(crate) fn root(&self) -> Node<'_> {
        Node(self.0.as_ref())
    }

    /// The value at `path` (gjson's `Get`).
    pub(crate) fn get(&self, path: &str) -> Node<'_> {
        self.root().get(path)
    }
}

/// The first JSON value `bytes` starts with; what follows it is ignored.
fn first_value(bytes: &[u8]) -> Option<Value> {
    serde_json::Deserializer::from_slice(bytes)
        .into_iter::<Value>()
        .next()?
        .ok()
}

/// A value gjson found, or none (gjson's `Result`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Node<'a>(Option<&'a Value>);

impl<'a> Node<'a> {
    /// `value` as a node.
    pub(crate) fn of(value: &'a Value) -> Self {
        Self(Some(value))
    }

    /// The value at `path` below this one: each part an object's key, or an
    /// index into an array.
    pub(crate) fn get(self, path: &str) -> Node<'a> {
        let found = self.0.and_then(|value| {
            path.split('.').try_fold(value, |value, part| match value {
                Value::Object(object) => object.get(part),
                Value::Array(items) => part.parse::<usize>().ok().and_then(|i| items.get(i)),
                _ => None,
            })
        });
        Node(found)
    }

    /// The value, if one was found.
    pub(crate) fn value(self) -> Option<&'a Value> {
        self.0
    }

    /// Whether a value was found; `null` counts (gjson's `Exists`).
    pub(crate) fn exists(self) -> bool {
        self.0.is_some()
    }

    /// Whether it is an object (gjson's `IsObject`).
    pub(crate) fn is_object(self) -> bool {
        self.0.is_some_and(Value::is_object)
    }

    /// Whether it is an array (gjson's `IsArray`).
    pub(crate) fn is_array(self) -> bool {
        self.0.is_some_and(Value::is_array)
    }

    /// Whether it is a string (gjson's `Type == String`).
    pub(crate) fn is_string(self) -> bool {
        self.0.is_some_and(Value::is_string)
    }

    /// Its items: an array's, nothing for no value or `null`, else the
    /// value alone (gjson's `Array`).
    pub(crate) fn array(self) -> Vec<Node<'a>> {
        match self.0 {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items.iter().map(Node::of).collect(),
            Some(other) => vec![Node::of(other)],
        }
    }

    /// Its text, as gjson's `String` gives it: a string as it is, nothing
    /// for no value or `null`, an integer as it was written, another number
    /// as a plain decimal, and an object or array as JSON.
    pub(crate) fn string(self) -> Cow<'a, str> {
        match self.0 {
            None | Some(Value::Null) => Cow::Borrowed(""),
            Some(Value::String(s)) => Cow::Borrowed(s),
            Some(Value::Bool(true)) => Cow::Borrowed("true"),
            Some(Value::Bool(false)) => Cow::Borrowed("false"),
            Some(Value::Number(n)) => {
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

    /// Its integer, as gjson's `Int` gives it: `true` is 1, a string holding
    /// only an integer is read, a number is cut to an integer, anything
    /// else is 0. A float out of `i64`'s range, or infinite, reads as the
    /// lowest `i64`, as Go converts it on amd64.
    pub(crate) fn int(self) -> i64 {
        match self.0 {
            Some(Value::Bool(true)) => 1,
            Some(Value::String(s)) => parse_int(s).unwrap_or(0),
            Some(Value::Number(n)) => {
                let text = n.to_string();
                // Like Go's ParseFloat, a number too large for f64 reads as
                // infinite.
                let f = text.parse::<f64>().unwrap_or(0.0);
                if f.abs() <= 9_007_199_254_740_991.0 {
                    f as i64
                } else {
                    parse_int(&text).unwrap_or_else(|| go_int64(f))
                }
            }
            _ => 0,
        }
    }
}

/// Go's `int64(f)` on amd64: truncated toward zero, and the lowest `i64`
/// for a float out of range or not a number.
fn go_int64(f: f64) -> i64 {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if (-LIMIT..LIMIT).contains(&f) {
        f as i64
    } else {
        i64::MIN
    }
}

/// gjson's strict integer parser: an optional `-`, then ASCII digits only,
/// wrapping as Go does.
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

/// gjson's `Valid`.
pub(crate) fn valid(bytes: &[u8]) -> bool {
    go::gjson_valid(bytes)
}

/// Go's `bytes.TrimSpace`.
pub(crate) fn trim_space(bytes: &[u8]) -> &[u8] {
    go::trim_space(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: paths reach keys and array items, gjson's `GetBytes`
    // scans to the first object while `ParseBytes` doesn't, and values
    // coerce as gjson coerces them.
    #[test]
    fn reads_as_gjson_reads() {
        let doc = Doc::scan(br#"data: {"a":{"b":[{"c":"x"},7]},"n":null,"f":1.50,"i":"-12"}"#);
        assert_eq!(doc.get("a.b.0.c").string(), "x");
        assert_eq!(doc.get("a.b.1").int(), 7);
        assert!(!doc.get("a.b.2").exists());
        assert!(!doc.get("a.b.x").exists());
        assert!(doc.get("n").exists());
        assert_eq!(doc.get("n").string(), "");
        assert!(doc.get("n").array().is_empty());
        assert_eq!(doc.get("f").string(), "1.5");
        assert_eq!(doc.get("f").int(), 1);
        assert_eq!(doc.get("i").int(), -12);
        assert_eq!(doc.get("a.b").array().len(), 2);
        assert_eq!(doc.get("a.b.0.c").array().len(), 1);
        assert!(doc.get("a").is_object() && doc.get("a.b").is_array());
        assert!(!Doc::parse(br#"data: {"a":1}"#).get("a").exists());
        assert_eq!(Doc::parse(b" \n{\"a\":1} trailing").get("a").int(), 1);
        assert!(!Doc::scan(b"{\"a\":").get("a").exists());
        assert_eq!(Node::default().int(), 0);
        assert_eq!(Doc::scan(b"{\"a\":\"1x\"}").get("a").int(), 0);
        assert_eq!(Doc::scan(b"{\"a\":true}").get("a").int(), 1);
    }
}
