//! Seeded random input for the translator registry.
//!
//! Half the requests go through the built-in translators: the other
//! generators' requests, with reasoning summary settings added now and then,
//! sent to the provider format they were made for or, sometimes, to another,
//! where the registry may have no translator and passes them on. The other
//! half go through a translator that changes nothing, between any two
//! formats, so the registry's own summary handling meets every format: small
//! bodies made of the fields it reads and writes, with values of every type.
//!
//! Responses are the other generators' event streams and final events, sent
//! through the registry for the pair they were made for or, sometimes, a pair
//! with no translator.

use serde_json::{Map, Value, json};

use super::Rng;
use crate::cases::Case;
use crate::cases::registry::FORMATS;

/// Format names the registry's tables don't hold. The summary handling reads
/// the first three as formats it knows.
const OTHER_FORMATS: &[&str] = &[
    " OpenAI ",
    "CLAUDE",
    "Codex",
    "openai-responses",
    "gemini-cli",
    "",
];

/// Models for the summary handling, which turns Claude thinking on as the
/// model allows: some with thinking levels, some with only a budget, some
/// that can't think, and some it doesn't know.
const MODELS: &[&str] = &[
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-opus-5",
    "claude-fable-5-1",
    "claude-sonnet-4-5-20250929",
    "claude-haiku-4-5-20251001",
    "claude-opus-4-1-20250805",
    "claude-3-7-sonnet-20250219",
    "claude-3-5-haiku-20241022",
    "claude-opus-4-6-thinking",
    "claude-sonnet-4-5-20250929(16000)",
    "claude-opus-4-6(high)",
    "claude-sonnet-4-5-20250929(none)",
    " claude-opus-4-6 ",
    "Claude-Opus-4-6",
    "gemini-2.5-pro",
    "gpt-5",
    "unknown-model",
    "",
];

/// Where each format keeps its summary setting.
const OPENAI_PATHS: &[&str] = &[
    "extra_body.google.thinking_config.include_thoughts",
    "extra_body.google.thinking_config.includeThoughts",
    "extra_body.google.thinkingConfig.include_thoughts",
    "extra_body.google.thinkingConfig.includeThoughts",
    "extra_body.extra_body.google.thinking_config.include_thoughts",
    "extra_body.extra_body.google.thinking_config.includeThoughts",
    "google.thinking_config.include_thoughts",
    "google.thinking_config.includeThoughts",
    "thinking.includeThoughts",
    "thinking.include_thoughts",
    "reasoning.includeThoughts",
    "reasoning.include_thoughts",
    "generationConfig.thinkingConfig.includeThoughts",
    "generationConfig.thinkingConfig.include_thoughts",
    "generation_config.thinking_config.include_thoughts",
    "generation_config.thinking_config.includeThoughts",
    "reasoning.summary",
    "reasoning.generate_summary",
    "reasoning.exclude",
    "include_reasoning",
    "reasoning.enabled",
    "reasoning_effort",
];
const RESPONSES_PATHS: &[&str] = &["reasoning.summary", "reasoning.generate_summary"];
const CLAUDE_PATHS: &[&str] = &[
    "thinking.display",
    "thinking.type",
    "thinking.budget_tokens",
    "max_tokens",
];
const GEMINI_PATHS: &[&str] = &[
    "generationConfig.thinkingConfig.includeThoughts",
    "generationConfig.thinkingConfig.include_thoughts",
    "generation_config.thinking_config.include_thoughts",
    "generation_config.thinking_config.includeThoughts",
];
const ANTIGRAVITY_PATHS: &[&str] = &[
    "request.generationConfig.thinkingConfig.includeThoughts",
    "request.generationConfig.thinkingConfig.include_thoughts",
    "request.generationConfig.thinking_config.includeThoughts",
    "request.generationConfig.thinking_config.include_thoughts",
];
const INTERACTIONS_PATHS: &[&str] = &[
    "generation_config.thinking_summaries",
    "generation_config.thinkingSummaries",
    "reasoning.summary",
    "generation_config.thinking_config.include_thoughts",
    "generation_config.thinking_config.includeThoughts",
    "generation_config.thinkingConfig.include_thoughts",
    "generation_config.thinkingConfig.includeThoughts",
];

/// Objects on the way to the settings, which a body may hold as something
/// else.
const PARENTS: &[&str] = &[
    "reasoning",
    "thinking",
    "generationConfig",
    "generation_config",
    "request",
    "request.generationConfig",
    "extra_body",
];

/// Values for a setting: what each format accepts, in other cases and with
/// spaces, and values of other types.
const VALUES: &[&str] = &[
    "true",
    "false",
    "\"auto\"",
    "\"concise\"",
    "\"detailed\"",
    "\"none\"",
    "\" Detailed \"",
    "\"AUTO\"",
    "\"summarized\"",
    "\"omitted\"",
    "\" Omitted \"",
    "\"high\"",
    "\"minimal\"",
    "\"\"",
    "\"true\"",
    "null",
    "1",
    "0",
    "{}",
    "[]",
];

const THINKING_TYPES: &[&str] = &[
    "\"adaptive\"",
    "\"enabled\"",
    "\" Enabled \"",
    "\"ADAPTIVE\"",
    "\"disabled\"",
    "\"\"",
    "1",
    "null",
];

/// Token counts as gjson's `Int()` reads them, within `i64`'s range: Go's
/// conversion of a float beyond it depends on the CPU.
const TOKENS: &[&str] = &[
    "-1",
    "0",
    "1",
    "1023",
    "1024",
    "1025",
    "2048",
    "64000",
    "\"4096\"",
    "\"1024\"",
    "\" 2048\"",
    "1.5",
    "1024.9",
    "1e3",
    "1.025e3",
    "-2",
    "9007199254740993",
    "null",
    "true",
    "false",
    "\"x\"",
];

/// Builds `count` request cases.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    let builtin = count / 2;
    let mut cases = builtin_requests(seed, builtin);
    cases.extend((0..(count - builtin) as u64).map(|index| identity_request(seed, index)));
    cases
}

/// Builds `count` stream cases and as many non-streaming cases.
pub fn response_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let source_seed = derived(seed);
    let counts = split(count);
    let sources = [
        (
            ("codex", "claude"),
            super::response::cases(source_seed, counts[0]),
        ),
        (
            ("codex", "openai-response"),
            super::responses::event_cases(source_seed, counts[1]),
        ),
        (
            ("codex", "openai"),
            super::chat::event_cases(source_seed, counts[2]),
        ),
        (
            ("claude", "openai"),
            super::claude_chat::event_cases(source_seed, counts[3]),
        ),
        (
            ("claude", "openai-response"),
            super::claude_responses::event_cases(source_seed, counts[4]),
        ),
    ];
    let (mut streams, mut finals) = (Vec::new(), Vec::new());
    let mut index = 0;
    for ((from, to), (source_streams, source_finals)) in sources {
        for (stream, last) in source_streams.into_iter().zip(source_finals) {
            let mut rng = rng(seed, index);
            index += 1;
            // Now and then a pair with no translator.
            let (from, to) = if rng.chance(10) {
                (rng.pick(&[from, to, "gemini", "Codex"]), rng.pick(FORMATS))
            } else {
                (from, to)
            };
            let options = json!({ "from": from, "to": to });
            streams.push(stream.with_options(options.clone()));
            finals.push(last.with_options(options));
        }
    }
    (streams, finals)
}

/// Builds `count` lookup cases.
pub fn lookup_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(seed, index);
            let mut format = || {
                if rng.chance(80) {
                    rng.pick(FORMATS)
                } else {
                    rng.pick(OTHER_FORMATS)
                }
            };
            let (from, to) = (format(), format());
            let count = rng.pick(&[0, 1, 7, -1, 123_456, i64::MAX, i64::MIN]);
            let body = rng.pick(&["{}", "", "raw", "{\"input_tokens\":1}"]);
            Case::new(format!("random-{seed}-{index}"), "", body)
                .with_options(json!({ "from": from, "to": to, "count": count }))
        })
        .collect()
}

/// A seed for the other generators, so their cases here differ from those in
/// their own suites.
fn derived(seed: u64) -> u64 {
    seed ^ 0x5245_4749_5354_5259
}

fn rng(seed: u64, index: u64) -> Rng {
    Rng(derived(seed) ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// `count` split across five sources.
fn split(count: usize) -> [usize; 5] {
    let mut counts = [count / 5; 5];
    counts[0] += count % 5;
    counts
}

/// Requests from the other generators through the built-in translators.
fn builtin_requests(seed: u64, count: usize) -> Vec<Case> {
    let source_seed = derived(seed);
    let counts = split(count);
    let sources = [
        (("claude", "codex"), super::cases(source_seed, counts[0])),
        (
            ("openai-response", "codex"),
            super::responses::request_cases(source_seed, counts[1]),
        ),
        (
            ("openai", "codex"),
            super::chat::request_cases(source_seed, counts[2]),
        ),
        (
            ("openai", "claude"),
            super::claude_chat::request_cases(source_seed, counts[3]),
        ),
        (
            ("openai-response", "claude"),
            super::claude_responses::request_cases(source_seed, counts[4]),
        ),
    ];
    let mut cases = Vec::with_capacity(count);
    for ((from, to), source) in sources {
        for mut case in source {
            let index = cases.len() as u64;
            let mut rng = rng(seed, index);
            let to = if rng.chance(20) {
                if rng.chance(80) {
                    rng.pick(FORMATS)
                } else {
                    rng.pick(OTHER_FORMATS)
                }
            } else {
                to
            };
            if rng.chance(30) {
                case.model = rng.pick(MODELS).to_owned();
            }
            if rng.chance(60) {
                // Requests that aren't JSON stay as they are.
                if let Ok(Value::Object(mut body)) = serde_json::from_str(&case.request) {
                    for _ in 0..1 + rng.below(3) {
                        let paths = if rng.chance(70) {
                            source_paths(from)
                        } else {
                            rng.pick(&[OPENAI_PATHS, CLAUDE_PATHS, GEMINI_PATHS])
                        };
                        let path = rng.pick(paths);
                        let value = value_for(&mut rng, path);
                        set(&mut body, path, value);
                    }
                    case.request = Value::Object(body).to_string();
                }
            }
            let stream = rng.chance(50);
            case.name = format!("builtin-{seed}-{index}");
            cases.push(case.with_options(json!({ "from": from, "to": to, "stream": stream })));
        }
    }
    cases
}

fn source_paths(format: &str) -> &'static [&'static str] {
    match format {
        "openai" => OPENAI_PATHS,
        "claude" => CLAUDE_PATHS,
        _ => RESPONSES_PATHS,
    }
}

/// A request through a translator that changes nothing.
fn identity_request(seed: u64, index: u64) -> Case {
    let mut rng = rng(seed, !index);
    let mut format = || {
        if rng.chance(85) {
            rng.pick(FORMATS)
        } else {
            rng.pick(OTHER_FORMATS)
        }
    };
    let (from, to) = (format(), format());

    let mut fields: Vec<(&str, Value)> = Vec::new();
    // Mostly the source's settings, which the registry reads.
    for _ in 0..1 + rng.below(3) {
        let paths = if rng.chance(70) {
            settings_of(from.trim().to_lowercase().as_str())
        } else {
            rng.pick(&[
                OPENAI_PATHS,
                CLAUDE_PATHS,
                GEMINI_PATHS,
                ANTIGRAVITY_PATHS,
                INTERACTIONS_PATHS,
            ])
        };
        let path = rng.pick(paths);
        fields.push((path, value_for(&mut rng, path)));
    }
    // Then what the provider's settings are written over.
    for _ in 0..rng.below(3) {
        let paths = settings_of(to.trim().to_lowercase().as_str());
        let path = rng.pick(paths);
        fields.push((path, value_for(&mut rng, path)));
    }
    if rng.chance(50) {
        fields.push(("thinking.type", raw(rng.pick(THINKING_TYPES))));
    }
    if rng.chance(30) {
        fields.push(("thinking.budget_tokens", raw(rng.pick(TOKENS))));
    }
    if rng.chance(40) {
        fields.push(("max_tokens", raw(rng.pick(TOKENS))));
    }
    if rng.chance(40) {
        let model = if rng.chance(90) {
            rng.pick(MODELS).into()
        } else {
            json!(5)
        };
        fields.push(("model", model));
    }
    if rng.chance(15) {
        let parent = rng.pick(PARENTS);
        fields.push((
            parent,
            raw(rng.pick(&["null", "\"x\"", "1", "[]", "[1]", "{}"])),
        ));
    }
    if rng.chance(50) {
        fields.push(("messages", json!([{ "role": "user", "content": "hi" }])));
    }
    rng.shuffle(&mut fields);

    let mut body = Map::new();
    for (path, value) in fields {
        set(&mut body, path, value);
    }
    let body = Value::Object(body);
    let request = if rng.chance(20) {
        serde_json::to_string_pretty(&body)
    } else {
        serde_json::to_string(&body)
    }
    .expect("a Value always serializes");
    let model = if rng.chance(15) {
        String::new()
    } else {
        rng.pick(MODELS).to_owned()
    };
    Case::new(format!("identity-{seed}-{index}"), model, request).with_options(json!({
        "from": from,
        "to": to,
        "stream": rng.chance(50),
        "identity": true,
    }))
}

/// Where `format` keeps its summary setting, or everyone's for a format the
/// registry doesn't know.
fn settings_of(format: &str) -> &'static [&'static str] {
    match format {
        "openai" => OPENAI_PATHS,
        "openai-response" | "codex" => RESPONSES_PATHS,
        "claude" => CLAUDE_PATHS,
        "gemini" => GEMINI_PATHS,
        "antigravity" => ANTIGRAVITY_PATHS,
        "interactions" => INTERACTIONS_PATHS,
        _ => OPENAI_PATHS,
    }
}

fn value_for(rng: &mut Rng, path: &str) -> Value {
    let pool = match path {
        "thinking.type" => THINKING_TYPES,
        "thinking.budget_tokens" | "max_tokens" => TOKENS,
        _ => VALUES,
    };
    raw(rng.pick(pool))
}

fn raw(text: &str) -> Value {
    serde_json::from_str(text).expect("the pools hold JSON")
}

/// Sets the value at a dotted path, making the objects on the way. A value
/// already on the way that isn't an object stays, and so does one already at
/// the path.
fn set(body: &mut Map<String, Value>, path: &str, value: Value) {
    let (parents, key) = match path.rsplit_once('.') {
        Some((parents, key)) => (Some(parents), key),
        None => (None, path),
    };
    let mut object = body;
    for parent in parents.into_iter().flat_map(|parents| parents.split('.')) {
        match object.entry(parent).or_insert_with(|| json!({})) {
            Value::Object(next) => object = next,
            _ => return,
        }
    }
    object.entry(key).or_insert(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        let first = request_cases(3, 40);
        let again = request_cases(3, 40);
        assert_eq!(first.len(), 40);
        for (a, b) in first.iter().zip(&again) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.options, b.options);
            assert!(
                serde_json::from_str::<Value>(&a.request).is_ok(),
                "{}",
                a.name
            );
        }
        let (streams, finals) = response_cases(3, 12);
        assert_eq!((streams.len(), finals.len()), (12, 12));
        assert_eq!(lookup_cases(3, 9).len(), 9);
    }

    #[test]
    fn settings_are_set_at_their_paths() {
        let mut body = Map::new();
        set(&mut body, "reasoning.summary", json!("auto"));
        set(&mut body, "reasoning.summary", json!("none"));
        set(&mut body, "thinking", json!("x"));
        set(&mut body, "thinking.type", json!("adaptive"));
        set(&mut body, "max_tokens", json!(5));
        assert_eq!(
            Value::Object(body),
            json!({ "reasoning": { "summary": "auto" }, "thinking": "x", "max_tokens": 5 })
        );
    }
}
