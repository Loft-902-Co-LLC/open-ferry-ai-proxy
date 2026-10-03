// Ported from the parts of gjson (v1.18.0) and sjson (v1.2.5) that
// CLIProxyAPI sdk/api/handlers/openai/openai_responses_websocket*.go relies
// on (v8.0.10, MIT): Get, Parse, Result's String, Int, Bool, Array and
// ForEach, unescape, SetBytes, SetRawBytes, DeleteBytes, appendRawPaths,
// appendBuild, appendStringify and deleteTailItem.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/tidwall/gjson
// https://github.com/tidwall/sjson

//! JSON as upstream reads and edits it: values found by path in the bytes as
//! written, and edits that splice those bytes, so numbers, key order and
//! escapes the client wrote survive.
//!
//! Paths are object keys joined by dots, as upstream's are. The scanner is a
//! private copy of `open_ferry_translate`'s, which that crate doesn't export.

use std::fmt::Write as _;

/// A value found in a document: its text as written, and where it starts.
#[derive(Clone, Copy, Debug)]
pub(super) struct Val<'a> {
    /// The value as written.
    pub(super) raw: &'a [u8],
    /// Where [`Val::raw`] starts in the document it was found in.
    pub(super) index: usize,
}

impl<'a> Val<'a> {
    /// The first value in `doc`, after white space (gjson `Parse`), when it
    /// is valid.
    pub(super) fn parse(doc: &'a [u8]) -> Option<Self> {
        let start = skip_space(doc, 0);
        let end = scan_value(doc, start)?;
        Some(Self {
            raw: &doc[start..end],
            index: start,
        })
    }

    /// The value at `path` inside this one (gjson `Result.Get`), indexed in
    /// this value's document.
    pub(super) fn get(&self, path: &str) -> Option<Val<'a>> {
        let found = get(self.raw, path)?;
        Some(Val {
            raw: found.raw,
            index: self.index + found.index,
        })
    }

    pub(super) fn is_object(&self) -> bool {
        self.raw.first() == Some(&b'{')
    }

    pub(super) fn is_array(&self) -> bool {
        self.raw.first() == Some(&b'[')
    }

    pub(super) fn is_null(&self) -> bool {
        self.raw.first() == Some(&b'n')
    }

    pub(super) fn is_string(&self) -> bool {
        self.raw.first() == Some(&b'"')
    }

    /// gjson `String`: a string unescaped, `null` as empty, an integer as
    /// written, another number in Go's shortest form, and anything else as
    /// written.
    pub(super) fn str(&self) -> String {
        match self.raw.first() {
            None | Some(b'n') => String::new(),
            Some(b'"') => String::from_utf8_lossy(&string_value(self.raw)).into_owned(),
            Some(b't' | b'f' | b'{' | b'[') => String::from_utf8_lossy(self.raw).into_owned(),
            Some(_) => {
                let digits = self.raw.strip_prefix(b"-").unwrap_or(self.raw);
                if digits.iter().all(u8::is_ascii_digit) {
                    String::from_utf8_lossy(self.raw).into_owned()
                } else {
                    format_float(self.num())
                }
            }
        }
    }

    /// gjson `Int`: `true` as 1, a string or a number as an integer, and
    /// anything else as 0.
    pub(super) fn int(&self) -> i64 {
        match self.raw.first() {
            Some(b't') => 1,
            Some(b'"') => parse_int(&string_value(self.raw)).unwrap_or(0),
            Some(b'-' | b'0'..=b'9') => {
                let f = self.num();
                if (-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(&f) {
                    // In range, so the cast only truncates.
                    #[allow(clippy::cast_possible_truncation)]
                    return f as i64;
                }
                // Out of range: Go's conversion is undefined; this saturates.
                #[allow(clippy::cast_possible_truncation)]
                parse_int(self.raw).unwrap_or(f as i64)
            }
            _ => 0,
        }
    }

    /// gjson `Bool`: `true`, a string Go's `ParseBool` reads as true once
    /// lowercased, or a non-zero number.
    pub(super) fn bool(&self) -> bool {
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
    pub(super) fn array(&self) -> Vec<Val<'a>> {
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
    pub(super) fn members(&self) -> Vec<(String, Val<'a>)> {
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
pub(super) fn get<'a>(doc: &'a [u8], path: &str) -> Option<Val<'a>> {
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

/// Four hex digits, or 0 when they aren't (gjson `runeit`).
fn hex4(digits: &[u8]) -> u32 {
    std::str::from_utf8(digits)
        .ok()
        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
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

/// Go's `strconv.FormatFloat(f, 'f', -1, 64)` (a private copy of
/// `open_ferry_translate::go::format_float`).
fn format_float(f: f64) -> String {
    if f == f64::INFINITY {
        "+Inf".to_owned()
    } else if f == f64::NEG_INFINITY {
        "-Inf".to_owned()
    } else {
        f.to_string()
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
pub(super) fn try_set_raw(doc: &[u8], path: &str, raw: &[u8]) -> Option<Vec<u8>> {
    set(doc, path, New::Raw(raw))
}

/// [`try_set_raw`], keeping `doc` as it is where sjson fails, as upstream
/// does where it ignores the error.
pub(super) fn set_raw(doc: &[u8], path: &str, raw: &[u8]) -> Vec<u8> {
    try_set_raw(doc, path, raw).unwrap_or_else(|| doc.to_vec())
}

/// sjson `SetBytes(doc, path, value)` for a string; see [`try_set_raw`].
pub(super) fn try_set_str(doc: &[u8], path: &str, value: &str) -> Option<Vec<u8>> {
    set(doc, path, New::Str(value))
}

/// [`try_set_str`], keeping `doc` where sjson fails.
pub(super) fn set_str(doc: &[u8], path: &str, value: &str) -> Vec<u8> {
    try_set_str(doc, path, value).unwrap_or_else(|| doc.to_vec())
}

/// sjson `SetBytes(doc, path, value)` for a bool, keeping `doc` where sjson
/// fails.
pub(super) fn set_bool(doc: &[u8], path: &str, value: bool) -> Vec<u8> {
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

/// sjson `DeleteBytes(doc, path)`. A missing key leaves `doc` as it is.
pub(super) fn delete(doc: &[u8], path: &str) -> Vec<u8> {
    let keys: Vec<&str> = path.split('.').collect();
    let mut buf = Vec::with_capacity(doc.len());
    match delete_paths(&mut buf, doc, &keys) {
        Some(()) => buf,
        None => doc.to_vec(),
    }
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
pub(super) fn compact_html(items: &[&[u8]]) -> Option<Vec<u8>> {
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
pub(super) fn fold_eq(key: &str, target: &str) -> bool {
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
pub(super) fn json_string(s: &str) -> String {
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

/// gjson `Valid` (`open_ferry_translate::go::json_valid`, which is public).
pub(super) fn valid(bytes: &[u8]) -> bool {
    open_ferry_translate::go::json_valid(bytes)
}

/// The end of the JSON value at `i`, if it is valid. Nesting is tracked on
/// the heap, so deep input can't overflow the stack.
fn scan_value(bytes: &[u8], mut i: usize) -> Option<usize> {
    // The open containers; `true` for an object.
    let mut open: Vec<bool> = Vec::new();
    loop {
        i = skip_space(bytes, i);
        match *bytes.get(i)? {
            b'{' => {
                i = skip_space(bytes, i + 1);
                if bytes.get(i) == Some(&b'}') {
                    i += 1;
                } else {
                    open.push(true);
                    i = scan_key(bytes, i)?;
                    continue;
                }
            }
            b'[' => {
                i = skip_space(bytes, i + 1);
                if bytes.get(i) == Some(&b']') {
                    i += 1;
                } else {
                    open.push(false);
                    continue;
                }
            }
            b'"' => i = scan_string(bytes, i)?,
            b't' => i = scan_literal(bytes, i, b"true")?,
            b'f' => i = scan_literal(bytes, i, b"false")?,
            b'n' => i = scan_literal(bytes, i, b"null")?,
            _ => i = scan_number(bytes, i)?,
        }
        loop {
            let Some(&object) = open.last() else {
                return Some(i);
            };
            i = skip_space(bytes, i);
            match *bytes.get(i)? {
                b',' => {
                    i += 1;
                    if object {
                        i = scan_key(bytes, skip_space(bytes, i))?;
                    }
                    break;
                }
                b'}' if object => {
                    open.pop();
                    i += 1;
                }
                b']' if !object => {
                    open.pop();
                    i += 1;
                }
                _ => return None,
            }
        }
    }
}

/// The position after `"key":` at `i`.
fn scan_key(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let i = skip_space(bytes, scan_string(bytes, i)?);
    (bytes.get(i) == Some(&b':')).then_some(i + 1)
}

/// The end of the string whose opening quote is at `i`.
fn scan_string(bytes: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    loop {
        match *bytes.get(i)? {
            b'"' => return Some(i + 1),
            b'\\' => match *bytes.get(i + 1)? {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => i += 2,
                b'u' => {
                    if !bytes.get(i + 2..i + 6)?.iter().all(u8::is_ascii_hexdigit) {
                        return None;
                    }
                    i += 6;
                }
                _ => return None,
            },
            c if c < b' ' => return None,
            _ => i += 1,
        }
    }
}

fn scan_literal(bytes: &[u8], i: usize, literal: &[u8]) -> Option<usize> {
    bytes[i..].starts_with(literal).then_some(i + literal.len())
}

fn scan_number(bytes: &[u8], mut i: usize) -> Option<usize> {
    if bytes.get(i) == Some(&b'-') {
        i += 1;
    }
    match *bytes.get(i)? {
        b'0' => i += 1,
        b'1'..=b'9' => i = skip_digits(bytes, i),
        _ => return None,
    }
    if bytes.get(i) == Some(&b'.') {
        let end = skip_digits(bytes, i + 1);
        if end == i + 1 {
            return None;
        }
        i = end;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let end = skip_digits(bytes, i);
        if end == i {
            return None;
        }
        i = end;
    }
    Some(i)
}

fn skip_digits(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    i
}

fn skip_space(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}
