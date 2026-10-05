// Ported from CLIProxyAPI internal/runtime/executor/openai_responses_signature.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Cleans the `reasoning` items of a Responses `input` before it goes to
//! Codex: cleartext `content` moves into an empty `summary` (Codex allows no
//! reasoning content), an `encrypted_content` that isn't a valid GPT
//! reasoning signature is dropped, and with `store` off, a reasoning item's
//! `id` goes when it has no usable `encrypted_content`, since Codex would
//! look the ID up and fail.
//!
//! Deviations from upstream:
//! - Edits a parsed body in place and reports whether anything changed,
//!   instead of splicing raw JSON; the edited items are written by
//!   `serde_json`.

use open_ferry_translate::signature::inspect_gpt_reasoning_signature;
use serde_json::{Map, Value, json};

use crate::json::{bool_of, get, str_at, str_of};

/// Sanitizes the reasoning items of `body`'s `input`
/// (`sanitizeOpenAIResponsesReasoningEncryptedContentWithCompat`). With
/// `is_compat`, a third-party Responses model's cleartext reasoning and IDs
/// are kept. Returns whether anything changed.
pub(crate) fn sanitize_reasoning(body: &mut Value, is_compat: bool) -> bool {
    // Codex doesn't persist items with store off, so a reasoning ID without
    // usable encrypted content would be looked up and fail.
    let strip_ids = !bool_of(get(body, "store"));
    let Some(Value::Array(items)) = body.get_mut("input") else {
        return false;
    };
    let mut changed = false;
    for (index, item) in items.iter_mut().enumerate() {
        if str_at(item, "type").trim() != "reasoning" {
            continue;
        }
        let Value::Object(item) = item else {
            continue;
        };
        changed |= sanitize_item(item, index, strip_ids, is_compat);
    }
    changed
}

fn sanitize_item(
    item: &mut Map<String, Value>,
    index: usize,
    strip_ids: bool,
    is_compat: bool,
) -> bool {
    let mut changed = false;
    let has_content = matches!(item.get("content"), Some(Value::Array(parts)) if !parts.is_empty());
    if !is_compat && has_content {
        if summary_is_empty(item.get("summary")) {
            promote_reasoning_text(item);
        }
        item.insert("content".to_owned(), json!([]));
        changed = true;
        tracing::debug!("codex: cleared reasoning content at input[{index}]");
    }

    let drop_id = !is_compat && strip_ids && item.contains_key("id");
    let Some(encrypted) = item.get("encrypted_content") else {
        if drop_id {
            item.shift_remove("id");
            changed = true;
            tracing::debug!(
                "codex: dropped orphan reasoning id at input[{index}] reason=missing encrypted_content with store disabled"
            );
        }
        return changed;
    };

    let reason = match encrypted {
        Value::String(raw) if raw.as_str() != raw.trim() => {
            Some("encrypted_content has leading or trailing whitespace".to_owned())
        }
        Value::String(raw) => inspect_gpt_reasoning_signature(raw)
            .err()
            .map(|error| error.to_string()),
        Value::Null => Some("encrypted_content is null".to_owned()),
        other => Some(format!(
            "encrypted_content must be a string, got {}",
            go_type_name(other)
        )),
    };
    let Some(reason) = reason else {
        return changed;
    };
    item.shift_remove("encrypted_content");
    if drop_id {
        item.shift_remove("id");
    }
    tracing::debug!(
        "codex: dropped invalid reasoning encrypted_content at input[{index}] reason={reason}"
    );
    true
}

/// Whether a reasoning item's summary is missing, null or an empty array
/// (`openaiResponsesReasoningSummaryIsEmpty`).
fn summary_is_empty(summary: Option<&Value>) -> bool {
    match summary {
        None | Some(Value::Null) => true,
        Some(Value::Array(parts)) => parts.is_empty(),
        Some(_) => false,
    }
}

/// Sets the item's `summary` to its non-empty `reasoning_text` parts, as
/// `summary_text`, when there are any
/// (`promoteOpenAIResponsesReasoningTextToSummary`).
fn promote_reasoning_text(item: &mut Map<String, Value>) {
    let Some(Value::Array(parts)) = item.get("content") else {
        return;
    };
    let summary: Vec<Value> = parts
        .iter()
        .filter(|part| str_at(part, "type").trim() == "reasoning_text")
        .map(|part| str_of(get(part, "text")))
        .filter(|text| !text.is_empty())
        .map(|text| json!({"type": "summary_text", "text": text}))
        .collect();
    if !summary.is_empty() {
        item.insert("summary".to_owned(), Value::Array(summary));
    }
}

/// gjson's name for a value's type.
fn go_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "Null",
        Value::Bool(false) => "False",
        Value::Bool(true) => "True",
        Value::Number(_) => "Number",
        Value::String(_) => "String",
        Value::Array(_) | Value::Object(_) => "JSON",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use base64::Engine as _;

    /// A well-formed GPT reasoning signature
    /// (`validOpenAIResponsesReasoningEncryptedContentForTest`).
    pub(crate) fn valid_signature() -> String {
        let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
        payload[0] = 0x80;
        for (i, byte) in payload.iter_mut().enumerate().skip(9) {
            *byte = i as u8;
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    }

    fn sanitize(raw: &str) -> Value {
        let mut body: Value = serde_json::from_str(raw).unwrap();
        sanitize_reasoning(&mut body, false);
        body
    }

    fn assert_empty_content(body: &Value, path: &str) {
        assert_eq!(get(body, path), Some(&json!([])), "{path} in {body}");
    }

    #[test]
    fn strips_orphan_ids_when_store_disabled() {
        let valid = valid_signature();
        let got = sanitize(&format!(
            r#"{{"store":false,"input":[{{"id":"rs_bad","type":"reasoning","encrypted_content":"bad","summary":[]}},{{"id":"rs_orphan","type":"reasoning","summary":[]}},{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[]}},{{"id":"msg_1","type":"message","role":"user","content":"hi"}}]}}"#
        ));
        assert!(get(&got, "input.0.encrypted_content").is_none(), "{got}");
        assert!(get(&got, "input.0.id").is_none(), "{got}");
        assert!(get(&got, "input.1.id").is_none(), "{got}");
        assert_eq!(str_at(&got, "input.2.id"), "rs_good");
        assert_eq!(str_at(&got, "input.2.encrypted_content"), valid);
        assert_eq!(str_at(&got, "input.3.id"), "msg_1");
    }

    #[test]
    fn keeps_ids_when_store_enabled() {
        let got = sanitize(
            r#"{"store":true,"input":[{"id":"rs_bad","type":"reasoning","encrypted_content":"bad","summary":[]},{"id":"rs_orphan","type":"reasoning","summary":[]}]}"#,
        );
        assert!(get(&got, "input.0.encrypted_content").is_none(), "{got}");
        assert_eq!(str_at(&got, "input.0.id"), "rs_bad");
        assert_eq!(str_at(&got, "input.1.id"), "rs_orphan");
    }

    #[test]
    fn moves_cleartext_content_to_summary() {
        let got = sanitize(
            r#"{"store":false,"input":[{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"The model thinking process from a previous turn with a third-party provider..."}],"encrypted_content":null},{"id":"msg_1","type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        );
        assert_empty_content(&got, "input.0.content");
        assert_eq!(str_at(&got, "input.0.summary.0.type"), "summary_text");
        assert_eq!(
            str_at(&got, "input.0.summary.0.text"),
            "The model thinking process from a previous turn with a third-party provider..."
        );
        assert!(get(&got, "input.0.encrypted_content").is_none(), "{got}");
        assert_eq!(str_at(&got, "input.1.content.0.type"), "input_text");
    }

    #[test]
    fn does_not_duplicate_existing_summary() {
        let got = sanitize(
            r#"{"store":false,"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"already summarized"}],"content":[{"type":"reasoning_text","text":"duplicate thinking"}]}]}"#,
        );
        assert_empty_content(&got, "input.0.content");
        assert_eq!(
            get(&got, "input.0.summary")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(str_at(&got, "input.0.summary.0.text"), "already summarized");
    }

    #[test]
    fn promotes_multiple_reasoning_text_parts() {
        let got = sanitize(
            r#"{"store":false,"input":[{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"step one"},{"type":"reasoning_text","text":"step two"}]}]}"#,
        );
        assert_empty_content(&got, "input.0.content");
        assert_eq!(
            get(&got, "input.0.summary")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(str_at(&got, "input.0.summary.0.text"), "step one");
        assert_eq!(str_at(&got, "input.0.summary.1.text"), "step two");
    }

    #[test]
    fn keeps_valid_encrypted_content_when_stripping_content() {
        let valid = valid_signature();
        let got = sanitize(&format!(
            r#"{{"store":false,"input":[{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[],"content":[{{"type":"reasoning_text","text":"cleartext thinking"}}]}}]}}"#
        ));
        assert_empty_content(&got, "input.0.content");
        assert_eq!(str_at(&got, "input.0.id"), "rs_good");
        assert_eq!(str_at(&got, "input.0.encrypted_content"), valid);
        assert_eq!(str_at(&got, "input.0.summary.0.text"), "cleartext thinking");
    }

    #[test]
    fn noop_leaves_the_body_unchanged() {
        let valid = valid_signature();
        let raw = format!(
            r#"{{"store":false,"input":[{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[]}},{{"role":"user","content":"hi"}}]}}"#
        );
        let mut body: Value = serde_json::from_str(&raw).unwrap();
        assert!(!sanitize_reasoning(&mut body, false));
        assert_eq!(body.to_string(), raw);
    }

    #[test]
    fn compat_preserves_reasoning_content_and_id() {
        let mut body: Value = serde_json::from_str(
            r#"{"store":false,"input":[{"id":"rs_compat","type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"keep cleartext thinking"}],"encrypted_content":null},{"id":"msg_1","type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        )
        .unwrap();
        assert!(sanitize_reasoning(&mut body, true));
        assert_eq!(str_at(&body, "input.0.id"), "rs_compat");
        assert_eq!(
            get(&body, "input.0.content")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(str_at(&body, "input.0.content.0.type"), "reasoning_text");
        assert_eq!(
            str_at(&body, "input.0.content.0.text"),
            "keep cleartext thinking"
        );
        assert_eq!(get(&body, "input.0.summary"), Some(&json!([])));
        assert!(get(&body, "input.0.encrypted_content").is_none(), "{body}");
    }

    #[test]
    fn rejects_whitespace_and_non_strings() {
        let valid = valid_signature();
        let got = sanitize(&format!(
            r#"{{"store":true,"input":[{{"id":"a","type":"reasoning","encrypted_content":" {valid}"}},{{"id":"b","type":" reasoning ","encrypted_content":123}},"reasoning",{{"type":"reasoning"}}]}}"#
        ));
        assert!(get(&got, "input.0.encrypted_content").is_none(), "{got}");
        assert!(get(&got, "input.1.encrypted_content").is_none(), "{got}");
        assert_eq!(str_at(&got, "input.0.id"), "a");
        assert_eq!(get(&got, "input.2"), Some(&json!("reasoning")));
    }
}
