// Ported from tidwall/gjson v1.18.0 gjson.go (MIT, see licenses/gjson-LICENSE),
// as used by CLIProxyAPI.
// https://github.com/tidwall/gjson

//! gjson `Get` of one top-level key, on text that need not be valid JSON.
//!
//! Upstream reads some fields with gjson from text that can be malformed,
//! such as the arguments of a custom tool call that a stream cut off. gjson
//! reads what it can from such text, where serde_json reads nothing, so the
//! parts of gjson that do it are ported here: finding an object's key, and
//! skipping or reading its value. A string with an escape in it is decoded
//! up to the first escape gjson doesn't know, or control character.

use crate::go;

/// A value [`get`] found. Other than a string, each is kept as written,
/// gjson's `Raw`, which runs to the end of the text if the value doesn't end.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Found<'t> {
    /// A string, decoded as gjson decodes it.
    String(String),
    /// What gjson reads as a number, such as `1.50`, `-`, `nan` or `1x`.
    Number(&'t str),
    /// `true`, `false` or `null`, or what gjson takes for one by its first
    /// letter, such as `tru`.
    Literal(&'t str),
    /// An object or array.
    Json(&'t str),
}

impl Found<'_> {
    /// gjson `String()`. A number written other than as an integer is read as
    /// Go reads a float, 0 if it can't be, and written back as one.
    pub(crate) fn into_string(self) -> String {
        match self {
            Found::String(text) => text,
            Found::Number(raw) => {
                let digits = raw.strip_prefix('-').unwrap_or(raw);
                if digits.bytes().all(|c| c.is_ascii_digit()) {
                    raw.to_owned()
                } else {
                    go::format_float(go::parse_float(raw))
                }
            }
            Found::Literal(raw) => match raw.as_bytes()[0] {
                b't' => "true".to_owned(),
                b'f' => "false".to_owned(),
                _ => String::new(),
            },
            Found::Json(raw) => raw.to_owned(),
        }
    }
}

/// gjson `Get(text, key)` for a plain object key, one with no gjson path
/// syntax in it. Only the object at the first `{` in `text` is read, unless a
/// `[` comes first, and the first entry with the key counts. `None` if gjson
/// finds nothing.
pub(crate) fn get<'t>(text: &'t str, key: &str) -> Option<Found<'t>> {
    let json = text.as_bytes();
    let start = json.iter().position(|&c| c == b'{' || c == b'[')?;
    // An array has no keys.
    if json[start] == b'[' {
        return None;
    }
    object(text, start + 1, key)
}

/// `parseObject` for a one-key path, from just inside the `{`.
fn object<'t>(text: &'t str, mut i: usize, key: &str) -> Option<Found<'t>> {
    let json = text.as_bytes();
    while i < json.len() {
        // Anything before the next key is skipped, unless the object ends.
        while json[i] != b'"' {
            if json[i] == b'}' {
                return None;
            }
            i += 1;
            if i == json.len() {
                return None;
            }
        }
        let (end, escaped) = string(json, i + 1)?;
        let name = &text[i + 1..end - 1];
        let matched = if escaped {
            unescape(name) == key
        } else {
            name == key
        };
        i = end;
        // So is anything before the value.
        loop {
            let c = *json.get(i)?;
            let (end, found): (usize, fn(&'t str) -> Found<'t>) = match c {
                b'"' => {
                    let (end, escaped) = string(json, i + 1)?;
                    if matched {
                        let body = &text[i + 1..end - 1];
                        return Some(Found::String(if escaped {
                            unescape(body)
                        } else {
                            body.to_owned()
                        }));
                    }
                    i = end;
                    break;
                }
                b'{' | b'[' => (squash(json, i), Found::Json),
                // gjson reads `n` as a number unless `u` follows.
                b'n' if json.get(i + 1).is_some_and(|&next| next != b'u') => {
                    (number(json, i), Found::Number)
                }
                b'n' | b't' | b'f' => (literal(json, i), Found::Literal),
                b'+' | b'-' | b'0'..=b'9' | b'i' | b'I' | b'N' => (number(json, i), Found::Number),
                _ => {
                    i += 1;
                    continue;
                }
            };
            if matched {
                return Some(found(&text[i..end]));
            }
            i = end;
            break;
        }
    }
    None
}

/// `parseString`, from just after the opening quote: the index past the
/// closing quote, and whether a backslash came before it. A quote closes the
/// string unless an odd number of backslashes comes right before it. `None`
/// if the string doesn't end.
fn string(json: &[u8], start: usize) -> Option<(usize, bool)> {
    let mut escaped = false;
    for i in start..json.len() {
        match json[i] {
            b'\\' => escaped = true,
            b'"' if backslashes_before(json, i).is_multiple_of(2) => {
                return Some((i + 1, escaped));
            }
            _ => {}
        }
    }
    None
}

/// How many backslashes come right before `json[i]`. gjson never counts the
/// first byte.
fn backslashes_before(json: &[u8], i: usize) -> usize {
    json[1..i].iter().rev().take_while(|&&c| c == b'\\').count()
}

/// `parseSquash`: the index past the object or array at `i`, or the end of
/// `json` if it doesn't close. Brackets of every kind, parentheses too, count
/// toward the nesting, except in strings.
fn squash(json: &[u8], i: usize) -> usize {
    let mut depth = 1;
    let mut i = i + 1;
    while i < json.len() {
        match json[i] {
            b'"' => match string(json, i + 1) {
                Some((end, _)) => {
                    i = end;
                    continue;
                }
                None => return json.len(),
            },
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    json.len()
}

/// `parseNumber`: a number runs to the next space, control character, `,`,
/// `]` or `}`.
fn number(json: &[u8], i: usize) -> usize {
    end_of(json, i, |c| c <= b' ' || matches!(c, b',' | b']' | b'}'))
}

/// `parseLiteral`: a literal runs to the next byte that isn't a lowercase
/// letter.
fn literal(json: &[u8], i: usize) -> usize {
    end_of(json, i, |c| !c.is_ascii_lowercase())
}

/// The index of the first byte after `i` that `ends` a value, or the end of
/// `json`.
fn end_of(json: &[u8], i: usize, ends: impl Fn(u8) -> bool) -> usize {
    json[i + 1..]
        .iter()
        .position(|&c| ends(c))
        .map_or(json.len(), |n| i + 1 + n)
}

/// gjson's `unescape`: a string's body decoded, up to a control character, an
/// escape it doesn't know or one cut short. A `\u` escape whose four bytes
/// aren't hex digits gives U+0000; a surrogate followed by another `\u`
/// escape is read with it as a pair, and one that isn't a pair gives U+FFFD.
fn unescape(text: &str) -> String {
    let json = text.as_bytes();
    let mut out = Vec::with_capacity(json.len());
    let mut i = 0;
    while i < json.len() {
        let c = json[i];
        if c < b' ' {
            break;
        }
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&escape) = json.get(i) else {
            break;
        };
        let byte = match escape {
            b'\\' | b'/' | b'"' => escape,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                if i + 5 > json.len() {
                    break;
                }
                let mut unit = hex4(&json[i + 1..i + 5]);
                i += 5;
                if (0xD800..0xE000).contains(&unit)
                    && json.len() - i >= 6
                    && json[i] == b'\\'
                    && json[i + 1] == b'u'
                {
                    unit = decode_surrogates(unit, hex4(&json[i + 2..i + 6]));
                    i += 6;
                }
                let c = char::from_u32(unit).unwrap_or('\u{FFFD}');
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                continue;
            }
            _ => break,
        };
        out.push(byte);
        i += 1;
    }
    // A `\u` escape's four bytes can end inside a character, leaving the rest
    // of it on its own; Go keeps those bytes.
    String::from_utf8_lossy(&out).into_owned()
}

/// Go's `strconv.ParseUint(digits, 16, 64)`, which is 0 unless `digits` are
/// all hex digits.
fn hex4(digits: &[u8]) -> u32 {
    digits
        .iter()
        .try_fold(0, |unit, &digit| {
            Some(unit << 4 | char::from(digit).to_digit(16)?)
        })
        .unwrap_or(0)
}

/// Go's `utf16.DecodeRune`: what a surrogate pair encodes, or U+FFFD if
/// `high` and `low` aren't one.
fn decode_surrogates(high: u32, low: u32) -> u32 {
    if (0xD800..0xDC00).contains(&high) && (0xDC00..0xE000).contains(&low) {
        0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
    } else {
        0xFFFD
    }
}

#[cfg(test)]
mod tests;
