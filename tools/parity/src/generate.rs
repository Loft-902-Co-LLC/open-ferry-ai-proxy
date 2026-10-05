//! Seeded random Claude Messages requests.
//!
//! The generator aims at the translator's corners rather than at realistic
//! traffic: loosely typed fields that gjson coerces, names and IDs around the
//! 64-byte limit, multi-byte characters at cut points, tool results out of
//! order, and text that Go and Rust might case-map or trim differently.
//!
//! [`response`] generates Codex event streams for the Claude response
//! translators, [`responses`] input for the Responses translators, [`chat`]
//! and [`claude_chat`] input for the Chat Completions translators to Codex and
//! Claude, [`claude_responses`] input for the Responses translators to
//! Claude, [`openai_responses`] and [`openai_claude`] input for the
//! translators between Chat Completions and Responses or Claude,
//! [`openai_chat`] the Chat Completions responses those read and input for
//! the Chat Completions passthrough, [`completions`] input for the legacy
//! Completions conversions, [`gemini`] input for the translators from Gemini
//! clients, [`signature`] reasoning signatures from every provider,
//! [`registry`] input for the translator registry, and
//! [`gemini_responses`] input for the translators between Responses and Gemini.
//! [`interactions`] generates Gemini Interactions requests, event streams and
//! responses, which the Interactions families' generators build on.
//! [`thinking`] generates input for upstream's thinking settings on Codex
//! and Chat Completions targets.
//! [`codex_models`] generates registrations for the Codex client model list.
//! [`multi_agent`] generates Codex clients' multi-agent v2 requests, Codex
//! sub-agents' delegation outputs, and upstream events to restore.
//! [`config_diff`] generates config pairs for the config change details.
//! [`payload`] generates payload rules with the requests and calls they
//! apply to.
//! [`usage`] generates upstream answers and stream lines for usage
//! parsing, and [`ttft`] stream events for the first-token classifiers.

pub mod chat;
pub mod claude_chat;
pub mod claude_responses;
pub mod codex_models;
pub mod completions;
pub mod config_diff;
pub mod gemini;
pub mod gemini_responses;
pub mod interactions;
pub mod multi_agent;
pub mod openai_chat;
pub mod openai_claude;
pub mod openai_responses;
pub mod payload;
pub mod registry;
pub mod response;
pub mod responses;
pub mod signature;
pub mod thinking;
pub mod to_gemini;
pub mod ttft;
pub mod usage;

use open_ferry_translate::json::exact;
use serde_json::{Value, json};

use crate::cases::Case;

/// Builds `count` random cases. Each case depends only on `seed` and its index.
pub fn cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let model = generator.model();
            let request = generator.request();
            let text = generator.render(&request);
            Case::new(format!("random-{seed}-{index}"), model, text)
        })
        .collect()
}

const MODELS: &[&str] = &[
    "gpt-5",
    "gpt-5",
    "gpt-5-codex",
    "gpt-5.1-codex-max",
    "o3",
    "",
    "gpt-5(high)",
    "  GPT-5  ",
    "grok-4",
    "grok-4(high)",
    " GROK-4.5 ",
    "gpt-5.4(grok)",
];

const TEXTS: &[&str] = &[
    "",
    " ",
    "\n",
    "hello",
    "What's the weather in Paris?",
    "line one\nline two",
    "tab\tand\rreturn",
    "quote \" backslash \\ slash /",
    "<b>html</b> & 'entities'",
    "café naïve façade",
    "日本語のテキスト",
    "emoji 🚀🔥👍🏽",
    "İstanbul ΑΣ",
    "\u{0}nul \u{1f}unit \u{7f}del",
    "\u{2028}separators\u{2029}",
    "\u{feff}byte order mark",
    "\u{85}next line\u{a0}no-break space",
    "x-anthropic-billing-header: cc_version=2.1.0; cc_entrypoint=cli",
    "  x-anthropic-billing-header:indented",
    "\u{a0}\u{3000}x-anthropic-billing-header: unicode spaces",
    "\u{200b}x-anthropic-billing-header: zero-width space",
    "X-Anthropic-Billing-Header: wrong case",
    "<system-reminder>\nalready wrapped\n</system-reminder>",
    "{\"looks\":\"like json\"}",
    "[1,2]",
    "null",
    "123",
    "1.50",
];

/// Number literals. Through [`num`], `1.50` and long integers reach the
/// translator as written, but `-0`, `-0.0`'s sign aside, and exponents are
/// respelled as `serde_json` reads them; through [`lit`], all are as
/// written. The last three are float64s halfway between two shortest
/// decimals, which Go rounds to even: `2156163594508435.2`,
/// `-628643006909686.2` and `2.9802322387695312e-8`.
const NUMBERS: &[&str] = &[
    "0",
    "-0",
    "1",
    "42",
    "-7",
    "0.5",
    "1.50",
    "2.0",
    "-0.0",
    "1e3",
    "1E+2",
    "-1.5e-3",
    "0.1",
    "9007199254740993",
    "123456789012345678901234567890",
    "1e30",
    "5e-324",
    "2156163594508435.25",
    "-628643006909686.25",
    "2.98023223876953125e-8",
];

const BUDGETS: &[&str] = &[
    "-2",
    "-1",
    "0",
    "1",
    "511",
    "512",
    "513",
    "1024",
    "1025",
    "8192",
    "8193",
    "24576",
    "24577",
    "100000",
    "1024.9",
    "512.0",
    "1e3",
    "9223372036854775807",
    "9223372036854775808",
    "99999999999999999999",
    "-9223372036854775809",
];

const PROPERTY_NAMES: &[&str] = &[
    "city", "unit", "count", "$id", "$schema", "pattern", "type", "名前", "a b", "",
];

const PATTERNS: &[&str] = &[
    r"^[a-z]+$",
    r"^\p{L}+$",
    r"\P{Lu}*",
    r"^\\p{L}$",
    r"^\d{3}$",
    r"[^\p{Cc}]",
    r"\p",
];

const PATTERN_KEYS: &[&str] = &[r"^x-", r"^\p{L}+$", r"^\\p{L}$", r"\P{N}"];

/// Effort names, including ones Go and Rust lowercase differently.
const EFFORTS: &[&str] = &[
    "low", "medium", "HIGH", " Medium ", "xhigh", "max", "", "MAXİMUM", "ΑΣ",
];

const SERVICE_TIERS: &[&str] = &[
    "fast",
    "priority",
    "Priority",
    " fast ",
    "flex",
    "default",
    "auto",
    "",
    "PRİORİTY",
];

/// SplitMix64. Plenty for picking test inputs, and needs no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())].clone()
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

struct Generator {
    rng: Rng,
    /// Names declared in `tools`, for `tool_use` parts and `tool_choice` to use.
    tool_names: Vec<Value>,
    /// IDs of the `tool_use` parts in the latest assistant message.
    tool_use_ids: Vec<Value>,
}

impl Generator {
    fn new(seed: u64, index: u64) -> Self {
        Self {
            rng: Rng(seed ^ index.wrapping_mul(0xD1B5_4A32_D192_ED03)),
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
        }
    }

    fn model(&mut self) -> String {
        self.rng.pick(MODELS).to_owned()
    }

    /// Serializes the request compactly or pretty-printed, sometimes with every
    /// non-ASCII character and `/` escaped. Upstream copies raw bytes in places,
    /// so the text form matters as well as the value.
    fn render(&mut self, request: &Value) -> String {
        let text = if self.rng.chance(30) {
            serde_json::to_string_pretty(request)
        } else {
            serde_json::to_string(request)
        }
        .expect("a Value always serializes");
        if self.rng.chance(20) {
            escape_text(&text)
        } else {
            text
        }
    }

    fn request(&mut self) -> Value {
        let mut fields = Vec::new();
        // Tools come first so messages and tool_choice can use their names.
        if self.rng.chance(60) {
            fields.push(("tools", self.tools()));
        }
        if self.rng.chance(85) {
            fields.push(("model", "claude-sonnet-4-5".into()));
        }
        if self.rng.chance(50) {
            fields.push(("system", self.system()));
        }
        if self.rng.chance(95) {
            fields.push(("messages", self.messages()));
        }
        if self.rng.chance(35) {
            fields.push(("tool_choice", self.tool_choice()));
        }
        if self.rng.chance(40) {
            fields.push(("thinking", self.thinking()));
        }
        if self.rng.chance(35) {
            fields.push(("output_config", self.output_config()));
        }
        if self.rng.chance(15) {
            let tier = self.loose_choice(SERVICE_TIERS);
            fields.push(("service_tier", tier));
        }
        if self.rng.chance(10) {
            let speed = self.one_of(&[json!("fast"), json!("Fast"), json!("standard"), json!(5)]);
            fields.push(("speed", speed));
        }
        if self.rng.chance(50) {
            fields.push(("max_tokens", num(self.rng.pick(&["1024", "32000", "1.5"]))));
        }
        if self.rng.chance(30) {
            fields.push(("stream", self.rng.chance(50).into()));
        }
        if self.rng.chance(15) {
            fields.push(("metadata", json!({ "user_id": "user_123" })));
        }
        if self.rng.chance(15) {
            fields.push(("temperature", self.number()));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
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
        match self.rng.below(20) {
            0 | 1 => self.web_search_tool(),
            2 => self.one_of(&[json!(1), json!("Bash"), Value::Null, json!([])]),
            _ => self.function_tool(),
        }
    }

    fn function_tool(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(92) {
            let name = self.tool_name();
            self.tool_names.push(name.clone());
            fields.push(("name", name));
        }
        if self.rng.chance(70) {
            fields.push(("description", self.text().into()));
        }
        if self.rng.chance(88) {
            let schema = if self.rng.chance(90) {
                self.schema(0)
            } else {
                self.one_of(&[
                    Value::Null,
                    json!("schema"),
                    json!([{ "type": "object" }]),
                    json!(5),
                ])
            };
            fields.push(("input_schema", schema));
        }
        if self.rng.chance(25) {
            let tool_type = self.one_of(&[
                json!("custom"),
                json!("function"),
                json!("Function"),
                json!("bash_20250124"),
                json!(5),
                Value::Null,
            ]);
            fields.push(("type", tool_type));
        }
        if self.rng.chance(20) {
            let strict = self.one_of(&[
                json!(true),
                json!(false),
                json!("false"),
                json!(0),
                Value::Null,
            ]);
            fields.push(("strict", strict));
        }
        if self.rng.chance(20) {
            fields.push(("cache_control", json!({ "type": "ephemeral" })));
        }
        if self.rng.chance(10) {
            fields.push(("defer_loading", true.into()));
        }
        if self.rng.chance(5) {
            fields.push(("parameters", json!({ "type": "string" })));
        }
        if self.rng.chance(10) {
            fields.push(("input_examples", json!([{ "city": "Paris" }])));
        }
        self.object(fields)
    }

    fn web_search_tool(&mut self) -> Value {
        let tool_type = self
            .rng
            .pick(&["web_search_20250305", "web_search_20260209"]);
        let mut fields = vec![("type", tool_type.into())];
        if self.rng.chance(85) {
            let name = self.one_of(&[json!("web_search"), json!("search"), json!("")]);
            self.tool_names.push(name.clone());
            fields.push(("name", name));
        }
        if self.rng.chance(40) {
            fields.push(("max_uses", 5.into()));
        }
        if self.rng.chance(40) {
            let domains = self.one_of(&[
                json!(["example.com", "docs.rs"]),
                json!("example.com"),
                json!([]),
            ]);
            fields.push(("allowed_domains", domains));
        }
        if self.rng.chance(20) {
            fields.push(("blocked_domains", json!(["spam.example"])));
        }
        if self.rng.chance(30) {
            let location = self.one_of(&[
                json!({ "type": "approximate", "city": "Paris", "country": "FR" }),
                json!("Paris"),
                Value::Null,
            ]);
            fields.push(("user_location", location));
        }
        self.object(fields)
    }

    fn tool_name(&mut self) -> Value {
        if !self.tool_names.is_empty() && self.rng.chance(20) {
            return self.rng.pick(&self.tool_names);
        }
        match self.rng.below(17) {
            0 => "get_weather".into(),
            1 => "Bash".into(),
            2 => "mcp__github__create_issue".into(),
            3 => "a".repeat(64).into(),
            // Over the limit and equal once cut, so these collide.
            4 => format!("{}{}", "a".repeat(64), self.rng.below(3)).into(),
            5 => format!("mcp__{}__do_thing", "server_".repeat(10)).into(),
            6 => format!("mcp__srv__{}", "t".repeat(70)).into(),
            7 => format!("mcp___{}", "z".repeat(70)).into(),
            // Cutting these at 64 bytes splits a two-byte character.
            8 => format!("a{}", "é".repeat(40)).into(),
            9 => format!("mcp__{}__outil_{}", "x".repeat(60), "é".repeat(30)).into(),
            10 => "outil_météo".into(),
            11 => "日本語ツール".into(),
            12 => "name with spaces".into(),
            13 => "".into(),
            14 => self.number(),
            15 => Value::Null,
            _ => "Read".into(),
        }
    }

    fn schema(&mut self, depth: usize) -> Value {
        let nested = depth < 3;
        let mut fields = Vec::new();
        let kind = self.rng.below(12);
        let schema_type = match kind {
            0..=4 => Some(json!("object")),
            5 => Some(json!("string")),
            6 => Some(json!("array")),
            7 => Some(json!(["object", "null"])),
            8 => None,
            9 => Some(Value::Null),
            10 => Some(json!("")),
            _ => Some(self.one_of(&[json!("integer"), json!(["string", "null"]), json!(5)])),
        };
        let objectish = !matches!(kind, 5 | 6 | 11);
        if let Some(schema_type) = schema_type {
            fields.push(("type", schema_type));
        }
        if self.rng.chance(25) {
            fields.push((
                "$schema",
                "https://json-schema.org/draft/2020-12/schema".into(),
            ));
        }
        if self.rng.chance(15) {
            fields.push(("$id", "urn:example:schema".into()));
        }
        if self.rng.chance(40) {
            fields.push(("description", self.text().into()));
        }

        if objectish && self.rng.chance(80) {
            if self.rng.chance(8) {
                fields.push(("properties", Value::Null));
            } else {
                let mut names: Vec<&str> = PROPERTY_NAMES.to_vec();
                self.rng.shuffle(&mut names);
                names.truncate(self.rng.below(5));
                let properties = names
                    .iter()
                    .map(|name| {
                        let schema = if nested {
                            self.schema(depth + 1)
                        } else {
                            json!({ "type": "string" })
                        };
                        (name.to_string(), schema)
                    })
                    .collect();
                fields.push(("properties", Value::Object(properties)));
                if self.rng.chance(60) {
                    let mut required: Vec<Value> = names.iter().map(|&name| name.into()).collect();
                    if self.rng.chance(40) {
                        required.truncate(self.rng.below(required.len() + 1));
                    }
                    let required = match self.rng.below(10) {
                        0 => json!("city"),
                        1 => {
                            required.push(5.into());
                            Value::Array(required)
                        }
                        _ => Value::Array(required),
                    };
                    fields.push(("required", required));
                }
            }
        }
        if objectish && nested && self.rng.chance(15) {
            let additional = match self.rng.below(3) {
                0 => json!(false),
                1 => json!(true),
                _ => self.schema(depth + 1),
            };
            fields.push(("additionalProperties", additional));
        }
        if objectish && nested && self.rng.chance(15) {
            let count = 1 + self.rng.below(3);
            let patterns = (0..count)
                .map(|_| {
                    (
                        self.rng.pick(PATTERN_KEYS).to_owned(),
                        self.schema(depth + 1),
                    )
                })
                .collect();
            fields.push(("patternProperties", Value::Object(patterns)));
        }
        if (kind == 5 && self.rng.chance(50)) || self.rng.chance(4) {
            fields.push(("pattern", self.rng.pick(PATTERNS).into()));
        }
        if kind == 6 && self.rng.chance(70) {
            let items = if nested && self.rng.chance(20) {
                json!([self.schema(depth + 1), self.schema(depth + 1)])
            } else if nested {
                self.schema(depth + 1)
            } else {
                json!({ "type": "string", "pattern": r"\p{L}" })
            };
            fields.push(("items", items));
        }
        if kind == 6 && nested && self.rng.chance(15) {
            fields.push(("prefixItems", json!([self.schema(depth + 1)])));
        }
        if nested && self.rng.chance(12) {
            let keyword = self.rng.pick(&["anyOf", "oneOf", "allOf"]);
            fields.push((
                keyword,
                json!([self.schema(depth + 1), self.schema(depth + 1)]),
            ));
        }
        if nested && self.rng.chance(6) {
            fields.push(("not", self.schema(depth + 1)));
        }
        if nested && self.rng.chance(5) {
            fields.push(("if", self.schema(depth + 1)));
            fields.push(("then", self.schema(depth + 1)));
            fields.push(("else", self.schema(depth + 1)));
        }
        if nested && self.rng.chance(8) {
            let keyword = self.rng.pick(&["$defs", "definitions"]);
            fields.push((keyword, json!({ "Thing": self.schema(depth + 1) })));
        }
        if nested && self.rng.chance(4) {
            let keyword = self.rng.pick(&["dependentSchemas", "dependencies"]);
            fields.push((keyword, json!({ "city": self.schema(depth + 1) })));
        }
        if self.rng.chance(12) {
            // Data, not schema: these keys must survive.
            let keyword = self.rng.pick(&["enum", "const", "default", "examples"]);
            let data = json!({ "$schema": "keep", "$id": "keep", "pattern": r"\p{L}" });
            fields.push((keyword, data));
        }
        if self.rng.chance(10) {
            fields.push(("minimum", self.number()));
        }
        self.object(fields)
    }

    // --- Messages ---

    fn system(&mut self) -> Value {
        match self.rng.below(8) {
            0..=2 => self.text().into(),
            3..=5 => {
                let count = 1 + self.rng.below(4);
                Value::Array(
                    (0..count)
                        .map(|_| match self.rng.below(8) {
                            0 => json!({ "type": "image" }),
                            1 => json!({ "text": "no type" }),
                            2 => json!("bare string"),
                            _ => {
                                let mut fields =
                                    vec![("type", "text".into()), ("text", self.loose_text())];
                                if self.rng.chance(20) {
                                    fields.push(("cache_control", json!({ "type": "ephemeral" })));
                                }
                                self.object(fields)
                            }
                        })
                        .collect(),
                )
            }
            6 => self.one_of(&[
                Value::Null,
                json!(5),
                json!({ "type": "text", "text": "an object" }),
            ]),
            _ => json!([]),
        }
    }

    fn messages(&mut self) -> Value {
        if self.rng.chance(3) {
            return self.one_of(&[json!({}), json!("hi"), Value::Null]);
        }
        let count = self.rng.below(8);
        let mut user_turn = false;
        let mut messages = Vec::with_capacity(count);
        for _ in 0..count {
            let message = match self.rng.below(100) {
                0..=9 => self.system_message(),
                10..=12 => self.odd_message(),
                _ => {
                    user_turn = !user_turn;
                    if user_turn {
                        self.user_message()
                    } else {
                        self.assistant_message()
                    }
                }
            };
            messages.push(message);
        }
        Value::Array(messages)
    }

    fn message(&mut self, role: Value, content: Option<Value>) -> Value {
        let mut fields = vec![("role", role)];
        if let Some(content) = content {
            fields.push(("content", content));
        }
        self.object(fields)
    }

    /// A mid-conversation system message, which becomes a `<system-reminder>`.
    fn system_message(&mut self) -> Value {
        let content = match self.rng.below(6) {
            0 | 1 => Some(self.text().into()),
            2 | 3 => {
                let count = 1 + self.rng.below(3);
                let blocks = (0..count)
                    .map(|_| match self.rng.below(6) {
                        0 => json!({ "type": "image" }),
                        1 => json!("bare string"),
                        _ => json!({ "type": "text", "text": self.loose_text() }),
                    })
                    .collect();
                Some(Value::Array(blocks))
            }
            4 => Some(self.one_of(&[
                json!("   "),
                Value::Null,
                json!(5),
                json!({ "type": "text" }),
            ])),
            _ => None,
        };
        self.message("system".into(), content)
    }

    fn odd_message(&mut self) -> Value {
        if self.rng.chance(30) {
            return self.one_of(&[json!("hello"), Value::Null, json!(5), json!([])]);
        }
        let role = self.one_of(&[
            json!("developer"),
            json!(""),
            json!("tool"),
            json!("USER"),
            json!(5),
            Value::Null,
        ]);
        let content = Some(self.content(|generator| generator.user_part()));
        self.message(role, content)
    }

    fn assistant_message(&mut self) -> Value {
        self.tool_use_ids.clear();
        let content = Some(self.content(|generator| generator.assistant_part()));
        self.message("assistant".into(), content)
    }

    fn user_message(&mut self) -> Value {
        let mut ids = std::mem::take(&mut self.tool_use_ids);
        if ids.is_empty() || self.rng.chance(15) {
            let content = Some(self.content(|generator| generator.user_part()));
            return self.message("user".into(), content);
        }

        match self.rng.below(10) {
            0..=4 => {}
            // Out of order: the translator puts results back in call order.
            5..=7 => self.rng.shuffle(&mut ids),
            8 => {
                ids.pop();
            }
            _ => ids.push(ids[0].clone()),
        }
        if self.rng.chance(10) {
            ids.push("toolu_unknown".into());
        }
        let mut parts: Vec<Value> = ids
            .into_iter()
            .map(|id| self.tool_result_part(Some(id)))
            .collect();
        for _ in 0..self.rng.below(3) {
            let part = self.user_part();
            let at = self.rng.below(parts.len() + 1);
            parts.insert(at, part);
        }
        self.message("user".into(), Some(Value::Array(parts)))
    }

    /// Message content: usually a list of parts, sometimes a string or junk.
    fn content(&mut self, mut part: impl FnMut(&mut Self) -> Value) -> Value {
        match self.rng.below(20) {
            0..=2 => self.text().into(),
            3 => self.one_of(&[
                Value::Null,
                json!(5),
                json!({ "type": "text", "text": "an object" }),
            ]),
            _ => {
                let count = 1 + self.rng.below(4);
                Value::Array((0..count).map(|_| part(self)).collect())
            }
        }
    }

    fn user_part(&mut self) -> Value {
        match self.rng.below(20) {
            0..=8 => self.text_part(),
            9..=12 => self.image_part(),
            13..=15 => self.document_part(),
            16 => {
                let id = self.tool_use_id();
                self.tool_result_part(id)
            }
            17 => self.thinking_part(),
            18 => {
                json!({ "type": "search_result", "source": "https://example.com", "title": "t", "content": [] })
            }
            _ => self.one_of(&[
                json!("bare string"),
                json!(5),
                Value::Null,
                json!({ "text": "no type" }),
            ]),
        }
    }

    fn assistant_part(&mut self) -> Value {
        match self.rng.below(20) {
            0..=6 => self.text_part(),
            7..=10 => self.thinking_part(),
            11..=16 => self.tool_use_part(),
            17 => json!({ "type": "redacted_thinking", "data": "EmwKAhgB" }),
            18 => json!({
                "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                "input": { "query": "rust" }
            }),
            _ => self.image_part(),
        }
    }

    fn text_part(&mut self) -> Value {
        let mut fields = vec![("type", "text".into())];
        if self.rng.chance(95) {
            fields.push(("text", self.loose_text()));
        }
        if self.rng.chance(10) {
            fields.push(("cache_control", json!({ "type": "ephemeral" })));
        }
        if self.rng.chance(5) {
            fields.push(("citations", json!([])));
        }
        self.object(fields)
    }

    fn thinking_part(&mut self) -> Value {
        let mut fields = vec![
            ("type", "thinking".into()),
            ("thinking", self.text().into()),
        ];
        if let Some(signature) = self.signature() {
            fields.push(("signature", signature));
        }
        self.object(fields)
    }

    fn image_part(&mut self) -> Value {
        let mut fields = vec![("type", "image".into())];
        if let Some(source) = self.image_source() {
            fields.push(("source", source));
        }
        self.object(fields)
    }

    fn image_source(&mut self) -> Option<Value> {
        Some(match self.rng.below(10) {
            0 => return None,
            1 => self.one_of(&[json!("abc"), Value::Null, json!([])]),
            2 => json!({ "type": "url", "url": "https://example.com/cat.png" }),
            _ => {
                let mut fields = vec![("type", "base64".into())];
                let data = self.one_of(&[
                    json!("iVBORw0KGgoAAAANSUhEUg=="),
                    json!(""),
                    json!(12),
                    Value::Null,
                ]);
                match self.rng.below(10) {
                    0..=6 => fields.push(("data", data)),
                    7 | 8 => fields.push(("base64", data)),
                    _ => {
                        fields.push(("data", "".into()));
                        fields.push(("base64", data));
                    }
                }
                let media_type = self.one_of(&[
                    json!("image/png"),
                    json!("image/jpeg"),
                    json!(""),
                    json!(" image/webp "),
                    Value::Null,
                ]);
                match self.rng.below(20) {
                    0..=13 => fields.push(("media_type", media_type)),
                    14..=16 => fields.push(("mime_type", media_type)),
                    _ => {}
                }
                self.object(fields)
            }
        })
    }

    fn document_part(&mut self) -> Value {
        let mut fields = vec![("type", "document".into())];
        if self.rng.chance(90) {
            let source_type = self
                .rng
                .pick(&["base64", "base64", "url", "text", "BASE64"]);
            let media_type = self.rng.pick(&[
                "application/pdf",
                "application/pdf",
                " Application/PDF ",
                "application/pdf\u{a0}",
                "APPLICATION/PDF",
                "text/plain",
                "",
            ]);
            let data = self.one_of(&[json!("JVBERi0xLjQK"), json!(""), json!(5)]);
            let data_key = if self.rng.chance(80) {
                "data"
            } else {
                "base64"
            };
            let source = self.object(vec![
                ("type", source_type.into()),
                ("media_type", media_type.into()),
                (data_key, data),
            ]);
            fields.push(("source", source));
        }
        if self.rng.chance(20) {
            fields.push(("title", "report.pdf".into()));
        }
        self.object(fields)
    }

    fn tool_use_part(&mut self) -> Value {
        let mut fields = vec![("type", "tool_use".into())];
        if let Some(id) = self.tool_use_id() {
            self.tool_use_ids.push(id.clone());
            fields.push(("id", id));
        }
        let name = if !self.tool_names.is_empty() && self.rng.chance(80) {
            self.rng.pick(&self.tool_names)
        } else {
            self.tool_name()
        };
        fields.push(("name", name));
        if let Some(input) = self.tool_input() {
            fields.push(("input", input));
        }
        if self.rng.chance(20) {
            self.with_tool_signature(&mut fields);
        }
        self.object(fields)
    }

    /// A signature on a `tool_use` part, under one of the keys upstream
    /// strips, and sometimes the model that made it.
    fn with_tool_signature(&mut self, fields: &mut Vec<(&str, Value)>) {
        let signature = self.signature().unwrap_or(Value::Null);
        match self.rng.below(6) {
            0 => fields.push(("signature", signature)),
            1 => fields.push(("thoughtSignature", signature)),
            2 => fields.push(("thought_signature", signature)),
            3 => {
                let google = json!({ "thought_signature": signature });
                fields.push(("extra_content", json!({ "google": google })));
            }
            4 => fields.push(("extra_content", json!({ "google": {} }))),
            _ => {}
        }
        if self.rng.chance(30) {
            let model = self.one_of(&[
                json!("claude-sonnet-4-6"),
                json!("gemini-3.1-pro"),
                json!("gpt-5.6-luna"),
                json!(""),
                json!(5),
            ]);
            fields.push(("model", model));
        }
    }

    fn tool_use_id(&mut self) -> Option<Value> {
        Some(match self.rng.below(14) {
            0..=7 => format!("toolu_{}", self.alphanumeric(24)).into(),
            // Over the 64-byte call_id limit.
            8 => format!("toolu_{}", self.alphanumeric(90)).into(),
            // Over the limit, and cutting it splits a character.
            9 => format!("toolu_{}", "é".repeat(40)).into(),
            10 => "".into(),
            11 => num(self.rng.pick(&["12345", "1.50"])),
            12 => return None,
            _ => match self.tool_use_ids.last() {
                Some(id) => id.clone(),
                None => Value::Null,
            },
        })
    }

    fn tool_input(&mut self) -> Option<Value> {
        Some(match self.rng.below(12) {
            0..=7 => {
                let mut keys = vec!["command", "path", "city", "n", "nested", "$schema"];
                self.rng.shuffle(&mut keys);
                keys.truncate(self.rng.below(5));
                let fields = keys
                    .into_iter()
                    .map(|key| {
                        let value = match self.rng.below(5) {
                            0 => self.number(),
                            1 => json!({ "deep": [1, "two", { "three": self.loose_text() }] }),
                            _ => self.loose_text(),
                        };
                        (key, value)
                    })
                    .collect();
                to_object(fields)
            }
            8 => self.loose_text(),
            9 => json!([1, "two", { "three": 3 }]),
            10 => Value::Null,
            _ => return None,
        })
    }

    fn tool_result_part(&mut self, id: Option<Value>) -> Value {
        let mut fields = vec![("type", "tool_result".into())];
        if let Some(id) = id {
            fields.push(("tool_use_id", id));
        }
        if let Some(content) = self.tool_result_content() {
            fields.push(("content", content));
        }
        if self.rng.chance(15) {
            fields.push(("is_error", true.into()));
        }
        self.object(fields)
    }

    fn tool_result_content(&mut self) -> Option<Value> {
        Some(match self.rng.below(10) {
            0..=3 => self.text().into(),
            4..=6 => {
                let count = 1 + self.rng.below(4);
                Value::Array(
                    (0..count)
                        .map(|_| match self.rng.below(8) {
                            0..=3 => self.text_part(),
                            4 | 5 => self.image_part(),
                            6 => json!({ "type": "search_result", "content": [{ "type": "text", "text": "x" }] }),
                            _ => self.one_of(&[json!("bare string"), json!(5)]),
                        })
                        .collect(),
                )
            }
            7 => self.one_of(&[json!([]), json!([{ "type": "document" }, "x"])]),
            8 => self.one_of(&[json!({ "result": 1.5 }), json!(7), json!(true), Value::Null]),
            _ => return None,
        })
    }

    /// A reasoning signature: usually GPT's, which Codex replays, otherwise
    /// any provider's. Now and then missing or not a string.
    fn signature(&mut self) -> Option<Value> {
        let signature = match self.rng.below(24) {
            0..=11 => {
                let signature = self.gpt_signature();
                self.damage(signature)
            }
            12..=21 => self.signature_text(),
            22 => return None,
            _ => return Some(self.one_of(&[json!(5), Value::Null, json!(true)])),
        };
        Some(signature.into())
    }

    // --- Other request fields ---

    fn tool_choice(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => {
                let mut fields = Vec::new();
                if self.rng.chance(90) {
                    let choice_type = self.one_of(&[
                        json!("auto"),
                        json!("any"),
                        json!("none"),
                        json!("tool"),
                        json!("tool"),
                        json!(""),
                        json!("AUTO"),
                        json!(5),
                    ]);
                    fields.push(("type", choice_type));
                }
                if self.rng.chance(60) {
                    let name = if !self.tool_names.is_empty() && self.rng.chance(70) {
                        self.rng.pick(&self.tool_names)
                    } else {
                        self.tool_name()
                    };
                    fields.push(("name", name));
                }
                if self.rng.chance(40) {
                    fields.push(("disable_parallel_tool_use", self.bool_like()));
                }
                self.object(fields)
            }
            6 => self.loose_choice(&["auto", "any", "none", "tool", "required", ""]),
            7 => Value::Null,
            8 => json!(5),
            _ => json!(["any"]),
        }
    }

    fn thinking(&mut self) -> Value {
        if self.rng.chance(10) {
            return self.one_of(&[json!("enabled"), json!(true), Value::Null, json!([])]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(95) {
            let thinking_type = self.one_of(&[
                json!("enabled"),
                json!("enabled"),
                json!("adaptive"),
                json!("auto"),
                json!("disabled"),
                json!("ENABLED"),
                json!(""),
                json!(5),
            ]);
            fields.push(("type", thinking_type));
        }
        if self.rng.chance(70) {
            let budget = match self.rng.below(10) {
                0..=6 => num(self.rng.pick(BUDGETS)),
                7 => self.loose_choice(&["2048", " 2048", "2048.0", "abc", "-1", ""]),
                8 => self.one_of(&[json!(true), json!(false), Value::Null]),
                _ => json!({}),
            };
            fields.push(("budget_tokens", budget));
        }
        self.object(fields)
    }

    fn output_config(&mut self) -> Value {
        if self.rng.chance(8) {
            return self.one_of(&[json!("json"), Value::Null, json!([])]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(60) {
            fields.push(("effort", self.loose_choice(EFFORTS)));
        }
        if self.rng.chance(60) {
            fields.push(("format", self.format()));
        }
        self.object(fields)
    }

    fn format(&mut self) -> Value {
        if self.rng.chance(10) {
            return self.one_of(&[json!("json_schema"), Value::Null, json!(5)]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(95) {
            let format_type =
                self.rng
                    .pick(&["json_schema", "json_schema", "JSON_SCHEMA", "text", ""]);
            fields.push(("type", format_type.into()));
        }
        if self.rng.chance(50) {
            let name = self.one_of(&[
                json!(""),
                json!("answer"),
                json!(5),
                Value::Null,
                json!("名前"),
            ]);
            fields.push(("name", name));
        }
        if self.rng.chance(40) {
            let strict = self.one_of(&[
                json!(true),
                json!(false),
                json!("false"),
                json!(0),
                Value::Null,
            ]);
            fields.push(("strict", strict));
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

    // --- Leaf values ---

    fn text(&mut self) -> String {
        match self.rng.below(10) {
            0 => format!("{}{}", self.rng.pick(TEXTS), self.rng.pick(TEXTS)),
            1 => {
                let unit = self.rng.pick(&["a", "é", "日", "🚀", "word "]);
                unit.repeat(10 + self.rng.below(60))
            }
            _ => self.rng.pick(TEXTS).to_owned(),
        }
    }

    /// A value where the protocol expects a string. gjson turns anything into
    /// a string, so this is usually text but not always.
    fn loose_text(&mut self) -> Value {
        if self.rng.chance(85) {
            return self.text().into();
        }
        match self.rng.below(4) {
            0 => self.number(),
            1 => self.one_of(&[json!(true), json!(false), Value::Null]),
            2 => json!({ "text": "nested" }),
            _ => json!(["a", 1]),
        }
    }

    /// One of `options` as a string, or now and then a non-string.
    fn loose_choice(&mut self, options: &[&str]) -> Value {
        if self.rng.chance(90) {
            self.rng.pick(options).into()
        } else {
            self.one_of(&[json!(5), Value::Null, json!(true)])
        }
    }

    fn bool_like(&mut self) -> Value {
        self.one_of(&[
            json!(true),
            json!(false),
            json!("true"),
            json!("TRUE"),
            json!("1"),
            json!("t"),
            json!("yes"),
            json!("false"),
            json!(1),
            json!(0),
            num("0.5"),
            Value::Null,
            json!({}),
        ])
    }

    fn number(&mut self) -> Value {
        num(self.rng.pick(NUMBERS))
    }

    fn alphanumeric(&mut self, len: usize) -> String {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        (0..len)
            .map(|_| char::from(CHARS[self.rng.below(CHARS.len())]))
            .collect()
    }

    fn one_of(&mut self, options: &[Value]) -> Value {
        self.rng.pick(options)
    }

    /// Builds an object, sometimes with its fields shuffled.
    fn object(&mut self, mut fields: Vec<(&str, Value)>) -> Value {
        if self.rng.chance(25) {
            self.rng.shuffle(&mut fields);
        }
        to_object(fields)
    }
}

fn to_object(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// Parses a number literal as `serde_json` reads it, which keeps its digits
/// (`arbitrary_precision` is on) but writes `-0` as `0` and an exponent with
/// a small `e` and a sign (`1E+2` as `1e+2`, `1e3` as `1e+3`). For a place
/// upstream reads as a number, where it converts the value anyway.
fn num(text: &str) -> Value {
    serde_json::from_str(text).expect("number literals are valid JSON")
}

/// Parses a number literal keeping its text exactly as written, `-0` and
/// exponents included. For a place where upstream copies the client's JSON
/// text (gjson's `Raw` into sjson's `SetRaw`), which the Interactions
/// translators keep as written.
fn lit(text: &str) -> Value {
    exact::from_str(text).expect("number literals are valid JSON")
}

/// Escapes every non-ASCII character as `\uXXXX`, and `/` as `\/`. Both only
/// occur inside strings in JSON text, so the value is unchanged.
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '/' => out.push_str("\\/"),
            c if c.is_ascii() => out.push(c),
            c => {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_lit_keeps_every_spelling() {
        for text in ["-0", "1E+2", "1e3", "-1.5e-3", "1.50", "-0.0"] {
            assert_eq!(lit(text).to_string(), text);
        }
        let respelled: Vec<String> = ["-0", "1E+2", "1e3", "1.50", "-0.0"]
            .iter()
            .map(|text| num(text).to_string())
            .collect();
        assert_eq!(respelled, ["0", "1e+2", "1e+3", "1.50", "-0.0"]);
    }

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        let first = cases(7, 200);
        let second = cases(7, 200);
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.request, b.request);
            serde_json::from_str::<Value>(&a.request).expect("generated request is valid JSON");
        }
    }

    #[test]
    fn escaped_text_parses_to_the_same_value() {
        let value = json!({ "a/b": "café 🚀 \\/ /" });
        let escaped = escape_text(&value.to_string());
        assert!(escaped.is_ascii());
        assert_eq!(serde_json::from_str::<Value>(&escaped).unwrap(), value);
    }
}
