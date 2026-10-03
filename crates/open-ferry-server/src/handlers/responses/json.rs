// Ported from tidwall/gjson v1.18.0 gjson.go and tidwall/sjson v1.2.5
// sjson.go (MIT), as CLIProxyAPI uses them.
// https://github.com/tidwall/gjson
// https://github.com/tidwall/sjson

//! The parts of gjson and sjson the Responses handlers use, on JSON text.
//!
//! Lookups take the first entry for a key, as gjson does, and edits keep
//! the rest of the text byte for byte, as sjson does.

use std::ops::Range;

use serde_json::{Map, Value};

/// Where the value at `key` is in the object `text`, the first if the key
/// repeats (gjson `Get(text, key)` for a plain key). `None` if `text` isn't
/// an object, the key isn't in it, or the object breaks off before it.
pub(super) fn member(text: &[u8], key: &str) -> Option<Range<usize>> {
    let mut i = skip_space(text, 0);
    if text.get(i) != Some(&b'{') {
        return None;
    }
    i = skip_space(text, i + 1);
    loop {
        if text.get(i) != Some(&b'"') {
            return None;
        }
        let key_end = scan_string(text, i)?;
        let matched = key_is(&text[i..key_end], key);
        i = skip_space(text, key_end);
        if text.get(i) != Some(&b':') {
            return None;
        }
        let start = skip_space(text, i + 1);
        let end = scan_value(text, start)?;
        if matched {
            return Some(start..end);
        }
        i = skip_space(text, end);
        if text.get(i) != Some(&b',') {
            return None;
        }
        i = skip_space(text, i + 1);
    }
}

/// The value at `path` in `text`, as written (gjson `Get(text, "a.b").Raw`
/// for plain keys).
pub(super) fn get<'t>(text: &'t [u8], path: &[&str]) -> Option<&'t [u8]> {
    let mut value = text;
    for key in path {
        value = &value[member(value, key)?];
    }
    Some(value)
}

/// gjson `Get(text, key)` on text that need not be JSON: the value at `key`
/// in the object at the first `{`, unless a `[` comes first.
pub(super) fn find<'t>(text: &'t [u8], key: &str) -> Option<&'t [u8]> {
    let start = text.iter().position(|&c| c == b'{' || c == b'[')?;
    if text[start] == b'[' {
        return None;
    }
    let text = &text[start..];
    Some(&text[member(text, key)?])
}

/// Whether `raw` is an object.
pub(super) fn is_object(raw: &[u8]) -> bool {
    raw.first() == Some(&b'{')
}

/// Whether `raw` is an array with nothing in it.
pub(super) fn is_empty_array(raw: &[u8]) -> bool {
    raw.first() == Some(&b'[') && raw.get(skip_space(raw, 1)) == Some(&b']')
}

/// gjson `String()` of the value `raw`.
pub(super) fn string(raw: &[u8]) -> String {
    match raw.first() {
        None | Some(b'n') => String::new(),
        Some(b't') => "true".to_owned(),
        Some(b'f') => "false".to_owned(),
        Some(b'"') => decode_string(raw),
        Some(b'{' | b'[') => String::from_utf8_lossy(raw).into_owned(),
        Some(_) => {
            let digits = raw.strip_prefix(b"-").unwrap_or(raw);
            if digits.iter().all(u8::is_ascii_digit) {
                String::from_utf8_lossy(raw).into_owned()
            } else {
                format_float(parse_float(raw))
            }
        }
    }
}

/// gjson `Int()` of the value `raw`: 1 for `true`, a string read as an
/// integer, a number truncated, and 0 for anything else.
pub(super) fn int(raw: &[u8]) -> i64 {
    match raw.first() {
        Some(b't') => 1,
        Some(b'"') => parse_int(decode_string(raw).as_bytes()).unwrap_or(0),
        Some(b'-' | b'0'..=b'9') => {
            let f = parse_float(raw);
            if (-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(&f) {
                return f as i64;
            }
            parse_int(raw).unwrap_or_else(|| go_int64(f))
        }
        _ => 0,
    }
}

/// sjson `DeleteBytes(text, key)` for a top-level key: `text` without the
/// key's first entry, and the comma that went with it. `None` if the key
/// isn't there.
pub(super) fn delete(text: &[u8], key: &str) -> Option<Vec<u8>> {
    let value = member(text, key)?;
    let (head, strip_next_comma) = delete_tail_item(&text[..value.start]);
    let mut rest = value.end;
    if strip_next_comma {
        let next = rest + text[rest..].iter().take_while(|&&c| c <= b' ').count();
        if text.get(next) == Some(&b',') {
            rest = next + 1;
        }
    }
    Some([&text[..head], &text[rest..]].concat())
}

/// sjson's `deleteTailItem`: how much of `head`, the text before a value,
/// is left once the value's key and the comma before it are gone, and
/// whether a comma after the value has to go instead.
fn delete_tail_item(head: &[u8]) -> (usize, bool) {
    let at = |i: isize| head[i as usize];
    let mut i = head.len() as isize - 1;
    while i >= 0 {
        match at(i) {
            b'[' => return (head.len(), true),
            b',' => return (i as usize, false),
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
                                        b'{' => return (i as usize + 1, true),
                                        b',' => return (i as usize, false),
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
    (head.len(), false)
}

/// sjson `SetRawBytes(text, "response.output", output)`. `None` where sjson
/// fails, which leaves the text as it was.
pub(super) fn set_response_output(text: &[u8], output: &[u8]) -> Option<Vec<u8>> {
    match member(text, "response") {
        Some(response) => {
            let set = set_raw(&text[response.clone()], "output", output)?;
            Some([&text[..response.start], &set, &text[response.end..]].concat())
        }
        None => set_raw(text, "response", &[b"{\"output\":", output, b"}"].concat()),
    }
}

/// sjson `SetRawBytes(text, key, raw)` for a top-level key: the value
/// replaced where the key is, otherwise the entry added at the end of the
/// object. Text that is neither an object nor an array is taken for `{}`;
/// an array fails.
fn set_raw(text: &[u8], key: &str, raw: &[u8]) -> Option<Vec<u8>> {
    if let Some(value) = member(text, key) {
        return Some([&text[..value.start], raw, &text[value.end..]].concat());
    }
    let start = text.iter().position(|&c| c > b' ');
    let object = match start.map(|start| (text[start], &text[start..])) {
        Some((b'{', object)) => object,
        Some((b'[', _)) => return None,
        _ => b"{}",
    };
    let end = (1..object.len())
        .rev()
        .find(|&i| object[i] == b'}')
        .unwrap_or(0);
    let has_entries = object[1..]
        .iter()
        .find(|&&c| c > b' ')
        .is_some_and(|&c| c != b'}' && c != b']');
    let mut out = object[..end].to_vec();
    if has_entries {
        out.push(b',');
    }
    out.extend_from_slice(serde_json::to_string(key).unwrap_or_default().as_bytes());
    out.push(b':');
    out.extend_from_slice(raw);
    out.push(b'}');
    Some(out)
}

/// `value` with each object's keys in the order Go's `json.Marshal` writes
/// a map's.
pub(super) fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(sorted_map(fields)),
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// [`sorted`] for an object.
pub(super) fn sorted_map(fields: &Map<String, Value>) -> Map<String, Value> {
    let mut entries: Vec<(&String, &Value)> = fields.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .into_iter()
        .map(|(key, value)| (key.clone(), sorted(value)))
        .collect()
}

/// Whether the quoted key `raw` is `key` once its escapes are read.
fn key_is(raw: &[u8], key: &str) -> bool {
    let inner = &raw[1..raw.len() - 1];
    if inner.contains(&b'\\') {
        decode_string(raw) == key
    } else {
        inner == key.as_bytes()
    }
}

/// A JSON string's text, or empty if it can't be read.
fn decode_string(raw: &[u8]) -> String {
    serde_json::from_slice(raw).unwrap_or_default()
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
/// matched by their brackets, outside strings.
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

/// gjson's `parseInt`: an optional `-` and digits, wrapping on overflow.
fn parse_int(text: &[u8]) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix(b"-") {
        Some(digits) => (true, digits),
        None => (false, text),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.wrapping_mul(10).wrapping_add(i64::from(c - b'0'));
    }
    Some(if negative { n.wrapping_neg() } else { n })
}

/// A JSON number as Go's `strconv.ParseFloat` reads it, infinite when too
/// big.
fn parse_float(raw: &[u8]) -> f64 {
    std::str::from_utf8(raw)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0)
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

/// Go's `strconv.FormatFloat(f, 'f', -1, 64)`.
fn format_float(f: f64) -> String {
    if f.is_infinite() {
        if f > 0.0 { "+Inf" } else { "-Inf" }.to_owned()
    } else {
        f.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_first_entry_for_a_key() {
        let text = br#" { "a" : 1 , "b":{"c":[1,"]"]}, "a":2, "esc":"x\"y" }"#;
        assert_eq!(get(text, &["a"]), Some(&b"1"[..]));
        assert_eq!(get(text, &["b", "c"]), Some(&br#"[1,"]"]"#[..]));
        assert_eq!(get(text, &["esc"]), Some(&br#""x\"y""#[..]));
        assert_eq!(get(text, &["b", "x"]), None);
        assert_eq!(get(b"[1]", &["a"]), None);
        assert_eq!(
            find(br#"oops {"sequence_number":5} more"#, "sequence_number"),
            Some(&b"5"[..])
        );
        assert_eq!(find(br#"[{"sequence_number":5}]"#, "sequence_number"), None);
        assert_eq!(find(b"no json", "sequence_number"), None);
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
            assert_eq!(int(raw.as_bytes()), want, "{raw}");
        }
        let strings = [
            (r#""a\nb""#, "a\nb"),
            ("1.50", "1.5"),
            ("-12", "-12"),
            ("1e2", "100"),
            ("true", "true"),
            ("null", ""),
            (r#"{ "a":1 }"#, r#"{ "a":1 }"#),
        ];
        for (raw, want) in strings {
            assert_eq!(string(raw.as_bytes()), want, "{raw}");
        }
        assert!(is_empty_array(b"[ ]"));
        assert!(!is_empty_array(b"[0]"));
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
            let got = delete(text.as_bytes(), "stream").unwrap();
            assert_eq!(String::from_utf8(got).unwrap(), want, "{text}");
        }
        assert_eq!(delete(br#"{"model":"x"}"#, "stream"), None);
    }

    #[test]
    fn sets_the_response_output_as_sjson_does() {
        let set = |text: &str| {
            set_response_output(text.as_bytes(), b"[1]").map(|out| String::from_utf8(out).unwrap())
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
