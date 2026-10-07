//! Seeded random requests for session reading.
//!
//! For `session/info`: the headers that name a session, a parent or an
//! agent, in any case, given twice, blank, padded or too long; bodies that
//! name them at the top, under `metadata` or `extra_body`, or in a nested
//! `request`, as text, numbers, booleans, objects or null; Claude Code's
//! `metadata.user_id` as JSON or in its legacy form; Codex's turn metadata;
//! and the connection's execution session. IDs come from a small pool, so
//! a parent is now and then its own child. Bodies are written compactly:
//! an object or array where an ID goes reads as its compact JSON, where
//! gjson reads its text as written (a known difference, see UPSTREAM.md).
//!
//! For `session/derive`: Chat Completions, Claude Messages, Responses,
//! Gemini and Interactions bodies, read as any client format in any case
//! and spacing, with instructions and user turns of text, media and other
//! parts, numbers Go reads as float64s, and `cache_control` keys.

use serde_json::{Map, Value, json};

use super::{Generator, escape_text, num};
use crate::cases::Case;

/// The headers session reading looks at, with Codex's turn metadata and
/// sub-agent kind given values of their own.
const HEADERS: &[&str] = &[
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
    "session-id",
    "session_id",
    "thread-id",
    "thread_id",
    "x-codex-parent-thread-id",
    "x-codex-turn-metadata",
    "x-openai-subagent",
    "x-session-id",
    "x-session-affinity",
    "x-parent-session-id",
    "x-parent-session-affinity",
    "x-parent-id",
    "x-slot-session-id",
    "x-parent-slot-session-id",
    "x-task-id",
    "x-task_id",
    "x-parent-task-id",
    "x-conversation-id",
    "x-parent-conversation-id",
    "x-thread-id",
    "x-parent-thread-id",
    "x-client-request-id",
    "x-agent-id",
    "x-request-id",
];

/// Body keys that name a session, a parent or an agent; a dot nests the
/// key in an object.
const BODY_KEYS: &[&str] = &[
    "session_id",
    "sessionId",
    "sessionID",
    "child_session_id",
    "childSessionId",
    "task_id",
    "taskId",
    "taskID",
    "action_id",
    "actionId",
    "actionID",
    "cachedContent",
    "cached_content",
    "thread_id",
    "threadId",
    "conversation_id",
    "conversationId",
    "chat_id",
    "chatId",
    "prompt_cache_key",
    "promptCacheKey",
    "parent_session_id",
    "parentSessionId",
    "parentSessionID",
    "parent_thread_id",
    "parentThreadId",
    "parentThreadID",
    "forked_from_thread_id",
    "forked_from_id",
    "parent_conversation_id",
    "parentConversationId",
    "parent_id",
    "parentId",
    "parentID",
    "parent_task_id",
    "parentTaskId",
    "parent_action_id",
    "parentActionId",
    "parent_session",
    "parentSession",
    "parent_subagent_id",
    "parentSubagentId",
    "forkSource.sessionId",
    "fork_source.session_id",
    "previousSessionId",
    "previous_session_id",
    "metadata.session_id",
    "metadata.sessionId",
    "metadata.sessionID",
    "metadata.child_session_id",
    "metadata.task_id",
    "metadata.taskId",
    "metadata.action_id",
    "metadata.thread_id",
    "metadata.conversation_id",
    "metadata.parent_session_id",
    "metadata.parentSessionId",
    "metadata.parent_thread_id",
    "metadata.forked_from_thread_id",
    "metadata.forked_from_id",
    "metadata.parent_id",
    "metadata.parent_task_id",
    "metadata.parent_action_id",
    "metadata.parent_subagent_id",
    "metadata.parent_session",
    "metadata.parent_agent_id",
    "metadata.parentAgentId",
    "metadata.forkSource.sessionId",
    "metadata.previousSessionId",
    "metadata.agent_id",
    "metadata.subagent_id",
    "metadata.user_id",
    "extra_body.session_id",
    "extra_body.sessionId",
    "extra_body.task_id",
    "extra_body.taskId",
    "extra_body.conversation_id",
    "extra_body.parent_session_id",
    "extra_body.parent_thread_id",
    "extra_body.forked_from_thread_id",
    "extra_body.forkSource.sessionId",
    "extra_body.parent_id",
    "extra_body.parent_task_id",
    "extra_body.parent_session",
    "conversation",
    "conversation.id",
    "user",
];

/// IDs, shared by headers and bodies so that sessions and parents meet.
const IDS: &[&str] = &[
    "child",
    "child",
    "parent",
    "parent",
    "root",
    "main",
    "agent-1",
    "sess-123",
    "01a06a06-e830-7da9-a866-98470a94389c",
    "  padded  ",
    "café",
    "日本",
    "a:b",
    "Child",
    "",
    " ",
    "\t",
    "tab\tinside",
    "{\"session_id\":\"in-header\"}",
];

/// IDs only a body can carry: a header value can't hold control bytes.
const BODY_ONLY_IDS: &[&str] = &[
    "line\nbreak",
    "\u{0}nul",
    "next\u{85}line",
    "del\u{7f}",
    "\u{2028}",
    "\u{feff}bom",
];

/// Numbers where an ID goes, without `-0`, which Go reads with its sign.
const NUMBERS: &[&str] = &[
    "0",
    "12",
    "-7",
    "1.50",
    "2.0",
    "1e3",
    "1E+2",
    "-1.5e-3",
    "9007199254740993",
    "123456789012345678901234567890",
    "1e30",
    "5e-324",
    "2156163594508435.25",
];

/// Agent names Codex's turn metadata gives.
const AGENT_NAMES: &[&str] = &[
    "/root/check_readme",
    "check_readme",
    "root",
    "main",
    "/root",
    "",
    " spaced ",
    "/a/b/c",
];

/// What `x-openai-subagent` says.
const SUBAGENTS: &[&str] = &[
    "collab_spawn",
    "review",
    "compact",
    "false",
    "FALSE",
    "",
    "true",
];

/// Client formats, as the executor names them and otherwise.
const FORMATS: &[&str] = &[
    "openai",
    "openai-response",
    "claude",
    "gemini",
    "codex",
    "interactions",
    " Claude ",
    "GEMINI",
    "Codex",
    "OpenAI-Response",
    "interactions\t",
    "openai-responses",
    "",
];

const CALLER_SCOPES: &[&str] = &[
    "",
    "",
    " ",
    "caller-a",
    " caller-a ",
    "caller-b",
    "8a6b4bf9c5d5d7f1a52c0f2a3c9d3e1c0e8b5f7e2d1c4b6a8f9e0d3c2b1a0f9e",
];

const ROLES: &[&str] = &[
    "user",
    "user",
    "user",
    "system",
    "developer",
    "assistant",
    "model",
    " User ",
    "SYSTEM",
    "tool",
    "",
];

/// `count` random cases for `session/info`, each depending only on `seed`
/// and its index.
pub fn info_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let headers = generator.session_headers();
            let body = generator.session_body();
            let execution_id = if generator.rng.chance(20) {
                generator.session_id_text(true)
            } else {
                String::new()
            };
            Case::new(format!("random-{seed}-{index}"), "", body).with_options(json!({
                "headers": headers,
                "execution_id": execution_id,
            }))
        })
        .collect()
}

/// `count` random cases for `session/derive`, each depending only on
/// `seed` and its index.
pub fn derive_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let format = generator.rng.pick(FORMATS);
            let caller_scope = generator.rng.pick(CALLER_SCOPES);
            let body = generator.derive_body();
            Case::new(format!("random-{seed}-{index}"), "", body).with_options(json!({
                "format": format,
                "caller_scope": caller_scope,
            }))
        })
        .collect()
}

impl Generator {
    // --- session/info ---

    /// Up to six headers as name and value pairs, sometimes a name twice.
    fn session_headers(&mut self) -> Vec<Value> {
        let mut headers = Vec::new();
        for _ in 0..self.rng.below(7) {
            let name = self.rng.pick(HEADERS);
            let value = match name {
                "x-codex-turn-metadata" => self.turn_metadata(),
                "x-openai-subagent" => self.rng.pick(SUBAGENTS).to_owned(),
                _ => self.session_id_text(false),
            };
            let name = self.header_case(name);
            headers.push(json!([name, value]));
            if self.rng.chance(10) {
                let value = self.session_id_text(false);
                headers.push(json!([name, value]));
            }
        }
        headers
    }

    /// `name` in lower case, as Go writes it, or in capitals.
    fn header_case(&mut self, name: &str) -> String {
        match self.rng.below(4) {
            0 => name.to_ascii_uppercase(),
            1 => name
                .split('-')
                .map(|word| {
                    let mut chars = word.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_ascii_uppercase().to_string() + chars.as_str()
                    })
                })
                .collect::<Vec<_>>()
                .join("-"),
            _ => name.to_owned(),
        }
    }

    /// Codex's turn metadata: a JSON object of the keys it reads, or text
    /// that isn't one.
    fn turn_metadata(&mut self) -> String {
        if self.rng.chance(10) {
            return self
                .rng
                .pick(&["", "not json", "[]", "{", "null", "{\"session_id\":1}"])
                .to_owned();
        }
        let mut fields = Map::new();
        for key in [
            "session_id",
            "thread_id",
            "parent_thread_id",
            "forked_from_thread_id",
            "forked_from_id",
        ] {
            if self.rng.chance(35) {
                fields.insert(key.to_owned(), self.session_id_value(false));
            }
        }
        if self.rng.chance(40) {
            fields.insert("agent_name".to_owned(), self.rng.pick(AGENT_NAMES).into());
        }
        if self.rng.chance(30) {
            let kind = self
                .rng
                .pick(&["thread_spawn", "review", "", "Thread_Spawn"]);
            fields.insert("subagent_kind".to_owned(), kind.into());
        }
        if self.rng.chance(20) {
            fields.insert("request_kind".to_owned(), "turn".into());
        }
        serde_json::to_string(&Value::Object(fields)).unwrap_or_default()
    }

    /// A body naming sessions, parents and agents, sometimes in a nested
    /// request, sometimes not JSON at all.
    fn session_body(&mut self) -> String {
        match self.rng.below(20) {
            0 => return String::new(),
            1 => {
                return self
                    .rng
                    .pick(&["null", "[]", "\"text\"", "{", "x{}"])
                    .to_owned();
            }
            _ => {}
        }
        let mut body = self.session_fields();
        if self.rng.chance(25) {
            let nested = self.session_fields();
            body.insert("request".to_owned(), Value::Object(nested));
            if self.rng.chance(30) {
                body.insert("contents".to_owned(), json!([]));
            }
        }
        if self.rng.chance(15) {
            let messages = json!([{"role": "user", "content": "hi"}]);
            body.insert("messages".to_owned(), messages);
        }
        let text = serde_json::to_string(&Value::Object(body)).unwrap_or_default();
        let text = if self.rng.chance(20) {
            escape_text(&text)
        } else {
            text
        };
        match self.rng.below(15) {
            0 => format!(" \r\n\t{text}"),
            1 => format!("{text}\n"),
            _ => text,
        }
    }

    /// Up to six of [`BODY_KEYS`], each with an ID.
    fn session_fields(&mut self) -> Map<String, Value> {
        let mut body = Map::new();
        for _ in 0..self.rng.below(7) {
            let key = self.rng.pick(BODY_KEYS);
            let value = match key {
                "metadata.user_id" | "user" => self.user_id(),
                "conversation" if self.rng.chance(50) => {
                    json!({ "id": self.session_id_value(true) })
                }
                _ => self.session_id_value(true),
            };
            insert_path(&mut body, key, value);
        }
        body
    }

    /// Claude Code's `metadata.user_id`: JSON naming a session, parent and
    /// agent, the legacy form, or a plain ID.
    fn user_id(&mut self) -> Value {
        match self.rng.below(4) {
            0 => {
                let mut fields = Map::new();
                for key in [
                    "session_id",
                    "parent_session_id",
                    "parent_agent_id",
                    "parent_id",
                    "agent_id",
                    "subagent_id",
                    "device_id",
                ] {
                    if self.rng.chance(40) {
                        fields.insert(key.to_owned(), self.session_id_value(true));
                    }
                }
                let text = serde_json::to_string(&Value::Object(fields)).unwrap_or_default();
                if self.rng.chance(20) {
                    format!(" {text}").into()
                } else {
                    text.into()
                }
            }
            1 => {
                let session = self.rng.pick(&[
                    "01a06a06-e830-7da9-a866-98470a94389c",
                    "abc-123",
                    "ABC-123",
                    "-",
                    "",
                    "0f_",
                ]);
                let head = self.rng.pick(&[
                    "user_123_acc__session_",
                    "user_ab12_account_0f3c_session_",
                    "session_",
                    "_session",
                    "user_1_session__session_",
                ]);
                format!("{head}{session}").into()
            }
            _ => self.session_id_value(true),
        }
    }

    /// An ID as JSON: mostly text, sometimes a number, boolean, object,
    /// array or null.
    fn session_id_value(&mut self, in_body: bool) -> Value {
        if self.rng.chance(85) {
            return self.session_id_text(in_body).into();
        }
        match self.rng.below(5) {
            0 | 1 => num(self.rng.pick(NUMBERS)),
            2 => self.one_of(&[json!(true), json!(false), Value::Null]),
            3 => json!({ "id": "nested" }),
            _ => json!(["a", 1]),
        }
    }

    /// An ID as text: from the pool, or around the 256-byte limit, or one
    /// a header can't carry when `in_body`.
    fn session_id_text(&mut self, in_body: bool) -> String {
        match self.rng.below(12) {
            0 => {
                let unit = self.rng.pick(&["s", "é", "日"]);
                let target = 250 + self.rng.below(12);
                let mut text = String::new();
                while text.len() + unit.len() <= target {
                    text.push_str(unit);
                }
                text
            }
            1 if in_body => self.rng.pick(BODY_ONLY_IDS).to_owned(),
            2 => {
                let len = 1 + self.rng.below(12);
                self.alphanumeric(len)
            }
            _ => self.rng.pick(IDS).to_owned(),
        }
    }

    // --- session/derive ---

    /// A body of one of the shapes derivation reads, rendered, now and then
    /// with something around it.
    fn derive_body(&mut self) -> String {
        let body = match self.rng.below(6) {
            0 | 1 => self.messages_body(),
            2 => self.responses_body(),
            3 => self.gemini_body(),
            4 => self.interactions_body(),
            _ => {
                let mut body = self.messages_body();
                if let (Value::Object(body), Value::Object(other)) =
                    (&mut body, self.responses_body())
                {
                    body.extend(other);
                }
                body
            }
        };
        let text = self.render(&body);
        match self.rng.below(25) {
            0 => String::new(),
            1 => format!("x{text}"),
            2 => format!(" \n{text}\t"),
            3 => format!("{text} x"),
            4 => format!("[{text}]"),
            _ => text,
        }
    }

    /// A Chat Completions or Claude Messages body.
    fn messages_body(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(40) {
            fields.push(("system", self.derive_content()));
        }
        if self.rng.chance(90) {
            fields.push(("messages", self.role_items()));
        }
        if self.rng.chance(20) {
            fields.push(("max_tokens", self.derive_number()));
        }
        self.object(fields)
    }

    /// A Responses body.
    fn responses_body(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(50) {
            fields.push(("instructions", self.derive_content()));
        }
        if self.rng.chance(90) {
            let input = if self.rng.chance(30) {
                self.loose_text()
            } else {
                self.role_items()
            };
            fields.push(("input", input));
        }
        self.object(fields)
    }

    /// A Gemini body, sometimes nesting its request.
    fn gemini_body(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(40) {
            let key = self.rng.pick(&["systemInstruction", "system_instruction"]);
            let instruction = if self.rng.chance(70) {
                json!({ "parts": self.parts() })
            } else {
                self.derive_content()
            };
            fields.push((key, instruction));
        }
        if self.rng.chance(30) {
            let key = self.rng.pick(&["cachedContent", "cached_content"]);
            let value = if self.rng.chance(85) {
                self.rng
                    .pick(&["cachedContents/abc", " cachedContents/abc ", "", "x"])
                    .into()
            } else {
                json!(5)
            };
            fields.push((key, value));
        }
        if self.rng.chance(90) {
            let count = 1 + self.rng.below(4);
            let contents = (0..count)
                .map(|_| {
                    let role = self.rng.pick(ROLES);
                    let parts = self.parts();
                    match self.rng.below(8) {
                        0 => json!({ "role": role, "content": parts }),
                        1 => json!({ "role": role, "text": "as text" }),
                        2 => json!({ "parts": parts }),
                        _ => json!({ "role": role, "parts": parts }),
                    }
                })
                .collect();
            fields.push(("contents", Value::Array(contents)));
        }
        let body = self.object(fields);
        if self.rng.chance(25) {
            let mut outer = vec![("request", body)];
            if self.rng.chance(50) {
                outer.push(("project_id", "proj".into()));
            }
            self.object(outer)
        } else {
            body
        }
    }

    /// A Gemini Interactions body.
    fn interactions_body(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(40) {
            let key = self.rng.pick(&["system_instruction", "systemInstruction"]);
            fields.push((key, self.derive_content()));
        }
        if self.rng.chance(90) {
            let input = if self.rng.chance(25) {
                self.loose_text()
            } else {
                let count = 1 + self.rng.below(4);
                Value::Array((0..count).map(|_| self.interaction_step(2)).collect())
            };
            fields.push(("input", input));
        }
        self.object(fields)
    }

    /// One Interactions step, sometimes holding steps of its own.
    fn interaction_step(&mut self, depth: usize) -> Value {
        if self.rng.chance(10) {
            return self.loose_text();
        }
        let mut fields = Vec::new();
        if self.rng.chance(50) {
            fields.push(("role", self.rng.pick(ROLES).into()));
        }
        if self.rng.chance(70) {
            let step_type = self.rng.pick(&[
                "user_input",
                "model_output",
                "message",
                "system_instruction",
                "developer_instruction",
                "function_call",
                "",
                "User_Input",
            ]);
            fields.push(("type", step_type.into()));
        }
        if depth > 0 && self.rng.chance(15) {
            let count = 1 + self.rng.below(3);
            let steps = (0..count)
                .map(|_| self.interaction_step(depth - 1))
                .collect();
            fields.push(("steps", Value::Array(steps)));
        } else {
            let key = self.rng.pick(&["content", "content", "parts", "text"]);
            fields.push((key, self.derive_content()));
        }
        self.object(fields)
    }

    /// Up to four items with a role and content, as Chat Completions,
    /// Claude and Responses bodies hold them.
    fn role_items(&mut self) -> Value {
        let count = self.rng.below(5);
        let items = (0..count)
            .map(|_| {
                if self.rng.chance(5) {
                    return self.loose_text();
                }
                let mut fields = Vec::new();
                if self.rng.chance(95) {
                    fields.push(("role", self.rng.pick(ROLES).into()));
                }
                if self.rng.chance(30) {
                    fields.push(("type", "message".into()));
                }
                if self.rng.chance(95) {
                    fields.push(("content", self.derive_content()));
                }
                self.object(fields)
            })
            .collect();
        Value::Array(items)
    }

    /// Content: text, a list of parts, or now and then something else.
    fn derive_content(&mut self) -> Value {
        match self.rng.below(10) {
            0..=3 => self.text().into(),
            4..=8 => self.parts(),
            _ => self.one_of(&[Value::Null, json!(3), json!(true), json!({"x": 1})]),
        }
    }

    /// Up to four parts of the kinds derivation tells apart.
    fn parts(&mut self) -> Value {
        let count = self.rng.below(5);
        Value::Array((0..count).map(|_| self.part()).collect())
    }

    fn part(&mut self) -> Value {
        match self.rng.below(16) {
            0..=4 => {
                let kind = self.rng.pick(&["text", "input_text", "output_text", ""]);
                json!({ "type": kind, "text": self.text() })
            }
            5 => json!({ "text": self.derive_number() }),
            6 => {
                let url = self.media_text();
                let image = if self.rng.chance(50) {
                    url
                } else {
                    json!({ "url": url, "detail": "auto" })
                };
                json!({ "type": "image_url", "image_url": image })
            }
            7 => {
                let key = self.rng.pick(&["inlineData", "inline_data"]);
                let mime = self.rng.pick(&["mimeType", "mime_type", "media_type"]);
                let data = self.media_text();
                json!({ key: { mime: "image/png", "data": data } })
            }
            8 => {
                let key = self.rng.pick(&["fileData", "file_data"]);
                let uri = self.rng.pick(&["fileUri", "file_uri", "uri", "url"]);
                let data = self.media_text();
                json!({ key: { uri: data } })
            }
            9 => {
                let kind = self.rng.pick(&["image", "document", "", " Image "]);
                let source = if self.rng.chance(70) {
                    json!({ "type": "base64", "media_type": "image/jpeg", "data": self.media_text() })
                } else {
                    self.media_text()
                };
                json!({ "type": kind, "media_type": "application/pdf", "source": source })
            }
            10 => json!({ "content": self.derive_content() }),
            11 => json!({ "parts": [{ "text": self.text() }] }),
            12 => {
                let mut fields = vec![
                    ("type", "tool_result".into()),
                    ("tool_use_id", "toolu_1".into()),
                    ("n", self.derive_number()),
                ];
                if self.rng.chance(60) {
                    let key = self
                        .rng
                        .pick(&["cache_control", "Cache_Control", " cache_control "]);
                    fields.push((key, json!({ "type": "ephemeral" })));
                }
                if self.rng.chance(40) {
                    fields.push(("html", "<b>&</b>".into()));
                }
                self.object(fields)
            }
            13 => self.text().into(),
            14 => self.derive_number(),
            _ => self.one_of(&[Value::Null, json!(false), json!([]), json!({})]),
        }
    }

    /// A media URL or data, sometimes empty or not text.
    fn media_text(&mut self) -> Value {
        match self.rng.below(8) {
            0 => "".into(),
            1 => json!(7),
            _ => self
                .rng
                .pick(&[
                    "https://example.invalid/a.png",
                    "data:image/png;base64,AAAA",
                    "gs://bucket/file.pdf",
                    "QUJD",
                ])
                .into(),
        }
    }

    /// A number Go reads as a float64, or now and then one out of its range.
    fn derive_number(&mut self) -> Value {
        if self.rng.chance(3) {
            num("1e400")
        } else {
            num(self.rng.pick(NUMBERS))
        }
    }
}

/// Inserts `value` at `path`, a dot nesting a key in an object; an object
/// already there takes the key, anything else is replaced.
fn insert_path(object: &mut Map<String, Value>, path: &str, value: Value) {
    match path.split_once('.') {
        None => {
            object.insert(path.to_owned(), value);
        }
        Some((head, rest)) => {
            let entry = object
                .entry(head.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Value::Object(inner) = entry {
                insert_path(inner, rest, value);
            }
        }
    }
}
