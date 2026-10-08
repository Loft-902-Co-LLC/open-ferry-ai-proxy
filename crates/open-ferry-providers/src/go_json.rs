// Ported from Go's encoding/json (decode.go: Unmarshal, unquote, and how
// decodeState fills structs, slices and interfaces; scanner.go: checkValid)
// (go1.26, BSD-3-Clause), as CLIProxyAPI decodes into structs with it
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! Go's `json.Unmarshal` into a struct, read from the JSON text rather
//! than from a parsed [`Value`], for where what Go makes of unusual JSON
//! shows.
//!
//! Go decodes a key each time it appears, so a repeated key decodes into
//! the same field again: an object merges into the struct already there,
//! and an array is decoded into the slice's own elements, even those a
//! shorter array left behind. A byte of invalid UTF-8 or a lone surrogate
//! in a string reads as U+FFFD, and values may nest 10000 deep.
//!
//! This is Go 1.26's `encoding/json`, which upstream's releases are built
//! with. From Go 1.27 the package runs on its v2 implementation, which
//! differs in places, such as unescaping a string before `time.Time`
//! reads it.
//!
//! Deviations from Go:
//! - A value decoded into an `any` that nests more than 128 deep fails the
//!   decode; Go reads it.
//! - Error texts are this module's own.

use std::borrow::Cow;

use serde::de::IgnoredAny;
use serde_json::{Map, Number, Value};

/// How deep Go's scanner lets values nest.
const MAX_NESTING_DEPTH: usize = 10_000;
/// How deep a value decoded into an `any` may nest here.
const MAX_ANY_DEPTH: usize = 128;

/// `data` as Go's `json.Unmarshal` checks it before decoding: one JSON
/// value with only whitespace around it, nested no deeper than Go allows.
/// Each byte of invalid UTF-8, which only a string may hold, reads as
/// U+FFFD, as Go reads it in a string.
pub(crate) fn check(data: &[u8]) -> Result<Cow<'_, str>, String> {
    let text = match std::str::from_utf8(data) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => {
            let mut text = String::with_capacity(data.len() + 16);
            for chunk in data.utf8_chunks() {
                text.push_str(chunk.valid());
                for _ in chunk.invalid() {
                    text.push(char::REPLACEMENT_CHARACTER);
                }
            }
            Cow::Owned(text)
        }
    };
    // serde_json checks the syntax as Go's scanner does, but without a
    // limit on nesting.
    serde_json::from_str::<IgnoredAny>(&text).map_err(|e| e.to_string())?;
    let bytes = text.as_bytes();
    let (mut depth, mut i) = (0, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = string_end(bytes, i);
                continue;
            }
            b'[' | b'{' => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return Err("exceeded max depth".to_owned());
                }
            }
            b']' | b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    Ok(text)
}

/// One value of a text [`check`] accepted, as written.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Raw<'t>(&'t str);

impl<'t> Raw<'t> {
    /// The value a text [`check`] accepted holds.
    pub(crate) fn of(text: &'t str) -> Self {
        Self(text.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r')))
    }

    /// The value as written: a string keeps its quotes and escapes.
    pub(crate) fn text(self) -> &'t str {
        self.0
    }

    pub(crate) fn is_null(self) -> bool {
        self.0 == "null"
    }

    /// Go's name for the kind of value, as its type errors give it.
    fn kind(self) -> &'static str {
        match self.0.as_bytes()[0] {
            b'n' => "null",
            b't' | b'f' => "bool",
            b'"' => "string",
            b'[' => "array",
            b'{' => "object",
            _ => "number",
        }
    }

    /// A string's text, unquoted as Go unquotes it.
    pub(crate) fn string(self) -> Option<String> {
        self.0.starts_with('"').then(|| unquote(self.0))
    }

    /// An object's members in order, repeated keys and all, each key
    /// unquoted.
    pub(crate) fn members(self) -> Option<Vec<(String, Raw<'t>)>> {
        let (text, bytes) = (self.0, self.0.as_bytes());
        if bytes[0] != b'{' {
            return None;
        }
        let mut members = Vec::new();
        let mut i = skip_space(bytes, 1);
        while bytes[i] == b'"' {
            let key_end = string_end(bytes, i);
            let key = unquote(&text[i..key_end]);
            // Past the colon.
            let start = skip_space(bytes, skip_space(bytes, key_end) + 1);
            let end = value_end(bytes, start);
            members.push((key, Raw(&text[start..end])));
            i = skip_space(bytes, end);
            if bytes[i] == b',' {
                i = skip_space(bytes, i + 1);
            }
        }
        Some(members)
    }

    /// An array's elements in order.
    pub(crate) fn elements(self) -> Option<Vec<Raw<'t>>> {
        let (text, bytes) = (self.0, self.0.as_bytes());
        if bytes[0] != b'[' {
            return None;
        }
        let mut elements = Vec::new();
        let mut i = skip_space(bytes, 1);
        while bytes[i] != b']' {
            let end = value_end(bytes, i);
            elements.push(Raw(&text[i..end]));
            i = skip_space(bytes, end);
            if bytes[i] == b',' {
                i = skip_space(bytes, i + 1);
            }
        }
        Some(elements)
    }
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_space(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).copied().is_some_and(is_space) {
        i += 1;
    }
    i
}

/// Where the value starting at `start` ends.
fn value_end(bytes: &[u8], start: usize) -> usize {
    match bytes[start] {
        b'"' => string_end(bytes, start),
        b'[' | b'{' => {
            let (mut depth, mut i) = (0, start);
            loop {
                match bytes[i] {
                    b'"' => {
                        i = string_end(bytes, i);
                        continue;
                    }
                    b'[' | b'{' => depth += 1,
                    b']' | b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        _ => bytes[start..]
            .iter()
            .position(|&byte| matches!(byte, b',' | b']' | b'}') || is_space(byte))
            .map_or(bytes.len(), |len| start + len),
    }
}

/// Where the string starting at `start` ends, past its closing quote.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    loop {
        match bytes[i] {
            b'"' => return i + 1,
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
}

/// Go's `unquote` of a string as written: its escapes decoded, and a `\u`
/// escape for half of a surrogate pair without the other half read as
/// U+FFFD.
fn unquote(quoted: &str) -> String {
    let mut rest = &quoted[1..quoted.len() - 1];
    let mut text = String::with_capacity(rest.len());
    while let Some(at) = rest.find('\\') {
        text.push_str(&rest[..at]);
        let escape = &rest[at + 1..];
        let (c, len) = match escape.as_bytes()[0] {
            b'b' => ('\x08', 1),
            b'f' => ('\x0c', 1),
            b'n' => ('\n', 1),
            b'r' => ('\r', 1),
            b't' => ('\t', 1),
            b'u' => {
                let unit = hex4(&escape[1..5]);
                let next = escape
                    .get(5..11)
                    .filter(|next| next.starts_with("\\u"))
                    .map(|next| hex4(&next[2..]));
                match (unit, next) {
                    (0xd800..0xdc00, Some(low @ 0xdc00..0xe000)) => {
                        let pair = 0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00);
                        (char::from_u32(pair).unwrap_or_default(), 11)
                    }
                    (0xd800..0xe000, _) => (char::REPLACEMENT_CHARACTER, 5),
                    _ => (char::from_u32(unit).unwrap_or_default(), 5),
                }
            }
            // A quote, backslash or slash stands for itself.
            other => (char::from(other), 1),
        };
        text.push(c);
        rest = &escape[len..];
    }
    text.push_str(rest);
    text
}

fn hex4(digits: &str) -> u32 {
    u32::from_str_radix(digits, 16).unwrap_or_default()
}

/// Go's error for a value of the wrong kind for its field.
pub(crate) fn type_error(raw: Raw<'_>, field: &str, go_type: &str) -> String {
    format!(
        "json: cannot unmarshal {} into Go struct field {field} of type {go_type}",
        raw.kind()
    )
}

/// The members of the object a Go struct decodes from, or `None` for a
/// `null`, which leaves the struct as it is.
pub(crate) fn object_or_null<'t>(
    raw: Raw<'t>,
    field: &str,
) -> Result<Option<Vec<(String, Raw<'t>)>>, String> {
    if raw.is_null() {
        return Ok(None);
    }
    raw.members()
        .map(Some)
        .ok_or_else(|| type_error(raw, field, "struct"))
}

/// Sets a Go `string`: `null` leaves it.
pub(crate) fn set_string(target: &mut String, raw: Raw<'_>, field: &str) -> Result<(), String> {
    if !raw.is_null() {
        *target = raw
            .string()
            .ok_or_else(|| type_error(raw, field, "string"))?;
    }
    Ok(())
}

/// Sets a Go `bool`: `null` leaves it.
pub(crate) fn set_bool(target: &mut bool, raw: Raw<'_>, field: &str) -> Result<(), String> {
    match raw.0 {
        "null" => {}
        "true" => *target = true,
        "false" => *target = false,
        _ => return Err(type_error(raw, field, "bool")),
    }
    Ok(())
}

/// Sets a Go `int`: `null` leaves it, and only an integer literal in range
/// fits.
pub(crate) fn set_int(target: &mut i64, raw: Raw<'_>, field: &str) -> Result<(), String> {
    if !raw.is_null() {
        *target = Some(raw)
            .filter(|raw| raw.kind() == "number")
            .and_then(|raw| raw.0.parse().ok())
            .ok_or_else(|| type_error(raw, field, "int"))?;
    }
    Ok(())
}

/// A Go `any`: `null` is nil, a number is a `float64`, which it must fit,
/// and an object is a map, in which a repeated key keeps its last value.
pub(crate) fn any_value(raw: Raw<'_>) -> Result<Value, String> {
    any_at(raw, 0)
}

fn any_at(raw: Raw<'_>, depth: usize) -> Result<Value, String> {
    Ok(match raw.kind() {
        "null" => Value::Null,
        "bool" => Value::Bool(raw.0 == "true"),
        "string" => Value::String(unquote(raw.0)),
        "number" => {
            if !raw.0.parse::<f64>().is_ok_and(f64::is_finite) {
                return Err(format!(
                    "json: cannot unmarshal number {} into Go value of type float64",
                    raw.0
                ));
            }
            Value::Number(raw.0.parse::<Number>().map_err(|e| e.to_string())?)
        }
        _ if depth == MAX_ANY_DEPTH => {
            return Err(format!("json: value nested more than {MAX_ANY_DEPTH} deep"));
        }
        "array" => Value::Array(
            raw.elements()
                .unwrap_or_default()
                .into_iter()
                .map(|item| any_at(item, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        _ => {
            let mut map = Map::new();
            for (key, item) in raw.members().unwrap_or_default() {
                map.insert(key, any_at(item, depth + 1)?);
            }
            Value::Object(map)
        }
    })
}

/// A Go slice as the decoder fills it. Go decodes an array into the
/// slice's own elements and then shortens it, so elements past its length
/// stay in its backing array, where a longer array decoded into it later
/// finds them: a `null` there leaves such an element as it was. A `null`
/// or `[]` gives a new, empty slice.
#[derive(Debug)]
pub(crate) struct Slice<T> {
    backing: Vec<T>,
    len: usize,
}

impl<T> Default for Slice<T> {
    fn default() -> Self {
        Self {
            backing: Vec::new(),
            len: 0,
        }
    }
}

impl<T: Default> Slice<T> {
    /// Decodes an array, `go_type` naming the slice's type for an error,
    /// with `element` decoding an item into an element.
    pub(crate) fn decode(
        &mut self,
        raw: Raw<'_>,
        field: &str,
        go_type: &str,
        mut element: impl FnMut(&mut T, Raw<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        if raw.is_null() {
            *self = Self::default();
            return Ok(());
        }
        let items = raw
            .elements()
            .ok_or_else(|| type_error(raw, field, go_type))?;
        if items.is_empty() {
            *self = Self::default();
            return Ok(());
        }
        for (i, item) in items.iter().enumerate() {
            if i == self.backing.len() {
                self.backing.push(T::default());
            }
            element(&mut self.backing[i], *item)?;
        }
        self.len = items.len();
        Ok(())
    }

    /// The slice's elements.
    pub(crate) fn into_vec(mut self) -> Vec<T> {
        self.backing.truncate(self.len);
        self.backing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backslash, so that escapes are built rather than written.
    const BS: &str = "\\";

    fn raw(text: &str) -> Raw<'_> {
        Raw::of(text)
    }

    #[test]
    fn checks_as_go_does() {
        for good in ["{}", " [1, -0, 2.5e-3, true, null, \"x\"] \r\n"] {
            assert!(check(good.as_bytes()).is_ok(), "{good:?}");
        }
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "1.",
            "{} x",
            "{a:1}",
            "\"\x01\"",
            "\"tab\t\"",
        ] {
            assert!(check(bad.as_bytes()).is_err(), "{bad:?}");
        }
        let bad_escape = format!("\"{BS}x\"");
        assert!(check(bad_escape.as_bytes()).is_err());
        let nested = |depth| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        assert!(check(nested(10_000).as_bytes()).is_ok());
        assert!(check(nested(10_001).as_bytes()).is_err());
        // Brackets in a string don't nest.
        let quoted = format!("[\"{}\"]", "[".repeat(10_001));
        assert!(check(quoted.as_bytes()).is_ok());
        // Invalid UTF-8 reads as U+FFFD a byte at a time in a string, and
        // fails anywhere else.
        let r = char::REPLACEMENT_CHARACTER;
        assert_eq!(
            check(b"\"a\xe2\x82b\xc0\x80\xff\"").unwrap(),
            format!("\"a{r}{r}b{r}{r}{r}\"")
        );
        assert!(check(b"[\"a\"\xff]").is_err());
    }

    #[test]
    fn reads_members_and_elements_as_written() {
        let text = format!(r#" {{ "a" : [1, {{"b":"]"}}] , "{BS}u0061":null,"a":-1e5 }} "#);
        let members = raw(&text).members().unwrap();
        let members: Vec<_> = members
            .iter()
            .map(|(k, v)| (k.as_str(), v.text()))
            .collect();
        assert_eq!(
            members,
            [("a", r#"[1, {"b":"]"}]"#), ("a", "null"), ("a", "-1e5")]
        );
        let items = raw(members[0].1).elements().unwrap();
        let items: Vec<_> = items.iter().map(|item| item.text()).collect();
        assert_eq!(items, ["1", r#"{"b":"]"}"#]);
        assert!(raw("[ ]").elements().unwrap().is_empty());
        assert!(raw("{ }").members().unwrap().is_empty());
        assert!(raw("\"x\"").members().is_none());
    }

    #[test]
    fn unquotes_as_go_does() {
        let r = char::REPLACEMENT_CHARACTER;
        let grin = char::from_u32(0x1f600).unwrap();
        for (escaped, want) in [
            (
                format!("\"{BS}\"{BS}{BS}{BS}/{BS}b{BS}f{BS}n{BS}r{BS}t\""),
                "\"\\/\x08\x0c\n\r\t".to_owned(),
            ),
            (format!("\"{BS}u00e9{BS}u0041\""), "\u{e9}A".to_owned()),
            (format!("\"{BS}ud83d{BS}ude00\""), grin.to_string()),
            // A lone half of a pair, or two high halves, or a low half
            // first: each reads as U+FFFD, and what follows is read anew.
            (format!("\"{BS}ud800x\""), format!("{r}x")),
            (
                format!("\"{BS}ud800{BS}ud83d{BS}ude00\""),
                format!("{r}{grin}"),
            ),
            (format!("\"{BS}ude00{BS}ud83d\""), format!("{r}{r}")),
            (format!("\"{BS}udfff\""), r.to_string()),
        ] {
            assert_eq!(raw(&escaped).string().unwrap(), want, "{escaped}");
        }
    }

    #[test]
    fn sets_fields_as_go_does() {
        let mut text = "kept".to_owned();
        set_string(&mut text, raw("null"), "f").unwrap();
        assert_eq!(text, "kept");
        set_string(&mut text, raw("\"new\""), "f").unwrap();
        assert_eq!(text, "new");
        assert_eq!(
            set_string(&mut text, raw("1"), "T.f").unwrap_err(),
            "json: cannot unmarshal number into Go struct field T.f of type string"
        );
        let mut flag = true;
        set_bool(&mut flag, raw("null"), "f").unwrap();
        assert!(flag);
        set_bool(&mut flag, raw("false"), "f").unwrap();
        assert!(!flag);
        assert!(set_bool(&mut flag, raw("\"true\""), "f").is_err());
        let mut int = 7;
        set_int(&mut int, raw("null"), "f").unwrap();
        assert_eq!(int, 7);
        set_int(&mut int, raw("-0"), "f").unwrap();
        assert_eq!(int, 0);
        set_int(&mut int, raw("-9223372036854775808"), "f").unwrap();
        assert_eq!(int, i64::MIN);
        for bad in ["1e3", "1.0", "9223372036854775808", "\"1\"", "true"] {
            assert!(set_int(&mut int, raw(bad), "f").is_err(), "{bad}");
        }
    }

    #[test]
    fn decodes_any_as_go_does() {
        assert_eq!(any_value(raw("null")).unwrap(), Value::Null);
        let value = any_value(raw(r#"{"a":1,"b":[true,"x"],"a":{"c":null}}"#)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"a": {"c": null}, "b": [true, "x"]})
        );
        // Numbers must fit a float64; one too small to tell from zero does.
        assert!(any_value(raw("[1e400]")).is_err());
        assert!(any_value(raw("-1e309")).is_err());
        assert_eq!(any_value(raw("1e-400")).unwrap().as_f64(), Some(0.0));
        let nested = |depth| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        assert!(any_value(raw(&nested(MAX_ANY_DEPTH))).is_ok());
        assert!(any_value(raw(&nested(MAX_ANY_DEPTH + 1))).is_err());
    }

    #[test]
    fn slices_reuse_elements_as_go_does() {
        let decode = |slice: &mut Slice<String>, text: &str| {
            slice.decode(raw(text), "f", "[]string", |item, value| {
                set_string(item, value, "f")
            })
        };
        let mut slice = Slice::default();
        decode(&mut slice, r#"["a","b","c"]"#).unwrap();
        decode(&mut slice, r#"["x"]"#).unwrap();
        decode(&mut slice, "[null,null]").unwrap();
        assert_eq!(slice.into_vec(), ["x", "b"]);

        for reset in ["[]", "null"] {
            let mut slice = Slice::default();
            decode(&mut slice, r#"["a","b"]"#).unwrap();
            decode(&mut slice, reset).unwrap();
            decode(&mut slice, "[null]").unwrap();
            assert_eq!(slice.into_vec(), [""], "{reset}");
        }
        let mut slice = Slice::default();
        assert_eq!(
            decode(&mut slice, "\"a\"").unwrap_err(),
            "json: cannot unmarshal string into Go struct field f of type []string"
        );
    }
}
