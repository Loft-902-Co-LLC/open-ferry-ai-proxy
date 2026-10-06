// Ported from Go's mime/mediatype.go (ParseMediaType, FormatMediaType,
// checkMediaTypeDisposition, consumeToken, consumeValue,
// consumeMediaParam, decode2231Enc, percentHexUnescape), mime/grammar.go
// (isTSpecial, isTokenChar, isToken), mime/encodedword.go (needsEncoding)
// and path/filepath (Base) (go1.26, BSD-3-Clause, see licenses/Go-LICENSE).
// https://github.com/golang/go

//! `Content-Type` and `Content-Disposition` values: a media type or
//! disposition and its parameters.
//!
//! Deviations from Go: [`base_name`] splits at `\` as well as `/` on every
//! system (see the module above).

use std::collections::HashMap;
use std::fmt;

use open_ferry_translate::go::{to_lower, trim_space};

use super::lossy;

/// Why a media type didn't parse (Go's errors from `ParseMediaType`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    text: &'static str,
    media_type: String,
}

impl ParseError {
    /// The media type read before a parameter failed to parse, as Go's
    /// `ParseMediaType` returns it with `ErrInvalidMediaParameter`; empty
    /// for any other error.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text)
    }
}

impl std::error::Error for ParseError {}

fn error(text: &'static str) -> ParseError {
    ParseError {
        text,
        media_type: String::new(),
    }
}

/// A media type and its parameters by lower-case name.
pub type MediaType = (String, HashMap<String, Vec<u8>>);

/// Go's `mime.ParseMediaType`: the media type or disposition `value`
/// names, lower case, and its parameters by lower-case name, RFC 2231
/// continuations joined and extended values (`name*=charset'lang'value`)
/// decoded and put first.
pub fn parse_media_type(value: &[u8]) -> Result<MediaType, ParseError> {
    let base_len = value.iter().position(|&b| b == b';').unwrap_or(value.len());
    let (base, mut rest) = value.split_at_checked(base_len).unwrap_or((value, b""));
    let kind = to_lower(&lossy(base)).trim().to_owned();
    check_media_type(kind.as_bytes())?;
    let mut params = HashMap::new();
    // The parameters whose names hold a `*`, by the name before it.
    let mut continued: HashMap<String, HashMap<String, Vec<u8>>> = HashMap::new();
    while !rest.is_empty() {
        rest = trim_left_space(rest);
        if rest.is_empty() {
            break;
        }
        let Some((key, value, after)) = consume_param(rest) else {
            // One `;` at the end is let be.
            if trim_space(rest) == b";" {
                break;
            }
            return Err(ParseError {
                text: "mime: invalid media parameter",
                media_type: kind,
            });
        };
        let map = match key.split_once('*') {
            Some((name, _)) => continued.entry(name.to_owned()).or_default(),
            None => &mut params,
        };
        if map.get(&key).is_some_and(|old| *old != value) {
            return Err(error("mime: duplicate parameter name"));
        }
        map.insert(key, value);
        rest = after;
    }
    for (name, pieces) in continued {
        if let Some(value) = pieces.get(&format!("{name}*")) {
            if let Some(decoded) = decode_2231(value) {
                params.insert(name, decoded);
            }
            continue;
        }
        let mut joined = Vec::new();
        let mut found = false;
        for n in 0_usize.. {
            let simple = format!("{name}*{n}");
            if let Some(value) = pieces.get(&simple) {
                found = true;
                joined.extend_from_slice(value);
                continue;
            }
            let Some(value) = pieces.get(&format!("{simple}*")) else {
                break;
            };
            found = true;
            let decoded = if n == 0 {
                decode_2231(value)
            } else {
                percent_unescape(value)
            };
            joined.extend(decoded.unwrap_or_default());
        }
        if found {
            params.insert(name, joined);
        }
    }
    Ok((kind, params))
}

/// Go's `checkMediaTypeDisposition`: `kind` must be a token, or two joined
/// by `/`.
fn check_media_type(kind: &[u8]) -> Result<(), ParseError> {
    let (main, rest) = consume_token(kind);
    if main.is_empty() {
        return Err(error("mime: no media type"));
    }
    if rest.is_empty() {
        return Ok(());
    }
    let Some(rest) = rest.strip_prefix(b"/") else {
        return Err(error("mime: expected slash after first token"));
    };
    let (sub, rest) = consume_token(rest);
    if sub.is_empty() {
        return Err(error("mime: expected token after slash"));
    }
    if !rest.is_empty() {
        return Err(error("mime: unexpected content after media subtype"));
    }
    Ok(())
}

/// Go's `consumeMediaParam`: the `;` and parameter at the start of `rest`,
/// as its lower-case name, its value and what follows.
fn consume_param(rest: &[u8]) -> Option<(String, Vec<u8>, &[u8])> {
    let rest = trim_left_space(rest).strip_prefix(b";")?;
    let (name, rest) = consume_token(trim_left_space(rest));
    if name.is_empty() {
        return None;
    }
    let rest = trim_left_space(trim_left_space(rest).strip_prefix(b"=")?);
    let (value, after) = consume_value(rest);
    if value.is_empty() && after.len() == rest.len() {
        return None;
    }
    Some((lossy(name).to_ascii_lowercase(), value, after))
}

/// Go's `consumeValue`: the token or quoted string at the start of `rest`,
/// and what follows; empty, with all of `rest`, when there is none. In a
/// quoted string `\` escapes a special character only, and before any
/// other is kept, as Go keeps the `\` of a Windows path a browser sends.
fn consume_value(rest: &[u8]) -> (Vec<u8>, &[u8]) {
    let Some(mut quoted) = rest.strip_prefix(b"\"") else {
        let (token, after) = consume_token(rest);
        return (token.to_vec(), after);
    };
    let mut value = Vec::new();
    loop {
        match quoted {
            [b'"', after @ ..] => return (value, after),
            [b'\x5c', c, after @ ..] if is_special(*c) => {
                value.push(*c);
                quoted = after;
            }
            [] | [b'\r' | b'\n', ..] => return (Vec::new(), rest),
            [c, after @ ..] => {
                value.push(*c);
                quoted = after;
            }
        }
    }
}

/// Go's `consumeToken`: the token at the start of `rest`, and what
/// follows.
fn consume_token(rest: &[u8]) -> (&[u8], &[u8]) {
    let end = rest
        .iter()
        .position(|&b| !is_token_byte(b))
        .unwrap_or(rest.len());
    rest.split_at_checked(end).unwrap_or((rest, b""))
}

/// Go's `isTokenChar`: printable ASCII, not a space nor special.
fn is_token_byte(b: u8) -> bool {
    b > 0x20 && b < 0x7f && !is_special(b)
}

/// Go's `isToken`: a non-empty run of token bytes.
fn is_token(text: &[u8]) -> bool {
    !text.is_empty() && text.iter().all(|&b| is_token_byte(b))
}

/// Go's `isTSpecial`: whether `b` is one of `()<>@,;:\"/[]?=`.
fn is_special(b: u8) -> bool {
    b"()<>@,;:\x5c\"/[]?=".contains(&b)
}

/// Go's `decode2231Enc`: an RFC 2231 extended value,
/// `charset'language'value`, its charset US-ASCII or UTF-8 in any case, and
/// its value percent-decoded; the language is ignored.
fn decode_2231(value: &[u8]) -> Option<Vec<u8>> {
    let (charset, rest) = split_at_byte(value, b'\'')?;
    let (_language, encoded) = split_at_byte(rest, b'\'')?;
    match to_lower(&lossy(charset)).as_str() {
        "us-ascii" | "utf-8" => percent_unescape(encoded),
        _ => None,
    }
}

/// Go's `percentHexUnescape`: `value` with each `%` and the two hex digits
/// after it read as that byte; `None` when a `%` isn't followed by two.
fn percent_unescape(value: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(value.len());
    let mut bytes = value.iter();
    while let Some(&b) = bytes.next() {
        if b != b'%' {
            out.push(b);
            continue;
        }
        let mut digit = || bytes.next().and_then(|&d| char::from(d).to_digit(16));
        let (high, low) = (digit()?, digit()?);
        out.push(u8::try_from((high << 4) | low).ok()?);
    }
    Some(out)
}

/// `bytes` before and after the first `at`, if it holds one.
fn split_at_byte(bytes: &[u8], at: u8) -> Option<(&[u8], &[u8])> {
    let index = bytes.iter().position(|&b| b == at)?;
    Some((bytes.get(..index)?, bytes.get(index + 1..)?))
}

/// Go's `strings.TrimLeftFunc(s, unicode.IsSpace)`.
fn trim_left_space(mut bytes: &[u8]) -> &[u8] {
    while let Some((c, width)) = decode_rune(bytes)
        && c.is_whitespace()
    {
        bytes = bytes.get(width..).unwrap_or_default();
    }
    bytes
}

/// Go's `utf8.DecodeRune`: the character at the start of `bytes` and its
/// length, or `None` where Go reads `RuneError` from one byte.
fn decode_rune(bytes: &[u8]) -> Option<(char, usize)> {
    let width = match *bytes.first()? {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let encoded = bytes.get(..width)?;
    let text = std::str::from_utf8(encoded).ok()?;
    text.chars().next().map(|c| (c, width))
}

/// Go's `mime.FormatMediaType`: `kind` (a token, or two joined by `/`)
/// lower case, then each parameter in name order as `; name=value`, the
/// value a token as it is, else quoted, else, when it holds a byte outside
/// printable ASCII other than a tab, as an RFC 2231 extended value
/// (`name*=utf-8''...`). Empty when `kind` or a name isn't a token.
pub fn format_media_type(kind: &str, params: &[(&str, &[u8])]) -> String {
    let mut out = String::new();
    match kind.split_once('/') {
        None => {
            if !is_token(kind.as_bytes()) {
                return String::new();
            }
            out.push_str(&kind.to_ascii_lowercase());
        }
        Some((major, sub)) => {
            if !is_token(major.as_bytes()) || !is_token(sub.as_bytes()) {
                return String::new();
            }
            out.push_str(&major.to_ascii_lowercase());
            out.push('/');
            out.push_str(&sub.to_ascii_lowercase());
        }
    }
    let mut sorted = params.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted.dedup_by(|a, b| a.0 == b.0);
    for (name, value) in sorted {
        out.push_str("; ");
        if !is_token(name.as_bytes()) {
            return String::new();
        }
        out.push_str(&name.to_ascii_lowercase());
        if value
            .iter()
            .any(|&b| !(b' '..=b'~').contains(&b) && b != b'\t')
        {
            out.push_str("*=utf-8''");
            for &b in value {
                if b <= b' ' || b >= 0x7f || b"*'%".contains(&b) || is_special(b) {
                    out.push_str(&format!("%{b:02X}"));
                } else {
                    out.push(char::from(b));
                }
            }
            continue;
        }
        out.push('=');
        // Only printable ASCII and tabs are left.
        let text = lossy(value);
        if is_token(value) {
            out.push_str(&text);
            continue;
        }
        out.push('"');
        for c in text.chars() {
            if c == '"' || c == '\x5c' {
                out.push('\x5c');
            }
            out.push(c);
        }
        out.push('"');
    }
    out
}

/// Go's `filepath.Base` on Windows, less volume names: what follows the
/// last `/` or `\`, past any at the end; `.` for an empty path, `\` for
/// a path of separators only.
pub fn base_name(path: &[u8]) -> &[u8] {
    let is_separator = |b: &u8| matches!(b, b'/' | b'\x5c');
    if path.is_empty() {
        return b".";
    }
    let end = path
        .iter()
        .rposition(|b| !is_separator(b))
        .map_or(0, |last| last + 1);
    let trimmed = path.get(..end).unwrap_or_default();
    if trimmed.is_empty() {
        return b"\x5c";
    }
    trimmed.rsplit(is_separator).next().unwrap_or(trimmed)
}
