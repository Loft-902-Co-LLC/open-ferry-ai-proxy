//! Seeded random input for the Responses → Claude translators.
//!
//! Requests aim at what the Claude translator reads: `instructions` and
//! system or developer items, message parts of every type with
//! `cache_control` at every level, Codex agent messages, reasoning items
//! signed by Claude, by other providers or redacted, tool calls with IDs
//! missing, repeated or in need of cleaning, their outputs paired,
//! duplicated, orphaned or ahead of the call, web searches, tools of every
//! kind declared at the top level, in namespaces and in `additional_tools`
//! items, and every shape of `tool_choice`, reasoning effort, token limit and
//! output format.
//!
//! Event streams are Claude's: text with citations, thinking with
//! signatures, redacted thinking, tool use (`apply_patch` included) and web
//! searches with their results, usage split between `message_start` and
//! `message_delta`, and lines that aren't events. Each comes with a generated
//! request as the client's original, so tool names are mapped back and
//! request fields echoed.
//!
//! Upstream's stream translator finishes pending tool calls by ranging over a
//! Go map, so with two or more pending at once its output order is random;
//! streams where that can happen are regenerated (see [`map_order_hazard`]).

use std::collections::BTreeSet;
use std::ops::{Deref, DerefMut};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::{EFFORTS, Rng, SERVICE_TIERS, escape_text, num, to_object};
use crate::cases::Case;

/// Builds `count` random Responses requests for a Claude upstream.
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

/// Builds `count` random Claude event streams, and a non-streaming case from
/// the whole of each, with the client's request they answer.
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed.rotate_left(47), index);
            let model =
                generator
                    .rng
                    .pick(&["claude-opus-4-6", "claude-sonnet-4-5-20250929", "", " "]);
            let request = generator.original_request();
            let translated_request = generator.translated_request();
            let (events, lines) = generator.stream();
            let body = generator.body(&events);
            let case = |events| Case {
                translated_request: translated_request.clone(),
                events,
                ..Case::new(format!("random-{seed}-{index}"), model, request.clone())
            };
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Claude models with effort levels, with budgets only, that reject an
/// assistant prefill, or unknown to the catalog. Some reject a prefill by a
/// version that follows a provider namespace, is dotted or comes before a
/// snapshot date; some name a family only in passing and don't.
const MODELS: &[&str] = &[
    "claude-opus-4-6",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-opus-4-7",
    "claude-opus-5-5",
    "claude-fable-5-1",
    "anthropic/claude-opus-5-thinking",
    "claude-sonnet-4.6",
    "claude-sonnet-4-10",
    " CLAUDE-OPUS-6 ",
    "claude-sonnet-4-20260217",
    "my-sonnet-4-6-wrapper",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-5-20250929",
    "claude-haiku-4-5-20251001",
    "claude-3-5-haiku-20241022",
    "claude-opus-4-6(high)",
    "claude-test",
    "gpt-5",
    "",
];

/// Loosely written integers, none out of int64's range: Go converts those by
/// the CPU's rules, and a token limit is then capped by the catalog.
const INTEGERS: &[&str] = &["1.5", "2.0", "-0", "1e3", "9007199254740993", "-7", "0.5"];

/// Namespace names: plain, already ending in the separator, padded, empty,
/// and long enough that qualified names need shortening.
const NAMESPACES: &[&str] = &[
    "mcp__github",
    "browser",
    "tools__",
    " spaced ",
    "",
    "mcp__",
    "namespace_with_a_rather_long_name_for_shortening",
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
    "\"text\"",
    "5",
    r#"{"a":"#,
    "not json",
];

/// A patch as `apply_patch` takes it.
const PATCH: &str = "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch";

/// Tool input as Claude streams it: JSON objects whole or cut short, custom
/// tool input wrapped in `input`, and patches.
const STREAMED_ARGUMENTS: &[&str] = &[
    r#"{"city":"Paris"}"#,
    r#"{"q":"café 🚀","n":1.50}"#,
    "{}",
    "",
    r#"{"a":"#,
    "not json",
    r#"{"input":"ls -la"}"#,
    r#"{"input":""}"#,
    r#"{"input":5}"#,
    r#"{"input":"*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch"}"#,
    r#"{"input":"*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch\n"}"#,
    r#"{"input":"*** Begin Patch\n*** Add File: cut.txt\n+no end"}"#,
    r#"{"input":"not a patch"}"#,
];

/// Request fields upstream's response translator copies into the response.
const ECHOED: &[&str] = &[
    "temperature",
    "top_p",
    "parallel_tool_calls",
    "store",
    "previous_response_id",
    "prompt_cache_key",
    "truncation",
    "max_tool_calls",
    "safety_identifier",
    "top_logprobs",
];

/// The redacted thinking marker in a reasoning item's `encrypted_content`.
const REDACTED_PREFIX: &str = "claude-redacted-thinking:";

/// The request generator, for its leaf values: text, numbers, tool names,
/// schemas and signatures.
struct Generator {
    base: super::Generator,
    /// Namespaces declared in `tools`, for calls and `tool_choice` to use.
    namespaces: Vec<String>,
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
    fn new(seed: u64, index: u64) -> Self {
        Self {
            base: super::Generator {
                // A different mix from the other generators', so cases don't
                // share their random choices.
                rng: Rng(seed.rotate_left(52) ^ index.wrapping_mul(0xA24B_AED4_963E_E407)),
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
            },
            namespaces: Vec::new(),
        }
    }

    // --- Requests ---

    fn request(&mut self) -> Value {
        let mut fields = Vec::new();
        // Tools and input come first so calls and tool_choice can use their names.
        if self.rng.chance(55) {
            fields.push(("tools", self.tools()));
        }
        if self.rng.chance(95) {
            fields.push(("input", self.input()));
        }
        if self.rng.chance(80) {
            let model =
                self.loose_choice(&["claude-sonnet-4-6", "claude-opus-4-6", "gpt-5", "", " "]);
            fields.push(("model", model));
        }
        if self.rng.chance(40) {
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
        if self.rng.chance(40) {
            fields.push(("reasoning", self.reasoning()));
        }
        if self.rng.chance(35) {
            fields.push(("max_output_tokens", self.token_limit()));
        }
        if self.rng.chance(15) {
            let tier = self.loose_choice(SERVICE_TIERS);
            fields.push(("service_tier", tier));
        }
        if self.rng.chance(25) {
            fields.push(("text", self.text_config()));
        }
        if self.rng.chance(8) {
            fields.push(("response_format", self.format()));
        }
        if self.rng.chance(20) {
            fields.push(("metadata", self.metadata()));
        }
        if self.rng.chance(15) {
            let user = self.loose_choice(&["user-123", "", " ", " padded "]);
            fields.push(("user", user));
        }
        if self.rng.chance(20) {
            fields.push(("stream", self.rng.chance(50).into()));
        }
        for &key in ECHOED {
            if self.rng.chance(8) {
                let value = self.echoed(key);
                fields.push((key, value));
            }
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    /// A value for a field the response translator echoes.
    fn echoed(&mut self, key: &str) -> Value {
        match key {
            "temperature" | "top_p" => {
                if self.rng.chance(80) {
                    self.number()
                } else {
                    self.one_of(&[json!("0.5"), Value::Null, json!(true)])
                }
            }
            "parallel_tool_calls" | "store" => self.bool_like(),
            "max_tool_calls" | "top_logprobs" => self.token_limit(),
            "truncation" => self.loose_choice(&["auto", "disabled", ""]),
            _ => self.loose_text(),
        }
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
            12 | 13 => self.namespace_tool(),
            14 | 15 => self.web_search_tool(),
            16 => {
                let tool_type = self.rng.pick(&[
                    "image_generation",
                    "file_search",
                    "code_interpreter",
                    "computer_use_preview",
                ]);
                json!({ "type": tool_type, "name": "builtin" })
            }
            17 => {
                // Types upstream copies through as they are, given a name.
                let tool_type = self
                    .rng
                    .pick(&["mcp", "local_shell", "Function", " custom "]);
                let mut fields = vec![("type", json!(tool_type))];
                if self.rng.chance(70) {
                    let name = self.declared_name();
                    fields.push(("name", name));
                }
                if self.rng.chance(50) {
                    fields.push(("server_label", json!("deepwiki")));
                }
                self.object(fields)
            }
            18 => {
                // Chat Completions style, which upstream also reads.
                let name = self.declared_name();
                let mut function = vec![("name", name)];
                if self.rng.chance(60) {
                    function.push(("description", self.text().into()));
                }
                if self.rng.chance(70) {
                    let key = self.rng.pick(&["parameters", "parametersJsonSchema"]);
                    function.push((key, self.schema(0)));
                }
                let function = self.object(function);
                json!({ "type": "function", "function": function })
            }
            19 => {
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
            fields.push(("description", self.text().into()));
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
        if self.rng.chance(15) {
            fields.push(("cache_control", self.cache_control()));
        }
        if self.rng.chance(8) {
            fields.push(("defer_loading", json!(true)));
        }
        self.object(fields)
    }

    fn custom_tool(&mut self) -> Value {
        let mut fields = vec![("type", json!("custom"))];
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
                json!("text"),
            ]);
            fields.push(("format", format));
        }
        if self.rng.chance(10) {
            fields.push(("cache_control", self.cache_control()));
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

    fn namespace_tool(&mut self) -> Value {
        let namespace = self.rng.pick(NAMESPACES);
        self.namespaces.push(namespace.to_owned());
        let mut fields = vec![("type", json!("namespace"))];
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
        let child = match self.rng.below(10) {
            0 => format!("mcp__{}", self.alphanumeric(4)),
            // Already qualified, or the namespace itself.
            1 if !namespace.is_empty() => format!("{namespace}__read"),
            2 if !namespace.is_empty() => namespace.to_owned(),
            3 => " padded ".to_owned(),
            _ => match self.tool_name() {
                Value::String(name) => name,
                _ => "read".to_owned(),
            },
        };
        self.tool_names.push(qualify(namespace, &child).into());
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
        if self.rng.chance(10) {
            fields.push(("cache_control", self.cache_control()));
        }
        self.object(fields)
    }

    fn web_search_tool(&mut self) -> Value {
        let mut fields = vec![("type", json!("web_search"))];
        if self.rng.chance(30) {
            let name = self.one_of(&[
                json!("web_search"),
                json!("search"),
                json!(""),
                json!(" web "),
            ]);
            fields.push(("name", name));
        }
        if self.rng.chance(25) {
            let access = self.one_of(&[
                json!(true),
                json!(false),
                json!("false"),
                json!(0),
                Value::Null,
            ]);
            fields.push(("external_web_access", access));
        }
        if self.rng.chance(30) {
            let uses = match self.rng.below(4) {
                0 => num(self.rng.pick(INTEGERS)),
                1 => self.one_of(&[json!("3"), Value::Null, json!(true)]),
                _ => json!(5),
            };
            fields.push(("max_uses", uses));
        }
        if self.rng.chance(30) {
            let filters = self.one_of(&[
                json!({ "allowed_domains": ["example.com", "docs.rs"] }),
                json!({ "allowed_domains": "example.com" }),
                json!({ "allowed_domains": [] }),
                json!({ "blocked_domains": ["spam.example"] }),
                json!("example.com"),
            ]);
            fields.push(("filters", filters));
        }
        if self.rng.chance(25) {
            let location = self.one_of(&[
                json!({ "type": "approximate", "city": "Paris", "country": "FR", "timezone": "Europe/Paris" }),
                json!({ "type": "approximate", "region": 5.10 }),
                json!("Paris"),
                Value::Null,
            ]);
            fields.push(("user_location", location));
        }
        if self.rng.chance(20) {
            fields.push(("search_context_size", json!("medium")));
        }
        self.object(fields)
    }

    fn cache_control(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => json!({ "type": "ephemeral" }),
            6 => json!({ "type": "ephemeral", "ttl": "1h" }),
            7 => json!({ "type": "persistent" }),
            _ => self.one_of(&[
                json!("ephemeral"),
                Value::Null,
                json!({}),
                json!({ "type": 5 }),
            ]),
        }
    }

    // --- Input items ---

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
            22..=33 => self.message_item("assistant"),
            34..=40 => self.system_item(),
            41..=43 => self.agent_message(),
            44..=53 => self.reasoning_item(),
            54..=69 => return self.tool_calls(items),
            70..=77 => {
                let id = self.pending_call_id();
                self.tool_output(id)
            }
            78..=81 => self.web_search_call(),
            82..=84 => self.additional_tools(),
            85..=89 => self.odd_message(),
            90..=94 => self.other_item(),
            _ => self.one_of(&[json!("hello"), Value::Null, json!(5), json!([])]),
        };
        items.push(item);
    }

    fn message_item(&mut self, role: &str) -> Value {
        let mut fields = Vec::new();
        match self.rng.below(20) {
            0..=16 => fields.push(("type", json!("message"))),
            17 => fields.push(("type", json!(""))),
            _ => {}
        }
        fields.push(("role", json!(role)));
        let user = role == "user";
        match self.rng.below(20) {
            0..=14 => {
                let count = 1 + self.rng.below(4);
                let parts = (0..count)
                    .map(|_| {
                        if user {
                            self.user_part()
                        } else {
                            self.assistant_part()
                        }
                    })
                    .collect();
                fields.push(("content", Value::Array(parts)));
            }
            15..=17 => fields.push(("content", self.loose_text())),
            18 => {
                let junk =
                    self.one_of(&[Value::Null, json!({ "type": "input_text", "text": "x" })]);
                fields.push(("content", junk));
            }
            _ => {}
        }
        if self.rng.chance(10) {
            fields.push(("cache_control", self.cache_control()));
        }
        if self.rng.chance(20) {
            fields.push(("id", format!("msg_{}", self.alphanumeric(8)).into()));
        }
        if self.rng.chance(10) {
            fields.push(("status", json!("completed")));
        }
        self.object(fields)
    }

    fn user_part(&mut self) -> Value {
        match self.rng.below(20) {
            0..=10 => self.text_part("input_text"),
            11..=13 => self.image_part(),
            14..=16 => self.file_part(),
            17 => self.text_part("output_text"),
            18 => self.refusal_part(),
            _ => self.junk_part(),
        }
    }

    fn assistant_part(&mut self) -> Value {
        match self.rng.below(20) {
            0..=12 => self.text_part("output_text"),
            13..=15 => self.refusal_part(),
            16 => self.text_part("input_text"),
            17 => self.image_part(),
            _ => self.junk_part(),
        }
    }

    fn text_part(&mut self, part_type: &str) -> Value {
        let mut fields = vec![("type", json!(part_type))];
        if self.rng.chance(95) {
            fields.push(("text", self.loose_text()));
        }
        if part_type == "output_text" && self.rng.chance(25) {
            fields.push(("annotations", self.annotations()));
        }
        if self.rng.chance(10) {
            fields.push(("cache_control", self.cache_control()));
        }
        if self.rng.chance(5) {
            fields.push(("logprobs", json!([])));
        }
        self.object(fields)
    }

    /// Output text annotations: citations with an encrypted index, which
    /// Claude takes back, and ones without.
    fn annotations(&mut self) -> Value {
        if self.rng.chance(10) {
            return self.one_of(&[json!({}), json!("cite"), Value::Null]);
        }
        let count = self.rng.below(3) + 1;
        Value::Array(
            (0..count)
                .map(|_| {
                    let mut fields = vec![
                        ("type", json!("url_citation")),
                        ("url", json!("https://example.com/a")),
                        ("title", json!("Example")),
                    ];
                    if self.rng.chance(60) {
                        let index = self.one_of(&[
                            json!("Eo8BCioIBxgCIiQ4"),
                            json!("Eo8BCioIBxgCIiQ4"),
                            json!(" "),
                            json!(""),
                            json!(5),
                        ]);
                        fields.push(("encrypted_index", index));
                    }
                    if self.rng.chance(40) {
                        fields.push(("cited_text", self.text().into()));
                    }
                    if self.rng.chance(30) {
                        fields.push(("start_index", json!(0)));
                        fields.push(("end_index", num("5.0")));
                    }
                    self.object(fields)
                })
                .collect(),
        )
    }

    fn refusal_part(&mut self) -> Value {
        let mut fields = vec![("type", json!("refusal"))];
        if self.rng.chance(90) {
            let refusal = if self.rng.chance(10) {
                json!("")
            } else {
                self.loose_text()
            };
            fields.push(("refusal", refusal));
        }
        if self.rng.chance(10) {
            fields.push(("cache_control", self.cache_control()));
        }
        self.object(fields)
    }

    fn image_part(&mut self) -> Value {
        let url = self.one_of(&[
            json!("data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="),
            json!("data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="),
            json!("data:;base64,iVBORw0KGgo="),
            json!("data:image/png;base64,"),
            json!("data:image/png,iVBORw0KGgo="),
            json!("data:image/jpeg;base64,/9j/4AAQ;base64,x"),
            json!("data:"),
            json!("https://example.com/cat.png"),
            json!(" https://example.com/padded.png "),
            json!(""),
            json!(5),
            json!({ "url": "https://example.com/nested.png" }),
        ]);
        let mut fields = vec![("type", json!("input_image"))];
        match self.rng.below(10) {
            0..=6 => fields.push(("image_url", url)),
            7 | 8 => fields.push(("url", url)),
            _ => {
                fields.push(("image_url", json!("")));
                fields.push(("url", url));
            }
        }
        if self.rng.chance(30) {
            fields.push(("detail", self.one_of(&[json!("auto"), json!("high")])));
        }
        if self.rng.chance(10) {
            fields.push(("cache_control", self.cache_control()));
        }
        self.object(fields)
    }

    fn file_part(&mut self) -> Value {
        let mut fields = vec![("type", json!("input_file"))];
        if self.rng.chance(85) {
            let data = self.one_of(&[
                json!("data:application/pdf;base64,JVBERi0xLjQK"),
                json!("data:application/pdf;base64,JVBERi0xLjQK"),
                json!("data:text/plain;charset=utf-8;base64,aGk="),
                json!("data:;base64,JVBERi0="),
                json!("data:application/pdf;base64"),
                json!("data:application/pdf,JVBE;Ri0="),
                json!("JVBERi0xLjQK"),
                json!(""),
                json!(5),
            ]);
            fields.push(("file_data", data));
        }
        if self.rng.chance(40) {
            fields.push(("filename", json!("report.pdf")));
        }
        if self.rng.chance(15) {
            fields.push(("file_id", json!("file_123")));
        }
        if self.rng.chance(10) {
            fields.push(("cache_control", self.cache_control()));
        }
        self.object(fields)
    }

    fn junk_part(&mut self) -> Value {
        self.one_of(&[
            json!({ "type": "input_audio", "input_audio": { "data": "UklGRg==" } }),
            json!({ "type": "text", "text": "chat style" }),
            json!({ "type": "summary_text", "text": "summary" }),
            json!({ "text": "no type" }),
            json!("bare string"),
            json!(5),
            Value::Null,
        ])
    }

    /// A system or developer item, which becomes a top-level system block.
    fn system_item(&mut self) -> Value {
        let role = self.one_of(&[
            json!("system"),
            json!("system"),
            json!("developer"),
            json!("developer"),
            json!(" Developer "),
            json!("SYSTEM"),
        ]);
        let mut fields = Vec::new();
        if self.rng.chance(80) {
            fields.push(("type", json!("message")));
        }
        fields.push(("role", role));
        match self.rng.below(10) {
            0..=3 => fields.push(("content", self.text().into())),
            4..=8 => {
                let count = 1 + self.rng.below(4);
                let parts = (0..count)
                    .map(|_| match self.rng.below(12) {
                        0..=4 => self.text_part("input_text"),
                        5 => self.text_part("output_text"),
                        6 => self.text_part("text"),
                        7 => self.image_part(),
                        8 => json!({ "type": " input_audio " }),
                        9 => json!({ "type": "" }),
                        10 => json!({ "text": "no type" }),
                        _ => json!("bare string"),
                    })
                    .collect();
                fields.push(("content", Value::Array(parts)));
            }
            _ => {
                let junk = self.one_of(&[Value::Null, json!(5), json!({ "text": "x" })]);
                fields.push(("content", junk));
            }
        }
        if self.rng.chance(20) {
            fields.push(("cache_control", self.cache_control()));
        }
        self.object(fields)
    }

    /// A Codex multi-agent message, which upstream turns into a user message.
    fn agent_message(&mut self) -> Value {
        let item_type = self.one_of(&[
            json!("agent_message"),
            json!("agent_message"),
            json!(" agent_message "),
        ]);
        let mut fields = vec![("type", item_type)];
        if self.rng.chance(40) {
            fields.push(("role", self.one_of(&[json!("assistant"), json!("system")])));
        }
        if self.rng.chance(30) {
            fields.push(("author", json!("worker-1")));
        }
        let content = if self.rng.chance(85) {
            let count = 1 + self.rng.below(3);
            let parts = (0..count)
                .map(|_| match self.rng.below(10) {
                    0..=5 => {
                        let part_type = self.one_of(&[
                            json!("encrypted_content"),
                            json!("encrypted_content"),
                            json!(" encrypted_content "),
                        ]);
                        let content = if self.rng.chance(85) {
                            self.text().into()
                        } else {
                            self.one_of(&[json!(5), Value::Null, json!({ "x": 1 })])
                        };
                        json!({ "type": part_type, "encrypted_content": content })
                    }
                    6 | 7 => self.text_part("input_text"),
                    8 => json!({ "type": "encrypted_content" }),
                    _ => self.junk_part(),
                })
                .collect();
            Value::Array(parts)
        } else {
            self.loose_text()
        };
        fields.push(("content", content));
        self.object(fields)
    }

    fn reasoning_item(&mut self) -> Value {
        let mut fields = vec![("type", json!("reasoning"))];
        if self.rng.chance(50) {
            fields.push(("id", format!("rs_{}", self.alphanumeric(10)).into()));
        }
        if self.rng.chance(90) {
            let signature = self.reasoning_signature();
            fields.push(("encrypted_content", signature));
        }
        if self.rng.chance(75) {
            let parts = self.reasoning_parts("summary_text");
            fields.push(("summary", parts));
        }
        if self.rng.chance(30) {
            let parts = self.reasoning_parts("reasoning_text");
            fields.push(("content", parts));
        }
        if self.rng.chance(10) {
            fields.push(("status", json!("completed")));
        }
        self.object(fields)
    }

    fn reasoning_parts(&mut self, part_type: &str) -> Value {
        if self.rng.chance(8) {
            return self.one_of(&[json!("text"), Value::Null, json!({})]);
        }
        let count = self.rng.below(3);
        Value::Array(
            (0..count)
                .map(|_| match self.rng.below(10) {
                    0..=6 => json!({ "type": part_type, "text": self.loose_text() }),
                    7 => json!(self.text()),
                    8 => json!({ "type": part_type }),
                    _ => json!(5),
                })
                .collect(),
        )
    }

    /// A reasoning item's signature: Claude's, valid for the target or not,
    /// another provider's, redacted thinking, or nothing.
    fn reasoning_signature(&mut self) -> Value {
        match self.rng.below(100) {
            0..=29 => {
                let model = self.rng.pick(&[
                    "claude-sonnet-4-6",
                    "claude-opus-4-6",
                    "claude-fable-5-1",
                    "claude-opus-5",
                ]);
                let signature = claude_signature(model);
                if self.rng.chance(20) {
                    self.damage(signature).into()
                } else {
                    signature.into()
                }
            }
            30..=49 => self.signature_text().into(),
            50..=59 => {
                let signature = self.gpt_signature();
                signature.into()
            }
            60..=74 => {
                let data = self.one_of(&[
                    json!("EmwKAhgBEgwvYXRoZXJzdGVwcw=="),
                    json!(" EmwKAhgB "),
                    json!(""),
                    json!(" "),
                ]);
                let lead = self.rng.pick(&["", "", " "]);
                format!(
                    "{lead}{REDACTED_PREFIX}{}",
                    data.as_str().unwrap_or_default()
                )
                .into()
            }
            75..=84 => json!(""),
            85..=89 => json!("not a signature"),
            _ => self.one_of(&[json!(5), Value::Null, json!(true)]),
        }
    }

    /// One to three tool calls, then usually their outputs: in order, shuffled,
    /// one short, one twice, or with an output ahead of its call.
    fn tool_calls(&mut self, items: &mut Vec<Value>) {
        let count = 1 + self.rng.below(3);
        let mut ids = Vec::new();
        let early = self.rng.chance(5);
        if early {
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
        match self.rng.below(10) {
            0..=4 => {}
            5..=6 => self.rng.shuffle(&mut ids),
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
        }
        if self.rng.chance(5) {
            // An output with no ID, paired by name or order.
            items.push(self.tool_output(None));
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
            0..=11 => format!("call_{}", self.alphanumeric(24)).into(),
            12 => format!("toolu_01{}", self.alphanumeric(20)).into(),
            // Needs cleaning for Claude.
            13 => "call.1:é/x y".into(),
            // Over the 64-byte limit.
            14 => format!("call_{}", self.alphanumeric(90)).into(),
            15 => "".into(),
            16 => " call_padded ".into(),
            17 => match self.tool_use_ids.last() {
                Some(id) => id.clone(),
                None => "call_same".into(),
            },
            18 if key == "id" => format!("fco_{}", self.alphanumeric(8)).into(),
            _ => num(self.rng.pick(&["12345", "1.50"])),
        };
        Some((key, id))
    }

    fn tool_call(&mut self, id: Option<(&'static str, Value)>) -> Value {
        let custom = self.rng.chance(20);
        let item_type = if custom {
            "custom_tool_call"
        } else {
            "function_call"
        };
        let mut fields = vec![("type", json!(item_type))];
        if let Some((key, id)) = id {
            fields.push((key, id));
        }
        if self.rng.chance(30) && !fields.iter().any(|(key, _)| *key == "id") {
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
        if self.rng.chance(12) {
            let namespace = if !self.namespaces.is_empty() && self.rng.chance(70) {
                json!(self.base.rng.pick(&self.namespaces))
            } else {
                json!(self.rng.pick(NAMESPACES))
            };
            fields.push(("namespace", namespace));
        }
        if custom {
            let input = match self.rng.below(10) {
                0..=3 => json!(PATCH),
                4..=7 => self.loose_text(),
                8 => json!({ "input": "nested" }),
                _ => json!(""),
            };
            fields.push(("input", input));
        } else if self.rng.chance(95) {
            let arguments = if self.rng.chance(90) {
                json!(self.rng.pick(ARGUMENTS))
            } else {
                self.one_of(&[json!({ "city": "Paris" }), json!(5), Value::Null])
            };
            fields.push(("arguments", arguments));
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
        let item_type = if self.rng.chance(85) {
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
        match self.rng.below(20) {
            0..=7 => self.text().into(),
            8 => json!(""),
            9..=15 => {
                let count = self.rng.below(4);
                Value::Array(
                    (0..count)
                        .map(|_| match self.rng.below(10) {
                            0..=3 => self.text_part("input_text"),
                            4 => self.text_part("output_text"),
                            5 | 6 => self.image_part(),
                            7 => self.file_part(),
                            _ => self.junk_part(),
                        })
                        .collect(),
                )
            }
            16 => json!({ "result": 1.50, "ok": true }),
            17 => json!(5),
            18 => json!("  "),
            _ => Value::Null,
        }
    }

    fn web_search_call(&mut self) -> Value {
        let mut fields = vec![("type", json!("web_search_call"))];
        if self.rng.chance(92) {
            let id = match self.rng.below(10) {
                0..=4 => format!("ws_{}", self.alphanumeric(12)),
                5 => format!("ws_srvtoolu_{}", self.alphanumeric(12)),
                6 => format!("srvtoolu_{}", self.alphanumeric(12)),
                7 => "ws_é-日 x".to_owned(),
                8 => "ws_".to_owned(),
                _ => " ".to_owned(),
            };
            fields.push(("id", id.into()));
        }
        fields.push(("status", json!("completed")));
        if self.rng.chance(90) {
            let action = self.one_of(&[
                json!({ "type": "search", "query": "rust serde" }),
                json!({ "type": "search", "query": " padded " }),
                json!({ "type": "search", "query": "", "queries": ["first", "second"] }),
                json!({ "type": "search", "queries": [" ", "x"] }),
                json!({ "type": "open_page", "url": "https://example.com/page" }),
                json!({ "type": "find", "pattern": "x" }),
                json!("search"),
            ]);
            fields.push(("action", action));
        }
        if self.rng.chance(60) {
            let results = self.search_results();
            fields.push(("results", results));
        }
        self.object(fields)
    }

    fn search_results(&mut self) -> Value {
        if self.rng.chance(10) {
            return self.one_of(&[
                json!({ "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" }),
                json!("results"),
                Value::Null,
            ]);
        }
        let count = self.rng.below(4);
        Value::Array(
            (0..count)
                .map(|_| match self.rng.below(10) {
                    0..=5 => {
                        let mut fields = vec![
                            ("type", json!("web_search_result")),
                            ("url", json!("https://example.com/r")),
                            ("title", json!("Result")),
                        ];
                        if self.rng.chance(85) {
                            let content = self.one_of(&[
                                json!("EqgfCioIARgBIiQ3"),
                                json!(" "),
                                json!(5),
                            ]);
                            fields.push(("encrypted_content", content));
                        }
                        if self.rng.chance(40) {
                            fields.push(("page_age", json!("2 days ago")));
                        }
                        self.object(fields)
                    }
                    6 => {
                        json!({ "type": "web_search_tool_result_error", "error_code": "unavailable" })
                    }
                    7 => json!({ "url": "https://example.com/untyped", "encrypted_content": "Eq" }),
                    _ => self.one_of(&[json!("hit"), json!(5), Value::Null]),
                })
                .collect(),
        )
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
        let count = 1 + self.rng.below(2);
        let parts = (0..count).map(|_| self.user_part()).collect();
        fields.push(("content", Value::Array(parts)));
        self.object(fields)
    }

    /// Items with no Claude counterpart.
    fn other_item(&mut self) -> Value {
        self.one_of(&[
            json!({ "type": "item_reference", "id": "msg_1" }),
            json!({ "type": "local_shell_call", "call_id": "call_shell", "action": { "command": ["ls"] } }),
            json!({ "type": "compaction", "encrypted_content": "abc" }),
            json!({ "type": "image_generation_call", "id": "ig_1", "result": "iVBOR" }),
            json!({ "type": "", "content": "no role" }),
            json!({ "content": "neither type nor role" }),
        ])
    }

    // --- Other request fields ---

    fn tool_choice(&mut self) -> Value {
        match self.rng.below(12) {
            0..=2 => self.loose_choice(&[
                "auto", "required", "required", "none", "any", " auto ", "AUTO", "",
            ]),
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
                json!({ "type": "web_search_preview" }),
                json!({ "type": "file_search" }),
            ]),
            10 => self.one_of(&[
                json!({ "type": "auto" }),
                json!({ "type": "none" }),
                json!({ "type": "required" }),
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
            json!(self.rng.pick(NAMESPACES))
        }
    }

    fn reasoning(&mut self) -> Value {
        if self.rng.chance(8) {
            return self.one_of(&[json!("high"), Value::Null, json!([])]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(75) {
            fields.push(("effort", self.effort()));
        }
        if self.rng.chance(35) {
            fields.push(("summary", self.summary_setting()));
        }
        if self.rng.chance(15) {
            fields.push(("generate_summary", self.summary_setting()));
        }
        self.object(fields)
    }

    fn effort(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => self.loose_choice(&[
                "none", "auto", "minimal", "low", "medium", "high", "xhigh", "max",
            ]),
            6..=7 => self.loose_choice(&[" High ", "MAX", "", "ultra", "Auto", " none "]),
            _ => self.loose_choice(EFFORTS),
        }
    }

    fn summary_setting(&mut self) -> Value {
        if self.rng.chance(10) {
            return Value::Null;
        }
        self.loose_choice(&["auto", "concise", "detailed", "none", " Detailed ", "bogus"])
    }

    /// A token limit, never out of int64's range (see [`INTEGERS`]).
    fn token_limit(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => self.one_of(&[
                json!(1024),
                json!(64000),
                json!(128000),
                json!(1),
                json!(0),
                json!(-5),
            ]),
            6..=7 => num(self.rng.pick(INTEGERS)),
            _ => self.one_of(&[json!("2048"), Value::Null, json!(true), json!(" 10 ")]),
        }
    }

    fn text_config(&mut self) -> Value {
        match self.rng.below(10) {
            0..=6 => json!({ "format": self.format() }),
            7 => json!({ "format": { "type": "json_object" }, "verbosity": "low" }),
            8 => json!({ "verbosity": "high" }),
            _ => self.one_of(&[json!("json"), Value::Null, json!([])]),
        }
    }

    fn metadata(&mut self) -> Value {
        match self.rng.below(10) {
            0..=4 => {
                let id = self.loose_choice(&["user-123", "", " ", " padded "]);
                json!({ "user_id": id })
            }
            5..=6 => json!({ "user_id": "user-123", "session": "s" }),
            _ => self.one_of(&[
                json!({}),
                json!("meta"),
                Value::Null,
                json!([{ "user_id": "x" }]),
            ]),
        }
    }

    // --- Event streams ---

    /// The client's request, as upstream's response translator is given it:
    /// usually one the generator builds, sometimes none, or not JSON.
    fn original_request(&mut self) -> String {
        let text = match self.rng.below(100) {
            // Absent. Not "null": upstream would take that as a request.
            0..=14 => String::new(),
            15 | 16 => "{not json".to_owned(),
            17 => "[]".to_owned(),
            _ => {
                let request = self.request();
                self.render(&request)
            }
        };
        self.tool_use_ids.clear();
        text
    }

    /// The request as sent to Claude, which upstream reads only when the
    /// original is missing.
    fn translated_request(&mut self) -> String {
        match self.rng.below(10) {
            0..=5 => String::new(),
            6 | 7 => {
                let model = self.loose_choice(&["claude-opus-4-6", "", " "]);
                json!({ "model": model }).to_string()
            }
            _ => {
                let request = self.request();
                self.tool_use_ids.clear();
                self.render(&request)
            }
        }
    }

    /// The events of a stream and the lines that carry them, drawn again while
    /// upstream's output for them could depend on Go's map order.
    fn stream(&mut self) -> (Vec<Value>, Vec<String>) {
        for _ in 0..8 {
            let events = self.events();
            let lines: Vec<String> = events.iter().map(|event| self.line(event)).collect();
            if !map_order_hazard(&lines) {
                return (events, lines);
            }
        }
        // Still unlucky: drop the tool calls and searches.
        let events: Vec<Value> = self
            .events()
            .into_iter()
            .filter(|event| {
                !matches!(block_type(event), Some("tool_use" | "server_tool_use"))
                    && delta_type(event) != Some("input_json_delta")
            })
            .collect();
        let lines = events.iter().map(|event| self.line(event)).collect();
        (events, lines)
    }

    fn events(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        if self.rng.chance(92) {
            events.push(self.message_start());
        }
        let mut position = 0;
        for _ in 0..self.rng.below(6) {
            for (start, deltas) in self.block() {
                let index = self.block_index(position);
                position += 1;
                if self.rng.chance(95) {
                    events.push(event(
                        "content_block_start",
                        &index,
                        vec![("content_block", start)],
                    ));
                }
                for delta in deltas {
                    events.push(event("content_block_delta", &index, vec![("delta", delta)]));
                }
                if self.rng.chance(93) {
                    events.push(event("content_block_stop", &index, Vec::new()));
                }
            }
            if self.rng.chance(4) {
                events.push(json!({ "type": "ping" }));
            }
        }
        if self.rng.chance(85) {
            events.push(self.message_delta());
        }
        if self.rng.chance(88) {
            events.push(json!({ "type": "message_stop" }));
        }
        if self.rng.chance(5) {
            // After the end, which both should ignore.
            events.push(event(
                "content_block_start",
                &Some(json!(position)),
                vec![("content_block", json!({ "type": "text", "text": "" }))],
            ));
        }
        if self.rng.chance(5) {
            let at = self.rng.below(events.len() + 1);
            let error = self.error();
            events.insert(at, error);
        }
        if self.rng.chance(4) {
            // A second message start, which starts over.
            let at = self.rng.below(events.len() + 1);
            let start = self.message_start();
            events.insert(at, start);
        }
        if events.len() > 1 && self.rng.chance(6) {
            let (a, b) = (self.rng.below(events.len()), self.rng.below(events.len()));
            events.swap(a, b);
        }
        events
    }

    fn message_start(&mut self) -> Value {
        if self.rng.chance(4) {
            return self.one_of(&[
                json!({ "type": "message_start" }),
                json!({ "type": "message_start", "message": "msg" }),
                json!({ "type": "message_start", "message": null }),
            ]);
        }
        let mut message = Vec::new();
        match self.rng.below(10) {
            0 => {}
            1 => message.push(("id", self.one_of(&[json!(""), json!(5), Value::Null]))),
            _ => message.push(("id", format!("msg_{}", self.alphanumeric(12)).into())),
        }
        message.push(("type", json!("message")));
        message.push(("role", json!("assistant")));
        if self.rng.chance(85) {
            let model = self.loose_choice(&["claude-opus-4-6", "claude-sonnet-4-5-20250929", ""]);
            message.push(("model", model));
        }
        message.push(("content", json!([])));
        message.push(("stop_reason", Value::Null));
        if self.rng.chance(85) {
            let usage = self.usage();
            message.push(("usage", usage));
        }
        let message = self.object(message);
        json!({ "type": "message_start", "message": message })
    }

    fn usage(&mut self) -> Value {
        if self.rng.chance(3) {
            return self.one_of(&[Value::Null, json!("usage"), json!([])]);
        }
        let mut fields = Vec::new();
        for key in [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ] {
            if self.rng.chance(60) {
                let count = self.count();
                fields.push((key, count));
            }
        }
        if self.rng.chance(10) {
            fields.push(("server_tool_use", json!({ "web_search_requests": 1 })));
        }
        if self.rng.chance(10) {
            fields.push(("service_tier", json!("standard")));
        }
        self.object(fields)
    }

    /// A token count, never out of int64's range (see [`INTEGERS`]).
    fn count(&mut self) -> Value {
        match self.rng.below(100) {
            0..=79 => json!(self.rng.below(50_000)),
            80..=84 => json!(0),
            85..=89 => num(self.rng.pick(INTEGERS)),
            90..=95 => self.one_of(&[json!("12"), Value::Null, json!(true)]),
            96..=98 => json!(-3),
            _ => json!(i64::MAX),
        }
    }

    /// A block's `index`: usually its position, sometimes missing or loosely
    /// typed, or another block's.
    fn block_index(&mut self, position: usize) -> Option<Value> {
        Some(match self.rng.below(100) {
            0..=89 => json!(position),
            90..=92 => return None,
            93..=94 => json!(position.to_string()),
            95..=96 => num("1.5"),
            97 => json!(-1),
            _ => json!(position + 1),
        })
    }

    /// One content block's start and deltas, or two for a web search and its
    /// results.
    fn block(&mut self) -> Vec<(Value, Vec<Value>)> {
        let block = match self.rng.below(100) {
            0..=29 => {
                let mut start = vec![("type", json!("text")), ("text", json!(""))];
                if self.rng.chance(10) {
                    start.push(("citations", json!([])));
                }
                let mut deltas = self.deltas("text_delta", "text");
                if self.rng.chance(15) {
                    let at = self.rng.below(deltas.len() + 1);
                    let citation = self.citation_delta();
                    deltas.insert(at, citation);
                }
                (to_object(start), deltas)
            }
            30..=44 => {
                let mut start = vec![("type", json!("thinking")), ("thinking", json!(""))];
                if self.rng.chance(20) {
                    start.push(("signature", json!("")));
                }
                let mut deltas = self.deltas("thinking_delta", "thinking");
                if self.rng.chance(65) {
                    let signature = match self.rng.below(10) {
                        0..=6 => json!(claude_signature("claude-sonnet-4-6")),
                        7 => json!("EqQBCkYIBxgCKkA="),
                        8 => json!(""),
                        _ => json!(5),
                    };
                    deltas.push(json!({ "type": "signature_delta", "signature": signature }));
                }
                (to_object(start), deltas)
            }
            45..=49 => {
                let data = self.one_of(&[json!("EmwKAhgB"), json!(""), json!(5)]);
                (
                    json!({ "type": "redacted_thinking", "data": data }),
                    Vec::new(),
                )
            }
            50..=74 => self.tool_use(),
            75..=84 => return self.web_search(),
            85..=89 => {
                let block = self.one_of(&[
                    json!({ "type": "server_tool_use", "id": "srvtoolu_x", "name": "code_execution", "input": {} }),
                    json!({ "type": "web_search_tool_result", "tool_use_id": "srvtoolu_none", "content": [] }),
                    json!({ "type": "server_tool_use", "name": "web_search" }),
                ]);
                (block, Vec::new())
            }
            _ => {
                let block = self.one_of(&[
                    json!({}),
                    json!("text"),
                    Value::Null,
                    json!({ "type": "citations" }),
                    json!({ "type": "image" }),
                ]);
                let delta = self.one_of(&[
                    json!({ "type": "citations_delta", "citation": {} }),
                    json!({ "type": "unknown_delta" }),
                    json!({ "type": "text_delta" }),
                ]);
                (block, vec![delta])
            }
        };
        vec![block]
    }

    fn citation_delta(&mut self) -> Value {
        let citation = self.one_of(&[
            json!({
                "type": "web_search_result_location",
                "url": "https://example.com/r",
                "title": "Result",
                "encrypted_index": "Eo8BCioIBxgCIiQ4",
                "cited_text": "Rust is fast."
            }),
            json!({ "url": "https://example.com/b", "title": "B", "cited_text": "1.50", "score": 1.50 }),
            json!({}),
            Value::Null,
        ]);
        if self.rng.chance(10) {
            return json!({ "type": "citations_delta" });
        }
        json!({ "type": "citations_delta", "citation": citation })
    }

    /// Text streamed in pieces as `kind` deltas holding `key`, now and then
    /// one that isn't a string.
    fn deltas(&mut self, kind: &str, key: &str) -> Vec<Value> {
        let text = self.text();
        self.chunks(&text)
            .into_iter()
            .map(|chunk| {
                let value = if self.rng.chance(4) {
                    self.loose_text()
                } else {
                    chunk.into()
                };
                to_object(vec![("type", kind.into()), (key, value)])
            })
            .collect()
    }

    fn tool_use(&mut self) -> (Value, Vec<Value>) {
        let mut block = vec![("type", json!("tool_use"))];
        match self.rng.below(100) {
            0..=84 => block.push(("id", format!("toolu_01{}", self.alphanumeric(20)).into())),
            85..=89 => block.push(("id", json!(""))),
            90..=94 => {}
            _ => block.push(("id", self.number())),
        }
        let patch = self.rng.chance(12);
        if self.rng.chance(95) {
            let name = if patch {
                json!("apply_patch")
            } else if !self.tool_names.is_empty() && self.rng.chance(75) {
                self.base.rng.pick(&self.base.tool_names)
            } else {
                self.tool_name()
            };
            block.push(("name", name));
        }
        match self.rng.below(100) {
            0..=84 => block.push(("input", json!({}))),
            85..=91 => block.push(("input", json!({ "city": "Paris" }))),
            92..=94 => block.push(("input", json!({ "input": PATCH }))),
            95..=96 => block.push(("input", json!("text"))),
            _ => {}
        }
        let arguments = if patch && self.rng.chance(70) {
            self.rng.pick(&STREAMED_ARGUMENTS[9..])
        } else {
            self.rng.pick(STREAMED_ARGUMENTS)
        };
        let deltas = if self.rng.chance(88) {
            self.chunks(arguments)
                .into_iter()
                .map(|chunk| {
                    if self.rng.chance(3) {
                        json!({ "type": "input_json_delta" })
                    } else {
                        json!({ "type": "input_json_delta", "partial_json": chunk })
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        (to_object(block), deltas)
    }

    /// A web search and, usually, its results.
    fn web_search(&mut self) -> Vec<(Value, Vec<Value>)> {
        let id = match self.rng.below(10) {
            0..=7 => format!("srvtoolu_{}", self.alphanumeric(16)),
            8 => "srvtoolu_é".to_owned(),
            _ => String::new(),
        };
        let mut start = vec![
            ("type", json!("server_tool_use")),
            ("id", json!(id)),
            ("name", json!("web_search")),
        ];
        let deltas = if self.rng.chance(70) {
            start.push(("input", json!({})));
            let query = self.rng.pick(&[
                r#"{"query":"rust serde"}"#,
                r#"{"query":" padded "}"#,
                r#"{"query":""}"#,
                r#"{"query":"#,
                "",
            ]);
            self.chunks(query)
                .into_iter()
                .map(|chunk| json!({ "type": "input_json_delta", "partial_json": chunk }))
                .collect()
        } else {
            start.push(("input", json!({ "query": "rust serde" })));
            Vec::new()
        };
        let mut blocks = vec![(to_object(start), deltas)];
        if self.rng.chance(85) {
            let tool_use_id = if self.rng.chance(92) {
                json!(id)
            } else {
                json!("srvtoolu_other")
            };
            let content = if self.rng.chance(85) {
                let count = self.rng.below(4);
                Value::Array(
                    (0..count)
                        .map(|_| match self.rng.below(10) {
                            0..=6 => {
                                let mut fields = vec![
                                    ("type", json!("web_search_result")),
                                    ("url", json!("https://example.com/r")),
                                    ("title", json!("Result")),
                                    ("encrypted_content", json!("EqgfCioIARgBIiQ3")),
                                ];
                                if self.rng.chance(40) {
                                    fields.push(("page_age", json!("2 days ago")));
                                }
                                if self.rng.chance(10) {
                                    fields.push(("url", json!(" ")));
                                }
                                self.object(fields)
                            }
                            7 => json!({ "type": "web_search_result", "title": "no url" }),
                            8 => json!({ "type": "web_search_tool_result_error", "error_code": "too_many_requests" }),
                            _ => json!(5),
                        })
                        .collect(),
                )
            } else {
                self.one_of(&[
                    json!({ "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" }),
                    json!("results"),
                    Value::Null,
                ])
            };
            let result = json!({
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": content
            });
            blocks.push((result, Vec::new()));
        }
        blocks
    }

    fn message_delta(&mut self) -> Value {
        let mut delta = Vec::new();
        if self.rng.chance(90) {
            let reason = self.loose_choice(&[
                "end_turn",
                "end_turn",
                "tool_use",
                "max_tokens",
                " MAX_TOKENS ",
                "stop_sequence",
                "refusal",
                "pause_turn",
                "model_context_window_exceeded",
                "",
            ]);
            delta.push(("stop_reason", reason));
        }
        if self.rng.chance(30) {
            delta.push(("stop_sequence", self.one_of(&[Value::Null, json!("END")])));
        }
        let mut fields = vec![
            ("type", json!("message_delta")),
            ("delta", to_object(delta)),
        ];
        if self.rng.chance(85) {
            let usage = self.usage();
            fields.push(("usage", usage));
        }
        to_object(fields)
    }

    fn error(&mut self) -> Value {
        self.one_of(&[
            json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
            json!({ "type": "error", "error": { "message": "bad" } }),
            json!({ "type": "error" }),
        ])
    }

    /// `text` cut at one or two random character boundaries, sometimes with an
    /// empty piece after.
    fn chunks(&mut self, text: &str) -> Vec<String> {
        let boundaries: Vec<usize> = text.char_indices().skip(1).map(|(at, _)| at).collect();
        let mut cuts = Vec::new();
        if !boundaries.is_empty() {
            for _ in 0..self.rng.below(3) {
                cuts.push(self.rng.pick(&boundaries));
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        let mut pieces = Vec::new();
        let mut from = 0;
        for cut in cuts {
            pieces.push(text[from..cut].to_owned());
            from = cut;
        }
        pieces.push(text[from..].to_owned());
        if self.rng.chance(5) {
            pieces.push(String::new());
        }
        pieces
    }

    /// An event as a stream line: usually `data: <JSON>`, sometimes spaced or
    /// escaped differently, and now and then a line that isn't data.
    fn line(&mut self, event: &Value) -> String {
        let json = event.to_string();
        match self.rng.below(100) {
            0..=79 => format!("data: {json}"),
            80..=84 => format!("data:{json}"),
            85..=87 => format!("data: {}", escape_text(&json)),
            88..=89 => format!("data:  {json} \r"),
            90..=91 => json,
            92..=93 => format!("event: {}", event_type(event)),
            94 => String::new(),
            95 => "data: [DONE]".to_owned(),
            96 => ": keep-alive".to_owned(),
            97 => format!(" data: {json}"),
            _ => "data: {not json".to_owned(),
        }
    }

    /// The whole stream as one SSE body, for the non-streaming translator.
    fn body(&mut self, events: &[Value]) -> String {
        let newline = if self.rng.chance(10) { "\r\n" } else { "\n" };
        let mut lines = Vec::new();
        for event in events {
            if self.rng.chance(40) {
                lines.push(format!("event: {}", event_type(event)));
            }
            lines.push(self.line(event));
            if self.rng.chance(50) {
                lines.push(String::new());
            }
        }
        lines.join(newline)
    }
}

/// `qualifyResponsesNamespaceToolName`: a namespace child's full name.
pub(super) fn qualify(namespace: &str, child: &str) -> String {
    let child = child.trim();
    if child.is_empty()
        || namespace.is_empty()
        || child.starts_with("mcp__")
        || child == namespace
        || child.starts_with(&format!("{namespace}__"))
    {
        child.to_owned()
    } else if namespace.ends_with("__") {
        format!("{namespace}{child}")
    } else {
        format!("{namespace}__{child}")
    }
}

/// A Claude thinking signature naming `model`, laid out as upstream's tests
/// build one.
pub fn claude_signature(model: &str) -> String {
    fn varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push(value as u8 | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }
    fn bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
        varint(out, field << 3 | 2);
        varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }
    let mut channel = Vec::new();
    varint(&mut channel, 1 << 3);
    varint(&mut channel, 12);
    varint(&mut channel, 2 << 3);
    varint(&mut channel, 2);
    bytes(&mut channel, 6, model.as_bytes());
    let mut container = Vec::new();
    bytes(&mut container, 1, &channel);
    let mut payload = Vec::new();
    bytes(&mut payload, 2, &container);
    varint(&mut payload, 3 << 3);
    varint(&mut payload, 1);
    STANDARD.encode(payload)
}

/// Whether upstream's output for these stream lines could depend on Go's
/// map iteration order.
///
/// When a content block starts, upstream finishes every earlier tool call
/// not yet finished by ranging over a map keyed by block index; at
/// `message_stop` it finishes all of them the same way. Each finished call
/// writes events, so with two or more to finish at once the order is
/// random. This follows the lines as upstream reads them: an event counts
/// only on a `data:` line, `message_start` with a message starts over, and
/// a block index is read as gjson's `Int` reads it. A call that started but
/// hasn't stopped may or may not be finished by a later block, so it is
/// counted as pending until `message_stop`, which errs on the safe side.
fn map_order_hazard(lines: &[String]) -> bool {
    let mut pending = BTreeSet::new();
    let mut stopped = BTreeSet::new();
    for line in lines {
        let Some(event) = data_event(line) else {
            continue;
        };
        let index = gjson_int(event.get("index"));
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") if event.get("message").is_some() => {
                pending.clear();
                stopped.clear();
            }
            Some("content_block_start") if event.get("content_block").is_some() => {
                if pending.iter().filter(|&&other| other != index).count() >= 2 {
                    return true;
                }
                pending.retain(|other| *other == index || !stopped.contains(other));
                if block_type(&event) == Some("tool_use") {
                    pending.insert(index);
                }
            }
            Some("content_block_stop") => {
                stopped.insert(index);
            }
            Some("message_stop") => return pending.len() >= 2,
            _ => {}
        }
    }
    false
}

/// The event on a line upstream reads, `data:` and JSON.
fn data_event(line: &str) -> Option<Value> {
    serde_json::from_str(line.strip_prefix("data:")?.trim()).ok()
}

/// gjson's `Int` of an event's `index`, for the values the generator writes.
fn gjson_int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64))
            .unwrap_or_default(),
        Some(Value::String(text)) => text.parse().unwrap_or_default(),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

/// The type of a `content_block_start` event's block.
fn block_type(event: &Value) -> Option<&str> {
    event.get("content_block")?.get("type")?.as_str()
}

/// The type of a `content_block_delta` event's delta.
fn delta_type(event: &Value) -> Option<&str> {
    event.get("delta")?.get("type")?.as_str()
}

fn event(kind: &str, index: &Option<Value>, fields: Vec<(&str, Value)>) -> Value {
    let mut all = vec![("type", json!(kind))];
    if let Some(index) = index {
        all.push(("index", index.clone()));
    }
    all.extend(fields);
    to_object(all)
}

fn event_type(event: &Value) -> &str {
    event.get("type").and_then(Value::as_str).unwrap_or("event")
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
        let (streams, finals) = event_cases(7, 200);
        let (again, finals_again) = event_cases(7, 200);
        for (a, b) in streams.iter().zip(&again) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.translated_request, b.translated_request);
            assert_eq!(a.events, b.events);
        }
        for (a, b) in finals.iter().zip(&finals_again) {
            assert_eq!(a.events, b.events);
        }
    }

    #[test]
    fn streams_never_finish_two_calls_at_once() {
        let (streams, _) = event_cases(3, 2000);
        assert!(streams.iter().all(|case| !map_order_hazard(&case.events)));
        let calls = streams
            .iter()
            .filter(|case| {
                case.events
                    .iter()
                    .any(|line| line.contains(r#""tool_use""#))
            })
            .count();
        assert!(calls > 400, "{calls} streams with tool calls");
    }

    #[test]
    fn map_order_hazard_counts_pending_calls() {
        let line = |event: Value| format!("data: {event}");
        let tool = |index: u64| {
            line(
                json!({ "type": "content_block_start", "index": index, "content_block": { "type": "tool_use", "name": "a" } }),
            )
        };
        let stop = |index: u64| line(json!({ "type": "content_block_stop", "index": index }));
        let text = |index: u64| {
            line(
                json!({ "type": "content_block_start", "index": index, "content_block": { "type": "text" } }),
            )
        };
        let message_stop = line(json!({ "type": "message_stop" }));
        // One at a time is fine.
        assert!(!map_order_hazard(&[
            tool(0),
            stop(0),
            tool(1),
            stop(1),
            text(2),
            message_stop.clone()
        ]));
        // Two stopped calls finished together by the next block.
        assert!(map_order_hazard(&[
            tool(0),
            tool(1),
            stop(0),
            stop(1),
            text(2)
        ]));
        // A stopped call is finished by the next block, leaving one.
        assert!(!map_order_hazard(&[
            tool(0),
            stop(0),
            tool(1),
            message_stop.clone()
        ]));
        // Two left open for message_stop.
        assert!(map_order_hazard(&[tool(0), tool(1), message_stop.clone()]));
        // Lines upstream doesn't read don't count.
        assert!(!map_order_hazard(&[
            tool(0),
            tool(1).replacen("data: ", "", 1),
            message_stop
        ]));
    }

    /// Guards against a generator that never reaches the translators' branches.
    #[test]
    fn request_cases_cover_the_translators_branches() {
        let cases = request_cases(1, 2000);
        let outputs = |translator: Translator| -> Vec<String> {
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
        let requests = outputs(Translator::ClaudeResponsesRequest);
        check(
            &requests,
            &[
                r#""system":[{"type":"text""#,
                r#""cache_control":{"type":"ephemeral""#,
                r#""citations":[{"#,
                r#""type":"tool_use""#,
                r#""type":"tool_result""#,
                r#""type":"image""#,
                r#""type":"document""#,
                r#""type":"thinking""#,
                r#""type":"redacted_thinking""#,
                r#""type":"server_tool_use""#,
                r#""type":"web_search_tool_result""#,
                r#""type":"web_search_20250305""#,
                r#""tool_choice":{"type":"any""#,
                r#""tool_choice":{"name":"#,
                r#""type":"adaptive""#,
                r#""budget_tokens""#,
                r#""effort""#,
                r#""speed":"fast""#,
                r#""user_id""#,
                "toolu_(generated-1)",
                "Tool result was empty.",
            ],
        );
        let compat = outputs(Translator::ClaudeResponsesRequestCompat);
        check(&compat, &[r#""type":"thinking""#]);
    }

    #[test]
    fn event_cases_cover_the_translators_branches() {
        let (streams, finals) = event_cases(1, 2000);
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
        let streams = outputs(Translator::ClaudeResponsesStream, &streams);
        check(
            &streams,
            &[
                "response.output_text.delta",
                "response.reasoning_summary_text.delta",
                "response.function_call_arguments.delta",
                r#""type":"function_call""#,
                r#""type":"custom_tool_call""#,
                r#""type":"web_search_call""#,
                r#""annotations":[{"#,
                "response.completed",
                "response.incomplete",
                "response.failed",
                r#""created_at":"(now)""#,
                r#""cached_tokens""#,
            ],
        );
        let finals = outputs(Translator::ClaudeResponsesNonStream, &finals);
        check(
            &finals,
            &[
                r#""type":"message""#,
                r#""type":"reasoning""#,
                r#""type":"function_call""#,
                r#""type":"web_search_call""#,
                r#""status":"incomplete""#,
                r#""created_at":"(now)""#,
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
