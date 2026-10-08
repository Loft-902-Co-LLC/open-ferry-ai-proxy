// Ported from CLIProxyAPI internal/util/translator.go (FixJSON, CanonicalToolName,
// ToolNameMapFromClaudeRequest, MapToolName) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Repairing tool call arguments that use single quotes, and restoring the
//! case of a tool's name as the client declared it.
//!
//! Deviations from upstream: none.

use std::collections::HashMap;

use serde_json::Value;

use crate::go;
use crate::json::{path, str_of};

/// The tools a Claude request declares, by [`canonical_tool_name`], each
/// with the name as the client first wrote it.
pub(crate) type ToolNameMap = HashMap<String, String>;

/// `FixJSON`: turns single-quoted strings into double-quoted ones, so that
/// `{'a': 'He said "hi"'}` becomes `{"a": "He said \"hi\""}`. Double-quoted
/// strings and everything outside strings are kept as they are. In a
/// single-quoted string, `\'` becomes `'`, a `\u` escape keeps up to four
/// hex digits, and other escapes keep their backslash. A single-quoted string
/// that doesn't end is closed.
pub(crate) fn fix_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if in_double {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_double = false;
            }
            continue;
        }
        if in_single {
            if escaped {
                escaped = false;
                match c {
                    '\'' => out.push('\''),
                    'u' => {
                        out.push_str("\\u");
                        for _ in 0..4 {
                            match chars.next_if(char::is_ascii_hexdigit) {
                                Some(digit) => out.push(digit),
                                None => break,
                            }
                        }
                    }
                    // `\\`, `\n` and the rest, known or not, keep the backslash.
                    _ => {
                        out.push('\\');
                        out.push(c);
                    }
                }
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                out.push('"');
                in_single = false;
            } else if c == '"' {
                out.push_str("\\\"");
            } else {
                out.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                in_double = true;
                out.push(c);
            }
            '\'' => {
                in_single = true;
                out.push('"');
            }
            _ => out.push(c),
        }
    }
    if in_single {
        out.push('"');
    }
    out
}

/// `CanonicalToolName`: the name trimmed, without leading underscores, in
/// lower case.
pub(crate) fn canonical_tool_name(name: &str) -> String {
    go::to_lower(name.trim().trim_start_matches('_'))
}

/// `ToolNameMapFromClaudeRequest`: the names of the tools a Claude request
/// declares, by `name` or else `function.name`. `None` if there are none.
pub(crate) fn tool_name_map_from_claude_request(request: &Value) -> Option<ToolNameMap> {
    let Some(Value::Array(tools)) = request.get("tools") else {
        return None;
    };
    let mut names = ToolNameMap::new();
    for tool in tools {
        let mut name = str_of(tool.get("name")).trim().to_owned();
        if name.is_empty() {
            name = str_of(path(tool, "function.name")).trim().to_owned();
        }
        if name.is_empty() {
            continue;
        }
        let key = canonical_tool_name(&name);
        if !key.is_empty() {
            names.entry(key).or_insert(name);
        }
    }
    (!names.is_empty()).then_some(names)
}

/// `MapToolName`: the declared name of the tool `name` refers to, or `name`
/// itself if it refers to none.
pub(crate) fn map_tool_name(names: Option<&ToolNameMap>, name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    names
        .and_then(|names| names.get(&canonical_tool_name(name)))
        .filter(|mapped| !mapped.is_empty())
        .map_or_else(|| name.to_owned(), Clone::clone)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn fix_json_converts_single_quotes() {
        assert_eq!(fix_json("{'a': 1, 'b': '2'}"), r#"{"a": 1, "b": "2"}"#);
        assert_eq!(
            fix_json(r#"{"t": 'He said "hi"'}"#),
            r#"{"t": "He said \"hi\""}"#
        );
        assert_eq!(fix_json(r#"{'a': 'it\'s'}"#), r#"{"a": "it's"}"#);
        assert_eq!(fix_json(r#"{'a': 'x\\y\n'}"#), r#"{"a": "x\\y\n"}"#);
        assert_eq!(fix_json(r#"{'a': '\q'}"#), r#"{"a": "\q"}"#);
        assert_eq!(fix_json(r#"{'a': '\u00e9z'}"#), r#"{"a": "\u00e9z"}"#);
        assert_eq!(fix_json(r#"{'a': '\u0g'}"#), r#"{"a": "\u0g"}"#);
        assert_eq!(fix_json("{'a': 'open"), r#"{"a": "open""#);
        assert_eq!(fix_json(r#"{"a": "it's \"x\""}"#), r#"{"a": "it's \"x\""}"#);
    }

    #[test]
    fn tool_names_map_back_to_declared_case() {
        let request = json!({"tools": [
            {"name": " Read "},
            {"name": "", "function": {"name": "__Write"}},
            {"name": "read"},
            {"name": 7},
            "junk"
        ]});
        let names = tool_name_map_from_claude_request(&request);
        assert_eq!(map_tool_name(names.as_ref(), "READ"), "Read");
        assert_eq!(map_tool_name(names.as_ref(), "write"), "__Write");
        assert_eq!(map_tool_name(names.as_ref(), "7"), "7");
        assert_eq!(map_tool_name(names.as_ref(), "other"), "other");
        assert_eq!(map_tool_name(None, "READ"), "READ");
        assert!(tool_name_map_from_claude_request(&json!({"tools": {}})).is_none());
        assert!(tool_name_map_from_claude_request(&json!({"tools": [{"name": " "}]})).is_none());
    }
}
