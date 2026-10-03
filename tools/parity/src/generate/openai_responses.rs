//! Seeded random input for the Responses → Chat Completions translators.
//!
//! Requests aim at what that translator reads: `instructions`, messages of
//! every role with text, image and video parts, `reasoning_content` on
//! assistant messages and tool calls, reasoning items with and without
//! summaries, function and custom tool calls with IDs missing, repeated or
//! padded, their outputs paired, duplicated, orphaned, out of place or ahead
//! of the call, as text, as parts with images or as JSON text holding them,
//! tools declared at the top level, in namespaces and in `additional_tools`
//! items with names around the 64-byte limit, and every shape of
//! `tool_choice`, `text.format`, token limit and reasoning setting.
//!
//! Custom tool call input is never an object or array here: upstream copies
//! its JSON text into the call's arguments, and we write it compactly, which
//! the comparison can't see through (see the hand-written cases).
//!
//! Responses are the Chat Completions generator's ([`super::openai_chat`])
//! answering such a request, with calls to its tools by the names the
//! translated request gives them, by the names the client wrote and by names
//! neither declares. Calls to custom tools carry `{"input": ...}` arguments,
//! and calls to `apply_patch` a patch, whole, cut short or not a patch at
//! all. The client's request is sometimes missing or not JSON, and the
//! translated one missing, translated from it, or a few fields that the
//! response repeats. Neither holds a negative zero or a number too large for
//! `int64`, which upstream writes as no JSON encoder would.

use std::ops::{Deref, DerefMut};

use open_ferry_translate::openai::responses::convert_openai_responses_request_to_openai_chat_completions;
use serde_json::{Value, json};

use super::claude_responses::qualify;
use super::openai_chat::Call;
use super::{EFFORTS, Rng, escape_text, num};
use crate::cases::Case;

/// Builds `count` random Responses requests for a Chat Completions upstream.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let model = generator.rng.pick(MODELS);
            let request = generator.request();
            let text = generator.render(&request);
            Case::new(format!("random-{seed}-{index}"), model, text)
        })
        .collect()
}

/// Builds `count` random Chat Completions streams answering random Responses
/// requests, and a non-streaming case from a whole response for each.
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed.rotate_left(13), index);
            generator.whole_cuts = true;
            let model = generator.rng.pick(MODELS);
            let mut original = generator.request();
            if generator.rng.chance(15) {
                // Declares apply_patch more often than the request generator
                // does, so its calls stream often enough.
                declare_apply_patch(&mut original);
                generator.tool_names.push(json!("apply_patch"));
            }
            tame_numbers(&mut original);
            let calls = calls(&original, &generator.tool_names);
            let (original_text, translated) = generator.requests(&original, model);
            let mut chat = super::openai_chat::Generator::new(seed.rotate_left(43), index);
            let lines = chat.stream(&calls);
            let body = chat.body(&calls);
            let case = |events| Case {
                model: model.to_owned(),
                translated_request: translated.clone(),
                ..Case::response(format!("random-{seed}-{index}"), &original_text, events)
            };
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Model names, which the translator only copies.
const MODELS: &[&str] = &[
    "gpt-4o",
    "gpt-4o",
    "gpt-5",
    "deepseek-chat",
    "qwen3-coder-plus",
    "",
    " Model ",
];

/// Function call arguments: objects, other JSON, broken JSON and nothing.
const ARGUMENTS: &[&str] = &[
    r#"{"city":"Paris"}"#,
    r#"{"path":"C:\\temp\\a.txt","lines":[1,2]}"#,
    r#"{"q":"café 🚀","n":1.50}"#,
    "{ \"spaced\" : true }",
    "{}",
    "",
    "[1,2]",
    "5",
    r#"{"a":"#,
    "not json",
];

/// A patch as `apply_patch` takes it.
const PATCH: &str = "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch";

/// Custom tool input, with characters sjson writes as they are (`<`, `&`)
/// and ones that make it use Go's encoder.
const CUSTOM_INPUTS: &[&str] = &[
    "ls -la",
    "echo '<b>' && cat a > b",
    "café 🚀",
    "quote \" backslash \\",
    "line\nbreak\ttab",
    "\u{2028}separators\u{2029}",
    "\u{7f}del \u{1}control",
    "{\"looks\":\"like json\"}",
    "",
];

/// The arguments of a call to a custom tool: its input wrapped in an object,
/// of other types, missing, or not wrapped.
const CUSTOM_ARGUMENTS: &[&str] = &[
    r#"{"input":"ls -la"}"#,
    r#"{"input":"echo '<b>' && cat a > b"}"#,
    r#"{"input":"café 🚀"}"#,
    r#"{"input":"line\nbreak\ttab \"quoted\" \\"}"#,
    r#"{"input":""}"#,
    r#"{"input":5}"#,
    r#"{"input":null}"#,
    r#"{"input":{"b":1,"a":[2]}}"#,
    r#"{"cmd":"ls"}"#,
    "ls -la",
    "",
];

/// The arguments of a call to `apply_patch`: patches whole, with escapes the
/// stream's decoder has to put back together, cut short, not patches, and not
/// wrapped as input.
const PATCH_ARGUMENTS: &[&str] = &[
    r#"{"input":"*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch"}"#,
    r#"{"input":"*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch\n"}"#,
    r#"{ "input" : "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch" }"#,
    concat!(
        r#"{"input":"*** Begin Patch\n*** Add File: caf"#,
        '\\',
        r#"u00e9.txt\n+"#,
        '\\',
        "ud83d",
        '\\',
        r#"ude80 \"quoted\" \\ back\/slash\n*** End Patch"}"#
    ),
    r#"{"input":"*** Begin Patch\n*** Add File: cut.txt\n+no end"}"#,
    r#"{"input":"*** Begin Patch"#,
    r#"{"input":"not a patch"}"#,
    r#"{"input":"*** Begin Patch\n*** Add File: a\n+x\n*** End Patch","extra":1}"#,
    r#"{"input":5}"#,
    r#"{"input":""}"#,
    r#"{"other":"x"}"#,
    "not json",
    "",
];

/// Reasoning text, from a small pool so repeats are common, with the
/// placeholder upstream writes for reasoning it can't show.
const REASONING: &[&str] = &[
    "Thought.",
    "Thought.",
    " Thought. ",
    "Checking the weather first.",
    "Line one.\nLine two.",
    "[reasoning unavailable]",
    " [reasoning unavailable] ",
    "",
    "  ",
];

const IMAGE_URLS: &[&str] = &[
    "https://example.com/a.png",
    "data:image/png;base64,iVBORw0KGgo=",
    "file_abc",
];

const VIDEO_URLS: &[&str] = &[
    "https://example.com/clip.mp4",
    "data:video/mp4;base64,AAAAIGZ0eXA=",
];

/// The request generator, for its leaf values: text, numbers, tool names and
/// schemas.
struct Generator {
    base: super::Generator,
    /// Namespaces declared in `tools`, for calls and `tool_choice` to use.
    namespaces: Vec<String>,
    /// Whether to lengthen a namespace child's name where cutting its full
    /// name to 64 bytes would split a character. Upstream cuts the bytes and
    /// we cut at the character (see UPSTREAM.md), and the responses name
    /// their calls by the request's translation, so in a response suite the
    /// two would read every call to such a tool differently.
    whole_cuts: bool,
}

impl Deref for Generator {
    type Target = super::Generator;

    fn deref(&self) -> &super::Generator {
        &self.base
    }
}

impl DerefMut for Generator {
    fn deref_mut(&mut self) -> &mut super::Generator {
        &mut self.base
    }
}

impl Generator {
    /// The client's request as JSON text, and the request as translated for a
    /// Chat Completions upstream, for `original`. Upstream reads the client's,
    /// unless it is missing or not JSON.
    fn requests(&mut self, original: &Value, model: &str) -> (String, String) {
        let original_text = match self.rng.below(100) {
            // Absent. Not "null": upstream would take that as a request.
            0..=14 => String::new(),
            15 | 16 => "{not json".to_owned(),
            17 => "[]".to_owned(),
            _ => self.render(original),
        };
        let translated = match self.rng.below(10) {
            0..=2 => return (original_text, String::new()),
            3..=6 => {
                convert_openai_responses_request_to_openai_chat_completions(model, original, true)
            }
            _ => self.echoed_fields(),
        };
        let mut translated = translated;
        tame_numbers(&mut translated);
        (original_text, translated.to_string())
    }

    /// A translated request with the fields a response repeats, loosely
    /// typed, and the `max_tokens` that stands in for `max_output_tokens`.
    fn echoed_fields(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(80) {
            let model = self.loose_choice(&["gpt-4o", "", " "]);
            fields.push(("model", model));
        }
        if self.rng.chance(40) {
            fields.push(("max_tokens", self.token_limit()));
        }
        for key in [
            "instructions",
            "max_output_tokens",
            "max_tool_calls",
            "parallel_tool_calls",
            "previous_response_id",
            "prompt_cache_key",
            "reasoning",
            "safety_identifier",
            "service_tier",
            "store",
            "temperature",
            "text",
            "tool_choice",
            "top_logprobs",
            "top_p",
            "truncation",
            "user",
            "metadata",
        ] {
            if !self.rng.chance(20) {
                continue;
            }
            let value = match key {
                "max_output_tokens" | "max_tool_calls" | "top_logprobs" => self.token_limit(),
                "parallel_tool_calls" | "store" => self.bool_like(),
                "reasoning" => self.reasoning(),
                "temperature" | "top_p" => self.number(),
                "text" => self.text_config(),
                "tool_choice" => self.tool_choice(),
                "metadata" => self.one_of(&[json!({ "k": "v" }), json!({}), json!("m")]),
                "service_tier" => self.loose_choice(&["auto", "flex", "priority", ""]),
                "truncation" => self.loose_choice(&["auto", "disabled"]),
                _ => self.loose_text(),
            };
            fields.push((key, value));
        }
        self.object(fields)
    }

    fn new(seed: u64, index: u64) -> Self {
        Self {
            base: super::Generator {
                // A different mix from the other generators', so cases don't
                // share their random choices.
                rng: Rng(seed.rotate_left(29) ^ index.wrapping_mul(0x9FB2_1C65_1E98_DF25)),
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
            },
            namespaces: Vec::new(),
            whole_cuts: false,
        }
    }

    // --- Requests ---

    fn request(&mut self) -> Value {
        let mut fields = Vec::new();
        // Tools and input come first so calls and tool_choice can use their names.
        if self.rng.chance(60) {
            fields.push(("tools", self.tools()));
        }
        if self.rng.chance(95) {
            fields.push(("input", self.input()));
        }
        if self.rng.chance(80) {
            let model = self.loose_choice(&["gpt-4o", "gpt-5", "", " "]);
            fields.push(("model", model));
        }
        if self.rng.chance(35) {
            let instructions = if self.rng.chance(90) {
                self.text().into()
            } else {
                self.one_of(&[
                    json!(5),
                    Value::Null,
                    json!(["rules"]),
                    json!({ "text": "x" }),
                ])
            };
            fields.push(("instructions", instructions));
        }
        if self.rng.chance(35) {
            fields.push(("tool_choice", self.tool_choice()));
        }
        if self.rng.chance(30) {
            fields.push(("parallel_tool_calls", self.bool_like()));
        }
        if self.rng.chance(40) {
            fields.push(("reasoning", self.reasoning()));
        }
        if self.rng.chance(10) {
            let effort = self.effort();
            fields.push(("reasoning_effort", effort));
        }
        if self.rng.chance(35) {
            fields.push(("max_output_tokens", self.token_limit()));
        }
        if self.rng.chance(30) {
            fields.push(("text", self.text_config()));
        }
        if self.rng.chance(20) {
            fields.push(("stream", self.rng.chance(50).into()));
        }
        if self.rng.chance(10) {
            fields.push(("store", self.bool_like()));
        }
        if self.rng.chance(10) {
            fields.push(("temperature", self.number()));
        }
        self.rng.shuffle(&mut fields);
        super::to_object(fields)
    }

    // --- Tools ---

    fn tools(&mut self) -> Value {
        if self.rng.chance(4) {
            return self.one_of(&[json!({}), json!("tools"), Value::Null]);
        }
        let count = self.rng.below(5);
        Value::Array((0..count).map(|_| self.tool()).collect())
    }

    fn tool(&mut self) -> Value {
        match self.rng.below(24) {
            0..=8 => self.function_tool(),
            9 | 10 => self.custom_tool(),
            11 => self.apply_patch_tool(),
            12..=15 => self.namespace_tool(),
            16 => {
                let tool_type = self.rng.pick(&[
                    "web_search",
                    "file_search",
                    "image_generation",
                    "code_interpreter",
                ]);
                json!({ "type": tool_type, "name": "builtin" })
            }
            17 => {
                // Types that aren't functions however they're written.
                let tool_type = self.rng.pick(&["mcp", "Function", "CUSTOM", "local_shell"]);
                let name = self.declared_name();
                json!({ "type": tool_type, "name": name })
            }
            18 | 19 => {
                // Chat Completions style, which upstream also reads.
                let name = self.declared_name();
                let mut function = vec![("name", name)];
                if self.rng.chance(60) {
                    function.push(("description", self.loose_text()));
                }
                if self.rng.chance(75) {
                    let key = self.rng.pick(&["parameters", "parametersJsonSchema"]);
                    function.push((key, self.schema(0)));
                }
                let function = self.object(function);
                let mut fields = vec![("type", json!("function")), ("function", function)];
                if self.rng.chance(15) {
                    // A top-level name wins over the function's.
                    let name = self.declared_name();
                    fields.push(("name", name));
                }
                self.object(fields)
            }
            20 => {
                let name = self.declared_name();
                let schema = self.schema(0);
                json!({ "name": name, "parameters": schema })
            }
            _ => self.one_of(&[json!(1), json!("Bash"), Value::Null, json!([])]),
        }
    }

    /// A tool name, remembered for calls and `tool_choice` to use.
    fn declared_name(&mut self) -> Value {
        let name = self.tool_name();
        self.tool_names.push(name.clone());
        name
    }

    fn function_tool(&mut self) -> Value {
        let mut fields = Vec::new();
        match self.rng.below(20) {
            0..=15 => fields.push(("type", json!("function"))),
            16 => fields.push(("type", json!(" function "))),
            17 => fields.push(("type", json!(""))),
            _ => {}
        }
        if self.rng.chance(94) {
            let name = self.declared_name();
            fields.push(("name", name));
        }
        if self.rng.chance(60) {
            fields.push(("description", self.loose_text()));
        }
        if self.rng.chance(85) {
            let key = match self.rng.below(10) {
                0..=6 => "parameters",
                7 => "parametersJsonSchema",
                8 => "input_schema",
                _ => "schema",
            };
            let schema = if self.rng.chance(90) {
                self.schema(0)
            } else {
                self.one_of(&[Value::Null, json!("schema"), json!([{ "type": "object" }])])
            };
            fields.push((key, schema));
        }
        if self.rng.chance(20) {
            fields.push(("strict", self.bool_like()));
        }
        self.object(fields)
    }

    fn custom_tool(&mut self) -> Value {
        let tool_type = if self.rng.chance(90) {
            "custom"
        } else {
            " custom "
        };
        let mut fields = vec![("type", json!(tool_type))];
        if self.rng.chance(95) {
            let name = self.declared_name();
            fields.push(("name", name));
        }
        if self.rng.chance(60) {
            fields.push(("description", self.text().into()));
        }
        if self.rng.chance(60) {
            let format = self.one_of(&[
                json!({ "type": "grammar", "syntax": "lark", "definition": "start: /.+/" }),
                json!({ "type": "grammar", "syntax": "regex", "definition": "^[a-z]+$" }),
                json!({ "type": "text" }),
            ]);
            fields.push(("format", format));
        }
        self.object(fields)
    }

    fn apply_patch_tool(&mut self) -> Value {
        let name = self.one_of(&[
            json!("apply_patch"),
            json!("apply_patch"),
            json!(" apply_patch "),
        ]);
        self.tool_names.push(json!("apply_patch"));
        let mut fields = vec![("type", json!("custom")), ("name", name)];
        if self.rng.chance(70) {
            fields.push((
                "description",
                json!("Use the `apply_patch` tool to edit files."),
            ));
        }
        if self.rng.chance(70) {
            let format = json!({
                "type": "grammar",
                "syntax": "lark",
                "definition": "start: begin_patch hunk+ end_patch\nbegin_patch: \"*** Begin Patch\" LF"
            });
            fields.push(("format", format));
        }
        self.object(fields)
    }

    /// A namespace name: plain, already ending in the separator, padded,
    /// empty, long enough that children are cut, to the same name for two of
    /// them, or with two-byte characters where children are cut.
    fn namespace(&mut self) -> String {
        match self.rng.below(12) {
            0 | 1 => "mcp__github".into(),
            2 | 3 => "browser".into(),
            4 => "tools__".into(),
            5 => " spaced ".into(),
            6 => "".into(),
            7 => "mcp__".into(),
            8 => "n".repeat(70),
            9 => format!("m{}", "n".repeat(70)),
            10 => "é".repeat(40),
            _ => "namespace_with_a_rather_long_name_for_shortening".into(),
        }
    }

    fn namespace_tool(&mut self) -> Value {
        let namespace = self.namespace();
        self.namespaces.push(namespace.clone());
        let namespace_type = if self.rng.chance(90) {
            "namespace"
        } else {
            " namespace "
        };
        let mut fields = vec![("type", json!(namespace_type))];
        if self.rng.chance(95) {
            fields.push(("name", json!(namespace)));
        }
        if self.rng.chance(50) {
            fields.push(("description", self.text().into()));
        }
        if self.rng.chance(95) {
            let children = if self.rng.chance(5) {
                self.one_of(&[json!({}), Value::Null, json!("tools")])
            } else {
                let count = self.rng.below(4);
                Value::Array(
                    (0..count)
                        .map(|_| self.namespace_child(namespace.trim()))
                        .collect(),
                )
            };
            fields.push(("tools", children));
        }
        self.object(fields)
    }

    fn namespace_child(&mut self, namespace: &str) -> Value {
        let mut child = match self.rng.below(12) {
            0 => format!("mcp__{}", self.alphanumeric(4)),
            // Already qualified, or the namespace itself.
            1 if !namespace.is_empty() => format!("{namespace}__read"),
            2 if !namespace.is_empty() => namespace.to_owned(),
            3 => " padded ".to_owned(),
            // Names other namespaces' children share.
            4 | 5 => self.rng.pick(&["read", "search", "read_file"]).to_owned(),
            _ => match self.tool_name() {
                Value::String(name) => name,
                _ => "read".to_owned(),
            },
        };
        while self.whole_cuts && {
            let full = qualify(namespace, &child);
            full.len() > 64 && !full.is_char_boundary(full.len() - 64)
        } {
            child.push('x');
        }
        // Calls name a child by its full name, or by its own.
        self.tool_names.push(qualify(namespace, &child).into());
        self.tool_names.push(child.trim().into());
        let mut fields = Vec::new();
        match self.rng.below(10) {
            0..=5 => fields.push(("type", json!("function"))),
            6 => fields.push(("type", json!("custom"))),
            7 => fields.push(("type", json!("web_search"))),
            _ => {}
        }
        fields.push(("name", json!(child)));
        if self.rng.chance(50) {
            fields.push(("description", self.text().into()));
        }
        if self.rng.chance(70) {
            fields.push(("parameters", self.schema(1)));
        }
        self.object(fields)
    }

    // --- Input ---

    fn input(&mut self) -> Value {
        match self.rng.below(100) {
            0..=2 => self.one_of(&[json!({}), Value::Null, json!(5)]),
            3..=11 => self.loose_text(),
            _ => {
                let mut items = Vec::new();
                for _ in 0..self.rng.below(10) {
                    self.push_items(&mut items);
                }
                Value::Array(items)
            }
        }
    }

    fn push_items(&mut self, items: &mut Vec<Value>) {
        let item = match self.rng.below(100) {
            0..=21 => self.message_item("user"),
            22..=35 => self.message_item("assistant"),
            36..=40 => {
                let role = self.rng.pick(&["system", "developer"]);
                self.message_item(role)
            }
            41..=50 => self.reasoning_item(),
            51..=69 => return self.tool_calls(items),
            70..=78 => {
                let id = self.pending_call_id();
                self.tool_output(id)
            }
            79..=82 => self.additional_tools(),
            83..=88 => self.odd_message(),
            89..=94 => self.other_item(),
            _ => self.one_of(&[json!("hello"), Value::Null, json!(5), json!([])]),
        };
        items.push(item);
    }

    fn message_item(&mut self, role: &str) -> Value {
        let mut fields = Vec::new();
        match self.rng.below(20) {
            0..=15 => fields.push(("type", json!("message"))),
            16 => fields.push(("type", json!(""))),
            _ => {}
        }
        fields.push(("role", json!(role)));
        if self.rng.chance(97) {
            let content = self.content(role);
            fields.push(("content", content));
        }
        let reasoning_chance = if role == "assistant" { 30 } else { 3 };
        if self.rng.chance(reasoning_chance) {
            fields.push(("reasoning_content", self.reasoning_text()));
        }
        if self.rng.chance(10) {
            fields.push(("id", format!("msg_{}", self.alphanumeric(8)).into()));
        }
        if self.rng.chance(10) {
            fields.push(("status", json!("completed")));
        }
        self.object(fields)
    }

    fn content(&mut self, role: &str) -> Value {
        match self.rng.below(20) {
            0..=4 => self.text().into(),
            5 => self.one_of(&[Value::Null, json!(5), json!({ "text": "object" })]),
            6 => json!([]),
            _ => {
                let count = 1 + self.rng.below(3);
                Value::Array((0..count).map(|_| self.message_part(role)).collect())
            }
        }
    }

    fn message_part(&mut self, role: &str) -> Value {
        match self.rng.below(20) {
            0..=8 => {
                let part_type = match self.rng.below(10) {
                    0..=6 if role == "assistant" => Some("output_text"),
                    0..=6 => Some("input_text"),
                    7 => Some("output_text"),
                    8 => Some(""),
                    _ => None,
                };
                let mut fields = Vec::new();
                if let Some(part_type) = part_type {
                    fields.push(("type", json!(part_type)));
                }
                if self.rng.chance(95) {
                    fields.push(("text", self.loose_text()));
                }
                if self.rng.chance(10) {
                    fields.push(("annotations", json!([])));
                }
                self.object(fields)
            }
            9..=11 => self.input_image(),
            12 | 13 => self.video_part(),
            14 => json!({ "type": "refusal", "refusal": "No." }),
            15 => json!({ "type": "input_file", "file_id": "file_1", "filename": "a.pdf" }),
            16 => {
                json!({ "type": "input_audio", "input_audio": { "data": "AAAA", "format": "wav" } })
            }
            _ => self.one_of(&[json!("bare string"), json!(5), Value::Null, json!({})]),
        }
    }

    fn input_image(&mut self) -> Value {
        let mut fields = vec![("type", json!("input_image"))];
        if self.rng.chance(92) {
            let url = if self.rng.chance(90) {
                json!(self.rng.pick(IMAGE_URLS))
            } else {
                self.one_of(&[json!(""), json!(5), json!({ "url": "https://x" })])
            };
            fields.push(("image_url", url));
        }
        if let Some(detail) = self.detail() {
            fields.push(("detail", detail));
        }
        self.object(fields)
    }

    /// An image detail: one Chat Completions accepts, `original`, which it
    /// doesn't, written in any case or padded, unknown, or not a string.
    fn detail(&mut self) -> Option<Value> {
        if self.rng.chance(40) {
            return None;
        }
        Some(match self.rng.below(12) {
            0 | 1 => json!("auto"),
            2 => json!("low"),
            3 => json!("high"),
            4 | 5 => json!("original"),
            6 => json!("HIGH"),
            7 => json!(" Low "),
            8 => json!("ORİGİNAL"),
            9 => json!("bogus"),
            10 => json!(""),
            _ => self.one_of(&[json!(5), Value::Null, json!(true)]),
        })
    }

    fn video_part(&mut self) -> Value {
        let part_type = self.rng.pick(&["input_video", "video_url"]);
        let mut fields = vec![("type", json!(part_type))];
        match self.rng.below(10) {
            0..=3 => fields.push(("video_url", json!(self.rng.pick(VIDEO_URLS)))),
            4..=6 => {
                let mut video = vec![("url", json!(self.rng.pick(VIDEO_URLS)))];
                if self.rng.chance(30) {
                    video.push(("processing", json!({ "fps": 1 })));
                }
                let video = self.object(video);
                fields.push(("video_url", video));
            }
            7 => {
                let video = self.one_of(&[json!(5), Value::Null, json!(["a"])]);
                fields.push(("video_url", video));
            }
            _ => {}
        }
        if self.rng.chance(30) {
            let processing = self.one_of(&[
                json!({ "fps": 2 }),
                json!({ "fps": 1.50, "start": "0s" }),
                json!("auto"),
                Value::Null,
            ]);
            fields.push(("processing", processing));
        }
        self.object(fields)
    }

    /// Reasoning text, or now and then a value that isn't a string.
    fn reasoning_text(&mut self) -> Value {
        if self.rng.chance(85) {
            json!(self.rng.pick(REASONING))
        } else {
            self.loose_text()
        }
    }

    fn reasoning_item(&mut self) -> Value {
        let mut fields = vec![("type", json!("reasoning"))];
        if self.rng.chance(60) {
            fields.push(("id", format!("rs_{}", self.alphanumeric(8)).into()));
        }
        if self.rng.chance(85) {
            let summary = if self.rng.chance(92) {
                let count = self.rng.below(4);
                Value::Array((0..count).map(|_| self.summary_part()).collect())
            } else {
                self.one_of(&[Value::Null, json!("summary"), json!({})])
            };
            fields.push(("summary", summary));
        }
        if self.rng.chance(40) {
            fields.push(("encrypted_content", json!("gAAAAABencrypted")));
        }
        if self.rng.chance(15) {
            fields.push((
                "content",
                json!([{ "type": "reasoning_text", "text": "Raw." }]),
            ));
        }
        self.object(fields)
    }

    fn summary_part(&mut self) -> Value {
        match self.rng.below(10) {
            0..=6 => {
                let text = self.reasoning_text();
                json!({ "type": "summary_text", "text": text })
            }
            7 => json!({ "type": "summary_text" }),
            8 => json!({ "type": "reasoning_text", "text": "Other." }),
            _ => self.one_of(&[json!("bare"), Value::Null]),
        }
    }

    fn tool_calls(&mut self, items: &mut Vec<Value>) {
        let count = 1 + self.rng.below(3);
        let mut ids = Vec::new();
        if self.rng.chance(5) {
            // An output that comes before its call.
            let id = format!("call_{}", self.alphanumeric(12));
            ids.push(json!(id.clone()));
            items.push(self.tool_output(Some(json!(id.clone()))));
            items.push(self.tool_call(Some(("call_id", json!(id)))));
        }
        for _ in 0..count {
            let id = self.call_id();
            if let Some((_, id)) = &id {
                ids.push(id.clone());
            }
            items.push(self.tool_call(id));
        }
        if self.rng.chance(35) {
            // Answered later, or never.
            self.tool_use_ids.extend(ids);
            return;
        }
        if self.rng.chance(15) {
            // Something between the calls and their outputs, for the outputs
            // to be moved back past.
            let role = self.rng.pick(&["user", "assistant", "developer"]);
            items.push(self.message_item(role));
        }
        match self.rng.below(10) {
            0..=4 => {}
            5 | 6 => self.rng.shuffle(&mut ids),
            7 => {
                ids.pop();
            }
            _ => {
                if let Some(first) = ids.first().cloned() {
                    ids.push(first);
                }
            }
        }
        if self.rng.chance(10) {
            ids.push(json!("call_unknown"));
        }
        for id in ids {
            items.push(self.tool_output(Some(id)));
            if self.rng.chance(5) {
                items.push(self.message_item("user"));
            }
        }
        if self.rng.chance(8) {
            // Outputs with no ID, paired by name or order, or not at all.
            for _ in 0..1 + self.rng.below(2) {
                items.push(self.tool_output(None));
            }
        }
    }

    /// A call ID under one of the keys upstream reads it from.
    fn call_id(&mut self) -> Option<(&'static str, Value)> {
        let key = match self.rng.below(20) {
            0..=15 => "call_id",
            16 => "tool_call_id",
            17 => "callId",
            18 => "id",
            _ => return None,
        };
        let id = match self.rng.below(20) {
            0..=12 => format!("call_{}", self.alphanumeric(24)).into(),
            13 => "".into(),
            14 => " call_padded ".into(),
            15 => match self.tool_use_ids.last() {
                Some(id) => id.clone(),
                None => "call_same".into(),
            },
            16 if key == "id" => format!("fco_{}", self.alphanumeric(8)).into(),
            17 => "call.1:é/x y".into(),
            _ => num(self.rng.pick(&["12345", "1.50"])),
        };
        Some((key, id))
    }

    fn tool_call(&mut self, id: Option<(&'static str, Value)>) -> Value {
        let custom = self.rng.chance(25);
        let item_type = if custom {
            "custom_tool_call"
        } else {
            "function_call"
        };
        let mut fields = vec![("type", json!(item_type))];
        if let Some((key, id)) = id {
            fields.push((key, id));
        }
        if self.rng.chance(20) && !fields.iter().any(|(key, _)| *key == "id") {
            let id = format!("fc_{}", self.alphanumeric(10));
            fields.push(("id", id.into()));
        }
        let name = if !self.tool_names.is_empty() && self.rng.chance(75) {
            self.base.rng.pick(&self.base.tool_names)
        } else {
            self.tool_name()
        };
        if self.rng.chance(97) {
            fields.push(("name", name));
        }
        if self.rng.chance(15) {
            let namespace = self.choice_namespace();
            fields.push(("namespace", namespace));
        }
        if custom {
            let input = match self.rng.below(10) {
                0..=2 => json!(PATCH),
                3..=7 => json!(self.rng.pick(CUSTOM_INPUTS)),
                8 => self.text().into(),
                _ => self.one_of(&[json!(5), json!(true), Value::Null]),
            };
            if self.rng.chance(95) {
                fields.push(("input", input));
            }
        } else if self.rng.chance(95) {
            let arguments = if self.rng.chance(90) {
                json!(self.rng.pick(ARGUMENTS))
            } else {
                self.one_of(&[json!({ "city": "Paris" }), json!(5), Value::Null])
            };
            fields.push(("arguments", arguments));
        }
        if self.rng.chance(8) {
            fields.push(("reasoning_content", self.reasoning_text()));
        }
        if self.rng.chance(15) {
            fields.push(("status", json!("completed")));
        }
        self.object(fields)
    }

    /// The ID of a call not yet answered, or none.
    fn pending_call_id(&mut self) -> Option<Value> {
        if self.tool_use_ids.is_empty() || self.rng.chance(20) {
            return self.rng.chance(50).then(|| json!("call_orphan"));
        }
        let pending = self.tool_use_ids.len();
        let at = self.rng.below(pending);
        Some(self.tool_use_ids.remove(at))
    }

    fn tool_output(&mut self, id: Option<Value>) -> Value {
        let item_type = if self.rng.chance(80) {
            "function_call_output"
        } else {
            "custom_tool_call_output"
        };
        let mut fields = vec![("type", json!(item_type))];
        if let Some(id) = id {
            let key = match self.rng.below(20) {
                0..=16 => "call_id",
                17 => "tool_call_id",
                18 => "callId",
                _ => "id",
            };
            fields.push((key, id));
        } else if self.rng.chance(50) && !self.tool_names.is_empty() {
            let name = self.base.rng.pick(&self.base.tool_names);
            fields.push(("name", name));
        }
        if self.rng.chance(95) {
            let output = self.output();
            fields.push(("output", output));
        }
        if self.rng.chance(10) {
            fields.push(("id", format!("fco_{}", self.alphanumeric(8)).into()));
        }
        self.object(fields)
    }

    fn output(&mut self) -> Value {
        match self.rng.below(24) {
            0..=6 => self.text().into(),
            7 => json!(""),
            8 => json!("  "),
            9..=14 => self.output_parts(),
            15..=18 => {
                // The parts as JSON text, as some clients send them.
                let parts = self.output_parts();
                let text = if self.rng.chance(30) {
                    serde_json::to_string_pretty(&parts)
                } else {
                    serde_json::to_string(&parts)
                }
                .expect("a Value always serializes");
                if self.rng.chance(20) {
                    escape_text(&text).into()
                } else {
                    text.into()
                }
            }
            19 => json!({ "result": 1.50, "ok": true }),
            20 => json!(5),
            21 => json!(r#"[{"type":"input_image","image_url":"https://example.com/a.png""#),
            22 => json!([]),
            _ => Value::Null,
        }
    }

    fn output_parts(&mut self) -> Value {
        let count = self.rng.below(4);
        Value::Array((0..count).map(|_| self.output_part()).collect())
    }

    fn output_part(&mut self) -> Value {
        match self.rng.below(20) {
            0..=5 => {
                let part_type = self.rng.pick(&["input_text", "output_text", "text"]);
                let text = if self.rng.chance(90) {
                    self.text().into()
                } else {
                    self.one_of(&[json!(5), Value::Null, json!({ "a": "b" })])
                };
                json!({ "type": part_type, "text": text })
            }
            6..=12 => self.output_image(),
            13 => json!({ "type": "input_file", "file_id": "file_1" }),
            14 => json!({ "type": "text" }),
            15 => json!("plain string part"),
            16 => json!({ "text": "no type" }),
            17 => json!({ "type": 5, "text": "odd type" }),
            _ => self.one_of(&[json!(5), Value::Null, json!({ "b": 1.50, "a": [] })]),
        }
    }

    /// An image part as a tool output has it, in Responses or Chat
    /// Completions form, its URL sometimes padded, empty or missing.
    fn output_image(&mut self) -> Value {
        let url = match self.rng.below(10) {
            0..=6 => json!(self.rng.pick(IMAGE_URLS)),
            7 => json!(" https://example.com/padded.png "),
            8 => self.one_of(&[json!(""), json!("   ")]),
            _ => self.one_of(&[json!(5), Value::Null, json!({ "url": "x" })]),
        };
        let has_url = self.rng.chance(95);
        let detail = self.detail();
        if self.rng.chance(50) {
            let mut fields = vec![("type", json!("input_image"))];
            if has_url {
                fields.push(("image_url", url));
            }
            if let Some(detail) = detail {
                fields.push(("detail", detail));
            }
            self.object(fields)
        } else {
            let mut image = Vec::new();
            if has_url {
                image.push(("url", url));
            }
            if let Some(detail) = detail {
                image.push(("detail", detail));
            }
            let image = self.object(image);
            json!({ "type": "image_url", "image_url": image })
        }
    }

    /// Tools declared in an input item, as Responses Lite sends them.
    fn additional_tools(&mut self) -> Value {
        let tools = if self.rng.chance(90) {
            let count = 1 + self.rng.below(3);
            Value::Array((0..count).map(|_| self.tool()).collect())
        } else {
            self.one_of(&[Value::Null, json!({}), json!("tools")])
        };
        json!({ "type": "additional_tools", "tools": tools })
    }

    /// A message with no type, an odd role, or both.
    fn odd_message(&mut self) -> Value {
        let role = self.one_of(&[
            json!("tool"),
            json!("USER"),
            json!("Assistant"),
            json!(" assistant "),
            json!(""),
            json!(5),
            Value::Null,
        ]);
        let mut fields = Vec::new();
        if self.rng.chance(40) {
            fields.push(("type", json!("message")));
        }
        if self.rng.chance(85) {
            fields.push(("role", role));
        }
        let content = self.content("user");
        fields.push(("content", content));
        self.object(fields)
    }

    /// Items with no Chat Completions counterpart.
    fn other_item(&mut self) -> Value {
        self.one_of(&[
            json!({ "type": "item_reference", "id": "msg_1" }),
            json!({ "type": "web_search_call", "id": "ws_1", "status": "completed" }),
            json!({ "type": "local_shell_call", "call_id": "call_shell", "action": { "command": ["ls"] } }),
            json!({ "type": "compaction", "encrypted_content": "abc" }),
            json!({ "type": "", "content": "no role" }),
            json!({ "content": "neither type nor role" }),
        ])
    }

    // --- Other request fields ---

    fn tool_choice(&mut self) -> Value {
        match self.rng.below(12) {
            0..=2 => self.loose_choice(&["auto", "required", "none", " auto ", "AUTO", ""]),
            3..=6 => {
                let name = self.choice_name();
                match self.rng.below(5) {
                    0 | 1 => json!({ "type": "function", "name": name }),
                    2 => json!({ "type": "function", "function": { "name": name } }),
                    3 => {
                        let namespace = self.choice_namespace();
                        json!({ "type": "function", "name": name, "namespace": namespace })
                    }
                    _ => {
                        let namespace = self.choice_namespace();
                        json!({ "type": "function", "function": { "name": name, "namespace": namespace } })
                    }
                }
            }
            7 => {
                let name = self.choice_name();
                match self.rng.below(3) {
                    0 => json!({ "type": "custom", "name": name }),
                    1 => json!({ "type": "custom", "custom": { "name": name } }),
                    _ => {
                        let namespace = self.choice_namespace();
                        json!({ "type": "custom", "custom": { "name": name, "namespace": namespace } })
                    }
                }
            }
            8 => {
                let name = self.choice_name();
                json!({ "type": "allowed_tools", "mode": "required", "tools": [{ "type": "function", "name": name }] })
            }
            9 => self.one_of(&[
                json!({ "type": "web_search" }),
                json!({ "type": "function" }),
                json!({ "type": "function", "name": "" }),
            ]),
            10 => self.one_of(&[
                json!({ "type": "auto" }),
                json!({ "type": " function ", "name": "search" }),
                json!({}),
            ]),
            _ => self.one_of(&[Value::Null, json!(5), json!(["auto"])]),
        }
    }

    fn choice_name(&mut self) -> Value {
        let name = if !self.tool_names.is_empty() && self.rng.chance(75) {
            self.base.rng.pick(&self.base.tool_names)
        } else {
            self.tool_name()
        };
        match name {
            Value::String(text) if self.rng.chance(10) => format!(" {text} ").into(),
            name => name,
        }
    }

    fn choice_namespace(&mut self) -> Value {
        if !self.namespaces.is_empty() && self.rng.chance(70) {
            json!(self.base.rng.pick(&self.namespaces))
        } else {
            let namespace = self.namespace();
            json!(namespace)
        }
    }

    fn reasoning(&mut self) -> Value {
        if self.rng.chance(10) {
            return self.one_of(&[
                json!("high"),
                json!("none"),
                json!(" FALSE "),
                Value::Null,
                json!([]),
                json!({}),
            ]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(75) {
            fields.push(("effort", self.effort()));
        }
        if self.rng.chance(35) {
            let summary = self.loose_choice(&["auto", "concise", "detailed", "none"]);
            fields.push(("summary", summary));
        }
        self.object(fields)
    }

    fn effort(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => self.loose_choice(&["none", "minimal", "low", "medium", "high", "xhigh"]),
            6 | 7 => self.loose_choice(&[" High ", "0", "false", "", "  ", " NONE "]),
            _ => self.loose_choice(EFFORTS),
        }
    }

    /// A token limit as the client wrote it, which is copied as it is.
    fn token_limit(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => self.one_of(&[json!(1024), json!(64000), json!(1), json!(0), json!(-5)]),
            6 | 7 => self.number(),
            _ => self.one_of(&[
                json!("2048"),
                Value::Null,
                json!(true),
                json!({ "b": 1, "a": 2.50 }),
            ]),
        }
    }

    fn text_config(&mut self) -> Value {
        match self.rng.below(10) {
            0..=6 => {
                let format = self.text_format();
                let mut fields = vec![("format", format)];
                if self.rng.chance(20) {
                    fields.push(("verbosity", json!("low")));
                }
                self.object(fields)
            }
            7 => json!({ "verbosity": "high" }),
            _ => self.one_of(&[json!("json"), Value::Null, json!([])]),
        }
    }

    fn text_format(&mut self) -> Value {
        match self.rng.below(12) {
            0 | 1 => json!({ "type": "text" }),
            2 | 3 => json!({ "type": "json_object" }),
            4..=8 => {
                let mut fields = vec![("type", json!("json_schema"))];
                if self.rng.chance(85) {
                    let name = self.loose_choice(&["weather", "answer", "名前", ""]);
                    fields.push(("name", name));
                }
                if self.rng.chance(40) {
                    fields.push(("description", self.loose_text()));
                }
                if self.rng.chance(50) {
                    fields.push(("strict", self.bool_like()));
                }
                if self.rng.chance(90) {
                    let schema = if self.rng.chance(85) {
                        self.schema(0)
                    } else {
                        self.one_of(&[json!("schema"), json!([]), Value::Null])
                    };
                    fields.push(("schema", schema));
                }
                self.object(fields)
            }
            9 => json!({ "type": "grammar", "grammar": "start: /.+/" }),
            10 => self.one_of(&[
                json!({ "type": 5 }),
                json!({}),
                json!({ "type": "JSON_OBJECT" }),
            ]),
            _ => self.one_of(&[json!("json_object"), Value::Null, json!([])]),
        }
    }
}

/// Adds the `apply_patch` custom tool to a request's tools.
fn declare_apply_patch(request: &mut Value) {
    let Value::Object(fields) = request else {
        return;
    };
    let tool = json!({ "type": "custom", "name": "apply_patch", "description": "Edit files." });
    match fields.get_mut("tools") {
        Some(Value::Array(tools)) => tools.push(tool),
        _ => {
            fields.insert("tools".into(), json!([tool]));
        }
    }
}

/// Replaces negative zeros, and numbers too large for `int64`, with numbers
/// of their own: upstream repeats some request fields as Go writes them, a
/// negative zero as `-0`, and reads a count beyond `int64` by the CPU's
/// rules (see UPSTREAM.md).
fn tame_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            let float = number.as_f64().unwrap_or(0.0);
            if float == 0.0 && number.to_string().starts_with('-') {
                *value = json!(0);
            } else if float.abs() >= 9.2e18 {
                *value = json!(7);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(tame_numbers),
        Value::Object(fields) => fields.values_mut().for_each(tame_numbers),
        _ => {}
    }
}

/// The calls a model might make to a request's tools: by the names the
/// translated request gives them, by the names the client wrote (`declared`)
/// and by a name neither declares, each with arguments of the kind its tool
/// takes.
fn calls(request: &Value, declared: &[Value]) -> Vec<Call> {
    let translated =
        convert_openai_responses_request_to_openai_chat_completions("gpt-4o", request, true);
    let mut calls: Vec<Call> = Vec::new();
    if let Some(Value::Array(tools)) = translated.get("tools") {
        for tool in tools {
            let Some(name) = tool.pointer("/function/name").and_then(Value::as_str) else {
                continue;
            };
            let arguments = if name == "apply_patch" {
                PATCH_ARGUMENTS
            } else if tool.pointer("/function/parameters/required") == Some(&json!(["input"])) {
                CUSTOM_ARGUMENTS
            } else {
                super::openai_chat::ARGUMENTS
            };
            calls.push((name.to_owned(), arguments));
        }
    }
    for name in declared.iter().filter_map(Value::as_str) {
        calls.push((name.to_owned(), super::openai_chat::ARGUMENTS));
    }
    calls.push(("undeclared_tool".to_owned(), CUSTOM_ARGUMENTS));
    calls
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translator::Translator;

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        let first = request_cases(7, 200);
        let second = request_cases(7, 200);
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.model, b.model);
            serde_json::from_str::<Value>(&a.request).expect("generated request is valid JSON");
        }
    }

    #[test]
    fn request_cases_cover_the_translators_branches() {
        let cases = request_cases(1, 2000);
        let outputs: Vec<Value> = cases
            .iter()
            .map(|case| {
                Translator::OpenAIResponsesRequest
                    .run_rust(case)
                    .expect("cases translate")
            })
            .collect();
        let texts: Vec<String> = outputs.iter().map(Value::to_string).collect();
        check(
            &texts,
            &[
                r#"{"role":"system","content":"#,
                r#""role":"tool","tool_call_id":"#,
                r#""tool_calls":[{"function":{"arguments":"#,
                r#""arguments":"{\"input\":"#,
                r#""reasoning_content":"[reasoning unavailable]""#,
                r#""reasoning_content":"Thought.""#,
                r#""type":"video_url","video_url":{"url":"#,
                r#""processing":"#,
                r#""type":"image_url","image_url":{"url":"#,
                r#""detail":"high""#,
                r#""response_format":{"type":"json_schema","json_schema":{"#,
                r#""response_format":{"type":"json_object"}"#,
                r#""max_tokens":"#,
                r#""parallel_tool_calls":true"#,
                r#""parallel_tool_calls":false"#,
                r#""tool_choice":{"type":"function","function":{"name":"#,
                r#""reasoning_effort":"#,
                r#""tools":[{"function":{"description":"#,
                r#""name":"apply_patch""#,
                "_1\"",
            ],
        );

        // Messages of each kind, counted from the parsed output.
        let count = |matches: &dyn Fn(&Value) -> bool| {
            outputs
                .iter()
                .filter(|output| {
                    output["messages"]
                        .as_array()
                        .is_some_and(|messages| messages.iter().any(matches))
                })
                .count()
        };
        let image_part = |content: &Value| {
            content
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "image_url"))
        };
        type Kind<'a> = (&'a str, &'a dyn Fn(&Value) -> bool);
        let kinds: [Kind<'_>; 4] = [
            ("tool message with images", &|message| {
                message["role"] == "tool" && image_part(&message["content"])
            }),
            ("tool message with text", &|message| {
                message["role"] == "tool" && message["content"].is_string()
            }),
            ("reasoning-only assistant message", &|message| {
                message["role"] == "assistant"
                    && message["content"] == ""
                    && message.get("tool_calls").is_none()
                    && message.get("reasoning_content").is_some()
            }),
            ("assistant text with tool calls", &|message| {
                message["role"] == "assistant"
                    && message["content"].is_array()
                    && message.get("tool_calls").is_some()
            }),
        ];
        for (kind, matches) in kinds {
            let found = count(matches);
            assert!(found >= 20, "{kind} in {found} outputs");
        }
    }

    /// Guards against a generator that never reaches the translators' branches.
    #[test]
    fn event_cases_cover_the_translators_branches() {
        let (streams, finals) = event_cases(1, 2000);
        let (again, _) = event_cases(1, 2000);
        for (a, b) in streams.iter().zip(&again) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.translated_request, b.translated_request);
            assert_eq!(a.events, b.events);
        }
        let outputs = |translator: Translator, cases: &[Case]| -> Vec<String> {
            cases
                .iter()
                .map(|case| {
                    translator
                        .run_rust(case)
                        .expect("cases translate")
                        .to_string()
                })
                .collect()
        };
        let streams = outputs(Translator::OpenAIResponsesStream, &streams);
        check(
            &streams,
            &[
                r#""event":"response.completed""#,
                r#""event":"response.incomplete""#,
                r#""event":"response.failed""#,
                r#""event":"response.custom_tool_call_input.delta""#,
                r#""event":"response.function_call_arguments.done""#,
                r#""event":"response.reasoning_summary_text.delta""#,
                r#""event":"response.output_text.delta""#,
                r#""type":"custom_tool_call""#,
                r#""namespace":"#,
                r#""instructions":"#,
                r#""cached_tokens":12"#,
            ],
        );
        let finals = outputs(Translator::OpenAIResponsesNonStream, &finals);
        check(
            &finals,
            &[
                r#""status":"incomplete""#,
                r#""type":"custom_tool_call""#,
                r#""type":"function_call""#,
                r#""type":"reasoning""#,
                r#""max_output_tokens":"#,
                r#""id":"resp_(generated)""#,
            ],
        );
    }

    fn check(outputs: &[String], needles: &[&str]) {
        for needle in needles {
            let count = outputs
                .iter()
                .filter(|output| output.contains(needle))
                .count();
            assert!(count >= 20, "{needle} in {count} outputs");
        }
    }
}
