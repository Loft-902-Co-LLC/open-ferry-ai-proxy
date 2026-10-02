// Ported from CLIProxyAPI internal/client/codex/apply-patch/tool.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex's `apply_patch` tool, carried through APIs that only know functions.
//!
//! Codex declares `apply_patch` as a custom tool whose input is the raw patch
//! text. A client that only knows function tools calls it as a function with
//! the arguments `{"input": "<patch>"}`. These helpers wrap and unwrap that
//! envelope.
//!
//! Not ported yet: the function's schema and description, which only upstream's
//! Gemini translators use.

use serde_json::Value;

use crate::go;
use crate::json::str_of;

/// `IsCustomTool`: whether a tool declaration is the custom `apply_patch` tool.
pub(crate) fn is_custom_tool(tool: &Value) -> bool {
    str_of(tool.get("type")) == "custom" && str_of(tool.get("name")).trim() == "apply_patch"
}

/// `WrapInput`: the patch text as function arguments, `{"input":"…"}`,
/// escaped as Go's `json.Marshal` escapes it.
pub(crate) fn wrap_input(input: &str) -> String {
    format!("{{\"input\":{}}}", go::json_string(input))
}

/// `EscapeInputFragment`: part of the patch text, escaped for the inside of
/// the JSON string [`wrap_input`] writes. Escaped parts join up to the escaped
/// whole.
pub(crate) fn escape_input_fragment(fragment: &str) -> String {
    let quoted = go::json_string(fragment);
    quoted[1..quoted.len() - 1].to_owned()
}

/// `UnwrapInput`: the patch text from function arguments, which must be one
/// JSON object holding one string field, `input`, and nothing else. Upstream
/// reads them with Go's JSON tokenizer and returns an error otherwise.
pub(crate) fn unwrap_input(arguments: &str) -> Option<String> {
    let mut json = Tokens(arguments.as_bytes());
    json.expect(b'{')?;
    if json.string()? != "input" {
        return None;
    }
    json.expect(b':')?;
    let input = json.string()?;
    json.expect(b'}')?;
    json.skip_whitespace();
    json.0.is_empty().then_some(input)
}

/// The JSON text still to read.
struct Tokens<'a>(&'a [u8]);

impl Tokens<'_> {
    fn skip_whitespace(&mut self) {
        while let [b' ' | b'\t' | b'\n' | b'\r', rest @ ..] = self.0 {
            self.0 = rest;
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        self.skip_whitespace();
        let (&first, rest) = self.0.split_first()?;
        (first == byte).then(|| self.0 = rest)
    }

    /// A string, decoded as Go decodes it: an unpaired surrogate escape
    /// becomes U+FFFD rather than an error.
    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let (&byte, rest) = self.0.split_first()?;
            self.0 = rest;
            match byte {
                b'"' => break,
                b'\\' => {
                    let (&escape, rest) = self.0.split_first()?;
                    self.0 = rest;
                    let c = match escape {
                        b'"' | b'\\' | b'/' => char::from(escape),
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        _ => return None,
                    };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                byte if byte < b' ' => return None,
                byte => out.push(byte),
            }
        }
        // Bytes were copied whole from a `str` or encoded from a `char`.
        Some(String::from_utf8(out).expect("decoded strings are UTF-8"))
    }

    /// The character a `\u` escape names, after the `\u`. A high surrogate
    /// takes the low surrogate escape that follows it, if there is one.
    fn unicode_escape(&mut self) -> Option<char> {
        let unit = self.hex4()?;
        if !(0xd800..0xe000).contains(&unit) {
            return char::from_u32(unit);
        }
        if (0xd800..0xdc00).contains(&unit)
            && let Some(rest) = self.0.strip_prefix(b"\\u")
        {
            let mut next = Tokens(rest);
            if let Some(low @ 0xdc00..0xe000) = next.hex4() {
                self.0 = next.0;
                return char::from_u32(0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00));
            }
        }
        Some(char::REPLACEMENT_CHARACTER)
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.0.get(..4)?;
        let mut unit = 0;
        for &digit in digits {
            unit = unit << 4 | char::from(digit).to_digit(16)?;
        }
        self.0 = &self.0[4..];
        Some(unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_only_the_custom_tool() {
        let cases = [
            (json!({"type":"custom","name":"apply_patch"}), true),
            (json!({"type":"custom","name":" \tapply_patch\n"}), true),
            (json!({"type":"function","name":"apply_patch"}), false),
            (
                json!({"type":"function","function":{"name":"apply_patch"}}),
                false,
            ),
            (json!({"type":"custom","name":"shell"}), false),
            (json!({"type":"custom","name":"APPLY_PATCH"}), false),
            (
                json!({"type":"custom","name":"functions.apply_patch"}),
                false,
            ),
            (json!({"type":" custom ","name":"apply_patch"}), false),
            (json!({"name":"apply_patch"}), false),
            (json!({"type":"custom"}), false),
            (json!({"type":true,"name":"apply_patch"}), false),
            (json!({"type":"custom","name":42}), false),
            (json!({"type":"custom","name":null}), false),
            (json!({}), false),
            (Value::Null, false),
        ];
        for (tool, want) in cases {
            assert_eq!(is_custom_tool(&tool), want, "{tool}");
        }
    }

    #[test]
    fn input_encoding() {
        let cases = [
            ("", r#"{"input":""}"#, ""),
            ("patch", r#"{"input":"patch"}"#, "patch"),
            (
                "  context\n+added\r\n\tline\n ",
                r#"{"input":"  context\n+added\r\n\tline\n "}"#,
                r"  context\n+added\r\n\tline\n ",
            ),
            (
                "\"C:\\file\"",
                r#"{"input":"\"C:\\file\""}"#,
                r#"\"C:\\file\""#,
            ),
            (
                "\u{0}\u{8}\u{c}\r\t",
                r#"{"input":"\u0000\b\f\r\t"}"#,
                r"\u0000\b\f\r\t",
            ),
            ("补丁🙂", r#"{"input":"补丁🙂"}"#, "补丁🙂"),
            (
                "<>&\u{2028}\u{2029}",
                r#"{"input":"\u003c\u003e\u0026\u2028\u2029"}"#,
                r"\u003c\u003e\u0026\u2028\u2029",
            ),
        ];
        for (input, wrapped, escaped) in cases {
            assert_eq!(wrap_input(input), wrapped);
            assert_eq!(escape_input_fragment(input), escaped);
            assert_eq!(unwrap_input(&wrap_input(input)).as_deref(), Some(input));
        }
    }

    #[test]
    fn escaped_fragments_compose() {
        let fragments = [
            "*** Begin Patch\n",
            "  context\n",
            "-\"old\"\r\n",
            "+C:\\new\n",
            "+补丁🙂\n",
            "*** End Patch\n",
        ];
        let encoded: String = fragments.into_iter().map(escape_input_fragment).collect();
        let decoded: Value = serde_json::from_str(&format!(r#"{{"input":"{encoded}"}}"#)).unwrap();
        assert_eq!(decoded["input"], fragments.concat());
    }

    #[test]
    fn unwrap_accepts_one_string_field() {
        let accepted = [
            (r#"{"input":""}"#, ""),
            (r#"{"input":"patch"}"#, "patch"),
            (
                r#"{"input":"  context\n\tline\r\n\n "}"#,
                "  context\n\tline\r\n\n ",
            ),
            (" \n\t{ \"input\" : \"patch\" }\r\n ", "patch"),
            (r#"{"\u0069nput":"patch"}"#, "patch"),
            (r#"{"input":"补丁🙂"}"#, "补丁🙂"),
            // Go decodes an unpaired surrogate as U+FFFD.
            (
                r#"{"input":"\ud83d\ude42 \ud83d \ude42x"}"#,
                "🙂 \u{fffd} \u{fffd}x",
            ),
        ];
        for (arguments, want) in accepted {
            assert_eq!(
                unwrap_input(arguments).as_deref(),
                Some(want),
                "{arguments}"
            );
        }

        let rejected = [
            "",
            " \r\n\t ",
            "{}",
            r#"{"patch":"text"}"#,
            r#"{"Input":"text"}"#,
            r#"{"input":null}"#,
            r#"{"input":true}"#,
            r#"{"input":1}"#,
            r#"{"input":[]}"#,
            r#"{"input":{}}"#,
            r#"{"input":"patch","other":"value"}"#,
            r#"{"other":"value","input":"patch"}"#,
            r#"{"input":"first","input":"second"}"#,
            r#"{"input":"patch","input":"patch"}"#,
            r#"{"input":"first","\u0069nput":"second"}"#,
            r#"{"input":"patch"}{"input":"second"}"#,
            r#"{"input":"patch"} []"#,
            r#"{"input":"patch"} null"#,
            r#"{"input":"patch"} true"#,
            r#"{"input":"patch"} invalid"#,
            r#"["patch"]"#,
            r#""patch""#,
            "null",
            "1",
            "false",
            "*** Begin Patch\n*** End Patch\n",
            r#"{"input":"patch""#,
            r#"{"input" "patch"}"#,
            r#"{"input":}"#,
            r#"{"input":"patch}"#,
            r#"{"input":"patch",}"#,
            r#"{input:"patch"}"#,
            "{\"input\":\"line\nline\"}",
            r#"{"input":"\x"}"#,
            r#"{"input":"\u12"}"#,
        ];
        for arguments in rejected {
            assert_eq!(unwrap_input(arguments), None, "{arguments}");
        }
    }
}
