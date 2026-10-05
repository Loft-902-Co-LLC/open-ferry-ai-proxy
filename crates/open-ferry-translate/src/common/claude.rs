// Ported from CLIProxyAPI (v8.0.15, MIT): internal/translator/common/claude_system.go,
// internal/translator/common/claude_messages.go, internal/util/claude_attribution.go
// and internal/util/claude_tool_id.go, and code repeated in
// internal/translator/claude/openai/chat-completions/claude_openai_request.go and
// internal/translator/claude/openai/responses/claude_openai-responses_request.go.
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers for Claude Messages requests and responses, shared by translators
//! that convert between them and other providers' formats.

use std::borrow::Cow;
use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::go;
use crate::json::{object, path, str_of};
use crate::models::ModelCatalog;
use crate::thinking::{claude_effort, has_level, level_to_budget};

const SYSTEM_REMINDER_START: &str = "<system-reminder>";
const SYSTEM_REMINDER_END: &str = "</system-reminder>";
const ATTRIBUTION_SYSTEM_PREFIX: &str = "x-anthropic-billing-header:";

/// Claude's limit on tool name length.
const FUNCTION_NAME_LIMIT: usize = 64;

const TOOL_CALL_ID_LETTERS: &[u8; 62] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

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
///
/// An ID takes the first result with that `tool_use_id` that no earlier ID
/// took. Upstream finds it by scanning every result for each ID, which takes
/// quadratic time; the results wait here in a queue per ID, so each match is
/// one lookup. The output is the same.
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

    // Each ID takes the first unused result carrying it. A queue of slots per
    // ID, in message order, finds that in one step.
    let mut waiting: HashMap<Cow<'_, str>, VecDeque<usize>> = HashMap::new();
    for &slot in &slots {
        let id = str_of(parts.get(slot).and_then(|part| part.get("tool_use_id")));
        waiting.entry(id).or_default().push_back(slot);
    }
    let mut reordered = Vec::with_capacity(slots.len());
    for id in tool_use_ids {
        let matched = if id.is_empty() {
            None
        } else {
            waiting.get_mut(id.as_str()).and_then(VecDeque::pop_front)
        };
        let Some(source) = matched else {
            return Cow::Borrowed(parts);
        };
        reordered.push(source);
    }

    let mut aligned = parts.to_vec();
    for (&slot, &source) in slots.iter().zip(&reordered) {
        aligned[slot] = parts[source].clone();
    }
    Cow::Owned(aligned)
}

/// Makes `id` a valid Claude `tool_use` ID (`^[a-zA-Z0-9_-]+$`) by replacing
/// every other character with `_`. An empty ID gets a generated one.
pub fn sanitize_tool_id(id: &str) -> String {
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

/// The client's own user ID: `metadata.user_id`, or else OpenAI's `user`, if
/// either is a non-blank string. Upstream makes one up when there's none; we
/// don't.
pub(crate) fn client_user_id(request: &Value) -> Option<&str> {
    [path(request, "metadata.user_id"), request.get("user")]
        .into_iter()
        .flatten()
        .find_map(|id| id.as_str().filter(|id| !id.trim().is_empty()))
}

/// Sets Claude's thinking from a reasoning effort: Chat Completions'
/// `reasoning_effort` or Responses' `reasoning.effort`. A model with effort levels
/// gets adaptive thinking with an effort Claude knows; `none` turns thinking
/// off. Other models get a thinking budget for the level, if it has one.
pub(crate) fn apply_reasoning_effort(
    out: &mut Map<String, Value>,
    effort: &str,
    model_name: &str,
    models: &ModelCatalog,
) {
    let effort = go::to_lower(effort.trim());
    if effort.is_empty() {
        return;
    }
    let levels = models
        .thinking(model_name)
        .map(|support| &support.levels)
        .filter(|levels| !levels.is_empty());
    if let Some(levels) = levels {
        let (kind, effort) = match effort.as_str() {
            "none" => ("disabled", None),
            "auto" => ("adaptive", None),
            _ => {
                let supports_max = has_level(levels, "max");
                let mapped = claude_effort(&effort, supports_max).map(str::to_owned);
                ("adaptive", Some(mapped.unwrap_or(effort)))
            }
        };
        out.insert("thinking".into(), json!({"type": kind}));
        if let Some(effort) = effort {
            out.insert("output_config".into(), json!({"effort": effort}));
        }
        return;
    }
    let thinking = match level_to_budget(&effort) {
        Some(0) => json!({"type": "disabled"}),
        Some(-1) => json!({"type": "enabled"}),
        Some(budget) if budget > 0 => json!({"type": "enabled", "budget_tokens": budget}),
        _ => return,
    };
    out.insert("thinking".into(), thinking);
}

/// `toolu_` and 24 random letters and digits, the form of Claude's own IDs.
pub(crate) fn generate_tool_call_id() -> String {
    let state = RandomState::new();
    let mut id = String::from("toolu_");
    let mut draws = 0u64;
    while id.len() < "toolu_".len() + 24 {
        draws += 1;
        let mut bits = state.hash_one(draws);
        // Ten 6-bit draws per hash, rejecting the two values past the
        // alphabet so every letter is as likely.
        for _ in 0..10 {
            let index = (bits & 63) as usize;
            bits >>= 6;
            if let Some(&letter) = TOOL_CALL_ID_LETTERS.get(index)
                && id.len() < "toolu_".len() + 24
            {
                id.push(char::from(letter));
            }
        }
    }
    id
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
    fn generated_tool_call_ids_look_like_claude_ids() {
        let id = generate_tool_call_id();
        let letters = id.strip_prefix("toolu_").unwrap();
        assert_eq!(letters.len(), 24);
        assert!(letters.bytes().all(|b| TOOL_CALL_ID_LETTERS.contains(&b)));
        assert_ne!(id, generate_tool_call_id());
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

    // Not upstream's: a duplicate ID goes to the first unused result in message
    // order, as upstream's scan finds it, and each later duplicate to the next.
    #[test]
    fn align_tool_results_gives_duplicate_ids_their_results_in_message_order() {
        let parts = vec![
            json!({"type": "tool_result", "tool_use_id": "b", "content": "b1"}),
            json!({"type": "tool_result", "tool_use_id": "a", "content": "a1"}),
            json!({"type": "text", "text": "keep"}),
            json!({"type": "tool_result", "tool_use_id": "b", "content": "b2"}),
            json!({"type": "tool_result", "tool_use_id": "a", "content": "a2"}),
        ];
        let ids = ["a", "b", "a", "b"].map(str::to_owned);
        let aligned = align_tool_results(&parts, &ids);
        let contents: Vec<&str> = aligned
            .iter()
            .map(|part| part["content"].as_str().unwrap_or("-"))
            .collect();
        assert_eq!(contents, ["a1", "b1", "-", "a2", "b2"]);
        assert_eq!(aligned[2]["text"], "keep");
    }

    // Not upstream's: every way the one-to-one match can fail leaves the parts
    // as they came, as upstream's does: a result no ID asks for (the counts
    // agree, the sets differ), an ID with no result, an ID whose only result an
    // earlier duplicate took, an empty ID, which matches nothing, even a result
    // without a `tool_use_id`, and a count that differs.
    #[test]
    fn align_tool_results_falls_back_unless_every_id_has_its_own_result() {
        let result = |id: Value| json!({"type": "tool_result", "tool_use_id": id});
        let cases: [(Vec<Value>, Vec<&str>); 5] = [
            (vec![result(json!("a")), result(json!("c"))], vec!["a", "b"]),
            (vec![result(json!("a")), result(json!("a"))], vec!["a", "b"]),
            (vec![result(json!("a")), result(json!("b"))], vec!["a", "a"]),
            (vec![result(json!(null)), result(json!("a"))], vec!["", "a"]),
            (vec![result(json!("a"))], vec!["a", "b"]),
        ];
        for (parts, ids) in cases {
            let ids: Vec<String> = ids.into_iter().map(str::to_owned).collect();
            let aligned = align_tool_results(&parts, &ids);
            assert!(matches!(aligned, Cow::Borrowed(_)), "{parts:?} {ids:?}");
        }
    }

    // Not upstream's: a result's `tool_use_id` is read as text as gjson reads it,
    // so a number matches the ID spelled the same.
    #[test]
    fn align_tool_results_reads_a_numeric_id_as_text() {
        let parts = vec![
            json!({"type": "tool_result", "tool_use_id": 2}),
            json!({"type": "tool_result", "tool_use_id": 1}),
        ];
        let aligned = align_tool_results(&parts, &["1".to_owned(), "2".to_owned()]);
        assert_eq!(aligned[0]["tool_use_id"], 1);
        assert_eq!(aligned[1]["tool_use_id"], 2);
    }

    // Not upstream's: 20,000 results in reverse order. Upstream scans the results
    // again for each ID, which took 80 seconds here in a debug build; this must
    // take milliseconds, so the bound is loose enough for a slow machine yet
    // far under what the scan costs.
    #[test]
    fn align_tool_results_is_linear_for_many_results_in_reverse_order() {
        const COUNT: usize = 20_000;
        let ids: Vec<String> = (0..COUNT).map(|n| n.to_string()).collect();
        let parts: Vec<Value> = ids
            .iter()
            .rev()
            .map(|id| json!({"type": "tool_result", "tool_use_id": id, "content": "x"}))
            .collect();

        let started = std::time::Instant::now();
        let aligned = align_tool_results(&parts, &ids);
        let elapsed = started.elapsed();

        assert!(matches!(aligned, Cow::Owned(_)));
        assert_eq!(aligned.len(), COUNT);
        for (part, id) in aligned.iter().zip(&ids) {
            assert_eq!(part["tool_use_id"], id.as_str());
        }
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "aligning {COUNT} results took {elapsed:?}"
        );
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
