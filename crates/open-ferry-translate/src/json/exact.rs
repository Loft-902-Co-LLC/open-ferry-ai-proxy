//! JSON read as `serde_json` reads it, with each number's text kept exactly
//! as written.
//!
//! With `arbitrary_precision` on, a [`Number`] holds its text and writes it
//! back unchanged. `serde_json`'s reader respells a few numbers as it reads
//! them, though: `-0` becomes `0`, and an exponent is written with a small
//! `e` and a sign, so `1E20` becomes `1e+20` and `1e5` becomes `1e+5`.
//! Upstream copies a client's JSON text byte for byte where it moves a value
//! (gjson's `Raw` into sjson's `SetRaw`), so text read here keeps every
//! number's spelling, whatever the translator then does with it.
//!
//! [`from_slice`] and [`from_str`] accept exactly what `serde_json` accepts,
//! and give the same value but for those numbers' text.

use serde_json::{Map, Number, Value};

/// How deep arrays and objects may nest. `serde_json` refuses JSON nested
/// this deep before [`Reader`] sees it, so this only guards the recursion.
const MAX_DEPTH: usize = 128;

/// `serde_json::from_slice` into a [`Value`], keeping each number's text.
///
/// # Errors
///
/// `serde_json`'s error, where `bytes` isn't one JSON value it can read.
pub fn from_slice(bytes: &[u8]) -> serde_json::Result<Value> {
    let value = serde_json::from_slice(bytes)?;
    Ok(keep_text(bytes, value))
}

/// `serde_json::from_str` into a [`Value`], keeping each number's text.
///
/// # Errors
///
/// `serde_json`'s error, where `text` isn't one JSON value it can read.
pub fn from_str(text: &str) -> serde_json::Result<Value> {
    from_slice(text.as_bytes())
}

/// The first value of a `serde_json` stream over `bytes`, keeping each
/// number's text: what follows it is left unread. `None` if `bytes` holds
/// nothing but white space.
///
/// # Errors
///
/// `serde_json`'s error, where the first value isn't one it can read.
pub fn first(bytes: &[u8]) -> Option<serde_json::Result<Value>> {
    let mut stream = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    let value = stream.next()?;
    let read = bytes.get(..stream.byte_offset()).unwrap_or(bytes);
    Some(value.map(|value| keep_text(read, value)))
}

/// `value`, which `serde_json` read from `bytes`, with each number's text
/// as `bytes` has it.
fn keep_text(bytes: &[u8], value: Value) -> Value {
    if !respells_a_number(bytes) {
        return value;
    }
    let mut reader = Reader { bytes, at: 0 };
    reader.value(0).unwrap_or(value)
}

/// Whether `bytes` holds a number outside a string that `serde_json` writes
/// other than as written (see [`respelled`]).
fn respells_a_number(bytes: &[u8]) -> bool {
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        at = match byte {
            b'"' => string_end(bytes, at),
            b'-' | b'0'..=b'9' => {
                let end = number_end(bytes, at);
                if bytes.get(at..end).is_some_and(respelled) {
                    return true;
                }
                end
            }
            _ => at + 1,
        };
    }
    false
}

/// Whether `serde_json` writes the number `token` other than as written:
/// `-0`, or one with an `E`, or with an exponent without its sign.
fn respelled(token: &[u8]) -> bool {
    token == b"-0"
        || token.iter().enumerate().any(|(index, &byte)| {
            byte == b'E' || (byte == b'e' && !matches!(token.get(index + 1), Some(b'+' | b'-')))
        })
}

/// The index just past the string whose opening quote is at `at`, or the
/// end of `bytes` if it doesn't end.
fn string_end(bytes: &[u8], at: usize) -> usize {
    let mut at = at + 1;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'"' => return at + 1,
            b'\\' => at += 2,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// The index just past the number starting at `at`.
fn number_end(bytes: &[u8], at: usize) -> usize {
    let mut end = at;
    while bytes
        .get(end)
        .is_some_and(|byte| matches!(byte, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
    {
        end += 1;
    }
    end
}

/// Reads JSON that `serde_json` has already read, so it is valid.
struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl Reader<'_> {
    fn skip_space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// The next byte after any space, consumed.
    fn next_token(&mut self) -> Option<u8> {
        self.skip_space();
        let byte = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    /// The value at the cursor, `depth` arrays and objects down. `None` only
    /// if the JSON isn't valid after all.
    fn value(&mut self, depth: usize) -> Option<Value> {
        self.skip_space();
        let value = match *self.bytes.get(self.at)? {
            b'{' => {
                if depth >= MAX_DEPTH {
                    return None;
                }
                self.at += 1;
                let mut fields = Map::new();
                self.skip_space();
                if self.bytes.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Some(Value::Object(fields));
                }
                loop {
                    self.skip_space();
                    let key = self.string()?;
                    if self.next_token()? != b':' {
                        return None;
                    }
                    // As `serde_json` does, a repeated key keeps its place
                    // and takes the last value.
                    fields.insert(key, self.value(depth + 1)?);
                    match self.next_token()? {
                        b',' => {}
                        b'}' => break Value::Object(fields),
                        _ => return None,
                    }
                }
            }
            b'[' => {
                if depth >= MAX_DEPTH {
                    return None;
                }
                self.at += 1;
                let mut items = Vec::new();
                self.skip_space();
                if self.bytes.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Some(Value::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    match self.next_token()? {
                        b',' => {}
                        b']' => break Value::Array(items),
                        _ => return None,
                    }
                }
            }
            b'"' => Value::String(self.string()?),
            b't' => self.literal("true", Value::Bool(true))?,
            b'f' => self.literal("false", Value::Bool(false))?,
            b'n' => self.literal("null", Value::Null)?,
            _ => {
                let end = number_end(self.bytes, self.at);
                let token = std::str::from_utf8(self.bytes.get(self.at..end)?).ok()?;
                self.at = end;
                // The token is a valid JSON number, which `serde_json` would
                // hold as text too, only respelled.
                Value::Number(Number::from_string_unchecked(token.to_owned()))
            }
        };
        Some(value)
    }

    /// `value` if `word` comes next, consumed.
    fn literal(&mut self, word: &str, value: Value) -> Option<Value> {
        let end = self.at + word.len();
        (self.bytes.get(self.at..end)? == word.as_bytes()).then(|| {
            self.at = end;
            value
        })
    }

    /// The string at the cursor, decoded as `serde_json` decodes it.
    fn string(&mut self) -> Option<String> {
        let start = self.at;
        let end = string_end(self.bytes, start);
        let token = self.bytes.get(start..end)?;
        self.at = end;
        match token.get(1..token.len().checked_sub(1)?) {
            Some(inner) if !inner.contains(&b'\\') => {
                std::str::from_utf8(inner).ok().map(str::to_owned)
            }
            _ => serde_json::from_slice(token).ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: gjson's `Raw` is the text as written, which upstream
    // copies; this reader keeps each number so.
    #[test]
    fn numbers_keep_their_text() {
        for text in [
            "-0",
            "1E20",
            "1e20",
            "1E+2",
            "-1.5E-3",
            "1e-7",
            "0.10",
            "-0.0",
            "-0e0",
            "123456789012345678901234567890",
            "9007199254740993",
            "1e400",
            "5",
            "-7",
        ] {
            assert_eq!(from_str(text).unwrap().to_string(), text);
        }
        let text = r#"{"x":-0,"y":[1E20,{"z":1e5}],"s":"-0 1E5","w":0.10}"#;
        assert_eq!(from_str(text).unwrap().to_string(), text);
    }

    // Not upstream's: everything but the respelled numbers is as
    // `serde_json` reads it.
    #[test]
    fn reads_the_rest_as_serde_json_does() {
        for text in [
            r#" { "a" : [ 1 , -0 , "x\"y\u00e9\ud83d\ude00" ] , "b" : { } , "c" : [ ] } "#,
            r#"{"a":1,"b":2,"a":-0}"#,
            r#"{"k\u0041":true,"n":null,"f":false,"e":1E2}"#,
            r#"["\\","\/","\n",""]"#,
            "\"caf\u{e9}\"",
        ] {
            let serde: Value = serde_json::from_str(text).unwrap();
            let exact = from_str(text).unwrap();
            assert_eq!(
                exact.to_string().replace("-0", "0").replace("1E2", "1e+2"),
                serde.to_string(),
                "{text}"
            );
        }
        // A repeated key keeps its first place and its last value.
        assert_eq!(
            from_str(r#"{"a":1,"b":2,"a":-0}"#).unwrap().to_string(),
            r#"{"a":-0,"b":2}"#
        );
    }

    // Not upstream's: what serde_json refuses is refused.
    #[test]
    fn refuses_what_serde_json_refuses() {
        let deep = format!("{}-0{}", "[".repeat(200), "]".repeat(200));
        for text in [
            "",
            "-",
            "01",
            "1E",
            "{\"a\":-0",
            "[-0] x",
            r#""\ud800""#,
            deep.as_str(),
        ] {
            assert!(from_str(text).is_err(), "{text}");
        }
        assert!(from_slice(b"[\"\xff\",-0]").is_err());
    }

    // Not upstream's: gjson's `ParseBytes` reads the first value and stops.
    #[test]
    fn first_reads_one_value() {
        let read =
            |text: &str| first(text.as_bytes()).map(|value| value.map(|v| v.to_string()).ok());
        assert_eq!(
            read(r#" {"a":-0} {"b":1E5}"#),
            Some(Some(r#"{"a":-0}"#.into()))
        );
        assert_eq!(read(r#"[1E5]x"#), Some(Some("[1E5]".into())));
        assert_eq!(read(r#"{"a":"#), Some(None));
        assert_eq!(read("  "), None);
    }
}
