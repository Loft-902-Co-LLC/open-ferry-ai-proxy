// Ported from tidwall/gjson v1.18.0 gjson.go and tidwall/sjson v1.2.5
// sjson.go (MIT), as CLIProxyAPI uses them in
// sdk/api/handlers/openai/openai_responses_handlers.go and
// openai_responses_websocket*.go (v8.0.10, MIT): Get, Parse, Result's
// String, Int, Bool, Array and ForEach, unescape, SetBytes, SetRawBytes,
// DeleteBytes, appendRawPaths, appendBuild, appendStringify and
// deleteTailItem.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/tidwall/gjson
// https://github.com/tidwall/sjson

//! JSON as the Responses handlers read and edit it, as upstream does with
//! gjson and sjson: values found by path in the bytes as written, the first
//! entry for a key winning, and edits that splice those bytes, so numbers,
//! key order and escapes the client wrote survive. Also the parts of Go's
//! `encoding/json` the handlers' output depends on.
//!
//! Lookups don't check that the text is JSON, as gjson's don't. Where
//! upstream checks, the caller does too, with [`valid`].
//!
//! Deviations from upstream:
//! - Paths are object keys joined by dots. gjson's escapes, wildcards, array
//!   indexes and modifiers aren't read; no caller uses them.
//! - In text that isn't JSON, a lookup stops at the first entry that isn't
//!   a quoted key, a colon and a value, and a value that isn't a string or
//!   in brackets runs to the next delimiter; gjson reads on as best it can.
//!   [`Val::parse`] takes the first value to its end, where gjson `Parse`
//!   takes the rest of the text after an opening bracket.
//! - Go's conversion of a float beyond an `int64` depends on the platform;
//!   [`Val::int`] gives what amd64 gives.

use std::fmt::Write as _;

use serde_json::{Map, Value};

/// A value found in a document: its text as written, and where it starts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Val<'a> {
    /// The value as written.
    pub(crate) raw: &'a [u8],
    /// Where [`Val::raw`] starts in the document it was found in.
    pub(crate) index: usize,
}

impl<'a> Val<'a> {
    /// The first value in `doc`, after white space (gjson `Parse`).
    pub(crate) fn parse(doc: &'a [u8]) -> Option<Self> {
        let start = skip_space(doc, 0);
        let end = scan_value(doc, start)?;
        Some(Self {
            raw: &doc[start..end],
            index: start,
        })
    }

    /// The value at `path` inside this one (gjson `Result.Get`), indexed in
    /// this value's document.
    pub(crate) fn get(&self, path: &str) -> Option<Val<'a>> {
        let found = get(self.raw, path)?;
        Some(Val {
            raw: found.raw,
            index: self.index + found.index,
        })
    }

    pub(crate) fn is_object(&self) -> bool {
        self.raw.first() == Some(&b'{')
    }

    pub(crate) fn is_array(&self) -> bool {
        self.raw.first() == Some(&b'[')
    }

    pub(crate) fn is_null(&self) -> bool {
        self.raw.first() == Some(&b'n')
    }

    pub(crate) fn is_string(&self) -> bool {
        self.raw.first() == Some(&b'"')
    }

    /// Whether this is an array with nothing in it.
    pub(crate) fn is_empty_array(&self) -> bool {
        self.is_array() && self.raw.get(skip_space(self.raw, 1)) == Some(&b']')
    }

    /// gjson `String`: a string unescaped, `null` as empty, `true` and
    /// `false`, an integer as written, another number in Go's shortest form,
    /// and an object or array as written.
    pub(crate) fn str(&self) -> String {
        match self.raw.first() {
            None | Some(b'n') => String::new(),
            Some(b't') => "true".to_owned(),
            Some(b'f') => "false".to_owned(),
            Some(b'"') => String::from_utf8_lossy(&string_value(self.raw)).into_owned(),
            Some(b'{' | b'[') => String::from_utf8_lossy(self.raw).into_owned(),
            Some(_) => {
                let digits = self.raw.strip_prefix(b"-").unwrap_or(self.raw);
                if digits.iter().all(u8::is_ascii_digit) {
                    String::from_utf8_lossy(self.raw).into_owned()
                } else {
                    open_ferry_translate::go::format_float(self.num())
                }
            }
        }
    }

    /// gjson `Int`: `true` as 1, a string read as an integer, a number
    /// truncated, and anything else as 0.
    pub(crate) fn int(&self) -> i64 {
        match self.raw.first() {
            Some(b't') => 1,
            Some(b'"') => parse_int(&string_value(self.raw)).unwrap_or(0),
            Some(b'-' | b'0'..=b'9') => {
                let f = self.num();
                if (-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(&f) {
                    return f as i64;
                }
                parse_int(self.raw).unwrap_or_else(|| go_int64(f))
            }
            _ => 0,
        }
    }

    /// gjson `Bool`: `true`, a string Go's `ParseBool` reads as true once
    /// lowercased, or a non-zero number.
    pub(crate) fn bool(&self) -> bool {
        match self.raw.first() {
            Some(b't') => true,
            Some(b'"') => {
                let text = String::from_utf8_lossy(&string_value(self.raw)).into_owned();
                matches!(
                    open_ferry_translate::go::to_lower(&text).as_str(),
                    "1" | "t" | "true"
                )
            }
            Some(b'-' | b'0'..=b'9') => self.num() != 0.0,
            _ => false,
        }
    }

    /// gjson `Array`: an array's items, nothing for `null`, and the value
    /// itself for anything else.
    pub(crate) fn array(&self) -> Vec<Val<'a>> {
        if self.is_null() {
            return Vec::new();
        }
        if !self.is_array() {
            return vec![*self];
        }
        let mut items = Vec::new();
        let mut i = skip_space(self.raw, 1);
        if self.raw.get(i) == Some(&b']') {
            return items;
        }
        while let Some(end) = scan_value(self.raw, i) {
            items.push(Val {
                raw: &self.raw[i..end],
                index: self.index + i,
            });
            i = skip_space(self.raw, end);
            if self.raw.get(i) != Some(&b',') {
                break;
            }
            i = skip_space(self.raw, i + 1);
        }
        items
    }

    /// An object's members, keys unescaped, in order (gjson `ForEach` on an
    /// object). Anything else has none.
    pub(crate) fn members(&self) -> Vec<(String, Val<'a>)> {
        object_members(self.raw, 0)
            .map(|(key, start, end)| {
                let key = String::from_utf8_lossy(&string_value(&self.raw[key])).into_owned();
                let value = Val {
                    raw: &self.raw[start..end],
                    index: self.index + start,
                };
                (key, value)
            })
            .collect()
    }

    /// The number, as Go's `ParseFloat` reads it.
    fn num(&self) -> f64 {
        std::str::from_utf8(self.raw)
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(0.0)
    }
}

/// gjson `GetBytes(doc, path)`.
pub(crate) fn get<'a>(doc: &'a [u8], path: &str) -> Option<Val<'a>> {
    let mut found: Option<(usize, usize)> = None;
    for key in path.split('.') {
        let at = found.map_or_else(|| skip_space(doc, 0), |(start, _)| start);
        found = Some(member(doc, at, key)?);
    }
    let (start, end) = found?;
    Some(Val {
        raw: &doc[start..end],
        index: start,
    })
}

/// gjson `Get(text, key)` on text that need not be JSON: the value at `key`
/// in the object at the first `{`, unless a `[` comes first.
pub(crate) fn find<'a>(text: &'a [u8], key: &str) -> Option<Val<'a>> {
    let at = text.iter().position(|&c| c == b'{' || c == b'[')?;
    let (start, end) = member(text, at, key)?;
    Some(Val {
        raw: &text[start..end],
        index: start,
    })
}

/// gjson `GetBytes(doc, path).String()`: empty when `path` is missing.
pub(crate) fn str_at(doc: &[u8], path: &str) -> String {
    get(doc, path).map(|value| value.str()).unwrap_or_default()
}

/// The span of the first value whose key is `key` in the object at `i`.
fn member(doc: &[u8], i: usize, key: &str) -> Option<(usize, usize)> {
    object_members(doc, i)
        .find(|(raw_key, _, _)| key_is(&doc[raw_key.clone()], key))
        .map(|(_, start, end)| (start, end))
}

/// Whether the quoted key `raw` is `key`, once unescaped.
fn key_is(raw: &[u8], key: &str) -> bool {
    let inner = &raw[1..raw.len() - 1];
    if inner.contains(&b'\\') {
        unescape(inner) == key.as_bytes()
    } else {
        inner == key.as_bytes()
    }
}

/// The members of the object at `i`: each key's span, quotes included, and
/// its value's start and end. It stops at anything malformed.
fn object_members(
    doc: &[u8],
    i: usize,
) -> impl Iterator<Item = (std::ops::Range<usize>, usize, usize)> + '_ {
    let mut next = (doc.get(i) == Some(&b'{')).then(|| skip_space(doc, i + 1));
    std::iter::from_fn(move || {
        let i = next.take()?;
        if doc.get(i) != Some(&b'"') {
            return None;
        }
        let key_end = scan_string(doc, i)?;
        let colon = skip_space(doc, key_end);
        if doc.get(colon) != Some(&b':') {
            return None;
        }
        let start = skip_space(doc, colon + 1);
        let end = scan_value(doc, start)?;
        let after = skip_space(doc, end);
        if doc.get(after) == Some(&b',') {
            next = Some(skip_space(doc, after + 1));
        }
        Some((i..key_end, start, end))
    })
}

/// A string value, quotes included, unescaped.
fn string_value(raw: &[u8]) -> Vec<u8> {
    let inner = raw.get(1..raw.len().saturating_sub(1)).unwrap_or_default();
    if inner.contains(&b'\\') {
        unescape(inner)
    } else {
        inner.to_vec()
    }
}

/// gjson `unescape`: a string's text without its escapes. A lone surrogate
/// becomes U+FFFD, and the text stops at a bad escape or a control byte.
fn unescape(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let c = text[i];
        if c < b' ' {
            return out;
        }
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&escape) = text.get(i) else {
            return out;
        };
        let simple = match escape {
            b'\\' => b'\\',
            b'/' => b'/',
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'"' => b'"',
            b'u' => 0,
            _ => return out,
        };
        if escape != b'u' {
            out.push(simple);
            i += 1;
            continue;
        }
        if i + 5 > text.len() {
            return out;
        }
        let mut code = hex4(&text[i + 1..i + 5]);
        i += 5;
        if (0xd800..0xe000).contains(&code)
            && text.get(i) == Some(&b'\\')
            && text.get(i + 1) == Some(&b'u')
        {
            let low = text.get(i + 2..i + 6).map_or(0, hex4);
            code = if (0xd800..0xdc00).contains(&code) && (0xdc00..0xe000).contains(&low) {
                0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00)
            } else {
                0xfffd
            };
            i += 6;
        }
        let c = char::from_u32(code).unwrap_or('\u{fffd}');
        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
    out
}

/// Four hex digits, or 0 when they aren't (gjson `runeit`, which takes no
/// sign).
fn hex4(digits: &[u8]) -> u32 {
    digits
        .iter()
        .try_fold(0, |code, &digit| {
            Some(code << 4 | char::from(digit).to_digit(16)?)
        })
        .unwrap_or(0)
}

/// gjson `parseInt`: an optional `-` and then only digits, wrapping as Go
/// does.
fn parse_int(text: &[u8]) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix(b"-") {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for &digit in digits {
        if !digit.is_ascii_digit() {
            return None;
        }
        n = n.wrapping_mul(10).wrapping_add(i64::from(digit - b'0'));
    }
    Some(if negative { n.wrapping_neg() } else { n })
}

/// Go's `int64(f)` on amd64, which gives the lowest `int64` for a float out
/// of range.
fn go_int64(f: f64) -> i64 {
    if f.is_nan() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        i64::MIN
    } else {
        f as i64
    }
}

/// What an edit writes.
#[derive(Clone, Copy)]
enum New<'v> {
    /// JSON text, as it is.
    Raw(&'v [u8]),
    /// A string, quoted.
    Str(&'v str),
}

/// sjson `SetRawBytes(doc, path, raw)`, or `None` where sjson fails: when
/// the key is missing and the document, or the value holding the key, is an
/// array.
pub(crate) fn try_set_raw(doc: &[u8], path: &str, raw: &[u8]) -> Option<Vec<u8>> {
    set(doc, path, New::Raw(raw))
}

/// [`try_set_raw`], keeping `doc` as it is where sjson fails, as upstream
/// does where it ignores the error.
pub(crate) fn set_raw(doc: &[u8], path: &str, raw: &[u8]) -> Vec<u8> {
    try_set_raw(doc, path, raw).unwrap_or_else(|| doc.to_vec())
}

/// sjson `SetBytes(doc, path, value)` for a string; see [`try_set_raw`].
pub(crate) fn try_set_str(doc: &[u8], path: &str, value: &str) -> Option<Vec<u8>> {
    set(doc, path, New::Str(value))
}

/// [`try_set_str`], keeping `doc` where sjson fails.
pub(crate) fn set_str(doc: &[u8], path: &str, value: &str) -> Vec<u8> {
    try_set_str(doc, path, value).unwrap_or_else(|| doc.to_vec())
}

/// sjson `SetBytes(doc, path, value)` for a bool, keeping `doc` where sjson
/// fails.
pub(crate) fn set_bool(doc: &[u8], path: &str, value: bool) -> Vec<u8> {
    let raw: &[u8] = if value { b"true" } else { b"false" };
    set_raw(doc, path, raw)
}

fn set(doc: &[u8], path: &str, new: New<'_>) -> Option<Vec<u8>> {
    let keys: Vec<&str> = path.split('.').collect();
    let mut buf = Vec::with_capacity(doc.len() + 32);
    append_paths(&mut buf, doc, &keys, new)?;
    Some(buf)
}

/// sjson `appendRawPaths` for a set.
fn append_paths(buf: &mut Vec<u8>, doc: &[u8], keys: &[&str], new: New<'_>) -> Option<()> {
    if let Some((start, end)) = member(doc, skip_space(doc, 0), keys[0]) {
        buf.extend_from_slice(&doc[..start]);
        if keys.len() > 1 {
            append_paths(buf, &doc[start..end], &keys[1..], new)?;
        } else {
            append_new(buf, new);
        }
        buf.extend_from_slice(&doc[end..]);
        return Some(());
    }
    let mut text = doc;
    if text.iter().all(|&b| b <= b' ') {
        text = b"{}";
    }
    // gjson `Parse`: a container runs from its opening bracket to the end.
    let mut start = text.iter().position(|&b| b > b' ').unwrap_or(0);
    if !matches!(text.get(start), Some(b'{' | b'[')) {
        text = b"{}";
        start = 0;
    }
    let container = &text[start..];
    if container[0] == b'[' {
        return None;
    }
    let comma = container[1..]
        .iter()
        .find(|&&b| b > b' ')
        .is_some_and(|&b| b != b'}' && b != b']');
    let end = container.iter().rposition(|&b| b == b'}').unwrap_or(0);
    buf.extend_from_slice(&container[..end]);
    if comma {
        buf.push(b',');
    }
    append_build(buf, keys, new);
    buf.push(b'}');
    Some(())
}

/// sjson `appendBuild`: `"key":` for each key, nesting objects, then the
/// value.
fn append_build(buf: &mut Vec<u8>, keys: &[&str], new: New<'_>) {
    append_stringify(buf, keys[0]);
    buf.push(b':');
    if keys.len() > 1 {
        buf.push(b'{');
        append_build(buf, &keys[1..], new);
        buf.push(b'}');
    } else {
        append_new(buf, new);
    }
}

fn append_new(buf: &mut Vec<u8>, new: New<'_>) {
    match new {
        New::Raw(raw) => buf.extend_from_slice(raw),
        New::Str(value) => append_stringify(buf, value),
    }
}

/// sjson `appendStringify`: quoted as it is, unless it needs escapes, when
/// it is quoted as Go's `json.Marshal` would.
fn append_stringify(buf: &mut Vec<u8>, value: &str) {
    let plain = value
        .bytes()
        .all(|b| (b' '..=0x7f).contains(&b) && b != b'"' && b != b'\\');
    if plain {
        buf.push(b'"');
        buf.extend_from_slice(value.as_bytes());
        buf.push(b'"');
    } else {
        buf.extend_from_slice(json_string(value).as_bytes());
    }
}

/// sjson `DeleteBytes(doc, path)`: `doc` without the path's first entry
/// and the comma that went with it, or `None` when it isn't there.
pub(crate) fn try_delete(doc: &[u8], path: &str) -> Option<Vec<u8>> {
    let keys: Vec<&str> = path.split('.').collect();
    let mut buf = Vec::with_capacity(doc.len());
    delete_paths(&mut buf, doc, &keys)?;
    Some(buf)
}

/// [`try_delete`], keeping `doc` as it is when the path isn't there, as
/// sjson does.
pub(crate) fn delete(doc: &[u8], path: &str) -> Vec<u8> {
    try_delete(doc, path).unwrap_or_else(|| doc.to_vec())
}

/// sjson `appendRawPaths` for a delete.
fn delete_paths(buf: &mut Vec<u8>, doc: &[u8], keys: &[&str]) -> Option<()> {
    let (start, end) = member(doc, skip_space(doc, 0), keys[0])?;
    buf.extend_from_slice(&doc[..start]);
    if keys.len() > 1 {
        delete_paths(buf, &doc[start..end], &keys[1..])?;
        buf.extend_from_slice(&doc[end..]);
        return Some(());
    }
    let (keep, delete_next_comma) = delete_tail_item(buf);
    buf.truncate(keep);
    let mut skip = 0;
    if delete_next_comma {
        for (j, &b) in doc[end..].iter().enumerate() {
            if b <= b' ' {
                continue;
            }
            if b == b',' {
                skip = j + 1;
            }
            break;
        }
    }
    buf.extend_from_slice(&doc[end + skip..]);
    Some(())
}

/// sjson `deleteTailItem`: how much of `buf`, which ends just before the
/// value being deleted, to keep so its key goes too, and whether the comma
/// after the value must go instead of the one before it.
fn delete_tail_item(buf: &[u8]) -> (usize, bool) {
    let at = |i: isize| buf[usize::try_from(i).unwrap_or_default()];
    let mut i = isize::try_from(buf.len()).unwrap_or(isize::MAX) - 1;
    while i >= 0 {
        match at(i) {
            b'[' => return (buf.len(), true),
            b',' => return (i.unsigned_abs(), false),
            b':' => {
                i -= 1;
                while i >= 0 {
                    if at(i) == b'"' {
                        i -= 1;
                        while i >= 0 {
                            if at(i) == b'"' {
                                i -= 1;
                                if i >= 0 && at(i) == b'\\' {
                                    i -= 2;
                                    continue;
                                }
                                while i >= 0 {
                                    match at(i) {
                                        b'{' => return (i.unsigned_abs() + 1, true),
                                        b',' => return (i.unsigned_abs(), false),
                                        _ => i -= 1,
                                    }
                                }
                            }
                            i -= 1;
                        }
                        break;
                    }
                    i -= 1;
                }
                break;
            }
            _ => {}
        }
        i -= 1;
    }
    (buf.len(), false)
}

/// Go's `json.Marshal` of `[]json.RawMessage`: the items compacted, with
/// `<`, `>`, `&`, U+2028 and U+2029 escaped, in an array. `None` when an
/// item isn't valid JSON.
pub(crate) fn compact_html(items: &[&[u8]]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(items.iter().map(|item| item.len() + 1).sum::<usize>() + 2);
    out.push(b'[');
    for (n, item) in items.iter().enumerate() {
        if !valid(item) {
            return None;
        }
        if n > 0 {
            out.push(b',');
        }
        compact_into(&mut out, item);
    }
    out.push(b']');
    Some(out)
}

/// Go's `appendCompact` with HTML escaping, for valid JSON.
fn compact_into(out: &mut Vec<u8>, item: &[u8]) {
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < item.len() {
        let c = item[i];
        if matches!(c, b'<' | b'>' | b'&') {
            let hex = b"0123456789abcdef";
            out.extend_from_slice(b"\\u00");
            out.extend_from_slice(&[hex[usize::from(c >> 4)], hex[usize::from(c & 0xf)]]);
            i += 1;
            escaped = false;
            continue;
        }
        if c == 0xe2
            && item.get(i + 1) == Some(&0x80)
            && matches!(item.get(i + 2), Some(0xa8 | 0xa9))
        {
            out.extend_from_slice(b"\\u202");
            out.push(if item[i + 2] == 0xa8 { b'8' } else { b'9' });
            i += 3;
            escaped = false;
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            out.push(c);
        } else if c == b'"' {
            in_string = true;
            out.push(c);
        } else if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            out.push(c);
        }
        i += 1;
    }
}

/// Go's `encoding/json` key match: `key` equals the ASCII `target` once
/// both are case-folded, where the long s (U+017F) folds with `s` and the
/// Kelvin sign (U+212A) with `k`.
pub(crate) fn fold_eq(key: &str, target: &str) -> bool {
    let mut target = target.bytes();
    for c in key.chars() {
        let folded = match c {
            '\u{17f}' => b'S',
            '\u{212a}' => b'K',
            c if c.is_ascii() => u8::try_from(c).unwrap_or_default().to_ascii_uppercase(),
            _ => return false,
        };
        if target.next().map(|b| b.to_ascii_uppercase()) != Some(folded) {
            return false;
        }
    }
    target.next().is_none()
}

/// Go's `json.Marshal` of a string (a private copy of
/// `open_ferry_translate::go::json_string`).
pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
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

/// Go's `json.Valid` (`open_ferry_translate::go::json_valid`, which is
/// public).
pub(crate) fn valid(bytes: &[u8]) -> bool {
    open_ferry_translate::go::json_valid(bytes)
}

/// gjson `Valid`, which has no limit on nesting
/// (`open_ferry_translate::go::gjson_valid`).
pub(crate) fn gjson_valid(bytes: &[u8]) -> bool {
    open_ferry_translate::go::gjson_valid(bytes)
}

/// `value` with each object's keys in the order Go's `json.Marshal` writes
/// a map's.
pub(crate) fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(sorted_map(fields)),
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// [`sorted`] for an object.
pub(crate) fn sorted_map(fields: &Map<String, Value>) -> Map<String, Value> {
    let mut entries: Vec<(&String, &Value)> = fields.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .into_iter()
        .map(|(key, value)| (key.clone(), sorted(value)))
        .collect()
}

/// The index past the white space at `i`.
fn skip_space(text: &[u8], mut i: usize) -> usize {
    while text
        .get(i)
        .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
    {
        i += 1;
    }
    i
}

/// The index past the string that starts at `i`.
fn scan_string(text: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    loop {
        match *text.get(i)? {
            b'"' => return Some(i + 1),
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
}

/// The index past the value that starts at `i`. Objects and arrays are
/// matched by their brackets, outside strings; anything else runs to the
/// next delimiter.
fn scan_value(text: &[u8], mut i: usize) -> Option<usize> {
    match *text.get(i)? {
        b'"' => scan_string(text, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            loop {
                match *text.get(i)? {
                    b'"' => {
                        i = scan_string(text, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        _ => {
            let len = text[i..]
                .iter()
                .take_while(|&&c| !matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'))
                .count();
            (len > 0).then_some(i + len)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value `raw`, as a lookup finds it.
    fn val(raw: &str) -> Val<'_> {
        Val {
            raw: raw.as_bytes(),
            index: 0,
        }
    }

    /// What a lookup found, as written.
    fn raw(found: Option<Val<'_>>) -> Option<&[u8]> {
        found.map(|value| value.raw)
    }

    #[test]
    fn finds_the_first_entry_for_a_key() {
        let text = br#" { "a" : 1 , "b":{"c":[1,"]"]}, "a":2, "esc":"x\"y" }"#;
        assert_eq!(raw(get(text, "a")), Some(&b"1"[..]));
        assert_eq!(raw(get(text, "b.c")), Some(&br#"[1,"]"]"#[..]));
        assert_eq!(raw(get(text, "esc")), Some(&br#""x\"y""#[..]));
        assert_eq!(raw(get(text, "b.x")), None);
        assert_eq!(raw(get(b"[1]", "a")), None);
        assert_eq!(
            raw(find(
                br#"oops {"sequence_number":5} more"#,
                "sequence_number"
            )),
            Some(&b"5"[..])
        );
        assert_eq!(
            raw(find(br#"[{"sequence_number":5}]"#, "sequence_number")),
            None
        );
        assert_eq!(raw(find(b"no json", "sequence_number")), None);
    }

    #[test]
    fn reads_values_as_gjson_does() {
        let ints = [
            ("true", 1),
            ("false", 0),
            ("null", 0),
            (r#""429""#, 429),
            (r#"" 429""#, 0),
            ("429.9", 429),
            ("-3.5", -3),
            ("1e3", 1000),
            ("9007199254740993", 9_007_199_254_740_993),
            ("{}", 0),
        ];
        for (raw, want) in ints {
            assert_eq!(val(raw).int(), want, "{raw}");
        }
        let strings = [
            (r#""a\nb""#, "a\nb"),
            ("1.50", "1.5"),
            ("-12", "-12"),
            ("1e2", "100"),
            ("1e400", "+Inf"),
            // Halfway between two shortest decimals: Go rounds to even.
            ("2156163594508435.25", "2156163594508435.2"),
            ("-628643006909686.25", "-628643006909686.2"),
            ("2.98023223876953125e-8", "0.000000029802322387695312"),
            ("true", "true"),
            ("null", ""),
            (r#"{ "a":1 }"#, r#"{ "a":1 }"#),
        ];
        for (raw, want) in strings {
            assert_eq!(val(raw).str(), want, "{raw}");
        }
        assert!(val("[ ]").is_empty_array());
        assert!(!val("[0]").is_empty_array());
    }

    #[test]
    fn deletes_as_sjson_does() {
        let cases = [
            (r#"{"stream":false,"model":"x"}"#, r#"{"model":"x"}"#),
            (r#"{"model":"x","stream":false}"#, r#"{"model":"x"}"#),
            (
                r#"{"model":"x","stream":null,"input":[]}"#,
                r#"{"model":"x","input":[]}"#,
            ),
            (r#"{ "stream": false , "a":1}"#, r#"{ "a":1}"#),
            (r#"{"stream":{"a":[1]}}"#, "{}"),
            (
                "{\n  \"model\": \"x\",\n  \"stream\": false\n}",
                "{\n  \"model\": \"x\"\n}",
            ),
            (r#"{"stream":1,"stream":2}"#, r#"{"stream":2}"#),
        ];
        for (text, want) in cases {
            let got = try_delete(text.as_bytes(), "stream").unwrap();
            assert_eq!(String::from_utf8(got).unwrap(), want, "{text}");
        }
        assert_eq!(try_delete(br#"{"model":"x"}"#, "stream"), None);
    }

    #[test]
    fn sets_the_response_output_as_sjson_does() {
        let set = |text: &str| {
            try_set_raw(text.as_bytes(), "response.output", b"[1]")
                .map(|out| String::from_utf8(out).unwrap())
        };
        assert_eq!(
            set(r#"{"type":"t","response":{"id":"r","output":[]}}"#).unwrap(),
            r#"{"type":"t","response":{"id":"r","output":[1]}}"#
        );
        assert_eq!(
            set(r#"{"type":"t","response":{"id":"r"}}"#).unwrap(),
            r#"{"type":"t","response":{"id":"r","output":[1]}}"#
        );
        assert_eq!(
            set(r#"{"response":{ }}"#).unwrap(),
            r#"{"response":{ "output":[1]}}"#
        );
        assert_eq!(
            set(r#"{"response":null}"#).unwrap(),
            r#"{"response":{"output":[1]}}"#
        );
        assert_eq!(
            set(r#"{"type":"t"}"#).unwrap(),
            r#"{"type":"t","response":{"output":[1]}}"#
        );
        assert_eq!(set(r#"{"response":[]}"#), None);
        assert_eq!(set("[]"), None);
    }

    #[test]
    fn sorts_keys_as_go_marshals_maps() {
        let value: Value = serde_json::from_str(r#"{"b":[{"d":1,"c":2}],"a":null}"#).unwrap();
        assert_eq!(
            sorted(&value).to_string(),
            r#"{"a":null,"b":[{"c":2,"d":1}]}"#
        );
    }
}
