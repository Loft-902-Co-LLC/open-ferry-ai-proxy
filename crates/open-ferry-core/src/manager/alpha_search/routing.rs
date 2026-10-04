// Ported from Go's encoding/json (decode.go: Unmarshal into a struct's
// string field, unquote; fold.go: foldName, foldRune; scanner.go:
// checkValid) (go1.26, BSD-3-Clause), as CLIProxyAPI
// internal/api/server_routes.go (codexAlphaSearch) decodes its routing
// struct with it (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! A Codex Alpha Search payload's `model`, as Go's `json.Unmarshal` reads
//! it into upstream's routing struct.
//!
//! Go reads nothing unless the whole payload is valid JSON, nested at most
//! 10,000 deep, and reads only an object. It then goes through the
//! top-level members in the order they are written, repeated keys included:
//! a key that is `model` after Go's case folding (`Model` and `MODEL` count;
//! no other letters fold to those of `model`) sets the model when its value
//! is a string, while `null` or any other value leaves it as it was. So the
//! last string wins. Strings are decoded as Go decodes them: a surrogate
//! escape that isn't half of a pair, and each byte that isn't part of valid
//! UTF-8, become U+FFFD.
//!
//! This reads the payload's bytes itself rather than through `serde_json`,
//! which keeps one entry per key, refuses invalid UTF-8 and lone surrogate
//! escapes, and stops at 128 levels, where Go reads on. The providers
//! crate's `go_json` decodes whole structs this way; this reads one field.
//!
//! Deviations from Go: none known.

use open_ferry_translate::go::{json_valid, simple_fold};

/// What Go writes in place of what it can't decode.
const REPLACEMENT: char = char::REPLACEMENT_CHARACTER;

/// The payload's `model`, trimmed, or empty when it names none (see the
/// module docs).
pub(super) fn payload_model(raw: &[u8]) -> String {
    let mut model = String::new();
    if !json_valid(raw) {
        return model;
    }
    let mut i = skip_space(raw, 0);
    if raw.get(i) != Some(&b'{') {
        return model;
    }
    i = skip_space(raw, i + 1);
    while raw.get(i) == Some(&b'"') {
        let key_end = string_end(raw, i);
        let key = unquote(
            raw.get(i + 1..key_end.saturating_sub(1))
                .unwrap_or_default(),
        );
        // Past the colon.
        let start = skip_space(raw, skip_space(raw, key_end) + 1);
        let end = value_end(raw, start);
        if raw.get(start) == Some(&b'"') && fold_name(&key) == "MODEL" {
            model = unquote(
                raw.get(start + 1..end.saturating_sub(1))
                    .unwrap_or_default(),
            );
        }
        i = skip_space(raw, end);
        if raw.get(i) == Some(&b',') {
            i = skip_space(raw, i + 1);
        }
    }
    model.trim().to_owned()
}

/// The first index from `i` that isn't JSON whitespace.
fn skip_space(raw: &[u8], mut i: usize) -> usize {
    while matches!(raw.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// The index just past the string that starts at `start`.
fn string_end(raw: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while let Some(&byte) = raw.get(i) {
        match byte {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    raw.len()
}

/// The index just past the value that starts at `start`, which is valid
/// JSON.
fn value_end(raw: &[u8], start: usize) -> usize {
    match raw.get(start) {
        Some(b'"') => string_end(raw, start),
        Some(b'{' | b'[') => {
            let mut depth = 0_usize;
            let mut i = start;
            while let Some(&byte) = raw.get(i) {
                match byte {
                    b'"' => {
                        i = string_end(raw, i);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            raw.len()
        }
        _ => {
            let mut i = start;
            while let Some(byte) = raw.get(i) {
                if matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                    break;
                }
                i += 1;
            }
            i
        }
    }
}

/// A key as Go's `foldName` folds it to match struct fields: each character
/// the smallest of its case-folding orbit (`foldRune`).
fn fold_name(name: &str) -> String {
    name.chars().map(fold_rune).collect()
}

/// The smallest character of `c`'s case-folding orbit (Go's `foldRune`).
fn fold_rune(mut c: char) -> char {
    loop {
        let next = simple_fold(c);
        if next <= c {
            return next;
        }
        c = next;
    }
}

/// A JSON string's contents, between its quotes, decoded as Go's
/// `unquote` decodes them.
fn unquote(s: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(&byte) = s.get(i) {
        if byte == b'\\' {
            let Some(&escape) = s.get(i + 1) else {
                break;
            };
            if escape != b'u' {
                out.push(match escape {
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    other => char::from(other),
                });
                i += 2;
                continue;
            }
            let Some(unit) = hex_escape(s, i) else {
                break;
            };
            i += 6;
            if !(0xD800..0xE000).contains(&unit) {
                out.push(char::from_u32(unit).unwrap_or(REPLACEMENT));
                continue;
            }
            // A surrogate: a high one followed by a low one is a pair, and
            // anything else is U+FFFD, with only this escape consumed.
            match hex_escape(s, i) {
                Some(low)
                    if (0xD800..0xDC00).contains(&unit) && (0xDC00..0xE000).contains(&low) =>
                {
                    let code = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                    out.push(char::from_u32(code).unwrap_or(REPLACEMENT));
                    i += 6;
                }
                _ => out.push(REPLACEMENT),
            }
        } else if byte.is_ascii() {
            out.push(char::from(byte));
            i += 1;
        } else {
            let (c, width) = decode_rune(s.get(i..).unwrap_or_default());
            out.push(c);
            i += width;
        }
    }
    out
}

/// The code unit of the `\u` escape at `i`, if there is one (Go's `getu4`).
fn hex_escape(s: &[u8], i: usize) -> Option<u32> {
    if s.get(i) != Some(&b'\\') || s.get(i + 1) != Some(&b'u') {
        return None;
    }
    let digits = std::str::from_utf8(s.get(i + 2..i + 6)?).ok()?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// The character `s` starts with and its length in bytes, or U+FFFD and 1
/// when it doesn't start with valid UTF-8 (Go's `utf8.DecodeRune`).
fn decode_rune(s: &[u8]) -> (char, usize) {
    let width = match s.first() {
        Some(0xC2..=0xDF) => 2,
        Some(0xE0..=0xEF) => 3,
        Some(0xF0..=0xF4) => 4,
        _ => 1,
    };
    match s.get(..width).map(std::str::from_utf8) {
        Some(Ok(text)) => text.chars().next().map_or((REPLACEMENT, 1), |c| (c, width)),
        _ => (REPLACEMENT, 1),
    }
}

#[cfg(test)]
mod tests {
    //! Not upstream's: Go's answers, recorded with Go 1.26.4's
    //! `json.Unmarshal` into upstream's routing struct.

    use super::*;

    /// `text` with each `~` made a backslash, so escapes read plainly.
    fn json(text: &str) -> Vec<u8> {
        text.replace('~', "\\").into_bytes()
    }

    fn model(text: &str) -> String {
        payload_model(&json(text))
    }

    #[test]
    fn the_last_string_under_any_case_of_model_wins() {
        let cases = [
            (
                r#"{"model":"missing","MODEL":"allowed","model":"missing"}"#,
                "missing",
            ),
            (r#"{"model":"allowed","model":null}"#, "allowed"),
            (
                r#"{"model":"missing","MODEL":"allowed","model":null}"#,
                "allowed",
            ),
            (r#"{"model":"allowed","model":7}"#, "allowed"),
            (
                r#"{"model":"missing","MODEL":"allowed","model":7}"#,
                "allowed",
            ),
            (r#"{"model":"allowed","model":true}"#, "allowed"),
            (r#"{"model":"allowed","model":{"x":1}}"#, "allowed"),
            (r#"{"model":"allowed","model":["missing"]}"#, "allowed"),
            (r#"{"Model":"a"}"#, "a"),
            (r#"{"mOdEl":"a"}"#, "a"),
            (r#"{"model":"x","Model":"y"}"#, "y"),
            (r#"{"MODEL":"y","model":"x"}"#, "x"),
            (r#"{"mod~u0065l":"a"}"#, "a"),
            (r#"{"MOD~u0045L":"a"}"#, "a"),
            ("{\"\u{1d0d}odel\":\"a\"}", ""),
            ("{\"\u{ff4d}odel\":\"a\"}", ""),
            (r#"{"x":{"model":"a"}}"#, ""),
            (r#"{"model":"a","model":"  "}"#, ""),
            ("{\"model\":\" a \u{a0}\"}", "a"),
        ];
        for (payload, want) in cases {
            assert_eq!(model(payload), want, "{payload}");
        }
    }

    #[test]
    fn reads_nothing_but_a_valid_object() {
        for payload in [
            r#"{"model":"a""#,
            r#"["model","a"]"#,
            r#"{"model":"a"} x"#,
            "null",
            r#""model""#,
            "",
        ] {
            assert_eq!(model(payload), "", "{payload}");
        }
    }

    #[test]
    fn reads_what_serde_json_refuses() {
        let mut invalid_utf8 = br#"{"model":"a","q":""#.to_vec();
        invalid_utf8.extend_from_slice(b"\xff\"}");
        assert_eq!(payload_model(&invalid_utf8), "a");
        assert_eq!(model(r#"{"model":"a","q":"~ud800"}"#), "a");
        let deep = format!(
            r#"{{"model":"a","d":{}{}}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        assert_eq!(payload_model(deep.as_bytes()), "a");
        assert_eq!(
            payload_model(b"{\"q\":[\"]}\",{\"x\":\"[\"}],\"model\":\"a\"}"),
            "a"
        );
    }

    #[test]
    fn decodes_strings_as_go_does() {
        assert_eq!(model(r#"{"model":"a~ud800b"}"#), "a\u{fffd}b");
        assert_eq!(
            model(r#"{"model":"a~udc00~ud800~udc00b"}"#),
            "a\u{fffd}\u{10000}b"
        );
        assert_eq!(
            payload_model(b"{\"model\":\"a\xe2\x82b\"}"),
            "a\u{fffd}\u{fffd}b"
        );
        assert_eq!(
            payload_model(b"{\"model\":\"a\xed\xa0\x80b\"}"),
            "a\u{fffd}\u{fffd}\u{fffd}b"
        );
        assert_eq!(model(r#"{"model":"a~u0000"}"#), "a\u{0}");
        assert_eq!(
            model(r#"{"model":"~"~~~/~b~f~n~r~tx"}"#),
            "\"\\/\u{8}\u{c}\n\r\tx"
        );
        assert_eq!(
            model(r#"{"model":"caf~u00e9 ~ud83d~ude00"}"#),
            "caf\u{e9} \u{1f600}"
        );
    }
}
