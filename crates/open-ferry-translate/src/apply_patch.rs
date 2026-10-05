// Ported from CLIProxyAPI internal/client/codex/apply-patch/tool.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex's `apply_patch` tool, carried through APIs that only know functions.
//!
//! Codex declares `apply_patch` as a custom tool whose input is the raw patch
//! text. A client that only knows function tools calls it as a function with
//! the arguments `{"input": "<patch>"}`. These helpers declare that function,
//! and wrap and unwrap the envelope. The `input` module decodes it as it
//! streams in, and [`responses`] bridges a Responses stream.

pub(crate) mod input;
pub mod responses;

use serde_json::Value;

use crate::go;
use crate::json::{path, str_of};

/// `parametersJSON`: the function's schema.
const PARAMETERS: &str = r#"{"type":"object","properties":{"input":{"type":"string","description":"The complete apply_patch patch text."}},"required":["input"],"additionalProperties":false}"#;

/// The sentence Codex puts in the custom tool's description, which no longer
/// holds for the function.
const FREEFORM_NOTE: &str = "This is a FREEFORM tool, so do not wrap the patch in JSON.";

const PATCH_INSTRUCTIONS: &str = "Call this function with a JSON object whose input field contains the complete patch text.
Use the Codex apply_patch format, not a conventional git unified diff.
Start with *** Begin Patch and end with *** End Patch.
Use *** Add File: path, *** Delete File: path, or *** Update File: path.
Every added-file content line starts with +.
For updates, use @@; context lines start with one space, removed lines with -, and added lines with +.
Use *** Move to: path for a rename and *** End of File when required by the patch grammar.
Example input:
*** Begin Patch
*** Update File: src/main.go
@@
-old
+new
*** End Patch";

/// `IsCustomTool`: whether a tool declaration is the custom `apply_patch` tool.
pub fn is_custom_tool(tool: &Value) -> bool {
    str_of(tool.get("type")) == "custom" && str_of(tool.get("name")).trim() == "apply_patch"
}

/// `Parameters`: the function's JSON Schema, one string field `input`.
pub(crate) fn parameters() -> Value {
    serde_json::from_str(PARAMETERS).expect("the schema is valid JSON")
}

/// `Description`: the function's description. It keeps the custom tool's own
/// description, explains the JSON envelope, and ends with the tool's patch
/// grammar.
pub(crate) fn description(tool: &Value) -> String {
    let original = str_of(tool.get("description")).replace(FREEFORM_NOTE, "");
    let mut description = String::new();
    if !original.trim().is_empty() {
        description.push_str(&original);
        description.push_str("\n\n");
    }
    description.push_str(PATCH_INSTRUCTIONS);
    let grammar = str_of(path(tool, "format.definition"));
    if !grammar.is_empty() {
        if grammar.contains("*** Environment ID:") {
            description.push_str("\n\nUse *** Environment ID: as specified by the patch grammar.");
        }
        description.push_str("\n\nOriginal patch grammar:\n");
        description.push_str(&grammar);
    }
    description
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
pub fn unwrap_input(arguments: &str) -> Option<String> {
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

/// One JSON string, quotes included, decoded as Go decodes it. `None` if it
/// isn't exactly one valid string.
fn decode_string(raw: &str) -> Option<String> {
    let mut json = Tokens(raw.as_bytes());
    let decoded = json.string()?;
    json.0.is_empty().then_some(decoded)
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
    fn description_explains_the_envelope_and_keeps_the_grammar() {
        let plain = description(&json!({"type": "custom", "name": "apply_patch"}));
        assert_eq!(plain, PATCH_INSTRUCTIONS);

        let tool = json!({
            "description": "Edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.",
            "format": {"definition": "start: *** Environment ID: x\n"}
        });
        assert_eq!(
            description(&tool),
            format!(
                "Edit files. \n\n{PATCH_INSTRUCTIONS}\n\nUse *** Environment ID: as specified by the patch grammar.\n\nOriginal patch grammar:\nstart: *** Environment ID: x\n"
            )
        );

        let only_note =
            json!({"description": " This is a FREEFORM tool, so do not wrap the patch in JSON. "});
        assert_eq!(description(&only_note), PATCH_INSTRUCTIONS);
        assert_eq!(parameters()["required"], json!(["input"]));
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
