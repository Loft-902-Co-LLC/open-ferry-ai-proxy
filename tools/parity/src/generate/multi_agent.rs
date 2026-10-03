//! Seeded random cases for a Codex client's multi-agent v2 requests and
//! orphan delegation outputs: collaboration tools at the top level, in
//! namespaces and in `additional_tools` items, with names, types and
//! `encrypted` marks that do and don't count; `spawn_agent` descriptions
//! with and without model sections and the marker; agent messages with
//! encrypted parts of every kind; registered models with and without a
//! catalog entry, odd descriptions and reasoning levels; delegation outputs
//! with and without their call; and upstream events with names to restore,
//! escapes and numbers Go's encoder writes its own way.

use serde_json::{Value, json};

use super::{Generator, num, to_object};
use crate::cases::Case;

const HEADING: &str = "Available model overrides (optional; inherited parent model is preferred):";

const USER_AGENTS: &[&str] = &[
    "codex-tui/0.154.0",
    "codex-tui/0.154.0",
    "Codex Desktop/0.146.0-alpha.3",
    "Codex Desktop/0.146.0-alpha.3",
    "codex_cli_rs/0.144.1",
    "codex_cli_rs/0.144.1",
    "codex_cli_rs",
    " codex_exec/1.2.3 ",
    "\u{a0}codex-tui/0.1\u{a0}",
    "codex-tui",
    "Codex Desktop",
    "CODEX-TUI/0.1",
    "codex_cli_rs_fake/1.0",
    "curl/8.7.1",
    "",
];

const SUBAGENTS: &[&str] = &[
    "collab_spawn",
    "collab_spawn",
    "collab_spawn",
    "COLLAB_SPAWN",
    " collab_spawn ",
    "Collab_Spawn",
    "collab-spawn",
    "other_subagent",
    "",
];

const TOOL_NAMES: &[&str] = &[
    "spawn_agent",
    "spawn_agent",
    "spawn_agent",
    "send_message",
    "send_message",
    "followup_task",
    " spawn_agent ",
    "Spawn_Agent",
    "list_agents",
    "unrelated_tool",
];

/// Names that use the optimized namespace's, which leave a request's
/// namespace alone.
const CONFLICT_NAMES: &[&str] = &[
    "collaboration-optimize",
    "collaboration-optimize__send_message",
    "collaboration-optimize.spawn_agent",
    " collaboration-optimize.x ",
];

const NAMESPACES: &[&str] = &[
    "collaboration",
    "collaboration",
    "collaboration",
    "collaboration",
    " collaboration ",
    "Collaboration",
    "agents",
    "collaboration-optimize-x",
];

const FUNCTION_TYPES: &[&str] = &[
    "function",
    "function",
    "function",
    "function",
    "function",
    " function ",
    "Function",
    "custom",
];

const AGENT_MESSAGE_TYPES: &[&str] = &[
    "agent_message",
    "agent_message",
    "agent_message",
    "agent_message",
    " agent_message ",
    "Agent_Message",
    "message",
];

const PART_TYPES: &[&str] = &[
    "encrypted_content",
    "encrypted_content",
    "encrypted_content",
    "input_text",
    " encrypted_content ",
    "output_text",
];

/// Models the Codex client catalog has an entry for.
const TEMPLATE_IDS: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "codex-auto-review",
];

/// Models the static catalog has, which lookups fall back to.
const STATIC_IDS: &[&str] = &["claude-sonnet-4-6", "gemini-2.5-pro", "kimi-k2-thinking"];

const CUSTOM_IDS: &[&str] = &[
    "custom-model",
    "alpha-model",
    "zeta-model",
    "Alpha",
    "my-model(8192)",
    "team/gpt-5.5",
    "GPT-5.5",
    "model  with   spaces",
    "back`tick",
    "\u{fc}nic\u{f6}de-model",
    "model.with.dots",
];

const DISPLAY_NAMES: &[&str] = &[
    "",
    "",
    "Alpha",
    "alpha",
    "Zeta",
    "GPT 5.5",
    "\u{dc}nic\u{f6}de",
    "<b>&</b>",
    "  Spaced  ",
    "Model",
];

const MODEL_DESCRIPTIONS: &[&str] = &[
    "",
    "",
    "  ",
    "Fast model.",
    "Ends with!",
    "Asks?",
    "No full stop",
    "Ends with an ellipsis...",
    "Multi\nline  text\twith   gaps",
    "<b>Bold</b> & more",
    "Has `code` in it",
    "\u{65e5}\u{672c}\u{8a9e}",
];

const LEVELS: &[&str] = &[
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "ultra",
    "LOW",
    " High ",
    "auto",
    "unsupported",
    "",
];

const PROVIDERS: &[&str] = &[
    "codex",
    "codex",
    "openai",
    "claude",
    "openai-compatibility",
    "gemini",
];

const CALL_IDS: &[&str] = &[
    "call_1", "call_1", "call_2", "call_3", " call_1 ", "", "  ", "5",
];

const DELEGATION_NAMES: &[&str] = &[
    "create_thread",
    "create_thread",
    "send_message_to_thread",
    "send_message_to_thread",
    "automation_update",
    " create_thread",
    "codex_app__create_thread",
];

const DELEGATION_NAMESPACES: &[&str] = &[
    "codex_app",
    "codex_app",
    "codex_app",
    "other_namespace",
    "Codex_App",
    " codex_app",
];

const RESTORE_TYPES: &[&str] = &[
    "function_call",
    "function_call",
    "custom_tool_call",
    "custom_tool_call",
    " function_call ",
    "function_call_output",
    "custom_tool_call_output",
    "namespace",
    "namespace",
    "message",
];

const RESTORE_NAMES: &[&str] = &[
    "collaboration-optimize.spawn_agent",
    "collaboration-optimize.send_message",
    "collaboration-optimize__send_message",
    "collaboration-optimize__",
    "collaboration-optimize.",
    "collaboration-optimize",
    "collaboration-optimize",
    "spawn_agent",
    " collaboration-optimize.x",
    "collaboration",
];

const EVENT_TYPES: &[&str] = &[
    "response.output_item.added",
    "response.output_item.done",
    "response.function_call_arguments.done",
    "response.completed",
];

/// Case options and requests for `multi-agent/prepare`.
pub fn prepare_cases(seed: u64, count: usize) -> Vec<Case> {
    request_cases(seed, count, false)
}

/// Case options and requests for `multi-agent/optimize`.
pub fn optimize_cases(seed: u64, count: usize) -> Vec<Case> {
    request_cases(seed, count, false)
}

/// Case options and requests for `multi-agent/input`.
pub fn input_cases(seed: u64, count: usize) -> Vec<Case> {
    request_cases(seed, count, true)
}

fn request_cases(seed: u64, count: usize, compat: bool) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let request = generator.multi_agent_request();
            let text = generator.render(&request);
            let options = generator.multi_agent_options(compat);
            Case::new(format!("random-{seed}-{index}"), "", text).with_options(options)
        })
        .collect()
}

/// Case options and requests for `multi-agent/orphan`.
pub fn orphan_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let (request, quoted) = generator.orphan_request();
            // Upstream quotes an output that isn't a string as the client
            // wrote it, and open-ferry as compact JSON (a documented
            // deviation), so such requests are written compactly.
            let text = if quoted {
                serde_json::to_string(&request).expect("a Value always serializes")
            } else {
                generator.render(&request)
            };
            let subagent = generator.rng.pick(SUBAGENTS);
            let options = json!({
                "user_agent": generator.rng.pick(USER_AGENTS),
                "subagent": subagent,
                "enabled": generator.rng.chance(85),
                "compat": false,
                "optimized": false,
                "registrations": [],
            });
            Case::new(format!("random-{seed}-{index}"), "", text).with_options(options)
        })
        .collect()
}

/// Events for `multi-agent/restore`.
pub fn restore_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let event = generator.restore_event();
            let mut text = generator.render(&event);
            match generator.rng.below(40) {
                0 => {
                    let cut = generator.rng.below(text.len().max(1));
                    text = text.chars().take(cut).collect();
                }
                1 => text.push_str(" \n"),
                2 => text.push_str(" {}"),
                _ => {}
            }
            let options = json!({ "optimized": generator.rng.chance(85) });
            Case::new(format!("random-{seed}-{index}"), "", text).with_options(options)
        })
        .collect()
}

impl Generator {
    fn multi_agent_options(&mut self, compat: bool) -> Value {
        let mut registrations = Vec::new();
        let mut ids: Vec<String> = Vec::new();
        if self.rng.chance(75) {
            for client in 0..=self.rng.below(3) {
                let mut models = Vec::new();
                for _ in 0..=self.rng.below(4) {
                    let id = self.multi_agent_model_id();
                    if ids.contains(&id) {
                        continue;
                    }
                    models.push(self.multi_agent_model(&id));
                    ids.push(id);
                }
                registrations.push(json!({
                    "client": format!("parity-multi-agent-{client}"),
                    "provider": self.rng.pick(PROVIDERS),
                    "models": models,
                }));
            }
        }
        json!({
            "user_agent": self.rng.pick(USER_AGENTS),
            "subagent": "",
            "enabled": self.rng.chance(85),
            "compat": compat && self.rng.chance(40),
            "optimized": false,
            "registrations": registrations,
        })
    }

    fn multi_agent_model_id(&mut self) -> String {
        let pool = match self.rng.below(10) {
            0..=3 => TEMPLATE_IDS,
            4 => STATIC_IDS,
            _ => CUSTOM_IDS,
        };
        self.rng.pick(pool).to_owned()
    }

    fn multi_agent_model(&mut self, id: &str) -> Value {
        let mut fields = vec![("id", json!(id))];
        if self.rng.chance(60) {
            fields.push(("display_name", self.rng.pick(DISPLAY_NAMES).into()));
        }
        if self.rng.chance(70) {
            fields.push(("description", self.rng.pick(MODEL_DESCRIPTIONS).into()));
        }
        if self.rng.chance(70) {
            let levels: Vec<&str> = (0..self.rng.below(6))
                .map(|_| self.rng.pick(LEVELS))
                .collect();
            fields.push(("thinking", json!({ "levels": levels })));
        }
        to_object(fields)
    }

    fn multi_agent_request(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(80) {
            fields.push(("model", "gpt-5.5".into()));
        }
        if self.rng.chance(80) {
            let tools = if self.rng.chance(95) {
                self.multi_agent_tools(0)
            } else {
                json!({"type": "function", "name": "spawn_agent"})
            };
            fields.push(("tools", tools));
        }
        if self.rng.chance(75) {
            let input = if self.rng.chance(95) {
                self.multi_agent_input()
            } else {
                self.text().into()
            };
            fields.push(("input", input));
        }
        if self.rng.chance(20) {
            fields.push(("instructions", self.text().into()));
        }
        if self.rng.chance(15) {
            fields.push(("temperature", self.number()));
        }
        if self.rng.chance(15) {
            fields.push(("stream", self.rng.chance(50).into()));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    fn multi_agent_tools(&mut self, depth: usize) -> Value {
        let tools = (0..=self.rng.below(4))
            .map(|_| self.multi_agent_tool(depth))
            .collect();
        Value::Array(tools)
    }

    fn multi_agent_tool(&mut self, depth: usize) -> Value {
        match self.rng.below(12) {
            0..=6 => self.collaboration_function(),
            7..=9 if depth < 2 => {
                let mut fields = vec![
                    (
                        "type",
                        self.loose_choice(&["namespace", "namespace", "namespace", " namespace "]),
                    ),
                    ("name", self.collaboration_name(NAMESPACES)),
                ];
                if self.rng.chance(30) {
                    fields.push(("description", self.text().into()));
                }
                let tools = if self.rng.chance(95) {
                    self.multi_agent_tools(depth + 1)
                } else {
                    json!("not tools")
                };
                fields.push(("tools", tools));
                self.object(fields)
            }
            10 => json!({"type": "web_search"}),
            _ => self.one_of(&[
                json!("spawn_agent"),
                json!(5),
                Value::Null,
                json!({"name": "spawn_agent", "description": "No type."}),
                json!({"type": "function", "name": 5}),
            ]),
        }
    }

    fn collaboration_function(&mut self) -> Value {
        let mut fields = vec![
            ("type", self.rng.pick(FUNCTION_TYPES).into()),
            ("name", self.collaboration_name(TOOL_NAMES)),
        ];
        if self.rng.chance(85) {
            let description = self.spawn_agent_description();
            fields.push(("description", description));
        }
        if self.rng.chance(85) {
            let parameters = self.collaboration_parameters();
            fields.push(("parameters", parameters));
        }
        if self.rng.chance(10) {
            fields.push(("strict", self.rng.chance(50).into()));
        }
        self.object(fields)
    }

    /// One of `names`, or now and then one of [`CONFLICT_NAMES`].
    fn collaboration_name(&mut self, names: &[&str]) -> Value {
        let names = if self.rng.chance(3) {
            CONFLICT_NAMES
        } else {
            names
        };
        self.rng.pick(names).into()
    }

    fn spawn_agent_description(&mut self) -> Value {
        let text = match self.rng.below(16) {
            0 | 1 => "Spawns an agent to work on a task.".to_owned(),
            2 => "Create a worker.".to_owned(),
            3 => String::new(),
            4 => "Create a worker.\n".to_owned(),
            5 => format!("{HEADING}\n- `old-model`: Old model.\nSpawns an agent."),
            6 => format!(
                "\n        {HEADING}\n- old duplicate\n- old duplicate\n        Spawns an agent to work on a task."
            ),
            7 => format!(
                "Intro.\n{HEADING}\n- a\n-b\n- c\nMiddle.\n  {HEADING}\n  - d\n\nSpawns an agent."
            ),
            8 => "Use this tool. Spawns an agent when needed.\nMore.".to_owned(),
            9 => HEADING.to_owned(),
            10 => format!("{HEADING}\n"),
            11 => "<b>Spawns an agent</b> & more".to_owned(),
            12 => format!("\u{a0}{HEADING}\n\u{a0}- x\nSpawns an agent"),
            13 => format!("Spawns an agent.\n{HEADING}\n- `x`: X.\n- `y`: Y."),
            14 => {
                return self.one_of(&[json!(42), Value::Null, json!({"text": "Spawns an agent."})]);
            }
            _ => self.text(),
        };
        Value::String(text)
    }

    fn collaboration_parameters(&mut self) -> Value {
        let encrypted = self.one_of(&[
            json!(true),
            json!(true),
            json!(true),
            json!(false),
            Value::Null,
            json!("yes"),
        ]);
        match self.rng.below(10) {
            0..=4 => {
                let mut message = vec![("type", json!("string"))];
                if self.rng.chance(85) {
                    message.push(("encrypted", encrypted));
                }
                if self.rng.chance(20) {
                    message.push(("description", self.text().into()));
                }
                let message = self.object(message);
                let mut properties = vec![("message", message)];
                if self.rng.chance(20) {
                    properties.push(("data", json!({"encrypted": "keep-me"})));
                }
                let properties = self.object(properties);
                let mut fields = vec![("type", json!("object")), ("properties", properties)];
                if self.rng.chance(20) {
                    fields.push(("required", json!(["message"])));
                }
                self.object(fields)
            }
            5 | 6 => json!({"properties": {"message": {"encrypted": encrypted}}}),
            7 => json!({"properties": {"other": {"encrypted": true}}}),
            8 => self.one_of(&[
                json!({"properties": "message"}),
                json!({"properties": {"message": ["encrypted"]}}),
                json!("parameters"),
            ]),
            _ => json!({"type": "object"}),
        }
    }

    fn multi_agent_input(&mut self) -> Value {
        let items = (0..self.rng.below(6))
            .map(|index| match self.rng.below(10) {
                0..=3 => self.agent_message(index),
                4 | 5 => {
                    let mut fields = vec![
                        ("type", self.loose_choice(&["additional_tools", "additional_tools", " additional_tools "])),
                        ("role", json!("developer")),
                    ];
                    let tools = if self.rng.chance(95) {
                        self.multi_agent_tools(0)
                    } else {
                        json!({})
                    };
                    fields.push(("tools", tools));
                    self.object(fields)
                }
                6 | 7 => {
                    let mut fields = vec![
                        ("type", json!("message")),
                        ("role", self.rng.pick(&["user", "assistant", "developer"]).into()),
                        ("content", self.text().into()),
                    ];
                    self.agent_metadata(&mut fields);
                    self.object(fields)
                }
                8 => json!({"type": "function_call", "call_id": "call_1", "name": "spawn_agent", "arguments": "{}"}),
                _ => self.one_of(&[json!("text"), json!(5), Value::Null, json!([1, 2])]),
            })
            .collect();
        Value::Array(items)
    }

    fn agent_message(&mut self, index: usize) -> Value {
        let mut fields = vec![
            ("type", self.rng.pick(AGENT_MESSAGE_TYPES).into()),
            ("id", json!(format!("amsg_{index}"))),
        ];
        if self.rng.chance(15) {
            fields.push(("role", self.rng.pick(&["assistant", "user"]).into()));
        }
        self.agent_metadata(&mut fields);
        let content = if self.rng.chance(92) {
            let parts = (0..self.rng.below(4))
                .map(|_| self.agent_message_part())
                .collect();
            Value::Array(parts)
        } else {
            self.text().into()
        };
        fields.push(("content", content));
        self.object(fields)
    }

    fn agent_metadata(&mut self, fields: &mut Vec<(&str, Value)>) {
        if self.rng.chance(50) {
            fields.push(("author", self.loose_choice(&["/root", "/root/worker"])));
        }
        if self.rng.chance(50) {
            fields.push(("recipient", self.loose_choice(&["/root/worker", "/root"])));
        }
        if self.rng.chance(40) {
            let passthrough =
                self.one_of(&[json!({"turn_id": "turn_1"}), Value::Null, json!("turn")]);
            fields.push(("internal_chat_message_metadata_passthrough", passthrough));
        }
    }

    fn agent_message_part(&mut self) -> Value {
        if self.rng.chance(5) {
            return json!("part");
        }
        let part_type = self.rng.pick(PART_TYPES);
        let mut fields = vec![("type", json!(part_type))];
        if part_type.contains("encrypted") {
            if self.rng.chance(90) {
                let content = self.loose_text();
                fields.push(("encrypted_content", content));
            }
            if self.rng.chance(10) {
                fields.push(("text", self.text().into()));
            }
        } else {
            fields.push(("text", self.text().into()));
        }
        self.object(fields)
    }

    /// A sub-agent's request, and whether one of its delegation outputs
    /// isn't a string.
    fn orphan_request(&mut self) -> (Value, bool) {
        let mut quoted = false;
        let items = (0..self.rng.below(6))
            .map(|_| match self.rng.below(10) {
                0..=4 => {
                    let mut fields = vec![
                        ("type", self.loose_choice(&["function_call_output", "function_call_output", " function_call_output"])),
                    ];
                    if let Some(call_id) = self.call_id() {
                        fields.push(("call_id", call_id));
                    }
                    if self.rng.chance(95) {
                        fields.push(("name", self.rng.pick(DELEGATION_NAMES).into()));
                    }
                    if self.rng.chance(90) {
                        fields.push(("namespace", self.rng.pick(DELEGATION_NAMESPACES).into()));
                    }
                    if self.rng.chance(95) {
                        let output = match self.rng.below(10) {
                            0..=6 => self.text().into(),
                            7 => json!([{"type": "input_text", "text": self.text()}]),
                            8 => self.one_of(&[Value::Null, json!(5), json!(true), num("1.50")]),
                            _ => json!({"text": self.text(), "n": 1}),
                        };
                        quoted |= !output.is_string();
                        fields.push(("output", output));
                    }
                    self.object(fields)
                }
                5..=7 => {
                    let mut fields = vec![(
                        "type",
                        self.loose_choice(&["function_call", "function_call", "custom_tool_call", " function_call"]),
                    )];
                    if let Some(call_id) = self.call_id() {
                        fields.push(("call_id", call_id));
                    }
                    fields.push(("name", self.rng.pick(DELEGATION_NAMES).into()));
                    fields.push(("namespace", json!("codex_app")));
                    fields.push(("arguments", json!("{}")));
                    self.object(fields)
                }
                8 => json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": self.text()}]}),
                _ => self.one_of(&[json!("text"), json!(5), Value::Null]),
            })
            .collect();
        let mut fields = vec![("model", json!("deepseek-v4-pro"))];
        let input = if self.rng.chance(95) {
            Value::Array(items)
        } else {
            json!("text")
        };
        fields.push(("input", input));
        (to_object(fields), quoted)
    }

    fn call_id(&mut self) -> Option<Value> {
        match self.rng.below(10) {
            0..=6 => Some(self.rng.pick(CALL_IDS).into()),
            7 => Some(self.one_of(&[
                json!(5),
                num("1.50"),
                json!("1.5"),
                json!(true),
                Value::Null,
            ])),
            _ => None,
        }
    }

    fn restore_event(&mut self) -> Value {
        match self.rng.below(6) {
            0 | 1 => {
                let item = self.restore_item(0);
                let event_type = self.rng.pick(EVENT_TYPES);
                let output_index = self.number();
                self.object(vec![
                    ("type", json!(event_type)),
                    ("output_index", output_index),
                    ("item", item),
                ])
            }
            2 | 3 => {
                let output = (0..self.rng.below(4))
                    .map(|_| self.restore_item(0))
                    .collect();
                let mut response = vec![
                    ("id", json!("resp_1")),
                    ("status", json!("completed")),
                    ("output", Value::Array(output)),
                ];
                if self.rng.chance(40) {
                    let tools = (0..self.rng.below(3))
                        .map(|_| self.restore_item(1))
                        .collect();
                    response.push(("tools", Value::Array(tools)));
                }
                if self.rng.chance(30) {
                    response.push((
                        "usage",
                        json!({"input_tokens": self.number(), "output_tokens": 7}),
                    ));
                }
                let response = self.object(response);
                self.object(vec![
                    ("type", json!("response.completed")),
                    ("response", response),
                ])
            }
            4 => self.restore_item(0),
            _ => Value::Array(
                (0..self.rng.below(3))
                    .map(|_| self.restore_item(1))
                    .collect(),
            ),
        }
    }

    fn restore_item(&mut self, depth: usize) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(95) {
            fields.push(("type", self.loose_choice(RESTORE_TYPES)));
        }
        if self.rng.chance(85) {
            fields.push(("name", self.loose_choice(RESTORE_NAMES)));
        }
        if self.rng.chance(50) {
            let namespace = self.loose_choice(&[
                "collaboration-optimize",
                "collaboration-optimize",
                "collaboration",
                " collaboration-optimize",
            ]);
            fields.push(("namespace", namespace));
        }
        if self.rng.chance(40) {
            fields.push(("call_id", json!("call_1")));
        }
        if self.rng.chance(40) {
            let arguments = if self.rng.chance(50) {
                json!("{\"namespace\":\"collaboration-optimize\"}")
            } else {
                json!({"namespace": "collaboration-optimize", "name": "collaboration-optimize__x"})
            };
            fields.push(("arguments", arguments));
        }
        if self.rng.chance(20) {
            fields.push((
                "input",
                json!({"type": "namespace", "name": "collaboration-optimize"}),
            ));
        }
        if self.rng.chance(30) && depth < 3 {
            let output = (0..self.rng.below(3))
                .map(|_| self.restore_item(depth + 1))
                .collect();
            fields.push(("output", Value::Array(output)));
        }
        if self.rng.chance(20) && depth < 3 {
            let tools = (0..self.rng.below(3))
                .map(|_| self.restore_item(depth + 1))
                .collect();
            fields.push(("tools", Value::Array(tools)));
        }
        if self.rng.chance(40) {
            fields.push(("text", self.loose_text()));
        }
        if self.rng.chance(30) {
            fields.push(("value", self.number()));
        }
        if self.rng.chance(15) {
            fields.push(("<&>", json!({"\u{2028}": [true, false, null, {}, []]})));
        }
        self.object(fields)
    }
}
