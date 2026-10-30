// Ported from CLIProxyAPI internal/translator/common/request.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers shared by translators of every format.

pub(crate) mod cache_control;
pub(crate) mod claude;
pub(crate) mod claude_native_response;
pub(crate) mod codex_replay_signature;
pub(crate) mod file_data;
pub(crate) mod gemini;
pub(crate) mod gemini_response;
pub(crate) mod interactions_usage;
pub(crate) mod mime_types;
pub(crate) mod openai_tools;
pub(crate) mod parts;
pub(crate) mod responses;
pub(crate) mod sse;
pub(crate) mod tool_names;
pub(crate) mod vertex_partial_args;

use serde_json::Value;

use crate::json::path;

/// The model the client asked for: `model`, or else `request.model`, from the
/// client's request, then from the translated request. Blank names are skipped.
pub(crate) fn request_model_name<'r>(
    original_request: &'r Value,
    request: &'r Value,
) -> Option<&'r str> {
    [original_request, request].into_iter().find_map(|request| {
        ["model", "request.model"].into_iter().find_map(|key| {
            path(request, key)?
                .as_str()
                .filter(|name| !name.trim().is_empty())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Go's unicode.IsSpace and Rust's is_whitespace both use White_Space.
    // All 25 code points are blank; U+001C, U+180E, U+200B and U+FEFF aren't.
    #[test]
    fn model_names_skip_go_unicode_whitespace() {
        let fallback = json!({"model":"fallback"});
        for ch in "\u{9}\u{a}\u{b}\u{c}\u{d} \u{85}\u{a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}".chars() {
            let original = json!({"model":ch.to_string()});
            assert_eq!(request_model_name(&original, &fallback), Some("fallback"));
        }
        for model in ["\u{1c}", "\u{180e}", "\u{200b}", "\u{feff}", " gpt-5 "] {
            let original = json!({"model":model});
            assert_eq!(request_model_name(&original, &fallback), Some(model));
        }
    }
}
