//! The requests the load generator sends: a short one, and a long
//! conversation of many turns with tool calls, in the Chat Completions,
//! Claude Messages and Responses formats. The text is made from a fixed
//! seed, so every run sends the same bytes.

use bytes::Bytes;
use serde_json::{Value, json};

/// Turns in the long conversation. With the text below, that makes a body of
/// about 310 KB.
const LONG_TURNS: usize = 120;

/// Every fourth assistant turn calls a tool.
const TOOL_EVERY: usize = 4;

const WORDS: [&str; 32] = [
    "proxy", "request", "stream", "model", "token", "account", "header", "config", "router",
    "answer", "client", "server", "latency", "buffer", "socket", "handler", "retry", "upstream",
    "format", "message", "content", "tool", "result", "error", "status", "event", "chunk", "line",
    "file", "test", "build", "check",
];

/// The client format of a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// OpenAI Chat Completions, `POST /v1/chat/completions`.
    Chat,
    /// Claude Messages, `POST /v1/messages`.
    Claude,
    /// OpenAI Responses, `POST /v1/responses`.
    Responses,
}

impl Format {
    /// The path a client sends this format to.
    pub fn path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Claude => "/v1/messages",
            Self::Responses => "/v1/responses",
        }
    }
}

/// A deterministic word generator (xorshift64).
struct Text(u64);

impl Text {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// About `len` bytes of words, ending with a full stop.
    fn words(&mut self, len: usize) -> String {
        let mut out = String::with_capacity(len + 16);
        while out.len() < len {
            if !out.is_empty() {
                out.push(' ');
            }
            let index = usize::try_from(self.next() % WORDS.len() as u64).unwrap_or(0);
            out.push_str(WORDS.get(index).copied().unwrap_or("proxy"));
        }
        out.push('.');
        out
    }
}

/// A short request: a system prompt and one question.
pub fn short(format: Format, model: &str, stream: bool) -> Bytes {
    let body = match format {
        Format::Chat => json!({
            "model": model,
            "stream": stream,
            "max_tokens": 64,
            "messages": [
                { "role": "system", "content": "You answer benchmark requests." },
                { "role": "user", "content": "Say hello in one sentence." },
            ],
        }),
        Format::Claude => json!({
            "model": model,
            "stream": stream,
            "max_tokens": 64,
            "system": "You answer benchmark requests.",
            "messages": [{ "role": "user", "content": "Say hello in one sentence." }],
        }),
        Format::Responses => json!({
            "model": model,
            "stream": stream,
            "max_output_tokens": 64,
            "instructions": "You answer benchmark requests.",
            "input": [responses_message("user", "Say hello in one sentence.")],
        }),
    };
    Bytes::from(body.to_string())
}

/// A message item of a Responses request's `input`.
fn responses_message(role: &str, text: &str) -> Value {
    let part = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": part, "text": text }],
    })
}

/// A long conversation of [`LONG_TURNS`] turns, as a coding agent sends one:
/// a system prompt, two tools, and turns of text where every fourth answer
/// reads a file and gets its contents back.
pub fn long(format: Format, model: &str, stream: bool) -> Bytes {
    let mut text = Text(0x9e37_79b9_7f4a_7c15);
    let system = text.words(2000);
    let body = match format {
        Format::Chat => {
            let mut messages = vec![json!({ "role": "system", "content": system })];
            for turn in 0..LONG_TURNS {
                messages.push(json!({ "role": "user", "content": text.words(900) }));
                if turn % TOOL_EVERY == TOOL_EVERY - 1 {
                    let id = format!("call_bench_{turn}");
                    messages.push(json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": json!({ "path": format!("src/module_{turn}.rs") }).to_string(),
                            },
                        }],
                    }));
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "content": text.words(2400),
                    }));
                }
                messages.push(json!({ "role": "assistant", "content": text.words(900) }));
            }
            messages.push(json!({ "role": "user", "content": "Sum it up in one sentence." }));
            json!({
                "model": model,
                "stream": stream,
                "max_tokens": 64,
                "messages": messages,
                "tools": [
                    {
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "description": "Reads a file of the project.",
                            "parameters": read_file_schema(),
                        },
                    },
                    {
                        "type": "function",
                        "function": {
                            "name": "run_command",
                            "description": "Runs a command in the project's directory.",
                            "parameters": run_command_schema(),
                        },
                    },
                ],
            })
        }
        Format::Claude => {
            let mut messages = Vec::new();
            // The tool result the next user turn starts with, if any.
            let mut result: Option<Value> = None;
            for turn in 0..LONG_TURNS {
                let mut user = Vec::new();
                user.extend(result.take());
                user.push(json!({ "type": "text", "text": text.words(900) }));
                messages.push(json!({ "role": "user", "content": user }));
                let mut assistant = vec![json!({ "type": "text", "text": text.words(900) })];
                if turn % TOOL_EVERY == TOOL_EVERY - 1 {
                    let id = format!("toolu_bench_{turn}");
                    assistant.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": "read_file",
                        "input": { "path": format!("src/module_{turn}.rs") },
                    }));
                    result = Some(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": text.words(2400),
                    }));
                }
                messages.push(json!({ "role": "assistant", "content": assistant }));
            }
            let mut last = Vec::new();
            last.extend(result.take());
            last.push(json!({ "type": "text", "text": "Sum it up in one sentence." }));
            messages.push(json!({ "role": "user", "content": last }));
            json!({
                "model": model,
                "stream": stream,
                "max_tokens": 64,
                "system": system,
                "messages": messages,
                "tools": [
                    {
                        "name": "read_file",
                        "description": "Reads a file of the project.",
                        "input_schema": read_file_schema(),
                    },
                    {
                        "name": "run_command",
                        "description": "Runs a command in the project's directory.",
                        "input_schema": run_command_schema(),
                    },
                ],
            })
        }
        Format::Responses => {
            // The items in the order of the Chat Completions messages: per
            // turn a question, for every fourth turn a call and its output,
            // and an answer.
            let mut input = Vec::new();
            for turn in 0..LONG_TURNS {
                input.push(responses_message("user", &text.words(900)));
                if turn % TOOL_EVERY == TOOL_EVERY - 1 {
                    let id = format!("call_bench_{turn}");
                    input.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": "read_file",
                        "arguments": json!({ "path": format!("src/module_{turn}.rs") }).to_string(),
                    }));
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": id,
                        "output": text.words(2400),
                    }));
                }
                input.push(responses_message("assistant", &text.words(900)));
            }
            input.push(responses_message("user", "Sum it up in one sentence."));
            json!({
                "model": model,
                "stream": stream,
                "max_output_tokens": 64,
                "instructions": system,
                "input": input,
                "tools": [
                    {
                        "type": "function",
                        "name": "read_file",
                        "description": "Reads a file of the project.",
                        "parameters": read_file_schema(),
                    },
                    {
                        "type": "function",
                        "name": "run_command",
                        "description": "Runs a command in the project's directory.",
                        "parameters": run_command_schema(),
                    },
                ],
            })
        }
    };
    Bytes::from(body.to_string())
}

fn read_file_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "path": { "type": "string", "description": "The file's path." } },
        "required": ["path"],
    })
}

fn run_command_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "The command line." },
            "timeout_ms": { "type": "integer", "description": "How long to wait." },
        },
        "required": ["command"],
    })
}

/// The number of messages in a long conversation (of input items, for
/// Responses), for the report.
pub fn long_messages(format: Format) -> usize {
    let body = long(format, "m", false);
    let list = match format {
        Format::Chat | Format::Claude => "messages",
        Format::Responses => "input",
    };
    serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value.get(list).and_then(Value::as_array).map(Vec::len))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the long conversation is a few hundred kilobytes, the
    // same every time, and its turns alternate as Claude requires.
    #[test]
    fn long_conversation_shape() {
        for format in [Format::Chat, Format::Claude, Format::Responses] {
            let body = long(format, "m", true);
            assert!((250_000..450_000).contains(&body.len()), "{}", body.len());
            assert_eq!(body, long(format, "m", true));
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["stream"], true);
            assert_eq!(value["tools"].as_array().unwrap().len(), 2);
        }
        let claude: Value = serde_json::from_slice(&long(Format::Claude, "m", false)).unwrap();
        let roles: Vec<&str> = claude["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect();
        assert!(roles.chunks(2).all(|pair| pair[0] == "user"));
        assert_eq!(roles.len(), 2 * LONG_TURNS + 1);
        assert_eq!(long_messages(Format::Claude), 2 * LONG_TURNS + 1);
        // A system prompt, then per turn a question and an answer, and for
        // every fourth turn a call and its result; then the last question.
        assert_eq!(
            long_messages(Format::Chat),
            1 + 2 * LONG_TURNS + LONG_TURNS / 2 + 1
        );
        // The same items, but the system prompt is the instructions.
        assert_eq!(
            long_messages(Format::Responses),
            2 * LONG_TURNS + LONG_TURNS / 2 + 1
        );
        let responses: Value =
            serde_json::from_slice(&long(Format::Responses, "m", false)).unwrap();
        assert_eq!(responses["input"][7]["type"], "function_call");
        assert_eq!(responses["input"][8]["type"], "function_call_output");
        assert_eq!(
            responses["input"][8]["call_id"],
            responses["input"][7]["call_id"]
        );
        assert_eq!(responses["tools"][0]["name"], "read_file");
    }

    // Not upstream's: a short request is small and asks to stream as told.
    #[test]
    fn short_request() {
        let value: Value = serde_json::from_slice(&short(Format::Chat, "m", false)).unwrap();
        assert_eq!(value["stream"], false);
        assert_eq!(value["messages"][1]["role"], "user");
        let value: Value = serde_json::from_slice(&short(Format::Claude, "m", true)).unwrap();
        assert_eq!(value["stream"], true);
        assert_eq!(value["system"], "You answer benchmark requests.");
        let value: Value = serde_json::from_slice(&short(Format::Responses, "m", true)).unwrap();
        assert_eq!(value["stream"], true);
        assert_eq!(value["instructions"], "You answer benchmark requests.");
        assert_eq!(value["input"][0]["content"][0]["type"], "input_text");
    }
}
