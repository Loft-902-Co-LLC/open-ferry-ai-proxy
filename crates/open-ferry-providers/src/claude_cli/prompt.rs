//! A Claude Messages request made into what Claude Code takes: one user
//! message on standard input, a system prompt, and the settings that go as
//! flags or variables.
//!
//! What Claude Code can't do for a client is refused with a 400: client
//! tools, a tool choice other than `none` or `auto`, tool use or tool
//! results in the messages, a last turn that isn't the user's, or no
//! message. Fields it has no use for, such as sampling settings and
//! metadata, are dropped, with a debug line.
//!
//! One user turn goes as it is. A conversation of several goes as a
//! transcript in one message: a note saying what follows, then each turn
//! as a label (`[user]` or `[assistant]`) and its blocks (images and
//! documents where they stood, earlier thinking left out), then an end
//! mark. The same turns always give the same transcript, and a new turn
//! only adds to it, so Claude Code's prompt cache keeps working.
//!
//! The message's first text never starts with `/`, which would ask for a
//! slash command: one that would gets an invisible word joiner (U+2060) in
//! front. Cache breakpoints are taken off the blocks, as Claude Code sets
//! its own, and empty text blocks are left out.

use open_ferry_core::exec::ExecError;
use serde_json::{Map, Value, json};

use crate::json::{int_at, str_at};

/// What leads a transcript.
pub(crate) const TRANSCRIPT_NOTE: &str = "The conversation so far follows, one turn after \
     another, each after a line naming who spoke: [user] or [assistant]. Reply to the last user \
     turn as the assistant would, with the reply alone, without a label.";

/// What ends a transcript.
pub(crate) const TRANSCRIPT_END: &str = "[end of conversation]";

/// What goes in front of a first text that starts with `/`.
const WORD_JOINER: char = '\u{2060}';

/// The effort levels `--effort` takes.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The fields read; the rest are dropped.
const USED_FIELDS: [&str; 9] = [
    "model",
    "messages",
    "system",
    "max_tokens",
    "thinking",
    "output_config",
    "stream",
    "tools",
    "tool_choice",
];

/// A request made ready for Claude Code.
#[derive(Debug, PartialEq)]
pub(crate) struct Prepared {
    /// The line for standard input, without its newline.
    pub(crate) input: Vec<u8>,
    /// The system prompt.
    pub(crate) system: String,
    /// The output cap, from `max_tokens`.
    pub(crate) max_output_tokens: Option<u64>,
    /// The thinking budget, from an enabled `thinking`.
    pub(crate) thinking_budget: Option<u64>,
    /// The effort, from `output_config.effort`.
    pub(crate) effort: Option<String>,
    /// Whether the client asked for thinking, so it gets the thinking
    /// blocks.
    pub(crate) thinking: bool,
}

/// A 400 for what Claude Code can't do.
pub(crate) fn invalid(message: impl Into<String>) -> ExecError {
    let message = message.into();
    let body = json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": message},
    });
    ExecError::upstream(400, body.to_string())
}

/// `body`, a Claude Messages request, ready for Claude Code.
pub(crate) fn prepare(body: &Value) -> Result<Prepared, ExecError> {
    check_tools(body)?;
    let ignored: Vec<&str> = body
        .as_object()
        .into_iter()
        .flat_map(Map::keys)
        .map(String::as_str)
        .filter(|key| !USED_FIELDS.contains(key))
        .collect();
    if !ignored.is_empty() {
        tracing::debug!(
            "claude-cli: ignoring fields Claude Code doesn't take: {}",
            ignored.join(", ")
        );
    }

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let turns = turns(messages)?;
    let mut content = match turns.as_slice() {
        [] => return Err(invalid("claude-cli: the request has no messages")),
        [(role, blocks)] if role == "user" => blocks.clone(),
        _ => transcript(&turns),
    };
    if content.is_empty() {
        return Err(invalid("claude-cli: the last user turn is empty"));
    }
    guard_slash(&mut content);
    let line = json!({
        "type": "user",
        "message": {"role": "user", "content": content},
        "parent_tool_use_id": null,
        "client_composed": true,
    });

    let thinking_type = str_at(body, "thinking.type");
    let budget = positive(int_at(body, "thinking.budget_tokens"));
    let effort = str_at(body, "output_config.effort");
    Ok(Prepared {
        input: line.to_string().into_bytes(),
        system: system_text(body.get("system")),
        max_output_tokens: positive(int_at(body, "max_tokens")),
        thinking_budget: budget.filter(|_| thinking_type == "enabled"),
        effort: EFFORTS.contains(&effort.as_str()).then_some(effort),
        thinking: matches!(thinking_type.as_str(), "enabled" | "adaptive"),
    })
}

fn positive(value: i64) -> Option<u64> {
    u64::try_from(value).ok().filter(|value| *value > 0)
}

/// Refuses client tools, a forcing tool choice, and tool blocks.
fn check_tools(body: &Value) -> Result<(), ExecError> {
    let tools = body.get("tools");
    if tools.is_some_and(|tools| tools.as_array().is_none_or(|tools| !tools.is_empty())) {
        return Err(tools_error("tools"));
    }
    if let Some(choice) = body.get("tool_choice").filter(|choice| !choice.is_null()) {
        let kind = choice
            .as_str()
            .map_or_else(|| str_at(choice, "type"), str::to_owned);
        if !matches!(kind.as_str(), "none" | "auto") {
            return Err(tools_error("tool_choice"));
        }
    }
    let has_tool_block = body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .any(|block| {
            let kind = str_at(block, "type");
            kind.contains("tool_use") || kind.contains("tool_result")
        });
    if has_tool_block {
        return Err(tools_error("tool use and tool results"));
    }
    Ok(())
}

fn tools_error(what: &str) -> ExecError {
    invalid(format!(
        "claude-cli: client tools aren't supported by claude-cli yet ({what} in the request)"
    ))
}

/// Each message as its role and its blocks, without cache breakpoints or
/// empty text; the last must be the user's.
fn turns(messages: &[Value]) -> Result<Vec<(String, Vec<Value>)>, ExecError> {
    let mut turns = Vec::with_capacity(messages.len());
    for message in messages {
        let role = str_at(message, "role");
        if role != "user" && role != "assistant" {
            return Err(invalid(format!(
                "claude-cli: a message has the role {role:?}; only user and assistant are taken"
            )));
        }
        turns.push((role, blocks(message.get("content"))));
    }
    if turns.last().is_some_and(|(role, _)| role != "user") {
        return Err(invalid(
            "claude-cli: the last message must be the user's; claude-cli can't continue an assistant turn",
        ));
    }
    Ok(turns)
}

/// A message's content as blocks.
fn blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({"type": "text", "text": text})]
        }
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| !(str_at(block, "type") == "text" && str_at(block, "text").is_empty()))
            .map(|block| {
                let mut block = block.clone();
                if let Some(block) = block.as_object_mut() {
                    block.remove("cache_control");
                }
                block
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The transcript of several turns, as one message's blocks.
fn transcript(turns: &[(String, Vec<Value>)]) -> Vec<Value> {
    let mut out = vec![text_block(TRANSCRIPT_NOTE)];
    for (role, blocks) in turns {
        out.push(text_block(&format!("[{role}]")));
        out.extend(
            blocks
                .iter()
                .filter(|block| {
                    !matches!(
                        str_at(block, "type").as_str(),
                        "thinking" | "redacted_thinking"
                    )
                })
                .cloned(),
        );
    }
    out.push(text_block(TRANSCRIPT_END));
    out
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// Puts a word joiner in front of the first text when it starts with `/`.
fn guard_slash(content: &mut [Value]) {
    let Some(first) = content
        .iter_mut()
        .find(|block| str_at(block, "type") == "text")
    else {
        return;
    };
    let text = str_at(first, "text");
    if text.trim_start().starts_with('/') {
        first["text"] = Value::String(format!("{WORD_JOINER}{text}"));
    }
}

/// The system prompt: a string, or the text of each block, a blank line
/// between them.
fn system_text(system: Option<&Value>) -> String {
    match system {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block {
                Value::String(text) => text.clone(),
                block => str_at(block, "text"),
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(prepared: &Prepared) -> Value {
        serde_json::from_slice(&prepared.input).unwrap()
    }

    #[test]
    fn one_turn_goes_as_it_is() {
        let body = json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 1024,
            "temperature": 0.5,
            "metadata": {"user_id": "u"},
            "system": [{"type": "text", "text": "Be brief.", "cache_control": {"type": "ephemeral"}}, {"type": "text", "text": "Be kind."}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": ""},
                {"type": "text", "text": "Hi", "cache_control": {"type": "ephemeral"}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ]}],
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "output_config": {"effort": "high"},
            "tools": [],
            "tool_choice": {"type": "auto"},
        });
        let prepared = prepare(&body).unwrap();
        assert_eq!(
            input(&prepared),
            json!({
                "type": "user",
                "message": {"role": "user", "content": [
                    {"type": "text", "text": "Hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                ]},
                "parent_tool_use_id": null,
                "client_composed": true,
            })
        );
        assert_eq!(prepared.system, "Be brief.\n\nBe kind.");
        assert_eq!(prepared.max_output_tokens, Some(1024));
        assert_eq!(prepared.thinking_budget, Some(2048));
        assert_eq!(prepared.effort.as_deref(), Some("high"));
        assert!(prepared.thinking);

        let plain =
            prepare(&json!({"messages": [{"role": "user", "content": "Hi"}], "system": "S"}))
                .unwrap();
        assert_eq!(
            input(&plain)["message"]["content"],
            json!([{"type": "text", "text": "Hi"}])
        );
        assert_eq!(plain.system, "S");
        assert_eq!(plain.max_output_tokens, None);
        assert_eq!(plain.effort, None);
        assert!(!plain.thinking);

        let adaptive = prepare(&json!({
            "messages": [{"role": "user", "content": "Hi"}],
            "thinking": {"type": "adaptive", "budget_tokens": 9},
            "output_config": {"effort": "extreme"},
        }))
        .unwrap();
        assert!(adaptive.thinking);
        assert_eq!(adaptive.thinking_budget, None);
        assert_eq!(adaptive.effort, None);
    }

    #[test]
    fn a_slash_never_leads() {
        for text in ["/compact", "  /login now"] {
            let prepared =
                prepare(&json!({"messages": [{"role": "user", "content": text}]})).unwrap();
            let sent = input(&prepared)["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(sent, format!("\u{2060}{text}"));
        }
        let prepared = prepare(&json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {}},
            {"type": "text", "text": "/x"},
            {"type": "text", "text": "/y"},
        ]}]}))
        .unwrap();
        let content = &input(&prepared)["message"]["content"];
        assert_eq!(content[1]["text"], "\u{2060}/x");
        assert_eq!(content[2]["text"], "/y");
    }

    #[test]
    fn several_turns_make_a_transcript_that_only_grows() {
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}});
        let first = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "What is this?"}, image]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "s"},
                {"type": "text", "text": "A dot."},
            ]},
            {"role": "user", "content": "Sure?"},
        ]});
        let prepared = prepare(&first).unwrap();
        let content = input(&prepared)["message"]["content"].clone();
        assert_eq!(
            content,
            json!([
                {"type": "text", "text": TRANSCRIPT_NOTE},
                {"type": "text", "text": "[user]"},
                {"type": "text", "text": "What is this?"},
                image,
                {"type": "text", "text": "[assistant]"},
                {"type": "text", "text": "A dot."},
                {"type": "text", "text": "[user]"},
                {"type": "text", "text": "Sure?"},
                {"type": "text", "text": TRANSCRIPT_END},
            ])
        );
        // The same turns give the same bytes.
        assert_eq!(prepare(&first).unwrap().input, prepared.input);

        // A next turn keeps everything before the end mark.
        let mut next = first.clone();
        let messages = next["messages"].as_array_mut().unwrap();
        messages.push(json!({"role": "assistant", "content": "Yes."}));
        messages.push(json!({"role": "user", "content": "Thanks"}));
        let longer = input(&prepare(&next).unwrap())["message"]["content"].clone();
        let longer = longer.as_array().unwrap();
        let content = content.as_array().unwrap();
        assert_eq!(longer[..content.len() - 1], content[..content.len() - 1]);
        assert_eq!(longer.last(), content.last());
    }

    #[test]
    fn refuses_what_claude_code_cant_do() {
        let user = json!({"role": "user", "content": "Hi"});
        for (body, want) in [
            (
                json!({"messages": [user], "tools": [{"name": "t", "input_schema": {}}]}),
                "client tools aren't supported by claude-cli yet",
            ),
            (
                json!({"messages": [user], "tool_choice": {"type": "any"}}),
                "client tools aren't supported by claude-cli yet",
            ),
            (
                json!({"messages": [user], "tool_choice": {"type": "tool", "name": "t"}}),
                "client tools aren't supported by claude-cli yet",
            ),
            (
                json!({"messages": [
                    {"role": "user", "content": "Hi"},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "x", "input": {}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "r"}]},
                ]}),
                "client tools aren't supported by claude-cli yet",
            ),
            (json!({"messages": []}), "the request has no messages"),
            (json!({}), "the request has no messages"),
            (
                json!({"messages": [user, {"role": "assistant", "content": "Well,"}]}),
                "can't continue an assistant turn",
            ),
            (
                json!({"messages": [{"role": "user", "content": ""}]}),
                "the last user turn is empty",
            ),
        ] {
            let error = prepare(&body).unwrap_err();
            assert_eq!(error.status, 400, "{body}");
            let message: Value = serde_json::from_str(&error.message).unwrap();
            assert_eq!(message["error"]["type"], "invalid_request_error");
            let text = message["error"]["message"].as_str().unwrap();
            assert!(text.contains(want), "{body}: {text}");
        }
        // `none` and `auto` are fine, as is an empty tool list.
        for choice in [
            json!({"type": "none"}),
            json!({"type": "auto"}),
            json!("auto"),
        ] {
            prepare(&json!({"messages": [user], "tool_choice": choice, "tools": []})).unwrap();
        }
    }
}
