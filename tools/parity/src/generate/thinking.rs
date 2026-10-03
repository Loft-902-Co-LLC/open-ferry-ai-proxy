//! Seeded random input for upstream's `ApplyThinkingWithModelInfo`, on a
//! Codex or Responses target ([`codex_cases`]) and on a Chat Completions one
//! ([`openai_cases`]).
//!
//! A case's model has a suffix now and then: a level, a budget, or neither.
//! The model the request is bound to has levels, a budget range, a family
//! and flags drawn at random, or is one nobody registered. The body and the
//! client's request are small objects made of the settings each format keeps
//! (efforts, budgets, effort updates in a Responses `input`, summary
//! settings), with values of every type, in random order. The client's
//! request may also be empty, or text that isn't an object or isn't JSON,
//! though never with an effort gjson could still read from broken JSON,
//! which upstream does and we don't (see UPSTREAM.md). Formats and
//! providers come in other cases and with spaces too.

use serde_json::{Map, Value, json};

use super::Rng;
use crate::cases::Case;
use crate::cases::thinking::case;

const CODEX_TARGETS: &[&str] = &["codex", "openai-response", " Codex ", "OPENAI-RESPONSE"];
const OPENAI_TARGETS: &[&str] = &["openai", "openai", " OpenAI ", "OPENAI"];

/// Client formats: those with settings of their own, more often the
/// Responses and Chat Completions ones, and names in other cases.
const FROMS: &[&str] = &[
    "openai-response",
    "openai-response",
    "openai-response",
    "codex",
    "openai",
    "openai",
    "claude",
    "gemini",
    "antigravity",
    "interactions",
    "kimi",
    "xai",
    " OpenAI-Response ",
    "CODEX",
    "Claude",
    "",
    "gemini-cli",
];

const CODEX_PROVIDERS: &[&str] = &["codex", "codex", "openai", "", " Codex ", "openrouter"];

/// Providers, some of them OpenRouter by name.
const OPENAI_PROVIDERS: &[&str] = &[
    "openai",
    "openrouter",
    "OpenRouter",
    " openrouter ",
    "my-openrouter",
    "openrouter.ai",
    "x:openrouter:y",
    "acme_openrouter/eu",
    "openrouterx",
    "open-router",
    "deepseek",
    "",
    "kimi",
];

const BASES: &[&str] = &["gpt-5.5", "gpt-6-sol", "custom-model", "", " GPT-5.5 "];

const SUFFIXES: &[&str] = &[
    "low", "medium", "high", "xhigh", "max", "minimal", "none", "auto", "-1", "0", "1", "1023",
    "8192", "24576", "100000", "-5", "HIGH", " high ", "fast", "", "1.5",
];

/// Model families, as a model's `Type`.
const TYPES: &[&str] = &[
    "openai",
    "openai",
    "claude",
    "gemini",
    "",
    " OpenAI ",
    "codex",
    "kimi",
    "xai",
    "antigravity",
    "interactions",
    "qwen",
];

const LEVELS: &[&str] = &[
    "minimal", "low", "medium", "high", "xhigh", "max", "none", "auto", " High ", "LOW",
];

/// Effort values, as JSON: levels, special values, in other cases and with
/// spaces, unknown ones, and values of other types.
const EFFORTS: &[&str] = &[
    "\"low\"",
    "\"medium\"",
    "\"high\"",
    "\"xhigh\"",
    "\"max\"",
    "\"minimal\"",
    "\"none\"",
    "\"auto\"",
    "\"\"",
    "\" High \"",
    "\"HIGH\"",
    "\"turbo\"",
    "\"İnce\"",
    "\"high\\t\"",
    "0",
    "8192",
    "1.5",
    "-1",
    "true",
    "null",
    "[\"high\"]",
    "{\"level\":\"high\"}",
];

const SUMMARIES: &[&str] = &[
    "\"auto\"",
    "\"concise\"",
    "\"detailed\"",
    "\"none\"",
    "\"\"",
    "\" Detailed \"",
    "\"AUTO\"",
    "\"off\"",
    "true",
    "false",
    "null",
    "1",
];

const BOOLS: &[&str] = &["true", "false", "\"true\"", "null", "0"];

const BUDGETS: &[&str] = &[
    "-1",
    "0",
    "1",
    "128",
    "1024",
    "8192",
    "20000",
    "32768",
    "40000",
    "1.5",
    "\"4096\"",
    "null",
    "true",
    "9223372036854775807",
];

const CLAUDE_TYPES: &[&str] = &[
    "\"enabled\"",
    "\"adaptive\"",
    "\"auto\"",
    "\"disabled\"",
    "\"Enabled\"",
    "1",
    "null",
];

/// Values where an object is expected.
const NOT_OBJECTS: &[&str] = &[
    "\"high\"",
    "1",
    "null",
    "[]",
    "true",
    "[{\"effort\":\"high\"}]",
];

/// Client requests that aren't objects, or aren't JSON but hold nothing
/// gjson would read.
const NOT_REQUESTS: &[&str] = &["nope", "{", "[]", "null", "\"hi\"", "1", "{\"input\":["];

pub fn codex_cases(seed: u64, count: usize) -> Vec<Case> {
    cases(seed.rotate_left(17), count, CODEX_TARGETS, CODEX_PROVIDERS)
}

pub fn openai_cases(seed: u64, count: usize) -> Vec<Case> {
    cases(
        seed.rotate_left(41),
        count,
        OPENAI_TARGETS,
        OPENAI_PROVIDERS,
    )
}

fn cases(seed: u64, count: usize, targets: &[&str], providers: &[&str]) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut rng = Rng(seed ^ index.wrapping_mul(0xD6E8_FEB8_6659_FD93));
            let to = rng.pick(targets);
            let chat = to.trim().eq_ignore_ascii_case("openai");
            let from = rng.pick(FROMS);
            let provider = rng.pick(providers);
            let mut model = rng.pick(BASES).to_owned();
            if rng.chance(45) {
                model = format!("{model}({})", rng.pick(SUFFIXES));
            }
            let info = model_info(&mut rng);
            let body = request(&mut rng, chat);
            let source = match rng.below(10) {
                0..=2 => String::new(),
                3 => rng.pick(NOT_REQUESTS).to_owned(),
                _ => {
                    let from = from.trim();
                    let responses = from.eq_ignore_ascii_case("openai-response")
                        || from.eq_ignore_ascii_case("codex");
                    request(&mut rng, !responses).to_string()
                }
            };
            case(
                &format!("random-{seed}-{index}"),
                &model,
                &body,
                &source,
                from,
                to,
                provider,
                info,
            )
        })
        .collect()
}

fn value(text: &str) -> Value {
    serde_json::from_str(text).expect("generator values are JSON")
}

/// A field's key, its chance in percent, and how to make its value.
type Field<'a> = (&'a str, u64, &'a dyn Fn(&mut Rng) -> Value);

/// Fields drawn from `fields`, each with its chance, in random order.
fn object(rng: &mut Rng, fields: &[Field<'_>]) -> Value {
    let mut chosen: Vec<_> = fields
        .iter()
        .filter(|(_, chance, _)| rng.chance(*chance))
        .collect();
    rng.shuffle(&mut chosen);
    let mut object = Map::new();
    for (key, _, make) in chosen {
        object.insert((*key).to_owned(), make(rng));
    }
    Value::Object(object)
}

fn model_info(rng: &mut Rng) -> Value {
    if rng.chance(12) {
        return Value::Null;
    }
    object(
        rng,
        &[
            ("id", 85, &|rng| {
                json!(rng.pick(&["gpt-5.5", "custom-model", "", "Claude-X"]))
            }),
            ("type", 85, &|rng| json!(rng.pick(TYPES))),
            ("user_defined", 18, &|_| json!(true)),
            ("support_configuration_update", 35, &|_| json!(true)),
            ("thinking", 85, &thinking_support),
        ],
    )
}

fn thinking_support(rng: &mut Rng) -> Value {
    object(
        rng,
        &[
            ("levels", 70, &|rng| {
                let mut levels: Vec<&str> =
                    LEVELS.iter().copied().filter(|_| rng.chance(40)).collect();
                if levels.is_empty() {
                    levels.push(rng.pick(LEVELS));
                }
                if rng.chance(15) {
                    rng.shuffle(&mut levels);
                }
                json!(levels)
            }),
            ("min", 35, &|rng| json!(rng.pick(&[0, 128, 1024]))),
            ("max", 35, &|rng| {
                json!(rng.pick(&[0, 8192, 32768, 128_000]))
            }),
            ("zero_allowed", 30, &|_| json!(true)),
            ("dynamic_allowed", 30, &|_| json!(true)),
        ],
    )
}

/// A request body: for Chat Completions if `chat`, else for Responses,
/// though either may hold any format's settings.
fn request(rng: &mut Rng, chat: bool) -> Value {
    let (input, messages) = if chat { (15, 60) } else { (70, 10) };
    object(
        rng,
        &[
            ("model", 30, &|_| json!("gpt")),
            ("input", input, &input_items),
            (
                "messages",
                messages,
                &|_| json!([{ "role": "user", "content": "hi" }]),
            ),
            ("reasoning", 60, &reasoning),
            ("reasoning_effort", 35, &|rng| value(rng.pick(EFFORTS))),
            ("include_reasoning", 15, &|rng| value(rng.pick(BOOLS))),
            ("thinking", 15, &|rng| {
                object(
                    rng,
                    &[
                        ("type", 80, &|rng| value(rng.pick(CLAUDE_TYPES))),
                        ("budget_tokens", 50, &|rng| value(rng.pick(BUDGETS))),
                        ("effort", 20, &|rng| value(rng.pick(EFFORTS))),
                        ("display", 20, &|rng| {
                            json!(rng.pick(&["summarized", "omitted"]))
                        }),
                    ],
                )
            }),
            (
                "output_config",
                10,
                &|rng| json!({ "effort": value(rng.pick(EFFORTS)) }),
            ),
            (
                "generationConfig",
                10,
                &|rng| json!({ "thinkingConfig": gemini_thinking(rng) }),
            ),
            (
                "request",
                5,
                &|rng| json!({ "generationConfig": { "thinkingConfig": gemini_thinking(rng) } }),
            ),
            ("generation_config", 8, &|rng| {
                object(
                    rng,
                    &[
                        ("thinking_level", 40, &|rng| value(rng.pick(EFFORTS))),
                        ("thinkingBudget", 30, &|rng| value(rng.pick(BUDGETS))),
                        ("thinking_summaries", 30, &|rng| {
                            json!(rng.pick(&["auto", "none"]))
                        }),
                        ("thinking_config", 30, &gemini_thinking),
                    ],
                )
            }),
        ],
    )
}

fn reasoning(rng: &mut Rng) -> Value {
    if rng.chance(15) {
        return value(rng.pick(NOT_OBJECTS));
    }
    object(
        rng,
        &[
            ("effort", 65, &|rng| value(rng.pick(EFFORTS))),
            ("summary", 45, &|rng| value(rng.pick(SUMMARIES))),
            ("generate_summary", 15, &|rng| value(rng.pick(SUMMARIES))),
            ("exclude", 20, &|rng| value(rng.pick(BOOLS))),
            ("enabled", 10, &|rng| value(rng.pick(BOOLS))),
            ("max_tokens", 10, &|rng| value(rng.pick(BUDGETS))),
        ],
    )
}

fn gemini_thinking(rng: &mut Rng) -> Value {
    object(
        rng,
        &[
            ("thinkingLevel", 35, &|rng| value(rng.pick(EFFORTS))),
            ("thinking_level", 15, &|rng| value(rng.pick(EFFORTS))),
            ("thinkingBudget", 40, &|rng| value(rng.pick(BUDGETS))),
            ("thinking_budget", 15, &|rng| value(rng.pick(BUDGETS))),
            ("includeThoughts", 30, &|rng| value(rng.pick(BOOLS))),
        ],
    )
}

/// A Responses `input`: messages and effort updates, some of them not
/// quite updates, or something other than a list.
fn input_items(rng: &mut Rng) -> Value {
    if rng.chance(15) {
        return value(rng.pick(&["\"hi\"", "null", "{}"]));
    }
    let items = (0..rng.below(5))
        .map(|_| match rng.below(10) {
            0..=3 => json!({ "type": "message", "role": "user", "content": "hi" }),
            4..=7 => json!({ "type": "configuration_update", "reasoning": { "effort": value(rng.pick(EFFORTS)) } }),
            8 => match rng.below(4) {
                0 => json!({ "type": "CONFIGURATION_UPDATE", "reasoning": { "effort": "high" } }),
                1 => json!({ "type": "configuration_update", "reasoning": value(rng.pick(NOT_OBJECTS)) }),
                2 => json!({ "type": "configuration_update" }),
                _ => json!({ "type": "configuration_update", "reasoning": { "summary": "auto" } }),
            },
            _ => value(rng.pick(&["\"x\"", "1", "null", "[]"])),
        })
        .collect();
    Value::Array(items)
}
