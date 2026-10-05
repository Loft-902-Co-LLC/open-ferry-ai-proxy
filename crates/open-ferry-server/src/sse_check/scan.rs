// Ported from sseJSONValidationDataPayload in CLIProxyAPI
// sdk/api/handlers/handlers_stream.go, responsesSSEDataLinesValid and
// responsesSSEHasField in sdk/api/handlers/openai/openai_responses_handlers.go,
// and Go's json.Valid (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What an event that is still arriving says so far, read a byte at a time.
//!
//! Deviations from upstream:
//! - Upstream reads the whole held-back event again for each chunk that
//!   doesn't finish it, so an event that arrives in many chunks takes time
//!   that grows with the square of its size. An [`EventScan`] reads each byte
//!   once, holding what it has learnt, and gives the same answers.

/// What a `data:` line holding only this says: the stream is over.
const DONE: &[u8] = b"[DONE]";

/// What an event says so far: whether it is blank, which fields it has, and
/// whether its data may go to the client. Its answers are those of
/// `go::trim_space`, [`super::data_lines_valid`] and `responsesSSEHasField`
/// for the bytes [`EventScan::advance`] last read.
#[derive(Debug, Default)]
pub(crate) struct EventScan {
    /// How far the event has been read.
    pos: usize,
    /// Where the read is in the current line.
    line: Line,
    /// Where the white space in a data line's value starts, when it may be
    /// the end of the line. It goes to the JSON check only if more of the
    /// value follows.
    held: Option<usize>,
    /// Whether the read stopped before a character that is cut off.
    cut: bool,
    /// Whether a line has had anything but white space.
    printed: bool,
    /// Whether a line starts with `event:`.
    event: bool,
    /// Whether a line starts with `data:`.
    data: bool,
    /// How many data lines have a value once trimmed.
    values: usize,
    /// How much of `[DONE]` the first value matches.
    done_len: usize,
    /// Whether the first value differs from `[DONE]`.
    not_done: bool,
    /// The values joined with `\n`, checked as JSON.
    json: JsonCheck,
}

/// Where an [`EventScan`] is in a line.
#[derive(Clone, Copy, Debug, Default)]
enum Line {
    /// Before anything but white space.
    #[default]
    Lead,
    /// In what may be a field's name, which starts here.
    Name(usize),
    /// In a line that isn't data, or past `event:`.
    Other,
    /// In a data line's value; `started` once past its leading white space.
    Data { started: bool },
}

impl EventScan {
    /// Reads `event` on from where the last read stopped. `event` is what
    /// that read was given, with any more of the event after it. Gives how
    /// many bytes it moved past.
    pub(crate) fn advance(&mut self, event: &[u8]) -> usize {
        let from = self.pos;
        self.cut = false;
        while let Some(&byte) = event.get(self.pos) {
            let rest = event.get(self.pos..).unwrap_or_default();
            match self.line {
                Line::Lead => match space_at(rest) {
                    Space::Char(len) => self.pos += len,
                    Space::Cut => {
                        self.cut = true;
                        break;
                    }
                    Space::No => {
                        self.printed = true;
                        self.line = Line::Name(self.pos);
                    }
                },
                Line::Name(start) => {
                    let name = event.get(start..).unwrap_or_default();
                    if name.starts_with(b"data:") {
                        self.data = true;
                        self.pos = start + 5;
                        self.line = Line::Data { started: false };
                    } else if name.starts_with(b"event:") {
                        self.event = true;
                        self.pos = start + 6;
                        self.line = Line::Other;
                    } else if b"data:".starts_with(name) || b"event:".starts_with(name) {
                        // The name may yet be a field's: wait for more.
                        self.pos = event.len();
                    } else {
                        self.line = Line::Other;
                    }
                }
                Line::Other => match rest.iter().position(|&b| b == b'\n') {
                    Some(end) => {
                        self.pos += end + 1;
                        self.line = Line::Lead;
                    }
                    None => self.pos = event.len(),
                },
                Line::Data { .. } if byte == b'\n' => {
                    // White space at the end of a value is trimmed.
                    self.held = None;
                    self.pos += 1;
                    self.line = Line::Lead;
                }
                Line::Data { started } => match space_at(rest) {
                    Space::Char(len) => {
                        if started && self.held.is_none() {
                            self.held = Some(self.pos);
                        }
                        self.pos += len;
                    }
                    Space::Cut => {
                        self.cut = true;
                        break;
                    }
                    Space::No => {
                        if !started {
                            self.line = Line::Data { started: true };
                            if self.values > 0 {
                                self.json.push(b'\n');
                            }
                            self.values += 1;
                        } else if let Some(held) = self.held.take() {
                            for &b in event.get(held..self.pos).unwrap_or_default() {
                                self.take(b);
                            }
                        }
                        self.take(byte);
                        self.pos += 1;
                    }
                },
            }
        }
        self.pos - from
    }

    /// Adds a byte of a data line's trimmed value.
    fn take(&mut self, byte: u8) {
        self.json.push(byte);
        if self.values == 1 {
            if DONE.get(self.done_len) == Some(&byte) {
                self.done_len += 1;
            } else {
                self.not_done = true;
            }
        }
    }

    /// Whether the event is all white space (`len(bytes.TrimSpace(event))
    /// == 0`).
    pub(crate) fn is_blank(&self) -> bool {
        !self.printed && !self.cut
    }

    /// Whether a line starts with `event:` (`responsesSSEHasField`).
    pub(crate) fn has_event(&self) -> bool {
        self.event
    }

    /// Whether a line starts with `data:` (`responsesSSEHasField`).
    pub(crate) fn has_data(&self) -> bool {
        self.data
    }

    /// Whether the event's data, if it has any, may go to the client:
    /// nothing, `[DONE]`, or JSON ([`super::data_lines_valid`]).
    pub(crate) fn data_valid(&self) -> bool {
        if !self.data {
            return true;
        }
        // A value that ends in a cut-off character isn't JSON.
        if self.cut && matches!(self.line, Line::Data { .. }) {
            return false;
        }
        match self.values {
            0 => true,
            1 if !self.not_done && self.done_len == DONE.len() => true,
            _ => self.json.is_complete(),
        }
    }
}

/// What starts a run of bytes, as Go's `unicode.IsSpace` reads it.
enum Space {
    /// A white space character this long.
    Char(usize),
    /// The start of a character, cut off, that may be white space.
    Cut,
    /// Anything else, bytes that aren't UTF-8 included.
    No,
}

/// What starts `bytes`, which isn't empty.
fn space_at(bytes: &[u8]) -> Space {
    let len = match bytes.first() {
        Some(b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r' | b' ') => return Space::Char(1),
        // The lead bytes of U+0085, U+00A0, U+1680, U+2000 to U+205F and
        // U+3000.
        Some(0xc2) => 2,
        Some(0xe1..=0xe3) => 3,
        _ => return Space::No,
    };
    match bytes.get(..len) {
        Some(seq) => match std::str::from_utf8(seq) {
            Ok(text) if text.chars().all(char::is_whitespace) => Space::Char(len),
            _ => Space::No,
        },
        None if bytes
            .get(1..)
            .unwrap_or_default()
            .iter()
            .all(|&b| b & 0xc0 == 0x80) =>
        {
            Space::Cut
        }
        None => Space::No,
    }
}

/// Go's `json.Valid`, given a byte at a time. Like Go, it doesn't check that
/// strings are UTF-8.
#[derive(Debug, Default)]
struct JsonCheck {
    state: Json,
    /// The open containers; `true` for an object.
    open: Vec<bool>,
}

/// Where a [`JsonCheck`] is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Json {
    /// A value is due.
    #[default]
    Value,
    /// A value or `]` is due, after `[`.
    ValueOrEnd,
    /// A key or `}` is due, after `{`.
    KeyOrEnd,
    /// A key is due, after `,` in an object.
    Key,
    /// In a string; `key` when it is an object's key.
    Str { key: bool },
    /// After `\` in a string.
    Escape { key: bool },
    /// In a `\u` escape, with this many hex digits to come.
    Hex { key: bool, left: u8 },
    /// `:` is due, after a key.
    Colon,
    /// In `true`, `false` or `null`, with these bytes to come.
    Literal(&'static [u8]),
    /// After a number's `-`.
    Minus,
    /// After a number's leading `0`.
    Zero,
    /// In a number's whole part.
    Int,
    /// After a number's `.`.
    Point,
    /// In a number's fraction.
    Frac,
    /// After a number's `e`.
    Exp,
    /// After the sign of a number's exponent.
    ExpSign,
    /// In a number's exponent.
    ExpDigits,
    /// After a value.
    After,
    /// Not JSON, whatever follows.
    Invalid,
}

impl JsonCheck {
    /// Adds the next byte.
    fn push(&mut self, byte: u8) {
        self.state = self.next(byte);
    }

    /// Whether what was added is one JSON value.
    fn is_complete(&self) -> bool {
        self.open.is_empty()
            && matches!(
                self.state,
                Json::After | Json::Zero | Json::Int | Json::Frac | Json::ExpDigits
            )
    }

    fn next(&mut self, byte: u8) -> Json {
        let space = matches!(byte, b' ' | b'\t' | b'\n' | b'\r');
        match self.state {
            Json::Invalid => Json::Invalid,
            state @ (Json::Value | Json::ValueOrEnd | Json::KeyOrEnd | Json::Key | Json::Colon)
                if space =>
            {
                state
            }
            Json::Value => self.value(byte),
            Json::ValueOrEnd if byte == b']' => self.close(false),
            Json::ValueOrEnd => self.value(byte),
            Json::KeyOrEnd if byte == b'}' => self.close(true),
            Json::KeyOrEnd | Json::Key if byte == b'"' => Json::Str { key: true },
            Json::KeyOrEnd | Json::Key => Json::Invalid,
            Json::Str { key } => match byte {
                b'"' if key => Json::Colon,
                b'"' => Json::After,
                b'\\' => Json::Escape { key },
                0..0x20 => Json::Invalid,
                _ => Json::Str { key },
            },
            Json::Escape { key } => match byte {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Json::Str { key },
                b'u' => Json::Hex { key, left: 4 },
                _ => Json::Invalid,
            },
            Json::Hex { key, left } if byte.is_ascii_hexdigit() => match left {
                1 => Json::Str { key },
                _ => Json::Hex {
                    key,
                    left: left - 1,
                },
            },
            Json::Hex { .. } => Json::Invalid,
            Json::Colon if byte == b':' => Json::Value,
            Json::Colon => Json::Invalid,
            Json::Literal(rest) => match rest.split_first() {
                Some((&want, [])) if want == byte => Json::After,
                Some((&want, rest)) if want == byte => Json::Literal(rest),
                _ => Json::Invalid,
            },
            Json::Minus => match byte {
                b'0' => Json::Zero,
                b'1'..=b'9' => Json::Int,
                _ => Json::Invalid,
            },
            Json::Zero => match byte {
                b'.' => Json::Point,
                b'e' | b'E' => Json::Exp,
                _ => self.after(byte),
            },
            Json::Int => match byte {
                b'0'..=b'9' => Json::Int,
                b'.' => Json::Point,
                b'e' | b'E' => Json::Exp,
                _ => self.after(byte),
            },
            Json::Point => match byte {
                b'0'..=b'9' => Json::Frac,
                _ => Json::Invalid,
            },
            Json::Frac => match byte {
                b'0'..=b'9' => Json::Frac,
                b'e' | b'E' => Json::Exp,
                _ => self.after(byte),
            },
            Json::Exp => match byte {
                b'+' | b'-' => Json::ExpSign,
                b'0'..=b'9' => Json::ExpDigits,
                _ => Json::Invalid,
            },
            Json::ExpSign => match byte {
                b'0'..=b'9' => Json::ExpDigits,
                _ => Json::Invalid,
            },
            Json::ExpDigits => match byte {
                b'0'..=b'9' => Json::ExpDigits,
                _ => self.after(byte),
            },
            Json::After => self.after(byte),
        }
    }

    /// The start of a value.
    fn value(&mut self, byte: u8) -> Json {
        match byte {
            b'{' | b'[' if self.open.len() >= open_ferry_translate::go::MAX_NESTING_DEPTH => {
                Json::Invalid
            }
            b'{' => {
                self.open.push(true);
                Json::KeyOrEnd
            }
            b'[' => {
                self.open.push(false);
                Json::ValueOrEnd
            }
            b'"' => Json::Str { key: false },
            b't' => Json::Literal(b"rue"),
            b'f' => Json::Literal(b"alse"),
            b'n' => Json::Literal(b"ull"),
            b'-' => Json::Minus,
            b'0' => Json::Zero,
            b'1'..=b'9' => Json::Int,
            _ => Json::Invalid,
        }
    }

    /// What follows a value.
    fn after(&mut self, byte: u8) -> Json {
        if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
            return Json::After;
        }
        match (self.open.last(), byte) {
            (Some(true), b',') => Json::Key,
            (Some(false), b',') => Json::Value,
            (Some(true), b'}') => self.close(true),
            (Some(false), b']') => self.close(false),
            _ => Json::Invalid,
        }
    }

    /// Closes the innermost container, an object if `object`.
    fn close(&mut self, object: bool) -> Json {
        if self.open.pop() == Some(object) {
            Json::After
        } else {
            Json::Invalid
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse_check::data_lines_valid;
    use open_ferry_translate::go;

    /// A small, seeded generator, so a failure can be run again.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn has_field(event: &[u8], prefix: &[u8]) -> bool {
        event
            .split(|&b| b == b'\n')
            .any(|line| go::trim_space(line).starts_with(prefix))
    }

    fn json_valid(bytes: &[u8]) -> bool {
        let mut check = JsonCheck::default();
        for &b in bytes {
            check.push(b);
        }
        check.is_complete()
    }

    /// Reads `event` in pieces cut at random, checking each answer against
    /// upstream's reading of what has been read.
    fn check_pieces(event: &[u8], rng: &mut Rng) {
        let mut scan = EventScan::default();
        let mut read = 0;
        let mut end = 0;
        while end < event.len() {
            end = (end + 1 + rng.below(6)).min(event.len());
            let seen = &event[..end];
            read += scan.advance(seen);
            let want = (
                go::trim_space(seen).is_empty(),
                has_field(seen, b"event:"),
                has_field(seen, b"data:"),
                data_lines_valid(seen),
            );
            let got = (
                scan.is_blank(),
                scan.has_event(),
                scan.has_data(),
                scan.data_valid(),
            );
            assert_eq!(got, want, "{:?}", go::quote_bytes(seen));
        }
        assert!(read <= event.len(), "{read} > {}", event.len());
    }

    #[test]
    fn reads_events_as_upstream_does() {
        let tokens: &[&[u8]] = &[
            b"data:",
            b"data:",
            b"data: ",
            b"event:",
            b"event: x",
            b"da",
            b"ev",
            b": c",
            b"id: 1",
            b"\n",
            b"\n",
            b"\n",
            b"\r",
            b" ",
            b" ",
            b"\t",
            b"\x0b",
            b"\x0c",
            b"\xc2\xa0",
            b"\xc2\x85",
            b"\xe1\x9a\x80",
            b"\xe2\x80\x8a",
            b"\xe2\x80\xa8",
            b"\xe2\x81\x9f",
            b"\xe3\x80\x80",
            b"\xe2\x80\x8b",
            b"\xc2",
            b"\xe2\x80",
            b"\xe2",
            b"\xff",
            b"\xc3\xa9",
            b"{",
            b"}",
            b"[",
            b"]",
            b"\"",
            b"\"",
            b"\\",
            b"\\u00e9",
            b"\\n",
            b"u",
            b"0",
            b"1",
            b"-",
            b".",
            b"e",
            b"+",
            b",",
            b":",
            b"a",
            b"true",
            b"nul",
            b"null",
            b"[DONE]",
            b"[DONE",
            b"{}",
            b"[]",
            b"\"k\":",
            b"\"v\"",
            b"12",
            b"1.5e3",
        ];
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..20_000 {
            let mut event = Vec::new();
            for _ in 0..rng.below(24) {
                event.extend_from_slice(tokens[rng.below(tokens.len())]);
            }
            check_pieces(&event, &mut rng);
        }
    }

    #[test]
    fn reads_events_with_json_as_upstream_does() {
        let events: &[&[u8]] = &[
            b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
            b"data: {\"a\":\n data: [1, 2.5e-3, -0, true, false, null]}\n",
            b"data: \xc2\xa0{\"a\":\"x\xc2\xa0y\"}\xe3\x80\x80\n",
            b"data: {\"a\":\"x\xc2\xa0\ndata: y\"}",
            b"data: [DONE]\xe2\x80\xa8\n\n",
            b"data: [DO NE]",
            b"data:\n\ndata:   \ndata: 1\n",
            b"data: \"\\ud800\\u00E9\\/\"",
            b"data: {\"a\":1}}",
            b"data: {\"a\" 1}",
            b"data: 01",
            b"data: [1,]",
        ];
        let mut rng = Rng(7);
        for event in events {
            for _ in 0..50 {
                check_pieces(event, &mut rng);
            }
        }
    }

    #[test]
    fn checks_json_as_go_does() {
        let cases: &[&[u8]] = &[
            b"{}",
            b"[]",
            b"0",
            b"-0",
            b"1",
            b"-1.5",
            b"1e5",
            b"1E+5",
            b"1e-5",
            b"0.0",
            b"\"\"",
            b"\"a\\u00e9\"",
            b"true",
            b"false",
            b"null",
            b" {\"a\" : [1 , {\"b\":null}]} ",
            b"\"\xff\"",
            b"",
            b" ",
            b"{",
            b"}",
            b"[",
            b"]",
            b"01",
            b"-",
            b"1.",
            b".5",
            b"1e",
            b"1e+",
            b"tru",
            b"truex",
            b"nul",
            b"{\"a\"}",
            b"{\"a\":}",
            b"{,}",
            b"[,]",
            b"[1,]",
            b"{\"a\":1,}",
            b"\"\\x\"",
            b"\"\\u12\"",
            b"\"\\u12g4\"",
            b"\"\n\"",
            b"\"a",
            b"1 2",
            b"{} {}",
            b"[1}",
            b"{\"a\":1]",
            b"{1:2}",
            b"\x0b1",
            b"1\x0c",
            b"+1",
            b"[-]",
        ];
        for &case in cases {
            assert_eq!(
                json_valid(case),
                go::json_valid(case),
                "{:?}",
                go::quote_bytes(case)
            );
        }
        let deep = format!("{}1{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(json_valid(deep.as_bytes()));
        assert!(!json_valid(&deep.as_bytes()[1..]));
        // Go's scanner allows 10,000 levels, an empty array among them.
        for (depth, inner) in [(10_001, "1"), (10_000, "[]"), (10_000, "{}")] {
            let deep = format!("{}{inner}{}", "[".repeat(depth), "]".repeat(depth));
            assert!(!json_valid(deep.as_bytes()), "{depth} {inner}");
            assert!(!go::json_valid(deep.as_bytes()), "{depth} {inner}");
        }
    }

    #[test]
    fn checks_random_json_as_go_does() {
        let tokens: &[&[u8]] = &[
            b"{",
            b"}",
            b"[",
            b"]",
            b"\"",
            b"\"k\"",
            b":",
            b",",
            b" ",
            b"\n",
            b"\t",
            b"\\",
            b"\\u",
            b"0",
            b"1",
            b"9",
            b"-",
            b".",
            b"e",
            b"E",
            b"+",
            b"a",
            b"f",
            b"true",
            b"false",
            b"null",
            b"\x01",
            b"\xc3\xa9",
        ];
        let mut rng = Rng(13);
        for _ in 0..50_000 {
            let mut text = Vec::new();
            for _ in 0..rng.below(12) {
                text.extend_from_slice(tokens[rng.below(tokens.len())]);
            }
            assert_eq!(
                json_valid(&text),
                go::json_valid(&text),
                "{:?}",
                go::quote_bytes(&text)
            );
        }
    }

    #[test]
    fn reads_each_byte_once() {
        let mut event = b"event: response.output_text.delta\ndata: {\"text\":\"".to_vec();
        let mut scan = EventScan::default();
        let mut read = 0;
        for _ in 0..4096 {
            event.extend_from_slice(&[b'x'; 1024]);
            read += scan.advance(&event);
            assert!(!scan.data_valid());
        }
        assert_eq!(read, event.len());
        event.extend_from_slice(b"\"}");
        read += scan.advance(&event);
        assert!(scan.data_valid());
        assert_eq!(read, event.len());
    }
}
