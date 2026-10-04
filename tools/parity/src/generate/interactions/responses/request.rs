//! Random input for the OpenAI Responses and Interactions request
//! translators' suites (P4 WP4-C1, see `crate::interactions::responses`):
//! - [`responses_cases`]: Responses requests for the Interactions request
//!   translator. Some come from the Responses → Gemini suites' generator
//!   (`crate::generate::gemini_responses`), which covers every kind of input
//!   item and tool, now and then with this translator's own fields added;
//!   the rest are made here, aimed at what this translator reads: tools
//!   declared at the top level, in `additional_tools` items and in
//!   namespaces, repeated, and the `automation_update` tool of the
//!   `mcp__codex_app` namespace under every spelling; calls and their
//!   outputs paired by ID, orphaned or named, with arguments as JSON text,
//!   objects, text that isn't JSON or values of other types; every
//!   `tool_choice` form; and the generation settings, loosely typed.
//! - [`interactions_cases`]: Interactions requests from the shared generator
//!   (`crate::generate::interactions`), now and then with the fields only
//!   this translator reads added: thinking levels under other keys, tools in
//!   other shapes, media parts by URL, file data and file name, and loose
//!   steps.
//!
//! Neither makes a model name with `antigravity` in it, the two tool
//! descriptions upstream rewrites (see the port's module docs), a number
//! that reads as infinite or NaN, an integer out of `i64`'s range, a repeated
//! key, or JSON text `serde_json` can't read but gjson can: those are the
//! port's deviations.

use serde_json::{Value, json};

use super::super::{MODELS as INTERACTIONS_MODELS, THINKING_LEVELS, odd_value, render, text};
use crate::cases::Case;
use crate::generate::{EFFORTS, Rng, gemini_responses, num, to_object};

/// Models, with a Devin one and blank ones. None names antigravity.
const MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-3-pro-preview",
    "gemini-2.5-flash",
    "models/gemini-2.5-flash",
    "gpt-5",
    "devin",
    " ",
    "",
];

/// Tool names: plain, the `automation_update` tool's under each spelling,
/// already qualified, padded, multi-byte and empty.
const NAMES: &[&str] = &[
    "get_weather",
    "search",
    "read_file",
    "automation_update",
    "Automation_Update",
    "mcp__codex_app__automation_update",
    "mcp__github__read_file",
    "lookup.v2",
    " padded ",
    "名前",
    "a",
    "",
];

/// Namespaces: the `mcp__codex_app` one under other spellings, one ending
/// in the separator, `mcp__` alone, blank and empty.
const NAMESPACES: &[&str] = &[
    "mcp__codex_app",
    " MCP__Codex_App ",
    "mcp__github",
    "browser",
    "tools__",
    "mcp__",
    " ",
    "",
];

const CALL_IDS: &[&str] = &["call_1", "call_2", "fc_3", "call_🚀", " ", ""];

const ROLES: &[&str] = &[
    "user",
    "assistant",
    "model",
    "system",
    "developer",
    "tool",
    "",
];

const TEXT_TYPES: &[&str] = &["input_text", "output_text", "text"];

const IMAGE_TYPES: &[&str] = &["input_image", "output_image"];

/// Image URLs: data URLs with and without a media type, parameters or a
/// comma, a web URL, and blank ones.
const IMAGE_URLS: &[&str] = &[
    "data:image/png;base64,aGVsbG8=",
    "data:;base64,aGk=",
    "data:image/jpeg,raw",
    "data:text/plain;charset=utf-8;base64,aGk=",
    "data:no-comma",
    "https://example.com/cat.png",
    " ",
    "",
];

const MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "", " "];

/// Call arguments and outputs as text: JSON of every kind, padded, and text
/// that isn't JSON.
const JSON_TEXTS: &[&str] = &[
    "{\"city\":\"Paris\"}",
    "{ \"city\" : \"Paris\", \"days\": 1.50 }",
    "{}",
    "[1,2]",
    "null",
    "1.50",
    "\"quoted\"",
    " {\"padded\":true} ",
    "not json",
    "{\"broken\":",
    "",
    " ",
];

/// Token limits, loosely typed, none out of `i64`'s range.
const TOKENS: &[&str] = &[
    "0",
    "1024",
    "-3",
    "1.5",
    "1e3",
    "9007199254740993",
    "\"2048\"",
    "\"abc\"",
    "true",
    "null",
];

/// Sampling settings, loosely typed, all finite as Go reads them.
const KNOBS: &[&str] = &[
    "0",
    "0.7",
    "1",
    "1.50",
    "2.0",
    "-0.0",
    "-0.5",
    "1e3",
    "5e-324",
    "1e30",
    "123456789012345678901234567890",
    "2.98023223876953125e-8",
    "\"0.5\"",
    "\"abc\"",
    "\" 1 \"",
    "\"0x1p-2\"",
    "true",
    "false",
    "null",
    "[]",
    "{}",
];

const SUMMARIES: &[&str] = &["auto", "concise", "detailed", "none", "AUTO", ""];

const PATCH: &str = "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch";

/// Builds `count` Responses request cases. Each depends only on `seed` and
/// its index.
pub fn responses_cases(seed: u64, count: usize) -> Vec<Case> {
    let borrowed = gemini_responses::request_cases(seed.rotate_left(23), count);
    borrowed
        .into_iter()
        .enumerate()
        .map(|(index, borrowed)| {
            let mut rng = rng(seed, index as u64);
            let name = format!("responses-{seed}-{index}");
            if rng.chance(40) {
                return Case { name, ..borrowed };
            }
            if rng.chance(30)
                && let Ok(Value::Object(mut fields)) = serde_json::from_str(&borrowed.request)
            {
                for (key, value) in own_fields(&mut rng) {
                    fields.insert(key.to_owned(), value);
                }
                let request = render(&mut rng, &Value::Object(fields));
                return Case::new(name, borrowed.model, request);
            }
            let model = rng.pick(MODELS);
            let request = responses_request(&mut rng);
            let request = render(&mut rng, &request);
            with_stream(&mut rng, Case::new(name, model, request))
        })
        .collect()
}

/// Builds `count` Interactions request cases. Each depends only on `seed`
/// and its index.
pub fn interactions_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(!seed, index);
            let model = if rng.chance(60) {
                rng.pick(INTERACTIONS_MODELS)
            } else {
                rng.pick(MODELS)
            };
            let mut request = super::super::request(&mut rng);
            if let Value::Object(fields) = &mut request {
                interactions_extras(&mut rng, fields);
            }
            let request = render(&mut rng, &request);
            let name = format!("interactions-{seed}-{index}");
            with_stream(&mut rng, Case::new(name, model, request))
        })
        .collect()
}

fn rng(seed: u64, index: u64) -> Rng {
    Rng(seed ^ 0x5245_5350_5245_5154 ^ index.wrapping_mul(0x9FB2_1C65_1E98_DF25))
}

/// `case`, sometimes with the translator's stream flag given.
fn with_stream(rng: &mut Rng, case: Case) -> Case {
    if rng.chance(40) {
        let stream = rng.chance(50);
        return case.with_options(json!({ "stream": stream }));
    }
    case
}

/// A JSON literal from one of the pools.
fn raw(text: &str) -> Value {
    serde_json::from_str(text).expect("the pools hold JSON")
}

/// A tool a request declares, for calls and `tool_choice` to name.
struct Declared {
    namespace: String,
    name: String,
}

/// A Responses request body.
fn responses_request(rng: &mut Rng) -> Value {
    let (tools, declared) = tools(rng);
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if rng.chance(60) {
        fields.push(("model", rng.pick(MODELS).into()));
    }
    if rng.chance(90) {
        fields.push(("input", input(rng, &declared)));
    }
    if !tools.is_empty() || rng.chance(5) {
        fields.push(("tools", Value::Array(tools)));
    } else if rng.chance(5) {
        fields.push(("tools", odd_value(rng)));
    }
    if rng.chance(35) {
        fields.push(("tool_choice", tool_choice(rng, &declared)));
    }
    fields.extend(own_fields(rng));
    if rng.chance(30) {
        rng.shuffle(&mut fields);
    }
    to_object(fields)
}

/// The top-level fields the translator reads besides `input`, `tools` and
/// `tool_choice`, each now and then, with distinct keys.
fn own_fields(rng: &mut Rng) -> Vec<(&'static str, Value)> {
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if rng.chance(25) {
        let stream = rng.pick(&[
            json!(true),
            json!(false),
            json!("true"),
            json!("f"),
            json!(1),
            Value::Null,
        ]);
        fields.push(("stream", stream));
    }
    if rng.chance(50) {
        fields.push(("instructions", instructions(rng)));
    }
    if rng.chance(20) {
        fields.push(("previous_response_id", id_value(rng)));
    }
    if rng.chance(15) {
        fields.push(("previous_interaction_id", id_value(rng)));
    }
    if rng.chance(15) {
        fields.push(if rng.chance(50) {
            ("environment_id", id_value(rng))
        } else {
            ("environment", json!({ "id": id_value(rng) }))
        });
    }
    if rng.chance(10) {
        let config = if rng.chance(80) {
            json!({ "type": "dynamic", "thinking_summaries": rng.pick(SUMMARIES) })
        } else {
            odd_value(rng)
        };
        fields.push(("agent_config", config));
    }
    if rng.chance(30) {
        fields.push(("reasoning", reasoning(rng)));
    }
    if rng.chance(20) {
        if rng.chance(50) {
            fields.push(("response_format", response_format(rng)));
        }
        if rng.chance(60) {
            let text = match rng.below(4) {
                0 => json!({ "verbosity": "low" }),
                1 => odd_value(rng),
                _ => json!({ "format": response_format(rng) }),
            };
            fields.push(("text", text));
        }
    }
    for key in ["max_output_tokens", "max_tokens", "max_completion_tokens"] {
        if rng.chance(12) {
            fields.push((key, raw(rng.pick(TOKENS))));
        }
    }
    for key in [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if rng.chance(15) {
            fields.push((key, raw(rng.pick(KNOBS))));
        }
    }
    if rng.chance(15) {
        let stop = match rng.below(3) {
            0 => text(rng).into(),
            1 => json!(["END", text(rng)]),
            _ => odd_value(rng),
        };
        fields.push(("stop", stop));
    }
    fields
}

/// An ID, mostly a string, sometimes blank or of another type.
fn id_value(rng: &mut Rng) -> Value {
    match rng.below(6) {
        0 => odd_value(rng),
        1 => rng.pick(&[" ", "", "\t"]).into(),
        _ => rng
            .pick(&["resp_1", "interaction_2", "env_3", "名前"])
            .into(),
    }
}

/// `instructions`: a string, an object with its text or content parts, or a
/// value of another type.
fn instructions(rng: &mut Rng) -> Value {
    match rng.below(6) {
        0..=2 => text(rng).into(),
        3 => json!({ "text": text(rng) }),
        4 => {
            let parts: Vec<Value> = (0..rng.below(4))
                .map(|_| match rng.below(4) {
                    0 => json!({ "type": "input_text" }),
                    _ => json!({ "type": "input_text", "text": text(rng) }),
                })
                .collect();
            json!({ "content": parts })
        }
        _ => odd_value(rng),
    }
}

fn reasoning(rng: &mut Rng) -> Value {
    if rng.chance(10) {
        return odd_value(rng);
    }
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if rng.chance(80) {
        let effort = if rng.chance(85) {
            rng.pick(EFFORTS).into()
        } else {
            odd_value(rng)
        };
        fields.push(("effort", effort));
    }
    if rng.chance(50) {
        let summary = if rng.chance(85) {
            rng.pick(SUMMARIES).into()
        } else {
            odd_value(rng)
        };
        fields.push(("summary", summary));
    }
    to_object(fields)
}

fn response_format(rng: &mut Rng) -> Value {
    match rng.below(4) {
        0 => json!({ "type": "text" }),
        1 => json!({
            "type": "json_schema",
            "name": "answer",
            "schema": { "type": "object", "properties": { "answer": { "type": "string" } } },
        }),
        2 => json!({ "type": "json_object" }),
        _ => odd_value(rng),
    }
}

/// A function's parameter schema, under one of the keys it can be given.
fn schema(rng: &mut Rng) -> (&'static str, Value) {
    let key = rng.pick(&[
        "parameters",
        "parameters",
        "parametersJsonSchema",
        "input_schema",
    ]);
    let schema = json!({
        "type": "object",
        "properties": { "city": { "type": "string" }, "days": { "type": "number", "maximum": num("1.50") } },
        "required": ["city"],
    });
    (key, schema)
}

/// A description, now and then of another type.
fn description(rng: &mut Rng) -> Value {
    if rng.chance(10) {
        odd_value(rng)
    } else {
        text(rng).into()
    }
}

/// A function or custom tool declared directly, with its name.
fn tool(rng: &mut Rng) -> (Value, String) {
    let name = rng.pick(NAMES).to_owned();
    let (key, parameters) = schema(rng);
    let tool = match rng.below(6) {
        0 | 1 => {
            json!({ "type": "function", "name": name, "description": description(rng), key: parameters })
        }
        2 => {
            json!({ "type": "function", "function": { "name": name, "description": description(rng), key: parameters } })
        }
        3 => json!({ "name": name, key: parameters }),
        4 => json!({ "type": "custom", "name": name, "description": description(rng) }),
        _ => json!({
            "type": "custom",
            "name": name,
            "format": { "type": "grammar", "syntax": "lark", "definition": "start: /.+/" },
        }),
    };
    (tool, name)
}

/// A request's tools, and the names they declare.
fn tools(rng: &mut Rng) -> (Vec<Value>, Vec<Declared>) {
    let mut tools = Vec::new();
    let mut declared = Vec::new();
    for _ in 0..rng.below(5) {
        match rng.below(10) {
            0..=3 => {
                let (tool, name) = tool(rng);
                tools.push(tool);
                declared.push(Declared {
                    namespace: String::new(),
                    name,
                });
            }
            4 => {
                tools.push(json!({
                    "type": "custom",
                    "name": "apply_patch",
                    "description": description(rng),
                    "format": { "type": "grammar", "syntax": "lark", "definition": "start: /.+/" },
                }));
                declared.push(Declared {
                    namespace: String::new(),
                    name: "apply_patch".into(),
                });
            }
            5 | 6 => {
                let namespace = rng.pick(NAMESPACES);
                let mut children = Vec::new();
                for _ in 0..1 + rng.below(3) {
                    let (child, name) = tool(rng);
                    children.push(child);
                    declared.push(Declared {
                        namespace: namespace.to_owned(),
                        name,
                    });
                }
                let key = rng.pick(&["tools", "tools", "children"]);
                tools.push(json!({ "type": "namespace", "name": namespace, "description": description(rng), key: children }));
            }
            7 => {
                let namespace = rng.pick(&["mcp__codex_app", " MCP__Codex_App "]);
                let name = rng.pick(&[
                    "automation_update",
                    "Automation_Update",
                    " automation_update ",
                ]);
                tools.push(json!({ "type": "namespace", "name": namespace, "tools": [
                    { "type": "function", "name": name, "description": "Update an automation." },
                    { "type": "function", "name": "list_automations" },
                ]}));
                for name in [name, "list_automations"] {
                    declared.push(Declared {
                        namespace: namespace.to_owned(),
                        name: name.to_owned(),
                    });
                }
            }
            8 => {
                let name = rng.pick(&[
                    "mcp__codex_app__automation_update",
                    " MCP__CODEX_APP__AUTOMATION_UPDATE ",
                ]);
                tools.push(json!({ "type": "function", "name": name }));
                declared.push(Declared {
                    namespace: String::new(),
                    name: name.to_owned(),
                });
            }
            _ => tools.push(rng.pick(&[
                json!({ "type": "web_search", "filters": { "allowed_domains": ["docs.rs"] } }),
                json!({ "type": "file_search" }),
                json!({ "type": "function", "name": 7 }),
                json!({ "type": "namespace", "name": "empty" }),
                json!("not a tool"),
            ])),
        }
    }
    if tools.len() > 1 && rng.chance(20) {
        // Repeats lose to the first declaration.
        let repeat = tools[rng.below(tools.len())].clone();
        tools.push(repeat);
    }
    (tools, declared)
}

/// The namespace and name a call or `tool_choice` gives: mostly a declared
/// tool's, by its namespace and local name or by its qualified name.
fn target(rng: &mut Rng, declared: &[Declared]) -> (Option<String>, String) {
    if !declared.is_empty() && rng.chance(75) {
        let tool = &declared[rng.below(declared.len())];
        if tool.namespace.is_empty() || rng.chance(30) {
            let name = if tool.namespace.trim().is_empty() {
                tool.name.clone()
            } else {
                format!("{}__{}", tool.namespace.trim(), tool.name.trim())
            };
            return (None, name);
        }
        return (Some(tool.namespace.clone()), tool.name.clone());
    }
    let namespace = rng.chance(20).then(|| rng.pick(NAMESPACES).to_owned());
    (namespace, rng.pick(NAMES).to_owned())
}

/// `input`: mostly a list of items, sometimes text, one item or a value of
/// another type.
fn input(rng: &mut Rng, declared: &[Declared]) -> Value {
    let mut calls = Vec::new();
    match rng.below(10) {
        0 => text(rng).into(),
        1 => item(rng, declared, &mut calls),
        2 => odd_value(rng),
        _ => {
            let mut items = Vec::new();
            for _ in 0..1 + rng.below(7) {
                items.push(item(rng, declared, &mut calls));
            }
            Value::Array(items)
        }
    }
}

/// One input item. `calls` holds the IDs of the calls made so far, for
/// outputs to answer.
fn item(rng: &mut Rng, declared: &[Declared], calls: &mut Vec<String>) -> Value {
    match rng.below(16) {
        0..=3 => json!({ "type": "message", "role": rng.pick(ROLES), "content": content(rng) }),
        4 | 5 => call(rng, declared, calls, false),
        6 => call(rng, declared, calls, true),
        7 | 8 => output(rng, calls),
        9 => {
            let text = if rng.chance(85) {
                text(rng).into()
            } else {
                odd_value(rng)
            };
            json!({ "type": rng.pick(TEXT_TYPES), "text": text })
        }
        10 => {
            let image_type = rng.pick(IMAGE_TYPES);
            image(rng, image_type)
        }
        11 => {
            let (tool, _) = tool(rng);
            json!({ "type": "additional_tools", "tools": [tool] })
        }
        12 => {
            let mut fields: Vec<(&str, Value)> = vec![
                ("type", json!("reasoning")),
                (
                    "summary",
                    json!([{ "type": "summary_text", "text": text(rng) }]),
                ),
            ];
            if rng.chance(30) {
                fields.push((
                    "content",
                    json!([{ "type": "reasoning_text", "text": text(rng) }]),
                ));
            }
            to_object(fields)
        }
        13 => json!({ "role": rng.pick(ROLES), "content": content(rng) }),
        14 => json!({ "type": "item_reference", "id": "msg_1" }),
        _ => {
            if rng.chance(50) {
                json!({ "type": "message", "role": rng.pick(ROLES) })
            } else {
                json!({ "type": "message", "role": rng.pick(ROLES), "content": odd_value(rng) })
            }
        }
    }
}

/// A message's content: parts, text, one part or a value of another type.
fn content(rng: &mut Rng) -> Value {
    match rng.below(20) {
        0..=3 => text(rng).into(),
        4 | 5 => part(rng),
        6 => odd_value(rng),
        _ => (0..rng.below(4)).map(|_| part(rng)).collect(),
    }
}

fn part(rng: &mut Rng) -> Value {
    match rng.below(10) {
        0..=4 => json!({ "type": rng.pick(TEXT_TYPES), "text": text(rng) }),
        5 => {
            if rng.chance(50) {
                json!({ "type": rng.pick(TEXT_TYPES), "text": odd_value(rng) })
            } else {
                json!({ "type": rng.pick(TEXT_TYPES) })
            }
        }
        6 | 7 => {
            let image_type = rng.pick(IMAGE_TYPES);
            image(rng, image_type)
        }
        8 => json!({ "type": rng.pick(&["refusal", "summary_text", ""]), "text": text(rng) }),
        _ => json!({ "type": rng.pick(&["input_file", "input_audio"]), "file_id": "file_1" }),
    }
}

/// An image part or item: by `image_url` or `url`, by `data` and its media
/// type, or both.
fn image(rng: &mut Rng, image_type: &str) -> Value {
    let mut fields: Vec<(&str, Value)> = vec![("type", image_type.into())];
    if rng.chance(70) {
        let key = rng.pick(&["image_url", "image_url", "url"]);
        let url = if rng.chance(90) {
            rng.pick(IMAGE_URLS).into()
        } else {
            odd_value(rng)
        };
        fields.push((key, url));
    }
    if rng.chance(40) {
        fields.push(("data", json!("aGVsbG8=")));
        if rng.chance(60) {
            fields.push(("mime_type", rng.pick(MIME_TYPES).into()));
        }
    }
    if rng.chance(20) {
        fields.push(("detail", json!("auto")));
    }
    to_object(fields)
}

/// A function call, or a custom tool call when `custom` is set.
fn call(rng: &mut Rng, declared: &[Declared], calls: &mut Vec<String>, custom: bool) -> Value {
    let (namespace, name) = target(rng, declared);
    let id = rng.pick(CALL_IDS);
    calls.push(id.to_owned());
    let call_type = if custom {
        "custom_tool_call"
    } else {
        "function_call"
    };
    let mut fields: Vec<(&str, Value)> = vec![("type", call_type.into())];
    if rng.chance(90) {
        fields.push((rng.pick(&["call_id", "call_id", "id"]), id.into()));
    }
    if rng.chance(30) {
        fields.push(("id", json!("fc_item")));
    }
    fields.push((
        "name",
        if rng.chance(95) {
            name.into()
        } else {
            odd_value(rng)
        },
    ));
    if let Some(namespace) = namespace {
        fields.push(("namespace", namespace.into()));
    }
    if custom && rng.chance(75) {
        let input = match rng.below(4) {
            0 | 1 => PATCH.into(),
            2 => text(rng).into(),
            _ => odd_value(rng),
        };
        fields.push(("input", input));
    } else if rng.chance(85) {
        fields.push(("arguments", arguments(rng)));
    }
    to_object(fields)
}

/// A call's arguments: JSON text, an object, or a value of another type.
fn arguments(rng: &mut Rng) -> Value {
    match rng.below(10) {
        0..=5 => rng.pick(JSON_TEXTS).into(),
        6 | 7 => json!({ "city": text(rng), "days": num("1.50") }),
        _ => odd_value(rng),
    }
}

/// An output for one of `calls`, or for a call never made.
fn output(rng: &mut Rng, calls: &[String]) -> Value {
    let output_type = rng.pick(&[
        "function_call_output",
        "function_call_output",
        "custom_tool_call_output",
    ]);
    let id = if !calls.is_empty() && rng.chance(75) {
        calls[rng.below(calls.len())].clone()
    } else {
        "call_orphan".to_owned()
    };
    let mut fields: Vec<(&str, Value)> = vec![("type", output_type.into())];
    if rng.chance(90) {
        fields.push((rng.pick(&["call_id", "call_id", "id"]), id.into()));
    }
    if rng.chance(25) {
        fields.push(("name", rng.pick(NAMES).into()));
        if rng.chance(40) {
            fields.push(("namespace", rng.pick(NAMESPACES).into()));
        }
    }
    let value = match rng.below(6) {
        0 | 1 => text(rng).into(),
        2 => rng.pick(JSON_TEXTS).into(),
        3 => json!({ "temp": num("1.50"), "note": text(rng) }),
        4 => json!([{ "type": "input_text", "text": text(rng) }]),
        _ => odd_value(rng),
    };
    match rng.below(10) {
        0 => {}
        1 | 2 => fields.push(("result", value)),
        _ => fields.push(("output", value)),
    }
    to_object(fields)
}

/// A `tool_choice`: a string, an object naming a tool every way, the
/// `automation_update` tool, or a value of another type.
fn tool_choice(rng: &mut Rng, declared: &[Declared]) -> Value {
    let (namespace, name) = target(rng, declared);
    let with_namespace = |mut choice: Value, at: &str| {
        if let (Some(namespace), Some(Value::Object(fields))) = (&namespace, choice.pointer_mut(at))
        {
            fields.insert("namespace".into(), namespace.as_str().into());
        }
        choice
    };
    match rng.below(9) {
        0 | 1 => rng
            .pick(&["auto", "none", "required", "any", "AUTO", ""])
            .into(),
        2 => with_namespace(json!({ "type": "function", "name": name }), ""),
        3 => with_namespace(
            json!({ "type": "function", "function": { "name": name } }),
            "/function",
        ),
        4 => {
            if rng.chance(50) {
                with_namespace(
                    json!({ "type": "custom", "custom": { "name": name } }),
                    "/custom",
                )
            } else {
                with_namespace(json!({ "type": "custom", "name": name }), "")
            }
        }
        5 => rng.pick(&[
            json!({ "type": "function", "namespace": "mcp__codex_app", "name": "automation_update" }),
            json!({ "type": "function", "namespace": " MCP__Codex_App ", "name": "Automation_Update" }),
            json!({ "type": "function", "name": " mcp__codex_app__automation_update " }),
            json!({ "type": "function", "function": { "name": "automation_update", "namespace": "mcp__codex_app" } }),
            json!({ "type": "custom", "custom": { "name": "MCP__CODEX_APP__AUTOMATION_UPDATE" } }),
            json!({ "type": "function", "namespace": "mcp__codex_app", "name": "list_automations" }),
        ]),
        6 => json!({ "type": "allowed_tools", "mode": "required", "tools": [{ "type": "function", "name": name }] }),
        7 => rng.pick(&[json!({ "type": "function" }), json!({ "type": "web_search" })]),
        _ => odd_value(rng),
    }
}

/// Adds to an Interactions request, now and then, what only this
/// translator reads.
fn interactions_extras(rng: &mut Rng, fields: &mut serde_json::Map<String, Value>) {
    if rng.chance(15) {
        fields.insert(
            "agent_config".into(),
            json!({ "type": "dynamic", "thinking_summaries": rng.pick(SUMMARIES) }),
        );
    }
    if rng.chance(20) {
        let level = rng.pick(THINKING_LEVELS);
        let (key, config) = match rng.below(3) {
            0 => ("thinkingConfig", json!({ "thinkingLevel": level })),
            1 => ("thinkingConfig", json!({ "thinking_level": level })),
            _ => ("thinking_config", json!({ "thinking_level": level })),
        };
        match fields.get_mut("generation_config") {
            Some(Value::Object(config_fields)) => {
                config_fields.insert(key.into(), config);
            }
            _ => {
                fields.insert("generation_config".into(), json!({ key: config }));
            }
        }
    }
    if rng.chance(10) {
        let modalities = rng.pick(&[json!(["TEXT"]), json!(["TEXT", "IMAGE"]), json!("TEXT")]);
        fields.insert("response_modalities".into(), modalities);
    }
    if rng.chance(15) {
        let name = rng.pick(NAMES);
        let tool = match rng.below(3) {
            0 => json!({ "type": "function", "function": {
                "name": name,
                "description": description(rng),
                "parameters": { "type": "object", "properties": { "q": { "type": "string" } } },
            }}),
            1 => json!({ "name": name, "parametersJsonSchema": { "type": "object" } }),
            _ => json!({ "name": name, "description": description(rng) }),
        };
        match fields.get_mut("tools") {
            Some(Value::Array(tools)) => tools.push(tool),
            _ => {
                fields.insert("tools".into(), json!([tool]));
            }
        }
    }
    if rng.chance(20)
        && let Some(Value::Array(steps)) = fields.get_mut("input")
    {
        for _ in 0..1 + rng.below(3) {
            let step = interactions_step(rng);
            let at = rng.below(steps.len() + 1);
            steps.insert(at, step);
        }
    }
    if rng.chance(10) {
        let key = rng.pick(&["previous_interaction_id", "previous_response_id"]);
        fields.insert(key.into(), id_value(rng));
    }
    if rng.chance(10) {
        let instruction =
            json!({ "parts": [{ "text": text(rng) }, { "text": odd_value(rng) }, {}] });
        fields.insert("system_instruction".into(), instruction);
    }
}

/// A step the shared generator doesn't make: media by URL, file data or
/// file name, thoughts with nested texts, calls by `id`, results under
/// `output`, and loose text.
fn interactions_step(rng: &mut Rng) -> Value {
    match rng.below(6) {
        0 => {
            let media_type = rng.pick(&["image", "video", "document", "audio"]);
            let mut part: Vec<(&str, Value)> = vec![("type", media_type.into())];
            match rng.below(4) {
                0 => part.push(("image_url", json!("https://example.com/a.png"))),
                1 => part.push(("file_data", json!("data:text/plain;base64,aGk="))),
                2 => part.push(("url", json!("https://example.com/v.mp4"))),
                _ => part.push(("data", json!("aGk="))),
            }
            if rng.chance(50) {
                part.push((
                    "mime_type",
                    rng.pick(&["video/mp4", "audio/", "wav", ""]).into(),
                ));
            }
            if rng.chance(40) {
                part.push(("filename", rng.pick(&["a.pdf", "名前.txt", ""]).into()));
            }
            let step_type = rng.pick(&["user_input", "model_output"]);
            json!({ "type": step_type, "content": [to_object(part)] })
        }
        1 => json!({ "type": "thought", "content": [
            { "type": "text", "content": { "text": text(rng) } },
            { "type": "text", "text": text(rng) },
        ]}),
        2 => {
            json!({ "type": "function_call", "id": "call_7", "name": rng.pick(NAMES), "arguments": { "nested": { "list": [1, num("1.50")] } } })
        }
        3 => json!({ "type": "function_result", "id": "call_7", "output": text(rng) }),
        4 => text(rng).into(),
        _ => {
            json!({ "type": "user_input", "content": { "only": { "type": "text", "text": text(rng) } } })
        }
    }
}

#[cfg(test)]
mod tests {
    use open_ferry_translate::openai::interactions::responses::convert_openai_responses_request_to_interactions;

    use super::*;

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        for (first, again) in [
            (responses_cases(5, 300), responses_cases(5, 300)),
            (interactions_cases(5, 300), interactions_cases(5, 300)),
        ] {
            assert_eq!(first.len(), 300);
            for (a, b) in first.iter().zip(&again) {
                assert_eq!(
                    (&a.name, &a.model, &a.request, &a.options),
                    (&b.name, &b.model, &b.request, &b.options)
                );
                serde_json::from_str::<Value>(&a.request).expect("a request is JSON");
            }
        }
    }

    #[test]
    fn no_antigravity_models_or_rewritten_descriptions() {
        let cases = responses_cases(9, 300)
            .into_iter()
            .chain(interactions_cases(9, 300));
        for case in cases {
            let text = format!("{} {}", case.model, case.request).to_lowercase();
            assert!(!text.contains("antigravity"), "{text}");
            assert!(!text.contains("returning output or a session id"), "{text}");
            assert!(!text.contains("writes characters to an existing"), "{text}");
        }
    }

    #[test]
    fn automation_update_is_left_out_now_and_then() {
        let (mut tools, mut choices) = (0, 0);
        for case in responses_cases(3, 1000) {
            let request: Value = serde_json::from_str(&case.request).expect("a request is JSON");
            let out =
                convert_openai_responses_request_to_interactions(&case.model, &request, false);
            let mentions = |value: Option<&Value>| {
                value.is_some_and(|value| {
                    value
                        .to_string()
                        .to_lowercase()
                        .contains("automation_update")
                })
            };
            if mentions(request.get("tools")) && !mentions(out.get("tools")) {
                tools += 1;
            }
            if mentions(request.get("tool_choice"))
                && out.pointer("/generation_config/tool_choice").is_none()
            {
                choices += 1;
            }
        }
        assert!(tools > 0 && choices > 0, "{tools} tools, {choices} choices");
    }
}
