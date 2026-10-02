// Ported from CLIProxyAPI (v8.0.10, MIT): internal/translator/common/claude_system.go,
// internal/translator/common/claude_messages.go, internal/util/claude_attribution.go
// and internal/util/claude_tool_id.go.
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers for Claude Messages requests and responses, shared by translators
//! that convert between them and other providers' formats.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::json::str_of;

const SYSTEM_REMINDER_START: &str = "<system-reminder>";
const SYSTEM_REMINDER_END: &str = "</system-reminder>";
const ATTRIBUTION_SYSTEM_PREFIX: &str = "x-anthropic-billing-header:";

/// Reports whether `text` is the attribution block Claude Code prepends to its
/// system prompt. It only means something to Anthropic, so other upstreams drop it.
pub(crate) fn is_attribution_system_text(text: &str) -> bool {
    text.trim_start().starts_with(ATTRIBUTION_SYSTEM_PREFIX)
}

/// Wraps text in a `<system-reminder>` envelope, so upstreams without
/// mid-conversation system messages still read it as an instruction.
pub(crate) fn system_reminder_text(text: &str) -> String {
    format!("{SYSTEM_REMINDER_START}\n{text}\n{SYSTEM_REMINDER_END}")
}

/// Converts the content of a message-level `system` role into reminder text.
/// Returns `None` when nothing but attribution or whitespace remains.
pub(crate) fn message_system_reminder_text(content: Option<&Value>) -> Option<String> {
    let parts: Vec<Cow<'_, str>> = match content {
        Some(Value::String(text)) => vec![Cow::Borrowed(text.as_str())],
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| str_of(item.get("type")) == "text")
            .map(|item| str_of(item.get("text")))
            .collect(),
        _ => Vec::new(),
    };
    let text = parts
        .iter()
        .filter(|text| !text.is_empty() && !is_attribution_system_text(text))
        .map(AsRef::as_ref)
        .collect::<Vec<&str>>()
        .join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(system_reminder_text(&text))
}

/// Reorders the `tool_result` parts of a user message to match the order of the
/// preceding `tool_use` IDs, keeping every other part in its slot. The input is
/// returned unchanged unless every ID matches exactly one result.
pub(crate) fn align_tool_results<'a>(
    parts: &'a [Value],
    tool_use_ids: &[String],
) -> Cow<'a, [Value]> {
    if tool_use_ids.is_empty() {
        return Cow::Borrowed(parts);
    }
    let slots: Vec<usize> = parts
        .iter()
        .enumerate()
        .filter(|(_, part)| str_of(part.get("type")) == "tool_result")
        .map(|(index, _)| index)
        .collect();
    if slots.len() != tool_use_ids.len() {
        return Cow::Borrowed(parts);
    }

    let mut used = vec![false; slots.len()];
    let mut reordered = Vec::with_capacity(slots.len());
    for id in tool_use_ids {
        let matched = slots.iter().enumerate().position(|(i, &slot)| {
            !used[i] && !id.is_empty() && str_of(parts[slot].get("tool_use_id")) == id.as_str()
        });
        let Some(i) = matched else {
            return Cow::Borrowed(parts);
        };
        used[i] = true;
        reordered.push(slots[i]);
    }

    let mut aligned = parts.to_vec();
    for (&slot, &source) in slots.iter().zip(&reordered) {
        aligned[slot] = parts[source].clone();
    }
    Cow::Owned(aligned)
}

/// Makes `id` a valid Claude `tool_use` ID (`^[a-zA-Z0-9_-]+$`) by replacing
/// every other character with `_`. An empty ID gets a generated one.
pub(crate) fn sanitize_tool_id(id: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sanitized: String = id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect();
    if !sanitized.is_empty() {
        return sanitized;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("toolu_{nanos}_{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_tool_id_replaces_each_invalid_character() {
        assert_eq!(sanitize_tool_id("call_AB-9"), "call_AB-9");
        assert_eq!(sanitize_tool_id("fc.1:é x"), "fc_1___x");
        let generated = sanitize_tool_id("");
        assert!(generated.starts_with("toolu_"), "{generated}");
        assert_ne!(generated, sanitize_tool_id(""));
    }

    #[test]
    fn attribution_detection_ignores_leading_whitespace() {
        assert!(is_attribution_system_text(
            "  x-anthropic-billing-header: cc_version=1"
        ));
        assert!(!is_attribution_system_text("Be helpful"));
    }

    #[test]
    fn reminder_text_joins_text_parts_and_skips_attribution() {
        let content = json!([
            {"type": "text", "text": "x-anthropic-billing-header: abc"},
            {"type": "text", "text": "one"},
            {"type": "image"},
            {"type": "text", "text": "two"}
        ]);
        assert_eq!(
            message_system_reminder_text(Some(&content)).as_deref(),
            Some("<system-reminder>\none\ntwo\n</system-reminder>")
        );
        assert_eq!(message_system_reminder_text(Some(&json!("   "))), None);
        assert_eq!(message_system_reminder_text(None), None);
    }

    #[test]
    fn align_tool_results_reorders_only_result_slots() {
        let parts = vec![
            json!({"type": "tool_result", "tool_use_id": "b"}),
            json!({"type": "text", "text": "keep"}),
            json!({"type": "tool_result", "tool_use_id": "a"}),
        ];
        let ids = ["a".to_owned(), "b".to_owned()];
        let aligned = align_tool_results(&parts, &ids);
        assert_eq!(aligned[0]["tool_use_id"], "a");
        assert_eq!(aligned[1]["text"], "keep");
        assert_eq!(aligned[2]["tool_use_id"], "b");
    }

    #[test]
    fn align_tool_results_leaves_mismatches_alone() {
        let parts = vec![json!({"type": "tool_result", "tool_use_id": "x"})];
        let aligned = align_tool_results(&parts, &["a".to_owned()]);
        assert!(matches!(aligned, Cow::Borrowed(_)));
    }
}
