//! The text of JSON values as written, for where upstream reads gjson `Raw`
//! or checks gjson `Valid`.

/// gjson `Get(key).Raw` on the JSON object `object`: its value as
/// written, the first if the key repeats. `object` must be valid JSON.
pub(crate) fn member<'t>(object: &'t str, key: &str) -> Option<&'t str> {
    let bytes = object.as_bytes();
    let mut i = skip_space(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return None;
    }
    i = skip_space(bytes, i + 1);
    while bytes.get(i) == Some(&b'"') {
        let key_end = scan_string(bytes, i)?;
        let raw_key = &object[i..key_end];
        // Past the colon.
        let start = skip_space(bytes, skip_space(bytes, key_end) + 1);
        let end = scan_value(bytes, start)?;
        if key_is(raw_key, key) {
            return Some(&object[start..end]);
        }
        i = skip_space(bytes, end);
        if bytes.get(i) == Some(&b',') {
            i = skip_space(bytes, i + 1);
        }
    }
    None
}

/// Whether the quoted, possibly escaped key `raw` is `key`.
fn key_is(raw: &str, key: &str) -> bool {
    match raw.get(1..raw.len() - 1) {
        Some(inner) if !inner.contains('\\') => inner == key,
        _ => serde_json::from_str::<String>(raw).is_ok_and(|raw| raw == key),
    }
}

/// gjson `Valid`: whether `text` is one JSON value. Unlike serde_json, it
/// accepts an unpaired surrogate escape.
pub(crate) fn valid(text: &str) -> bool {
    let bytes = text.as_bytes();
    scan_value(bytes, skip_space(bytes, 0)).is_some_and(|end| skip_space(bytes, end) == bytes.len())
}

/// The end of the JSON value at `i`, if it is valid. Nesting is tracked on
/// the heap, so deep input can't overflow the stack.
fn scan_value(bytes: &[u8], mut i: usize) -> Option<usize> {
    // The open containers; `true` for an object.
    let mut open: Vec<bool> = Vec::new();
    loop {
        // A value starts here.
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
        // A value ended: close containers until another value is due.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_is_the_first_value_as_written() {
        let object = r#" { "a" : [1, {"b": "}"}] , "k\u0065y": 1.50, "key": 2 } "#;
        assert_eq!(member(object, "a"), Some(r#"[1, {"b": "}"}]"#));
        assert_eq!(member(object, "key"), Some("1.50"));
        assert_eq!(member(object, "b"), None);
        assert_eq!(member("[1]", "a"), None);
        assert_eq!(member("{}", "a"), None);
    }

    #[test]
    fn valid_matches_gjson() {
        for text in [
            r#""\ud800""#,
            r#" {"a": [true, null, -0.5e+3]} "#,
            "0",
            r#""\u00e9""#,
        ] {
            assert!(valid(text), "{text}");
        }
        for text in [
            "",
            "01",
            "1.",
            "-",
            "tru",
            "[1] 2",
            "[1,]",
            r#"{"a" 1}"#,
            r#"{"a":1,}"#,
            r#""\q""#,
            r#""\u12""#,
            "\"a\nb\"",
        ] {
            assert!(!valid(text), "{text}");
        }
        // Depth doesn't overflow the stack.
        let deep = "[".repeat(100_000) + &"]".repeat(100_000);
        assert!(valid(&deep));
        assert!(!valid(&deep[1..]));
    }
}
