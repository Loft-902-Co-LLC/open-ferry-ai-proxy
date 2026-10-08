// Ported from tidwall/gjson v1.18.0 gjson.go (Parse, Get) (MIT), and Go's
// encoding/json (Unmarshal) (BSD-3-Clause, see licenses/Go-LICENSE), as
// CLIProxyAPI's sdk/cliproxy/session reads a request body with them
// (v8.0.20, MIT).
// https://github.com/tidwall/gjson
// https://github.com/router-for-me/CLIProxyAPI

//! A request body, parsed once for every read the session module makes of
//! it.
//!
//! Upstream reads a body three ways: gjson's `Parse` for the session
//! fields, which reads nothing unless the first byte past the white space
//! opens an object or array; gjson's `GetBytes` for the message history,
//! which scans to the first `{` or `[`; and `json.Unmarshal` into a map
//! for the derived identity, which takes only a body that is one object.
//! [`Payload`] parses the body once, from its first `{` or `[`, and keeps
//! which of the three the value counts for.
//!
//! Deviations from upstream:
//! - The value is parsed whole, and one that doesn't parse, such as one
//!   cut short, nested more than 128 deep, or holding a lone surrogate
//!   escape, has nothing in it: gjson reads what it can of broken JSON,
//!   and Go's `encoding/json` nests 10000 deep and reads a lone surrogate
//!   as U+FFFD.
//! - Bytes that aren't UTF-8 read as U+FFFD, one for each broken sequence:
//!   gjson keeps them as they are, and `encoding/json` gives one U+FFFD
//!   for each byte.
//! - A key given twice reads as its last value; gjson reads the first.
//! - An integer written `-0` reads as `0`; Go's `encoding/json` reads it as
//!   negative zero.

use serde_json::{Map, Value};

use crate::observe::usage::json::Node;

/// A request body, parsed once.
#[derive(Debug, Default)]
pub struct Payload {
    /// The first object or array in the body, from its first `{` or `[`.
    value: Option<Value>,
    /// Whether the body starts with that value past white space and
    /// control bytes, so gjson's `Parse` reads it.
    opens: bool,
    /// Whether the body is that value alone, an object with only JSON white
    /// space around it, so `json.Unmarshal` into a map reads it.
    whole: bool,
}

impl Payload {
    /// `bytes` parsed. Bytes that aren't UTF-8 read as U+FFFD.
    pub fn parse(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes);
        Self::parse_text(&text)
    }

    fn parse_text(text: &str) -> Self {
        let Some(start) = text.bytes().position(|b| b == b'{' || b == b'[') else {
            return Self::default();
        };
        let opens = text.bytes().position(|b| b > b' ') == Some(start);
        let Some(rest) = text.get(start..) else {
            return Self::default();
        };
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        let Some(Ok(value)) = values.next() else {
            return Self::default();
        };
        let end = values.byte_offset();
        let whole = opens
            && value.is_object()
            && text.get(..start).is_some_and(json_space_only)
            && rest.get(end..).is_some_and(json_space_only);
        Self {
            value: Some(value),
            opens,
            whole,
        }
    }

    /// The body as gjson's `Parse` reads it: nothing unless it starts with
    /// an object or array.
    pub(crate) fn root(&self) -> Node<'_> {
        match &self.value {
            Some(value) if self.opens => Node::of(value),
            _ => Node::default(),
        }
    }

    /// The body as gjson's `GetBytes` reads it: from its first `{` or `[`.
    pub(crate) fn scan(&self) -> Node<'_> {
        self.value.as_ref().map(Node::of).unwrap_or_default()
    }

    /// The body as `json.Unmarshal` into a map reads it: an object, alone.
    pub(crate) fn object(&self) -> Option<&Map<String, Value>> {
        self.value.as_ref().filter(|_| self.whole)?.as_object()
    }
}

/// Whether `text` is only JSON's white space: spaces, tabs, line feeds and
/// carriage returns.
fn json_space_only(text: &str) -> bool {
    text.bytes()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the three ways upstream reads a body, from one parse.
    #[test]
    fn reads_as_gjson_and_encoding_json_read() {
        let payload = Payload::parse(b" \n{\"a\":{\"b\":\"x\"}} \r\n");
        assert_eq!(payload.root().get("a.b").string(), "x");
        assert_eq!(payload.scan().get("a.b").string(), "x");
        assert!(payload.object().is_some());

        let prefixed = Payload::parse(b"data: {\"a\":1}");
        assert!(!prefixed.root().get("a").exists());
        assert_eq!(prefixed.scan().get("a").int(), 1);
        assert!(prefixed.object().is_none());

        let trailing = Payload::parse(b"{\"a\":1} x");
        assert_eq!(trailing.root().get("a").int(), 1);
        assert!(trailing.object().is_none());

        let control = Payload::parse(b"\x01{\"a\":1}");
        assert_eq!(control.root().get("a").int(), 1);
        assert!(control.object().is_none());

        let array = Payload::parse(b"[{\"a\":1}]");
        assert!(array.root().exists());
        assert!(array.object().is_none());

        assert!(!Payload::parse(b"").scan().exists());
        assert!(!Payload::parse(b"{\"a\":").root().exists());
        assert!(!Payload::parse(b"\"a\"").root().exists());

        let broken = Payload::parse(b"{\"a\":\"x\xffy\"}");
        assert_eq!(broken.root().get("a").string(), "x\u{fffd}y");
        assert!(broken.object().is_some());
    }
}
