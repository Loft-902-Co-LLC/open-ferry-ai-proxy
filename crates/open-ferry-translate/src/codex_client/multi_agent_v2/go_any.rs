//! JSON decoded as Go's `encoding/json` decodes it into an `any` with
//! `UseNumber`, and written as `json.Marshal` writes that back, for
//! [`super::restore_response`].
//!
//! Go reads each byte of a string that isn't part of valid UTF-8, and each
//! `\u` escape of a surrogate that isn't half of a pair, as U+FFFD; keeps a
//! number's text as written; and of a key repeated in an object keeps the
//! last value. It writes object keys sorted by their bytes and escapes
//! strings for HTML.

use std::collections::BTreeMap;

use crate::go;

/// How deep arrays and objects may nest here. Go allows 10,000, but this
/// parser recurses, so it stops where serde_json does.
pub(super) const MAX_DEPTH: usize = 128;

/// A decoded JSON value.
pub(super) enum Node<'d> {
    /// `null`, `true` or `false`, as written.
    Literal(&'static str),
    /// A number, as written.
    Number(&'d str),
    String(String),
    Array(Vec<Node<'d>>),
    /// An object, sorted by key as Go writes it.
    Object(BTreeMap<String, Node<'d>>),
}

impl<'d> Node<'d> {
    /// `data` decoded, if it is one JSON value nested at most [`MAX_DEPTH`]
    /// deep. `data` should be valid JSON already: this checks only what it
    /// needs to.
    pub(super) fn parse(data: &'d [u8]) -> Option<Self> {
        let mut parser = Parser { data, pos: 0 };
        let node = parser.value(0)?;
        parser.skip_space();
        (parser.pos == data.len()).then_some(node)
    }

    /// The text of a string, or `None` for anything else.
    pub(super) fn as_str(&self) -> Option<&str> {
        match self {
            Node::String(text) => Some(text),
            _ => None,
        }
    }

    /// Writes this value as Go's `json.Marshal` writes it.
    pub(super) fn write(&self, out: &mut String) {
        match self {
            Node::Literal(text) | Node::Number(text) => out.push_str(text),
            Node::String(text) => out.push_str(&go::json_string(text)),
            Node::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Node::Object(fields) => {
                out.push('{');
                for (index, (key, item)) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&go::json_string(key));
                    out.push(':');
                    item.write(out);
                }
                out.push('}');
            }
        }
    }
}

struct Parser<'d> {
    data: &'d [u8],
    pos: usize,
}

impl<'d> Parser<'d> {
    fn skip_space(&mut self) {
        while matches!(self.data.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// The next byte after any space, consumed.
    fn next_token(&mut self) -> Option<u8> {
        self.skip_space();
        let byte = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(byte)
    }

    /// Consumes `text` if it comes next.
    fn eat(&mut self, text: &[u8]) -> bool {
        let found = self.data[self.pos..].starts_with(text);
        if found {
            self.pos += text.len();
        }
        found
    }

    fn value(&mut self, depth: usize) -> Option<Node<'d>> {
        self.skip_space();
        match *self.data.get(self.pos)? {
            b'{' => {
                if depth >= MAX_DEPTH {
                    return None;
                }
                self.pos += 1;
                let mut fields = BTreeMap::new();
                self.skip_space();
                if self.eat(b"}") {
                    return Some(Node::Object(fields));
                }
                loop {
                    self.skip_space();
                    let key = self.string()?;
                    if self.next_token()? != b':' {
                        return None;
                    }
                    let item = self.value(depth + 1)?;
                    fields.insert(key, item);
                    match self.next_token()? {
                        b',' => {}
                        b'}' => return Some(Node::Object(fields)),
                        _ => return None,
                    }
                }
            }
            b'[' => {
                if depth >= MAX_DEPTH {
                    return None;
                }
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_space();
                if self.eat(b"]") {
                    return Some(Node::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    match self.next_token()? {
                        b',' => {}
                        b']' => return Some(Node::Array(items)),
                        _ => return None,
                    }
                }
            }
            b'"' => self.string().map(Node::String),
            b't' => self.eat(b"true").then_some(Node::Literal("true")),
            b'f' => self.eat(b"false").then_some(Node::Literal("false")),
            b'n' => self.eat(b"null").then_some(Node::Literal("null")),
            b'-' | b'0'..=b'9' => {
                let start = self.pos;
                while matches!(
                    self.data.get(self.pos),
                    Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                ) {
                    self.pos += 1;
                }
                std::str::from_utf8(&self.data[start..self.pos])
                    .ok()
                    .map(Node::Number)
            }
            _ => None,
        }
    }

    /// The string at the current position, decoded as Go's `unquote` does.
    fn string(&mut self) -> Option<String> {
        if self.data.get(self.pos) != Some(&b'"') {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&byte) = self.data.get(self.pos) {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            push_lossy(&mut out, &self.data[start..self.pos]);
            match *self.data.get(self.pos)? {
                b'"' => {
                    self.pos += 1;
                    return Some(out);
                }
                b'\\' => {
                    let escape = *self.data.get(self.pos + 1)?;
                    self.pos += 2;
                    let c = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        _ => return None,
                    };
                    out.push(c);
                }
                _ => return None,
            }
        }
    }

    /// The character of a `\u` escape whose `\u` was just consumed. A
    /// surrogate takes the escape after it as its other half when that
    /// makes a pair, and is U+FFFD otherwise.
    fn unicode_escape(&mut self) -> Option<char> {
        let unit = hex4(self.data.get(self.pos..self.pos + 4)?)?;
        self.pos += 4;
        if !(0xD800..0xE000).contains(&unit) {
            return char::from_u32(unit);
        }
        if unit < 0xDC00
            && self.data.get(self.pos..self.pos + 2) == Some(b"\\u")
            && let Some(low) = self
                .data
                .get(self.pos + 2..self.pos + 6)
                .and_then(hex4)
                .filter(|low| (0xDC00..0xE000).contains(low))
        {
            self.pos += 6;
            return char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00));
        }
        Some(char::REPLACEMENT_CHARACTER)
    }
}

/// The value of four hex digits.
fn hex4(digits: &[u8]) -> Option<u32> {
    digits.iter().try_fold(0, |value, &digit| {
        Some(value * 16 + char::from(digit).to_digit(16)?)
    })
}

/// Appends `bytes`, each byte that isn't part of valid UTF-8 as U+FFFD.
fn push_lossy(out: &mut String, bytes: &[u8]) {
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for _ in chunk.invalid() {
            out.push(char::REPLACEMENT_CHARACTER);
        }
    }
}
