// Ported from CLIProxyAPI internal/translator/claude/openai/chat-completions/claude_openai_response.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude Messages events → OpenAI Chat Completions responses.
//!
//! [`ClaudeToOpenAIChatCompletionsStream`] turns each Claude event into at
//! most one `chat.completion.chunk`, and
//! [`convert_claude_response_to_openai_chat_completions_non_stream`] turns a
//! whole event stream, or a whole Messages response, into one
//! `chat.completion`. Text and thinking stream as they come. A tool call is
//! sent whole once its block ends, since Chat Completions clients expect a
//! call's name and ID with its first chunk.
//!
//! Deviations from upstream:
//! - A `data:` line that is not valid JSON or UTF-8 gives nothing. gjson
//!   reads what it can from malformed JSON. So does a whole Messages
//!   response that isn't UTF-8 or that serde_json can't read (see
//!   [`claude_native_response`](crate::common::claude_native_response)).
//! - A non-string value read as text is written as compact JSON, where
//!   upstream uses its JSON text.
//! - A token count too large for `i64`, such as `1e400`, saturates. Go's result
//!   depends on the CPU; amd64 gives the minimum `i64`.
//! - A complete response lists its tool calls by block index, skipping
//!   negative ones as upstream does. Upstream counts through every index up
//!   to the largest, which takes forever for an index like `1e18`.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::common::claude_native_response::{Native, messages_json_to_sse};
use crate::json::{int_of, object, path, str_of};

/// Translates a Claude event stream into Chat Completions chunks, one line at
/// a time. Keep one per response: it tracks the response's tool calls and
/// token counts.
pub struct ClaudeToOpenAIChatCompletionsStream {
    /// The model the client asked for.
    model: String,
    response_id: String,
    /// When the message started, in Unix seconds, or 0 before then.
    created_at: i64,
    usage: Usage,
    trailing_usage_sent: bool,
    /// Tool calls whose blocks haven't ended, by block index.
    tool_calls: HashMap<i64, ToolCall>,
    /// The `index` the next tool call gets in the chunks.
    next_tool_call_index: i64,
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    /// The call's position among this message's calls.
    index: i64,
    arguments: String,
}

impl ClaudeToOpenAIChatCompletionsStream {
    /// `model` is the model the client asked for. Chunks name it, not the
    /// model Claude reports.
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            response_id: String::new(),
            created_at: 0,
            usage: Usage::default(),
            trailing_usage_sent: false,
            tool_calls: HashMap::new(),
            next_tool_call_index: 0,
        }
    }

    /// Translates one line of the Claude event stream. Returns the chunk to
    /// send, if the line gives one.
    pub fn translate_line(&mut self, line: &[u8]) -> Option<Value> {
        let event = parse_data_line(line)?;
        let index = || event.get("index").map_or(0, int_of);
        match &*str_of(event.get("type")) {
            "message_start" => {
                if let Some(message) = event.get("message") {
                    self.response_id = str_of(message.get("id")).into_owned();
                    self.created_at = now();
                    self.next_tool_call_index = 0;
                    self.usage.merge(message.get("usage"));
                    let mut chunk = self.chunk();
                    set_delta(&mut chunk, "role", "assistant".into());
                    return Some(Value::Object(chunk));
                }
                Some(Value::Object(self.chunk()))
            }
            "content_block_start" => {
                let block = event.get("content_block")?;
                if str_of(block.get("type")) == "tool_use" {
                    let call = ToolCall {
                        id: str_of(block.get("id")).into_owned(),
                        name: str_of(block.get("name")).into_owned(),
                        index: self.next_tool_call_index,
                        arguments: String::new(),
                    };
                    self.next_tool_call_index += 1;
                    self.tool_calls.insert(index(), call);
                }
                None
            }
            "content_block_delta" => {
                let delta = event.get("delta")?;
                let (key, field) = match &*str_of(delta.get("type")) {
                    "text_delta" => ("content", "text"),
                    "thinking_delta" => ("reasoning_content", "thinking"),
                    "input_json_delta" => {
                        if let Some(partial) = delta.get("partial_json")
                            && let Some(call) = self.tool_calls.get_mut(&index())
                        {
                            call.arguments.push_str(&str_of(Some(partial)));
                        }
                        return None;
                    }
                    _ => return None,
                };
                let text = delta.get(field)?;
                let mut chunk = self.chunk();
                set_delta(&mut chunk, key, str_of(Some(text)).into());
                Some(Value::Object(chunk))
            }
            "content_block_stop" => {
                let call = self.tool_calls.remove(&index())?;
                let arguments = if call.arguments.is_empty() {
                    "{}".to_owned()
                } else {
                    call.arguments
                };
                let tool_call = object([
                    ("index", call.index.into()),
                    ("id", call.id.into()),
                    ("type", "function".into()),
                    (
                        "function",
                        object([("name", call.name.into()), ("arguments", arguments.into())]),
                    ),
                ]);
                let mut chunk = self.chunk();
                set_delta(&mut chunk, "tool_calls", vec![tool_call].into());
                Some(Value::Object(chunk))
            }
            "message_delta" => {
                let mut chunk = self.chunk();
                if let Some(reason) = path(&event, "delta.stop_reason") {
                    let finish = finish_reason(&str_of(Some(reason)));
                    if let Some(Value::Object(choice)) = first_choice(&mut chunk) {
                        choice.insert("finish_reason".into(), finish.into());
                    }
                }
                if let Some(usage) = event.get("usage") {
                    self.usage.merge(Some(usage));
                    chunk.insert("usage".into(), self.usage.to_openai());
                }
                Some(Value::Object(chunk))
            }
            "message_stop" => {
                // OpenAI's trailing usage chunk, with no choices.
                if !self.usage.reported || self.trailing_usage_sent {
                    return None;
                }
                self.trailing_usage_sent = true;
                let mut chunk = self.chunk();
                chunk.insert("choices".into(), json!([]));
                chunk.insert("usage".into(), self.usage.to_openai());
                Some(Value::Object(chunk))
            }
            "error" => {
                let error = event.get("error")?;
                Some(json!({
                    "error": {
                        "message": str_of(error.get("message")),
                        "type": str_of(error.get("type")),
                    }
                }))
            }
            _ => None,
        }
    }

    /// An empty chunk for this response so far.
    fn chunk(&self) -> Map<String, Value> {
        let Value::Object(chunk) = json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": self.created_at,
            "model": self.model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": null}],
        }) else {
            unreachable!("a chunk is an object");
        };
        chunk
    }
}

fn first_choice(chunk: &mut Map<String, Value>) -> Option<&mut Value> {
    chunk.get_mut("choices")?.get_mut(0)
}

fn set_delta(chunk: &mut Map<String, Value>, key: &str, value: Value) {
    if let Some(Value::Object(delta)) =
        first_choice(chunk).and_then(|choice| choice.get_mut("delta"))
    {
        delta.insert(key.into(), value);
    }
}

/// Converts a complete Claude event stream, as SSE text, or a whole Messages
/// response, into one Chat Completions response. The response names the
/// model Claude reports.
pub fn convert_claude_response_to_openai_chat_completions_non_stream(response: &[u8]) -> Value {
    let native = messages_json_to_sse(response);
    let response = match &native {
        Native::Events { sse, .. } => sse.as_bytes(),
        Native::Other | Native::Unreadable => response,
    };
    let mut message_id = String::new();
    let mut model = String::new();
    let mut created_at = 0;
    let mut stop_reason = String::new();
    let mut content = String::new();
    let mut reasoning: Option<String> = None;
    let mut usage = Usage::default();
    let mut tool_calls: BTreeMap<i64, ToolCall> = BTreeMap::new();

    for event in response
        .split(|&byte| byte == b'\n')
        .filter_map(parse_data_line)
    {
        let index = event.get("index").map_or(0, int_of);
        match &*str_of(event.get("type")) {
            "message_start" => {
                if let Some(message) = event.get("message") {
                    message_id = str_of(message.get("id")).into_owned();
                    model = str_of(message.get("model")).into_owned();
                    created_at = now();
                    usage.merge(message.get("usage"));
                }
            }
            "content_block_start" => {
                if let Some(block) = event.get("content_block")
                    && str_of(block.get("type")) == "tool_use"
                {
                    let call = ToolCall {
                        id: str_of(block.get("id")).into_owned(),
                        name: str_of(block.get("name")).into_owned(),
                        ..ToolCall::default()
                    };
                    tool_calls.insert(index, call);
                }
            }
            "content_block_delta" => {
                let Some(delta) = event.get("delta") else {
                    continue;
                };
                match &*str_of(delta.get("type")) {
                    "text_delta" => {
                        if let Some(text) = delta.get("text") {
                            content.push_str(&str_of(Some(text)));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(thinking) = delta.get("thinking") {
                            reasoning
                                .get_or_insert_default()
                                .push_str(&str_of(Some(thinking)));
                        }
                    }
                    "input_json_delta" => {
                        if let Some(partial) = delta.get("partial_json")
                            && let Some(call) = tool_calls.get_mut(&index)
                        {
                            call.arguments.push_str(&str_of(Some(partial)));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(call) = tool_calls.get_mut(&index)
                    && call.arguments.is_empty()
                {
                    call.arguments.push_str("{}");
                }
            }
            "message_delta" => {
                if let Some(reason) = path(&event, "delta.stop_reason") {
                    stop_reason = str_of(Some(reason)).into_owned();
                }
                if let Some(event_usage) = event.get("usage") {
                    usage.merge(Some(event_usage));
                }
            }
            _ => {}
        }
    }

    let mut message = Map::new();
    message.insert("role".into(), "assistant".into());
    message.insert("content".into(), content.into());
    if let Some(reasoning) = reasoning {
        message.insert("reasoning_content".into(), reasoning.into());
    }
    let calls: Vec<Value> = tool_calls
        .range(0..)
        .map(|(_, call)| {
            object([
                ("id", call.id.as_str().into()),
                ("type", "function".into()),
                (
                    "function",
                    object([
                        ("name", call.name.as_str().into()),
                        ("arguments", call.arguments.as_str().into()),
                    ]),
                ),
            ])
        })
        .collect();
    let finish = if calls.is_empty() {
        finish_reason(&stop_reason)
    } else {
        "tool_calls"
    };
    if !calls.is_empty() {
        message.insert("tool_calls".into(), calls.into());
    }
    let usage = if usage.reported {
        usage.to_openai()
    } else {
        json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})
    };
    object([
        ("id", message_id.into()),
        ("object", "chat.completion".into()),
        ("created", created_at.into()),
        ("model", model.into()),
        (
            "choices",
            json!([{"index": 0, "message": message, "finish_reason": finish}]),
        ),
        ("usage", usage),
    ])
}

/// The event on a `data:` line.
fn parse_data_line(line: &[u8]) -> Option<Value> {
    let data = std::str::from_utf8(line.strip_prefix(b"data:")?).ok()?;
    serde_json::from_str(data.trim()).ok()
}

/// A Claude stop reason as a Chat Completions finish reason.
fn finish_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "refusal" | "sensitive" => "content_filter",
        // end_turn, stop_sequence and anything new.
        _ => "stop",
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// The token counts Claude reported for a message so far. A later report
/// replaces the counts it names.
#[derive(Default)]
struct Usage {
    input: i64,
    output: i64,
    cache_creation: i64,
    cache_read: i64,
    /// Whether Claude reported any usage at all.
    reported: bool,
}

impl Usage {
    fn merge(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        self.reported = true;
        for (key, count) in [
            ("input_tokens", &mut self.input),
            ("output_tokens", &mut self.output),
            ("cache_creation_input_tokens", &mut self.cache_creation),
            ("cache_read_input_tokens", &mut self.cache_read),
        ] {
            if let Some(value) = usage.get(key) {
                *count = int_of(value);
            }
        }
    }

    /// The counts as Chat Completions usage. Claude counts cached input apart
    /// from other input; OpenAI's prompt tokens include it. Sums wrap as Go's
    /// do.
    fn to_openai(&self) -> Value {
        let prompt = self
            .input
            .wrapping_add(self.cache_creation)
            .wrapping_add(self.cache_read);
        json!({
            "prompt_tokens": prompt,
            "completion_tokens": self.output,
            "total_tokens": prompt.wrapping_add(self.output),
            "prompt_tokens_details": {
                "cached_tokens": self.cache_read,
                "cached_creation_tokens": self.cache_creation,
                "cache_write_tokens": self.cache_creation,
            },
        })
    }
}

#[cfg(test)]
mod tests;
