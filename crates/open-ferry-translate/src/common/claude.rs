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

use crate::go;
use crate::json::{object, path, str_of};

const SYSTEM_REMINDER_START: &str = "<system-reminder>";
const SYSTEM_REMINDER_END: &str = "</system-reminder>";
const ATTRIBUTION_SYSTEM_PREFIX: &str = "x-anthropic-billing-header:";

/// Claude's limit on tool name length.
const FUNCTION_NAME_LIMIT: usize = 64;

const JSON_OBJECT_INSTRUCTION: &str = "You must format your entire response as a valid JSON object. Do not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.";
const JSON_SCHEMA_INSTRUCTION: &str = "You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\n";
const JSON_SCHEMA_INSTRUCTION_END: &str = "\nDo not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.";

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

/// Groups Claude messages into turns (`ClaudeMessageAccumulator`): Claude
/// wants user and assistant turns to alternate, so consecutive messages of
/// one role are joined. An assistant turn's `tool_use` blocks move to its end.
#[derive(Default)]
pub(crate) struct MessageAccumulator {
    messages: Vec<Value>,
    role: Option<&'static str>,
    content: Vec<Value>,
    tool_uses: Vec<Value>,
}

impl MessageAccumulator {
    /// Adds a message's blocks to the current turn, or starts a new turn if
    /// the role changed. A message without blocks is skipped, so it doesn't
    /// end a turn either.
    pub(crate) fn push(&mut self, role: &'static str, blocks: Vec<Value>) {
        if blocks.is_empty() {
            return;
        }
        if self.role.is_some_and(|current| current != role) {
            self.flush();
        }
        self.role = Some(role);
        for block in blocks {
            if role == "assistant" && str_of(block.get("type")) == "tool_use" {
                self.tool_uses.push(block);
            } else {
                self.content.push(block);
            }
        }
    }

    fn flush(&mut self) {
        let Some(role) = self.role.take() else {
            return;
        };
        let mut content = std::mem::take(&mut self.content);
        content.append(&mut self.tool_uses);
        if !content.is_empty() {
            self.messages
                .push(object([("role", role.into()), ("content", content.into())]));
        }
    }

    /// Ends the last turn and returns every message.
    pub(crate) fn into_messages(mut self) -> Vec<Value> {
        self.flush();
        self.messages
    }
}

/// Writes a Chat Completions `response_format` (or Responses `text.format`)
/// as an instruction for Claude's system prompt, since Claude has no such
/// setting (`BuildClaudeStructuredOutputInstruction`).
///
/// The schema is written as compact JSON, where upstream copies the client's
/// text.
pub(crate) fn structured_output_instruction(format: Option<&Value>) -> Option<String> {
    let format = format?;
    match go::to_lower(str_of(format.get("type")).trim()).as_str() {
        "json_object" => Some(JSON_OBJECT_INSTRUCTION.to_owned()),
        "json_schema" => {
            let Some(schema) = path(format, "json_schema.schema").or_else(|| format.get("schema"))
            else {
                return Some(JSON_OBJECT_INSTRUCTION.to_owned());
            };
            let field = |key: &str| {
                let value = str_of(path(format, &format!("json_schema.{key}")));
                let value = value.trim();
                if value.is_empty() {
                    str_of(format.get(key)).trim().to_owned()
                } else {
                    value.to_owned()
                }
            };
            let mut text = JSON_SCHEMA_INSTRUCTION.to_owned();
            let name = field("name");
            if !name.is_empty() {
                text.push_str(&format!("Schema Name: {name}\n"));
            }
            let description = field("description");
            if !description.is_empty() {
                text.push_str(&format!("Schema Description: {description}\n"));
            }
            text.push_str("JSON Schema:\n");
            text.push_str(&schema.to_string());
            text.push_str(JSON_SCHEMA_INSTRUCTION_END);
            Some(text)
        }
        _ => None,
    }
}

/// Makes `name` a valid Claude tool name (`^[a-zA-Z0-9_-]{1,64}$`) by
/// replacing every other character with `_` and cutting it to 64 bytes. An
/// empty name stays empty.
pub(crate) fn sanitize_function_name(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect();
    // Every character is ASCII now, so any byte is a boundary.
    sanitized.truncate(FUNCTION_NAME_LIMIT);
    sanitized
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

    #[test]
    fn function_names_are_sanitized_and_cut() {
        assert_eq!(sanitize_function_name(""), "");
        assert_eq!(
            sanitize_function_name("mcp.server:tool \u{e9}"),
            "mcp_server_tool__"
        );
        let long = "a".repeat(70);
        assert_eq!(sanitize_function_name(&long).len(), 64);
        assert_eq!(sanitize_function_name(&"\u{e9}".repeat(40)).len(), 40);
    }

    #[test]
    fn accumulator_joins_turns_and_moves_tool_uses_last() {
        let mut turns = MessageAccumulator::default();
        turns.push("user", vec![json!({"type": "text", "text": "a"})]);
        turns.push("assistant", vec![json!({"type": "tool_use", "id": "1"})]);
        turns.push("assistant", Vec::new());
        turns.push("assistant", vec![json!({"type": "text", "text": "b"})]);
        turns.push("user", vec![json!({"type": "tool_result"})]);
        assert_eq!(
            Value::from(turns.into_messages()),
            json!([
                {"role": "user", "content": [{"type": "text", "text": "a"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "b"},
                    {"type": "tool_use", "id": "1"}
                ]},
                {"role": "user", "content": [{"type": "tool_result"}]}
            ])
        );
    }

    #[test]
    fn structured_output_instructions() {
        assert_eq!(structured_output_instruction(None), None);
        assert_eq!(
            structured_output_instruction(Some(&json!({"type": "text"}))),
            None
        );
        assert_eq!(
            structured_output_instruction(Some(&json!({"type": " JSON_OBJECT "}))).as_deref(),
            Some(JSON_OBJECT_INSTRUCTION)
        );
        assert_eq!(
            structured_output_instruction(Some(&json!({"type": "json_schema"}))).as_deref(),
            Some(JSON_OBJECT_INSTRUCTION)
        );
        let format = json!({
            "type": "json_schema",
            "name": "outer",
            "json_schema": {"name": " ", "description": "Desc", "schema": {"type": "object"}}
        });
        assert_eq!(
            structured_output_instruction(Some(&format)).unwrap(),
            format!(
                "{JSON_SCHEMA_INSTRUCTION}Schema Name: outer\nSchema Description: Desc\nJSON Schema:\n{{\"type\":\"object\"}}{JSON_SCHEMA_INSTRUCTION_END}"
            )
        );
    }
}
