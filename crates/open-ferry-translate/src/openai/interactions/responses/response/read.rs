// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/interactions_openai_responses_response.go
// (interactionsSSEPayload) (v8.0.10, MIT), and how tidwall/gjson v1.18.0's
// Parse and Get read text (MIT, see licenses/gjson-LICENSE).
// https://github.com/router-for-me/CLIProxyAPI

//! Reading an event or a response as upstream does.
//!
//! Upstream reads each event with gjson, which reads what it can from text
//! that isn't valid JSON, and checks gjson's `Valid` separately: an
//! `apply_patch` call in invalid JSON fails the stream. Several of its tests
//! send an event cut off before its end, so [`read`] reads such text the
//! way gjson reads a cut-off value, and reports whether it was valid.
//!
//! Deviations from upstream:
//! - Invalid JSON is read up to its first error, as if it had been cut off
//!   there: the members and elements before it are kept, a container left
//!   open is closed, and a member whose value doesn't end is dropped. gjson
//!   skips some errors and reads on past them.
//! - Text that isn't UTF-8 is read with each invalid sequence as U+FFFD.
//! - Where a key appears twice in an object, the last one counts; gjson
//!   reads the first.
//! - Valid JSON that serde_json can't read, nested more than 128 levels deep
//!   or holding an unpaired surrogate escape such as `\ud800`, is read as
//!   invalid JSON is, up to that point.

use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::go;
use crate::json::exact;

/// How deep [`read`] reads nested arrays and objects, as serde_json does.
const MAX_DEPTH: usize = 128;

/// `interactionsSSEPayload`: the JSON in an SSE frame or `data:` line, or
/// the text itself, trimmed. A frame's `data:` lines are joined with `\n`.
pub(super) fn sse_payload(raw: &[u8]) -> Cow<'_, [u8]> {
    let trimmed = go::trim_space(raw);
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return Cow::Borrowed(trimmed);
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        return Cow::Borrowed(go::trim_space(rest));
    }
    let lines: Vec<&[u8]> = trimmed
        .split(|&b| b == b'\n')
        .filter_map(|line| go::trim_space(line).strip_prefix(b"data:"))
        .map(go::trim_space)
        .collect();
    if lines.is_empty() {
        return Cow::Borrowed(trimmed);
    }
    Cow::Owned(lines.join(&b'\n'))
}

/// gjson's `Parse` of `text`, and gjson's `Valid`. The value is `None` where
/// gjson finds none, text that doesn't start (after white space) as a JSON
/// value does. A value that isn't an array or object, gjson reads nothing
/// from, so it may be read as `null` if it isn't valid.
pub(super) fn read(text: &[u8]) -> (Option<Value>, bool) {
    let valid = go::gjson_valid(text);
    let text = String::from_utf8_lossy(text);
    if valid && let Ok(value) = exact::from_str(&text) {
        return (Some(value), true);
    }
    (lenient(&text), valid)
}

/// What gjson's `Parse` finds in `text`, which isn't valid JSON, read up to
/// its first error.
fn lenient(text: &str) -> Option<Value> {
    // gjson skips every byte up to a space.
    let start = text.bytes().position(|b| b > b' ')?;
    let mut reader = Reader {
        text,
        at: start,
        depth: 0,
    };
    match text.as_bytes().get(start)? {
        b'{' | b'[' => reader.value().map(|(value, _)| value),
        b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' | b'n' | b't' | b'f' | b'"' => {
            Some(Value::Null)
        }
        _ => None,
    }
}

/// Reads JSON values from `text`, stopping at the first error.
struct Reader<'t> {
    text: &'t str,
    at: usize,
    depth: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// The value at the cursor, and whether it ended. `None` if none starts
    /// there, or a string, number or literal doesn't end. An array or object
    /// that doesn't end is what it holds so far.
    fn value(&mut self) -> Option<(Value, bool)> {
        self.skip_space();
        match self.peek()? {
            b'{' => self.container(true),
            b'[' => self.container(false),
            b'"' => self.string().map(|text| (Value::String(text), true)),
            _ => self.scalar().map(|value| (value, true)),
        }
    }

    /// An object (`object`) or array from its opening bracket.
    fn container(&mut self, object: bool) -> Option<(Value, bool)> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        self.depth += 1;
        self.at += 1;
        let mut fields = Map::new();
        let mut items = Vec::new();
        let close = if object { b'}' } else { b']' };
        let ended = self.members(object, close, &mut fields, &mut items);
        self.depth -= 1;
        let value = if object {
            Value::Object(fields)
        } else {
            Value::Array(items)
        };
        Some((value, ended))
    }

    /// Reads members into `fields`, or elements into `items`, up to `close`.
    /// Returns whether the container ended.
    fn members(
        &mut self,
        object: bool,
        close: u8,
        fields: &mut Map<String, Value>,
        items: &mut Vec<Value>,
    ) -> bool {
        self.skip_space();
        if self.peek() == Some(close) {
            self.at += 1;
            return true;
        }
        loop {
            let key = if object {
                self.skip_space();
                if self.peek() != Some(b'"') {
                    return false;
                }
                let Some(key) = self.string() else {
                    return false;
                };
                self.skip_space();
                if self.peek() != Some(b':') {
                    return false;
                }
                self.at += 1;
                Some(key)
            } else {
                None
            };
            let Some((value, ended)) = self.value() else {
                return false;
            };
            match key {
                Some(key) => {
                    fields.insert(key, value);
                }
                None => items.push(value),
            }
            if !ended {
                return false;
            }
            self.skip_space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(c) if c == close => {
                    self.at += 1;
                    return true;
                }
                _ => return false,
            }
        }
    }

    /// A string from its opening quote, if it ends and is valid.
    fn string(&mut self) -> Option<String> {
        let bytes = self.text.as_bytes();
        let start = self.at;
        let mut i = start + 1;
        loop {
            match bytes.get(i)? {
                b'"' => break,
                b'\\' => i += 2,
                _ => i += 1,
            }
        }
        let token = self.text.get(start..=i)?;
        let text = serde_json::from_str(token).ok()?;
        self.at = i + 1;
        Some(text)
    }

    /// A number or literal, if it is valid.
    fn scalar(&mut self) -> Option<Value> {
        let bytes = self.text.as_bytes();
        let start = self.at;
        let mut end = start;
        while bytes
            .get(end)
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        {
            end += 1;
        }
        let token = self.text.get(start..end)?;
        let value = match token {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" => Value::Null,
            _ if token.starts_with(['-', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9']) => {
                match exact::from_str(token) {
                    Ok(number @ Value::Number(_)) => number,
                    _ => return None,
                }
            }
            _ => return None,
        };
        self.at = end;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Not upstream's: upstream has no test of interactionsSSEPayload of its own.
    #[test]
    fn payloads_come_out_of_frames_and_lines() {
        for (raw, payload) in [
            (" {\"a\":1}\n", "{\"a\":1}"),
            ("data: {\"a\":1}", "{\"a\":1}"),
            ("event: x\ndata: {\"a\":\n data:1}\n\n", "{\"a\":\n1}"),
            ("event: x\n", "event: x"),
            (" [DONE] ", "[DONE]"),
            ("", ""),
        ] {
            assert_eq!(sse_payload(raw.as_bytes()).as_ref(), payload.as_bytes());
        }
    }

    // Not upstream's: how gjson reads a cut-off event.
    #[test]
    fn cut_off_text_keeps_what_ended() {
        let (value, valid) = read(br#"{"a":1,"b":{"c":[1,2"#);
        assert!(!valid);
        assert_eq!(value, Some(json!({ "a": 1, "b": { "c": [1, 2] } })));
        let (value, _) = read(br#"{"a":"x","b":"unended"#);
        assert_eq!(value, Some(json!({ "a": "x" })));
        let (value, _) = read(br#"{"a":1} trailing"#);
        assert_eq!(value, Some(json!({ "a": 1 })));
        assert_eq!(read(b"tru"), (Some(Value::Null), false));
        assert_eq!(read(b"<html>"), (None, false));
        assert_eq!(read(b"  "), (None, false));
        assert_eq!(read(br#"{"a":2}"#), (Some(json!({ "a": 2 })), true));
    }
}
