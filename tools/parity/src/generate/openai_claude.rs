//! Seeded random input for the Claude → Chat Completions translators.
//!
//! Requests are the Claude request generator's ([`super::cases`]) with what
//! only the Chat Completions translator reads added: `top_p`,
//! `stop_sequences` and `user`, loosely typed, but never a `top_p` that
//! isn't finite, which upstream writes as invalid JSON. Each asks for a
//! stream or not.
//!
//! Responses are the Chat Completions generator's ([`super::openai_chat`])
//! answering a Claude request that declares tools by names in various cases,
//! padded or with leading underscores, and asks for a stream or doesn't. The
//! calls name those tools as declared, in another case or spacing, or not at
//! all, with arguments that are sometimes single-quoted.

use serde_json::{Value, json};

use super::openai_chat::{ARGUMENTS, Call, Generator};
use crate::cases::Case;

/// Builds `count` random Claude requests for a Chat Completions upstream.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    super::cases(seed.rotate_left(61), count)
        .into_iter()
        .enumerate()
        .map(|(index, case)| {
            let mut generator = Generator::new(seed.rotate_left(5), index as u64);
            let mut request: Value =
                serde_json::from_str(&case.request).expect("generated requests are JSON");
            request_fields(&mut generator, &mut request);
            let text = generator.render(&request);
            let stream = generator.rng.chance(50);
            Case::new(case.name, case.model, text).with_options(json!({ "stream": stream }))
        })
        .collect()
}

/// Builds `count` random Chat Completions streams answering random Claude
/// requests, and a non-streaming case from a whole response for each.
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed.rotate_left(7), index);
            let (request, calls) = original_request(&mut generator);
            let lines = generator.stream(&calls);
            let body = generator.body(&calls);
            let case = |events| Case::response(format!("random-{seed}-{index}"), &request, events);
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Tool names a client declares: Claude Code's, MCP tools, names with
/// leading underscores or padding, and names whose case maps differ between
/// Unicode's tables.
const TOOL_NAMES: &[&str] = &[
    "Bash",
    "Read",
    "TodoWrite",
    "mcp__github__get_me",
    "__Edit",
    "Write ",
    "web_search",
    "bash",
    "Ölçü",
    "ΑΣ",
    "İstanbul",
    "\u{212a}elvin",
    "",
];

/// Arguments as some models write them: single-quoted, which the translator
/// repairs, and with escapes inside the quotes.
const QUOTED_ARGUMENTS: &[&str] = &[
    r#"{'city': 'Paris'}"#,
    r#"{'a': 'it\'s', "b": 'He said "hi"'}"#,
    r#"{'a': 'x\\y\n', 'n': 1}"#,
    r#"{'a': '\q'}"#,
    r#"{'a': 'open"#,
    r#"{'a': 1, 'b': [1, 'two']}"#,
];

/// Adds what only the Chat Completions translator reads to a Claude request.
fn request_fields(generator: &mut Generator, request: &mut Value) {
    let Value::Object(fields) = request else {
        return;
    };
    if generator.rng.chance(30) {
        let top_p = match generator.rng.below(10) {
            0..=5 => generator.number(),
            6 => json!("0.9"),
            7 => json!("1e-1"),
            8 => Value::Null,
            _ => json!(true),
        };
        fields.insert("top_p".into(), top_p);
    }
    if generator.rng.chance(25) {
        let stops = match generator.rng.below(10) {
            0..=6 => {
                let count = generator.rng.below(4);
                Value::Array((0..count).map(|_| generator.loose_text()).collect())
            }
            7 => json!("END"),
            8 => Value::Null,
            _ => json!({ "0": "END" }),
        };
        fields.insert("stop_sequences".into(), stops);
    }
    if generator.rng.chance(15) {
        let user = generator.loose_text();
        fields.insert("user".into(), user);
    }
    if generator.rng.chance(10) {
        // Calls one at a time, which the Claude generator asks for less often.
        let choice_type = generator.rng.pick(&["auto", "any", "none"]);
        let choice = json!({ "type": choice_type, "disable_parallel_tool_use": true });
        fields.insert("tool_choice".into(), choice);
    }
}

/// The client's Claude request, as JSON text, and the calls a model might
/// make to its tools.
fn original_request(generator: &mut Generator) -> (String, Vec<Call>) {
    let mut fields = Vec::new();
    let mut declared = Vec::new();
    if generator.rng.chance(75) {
        let tools: Vec<Value> = (0..generator.rng.below(5))
            .map(|_| {
                let name = generator.rng.pick(TOOL_NAMES);
                declared.push(name);
                match generator.rng.below(10) {
                    0..=6 => {
                        json!({ "name": name, "description": "A tool.", "input_schema": { "type": "object", "properties": {} } })
                    }
                    7 => json!({ "type": "function", "function": { "name": name } }),
                    8 => json!({ "name": "", "function": { "name": name } }),
                    _ => json!({ "type": "web_search_20250305", "name": name, "max_uses": 5 }),
                }
            })
            .collect();
        let tools = if generator.rng.chance(5) {
            generator.one_of(&[json!({}), json!("tools"), json!([{ "name": 5 }, 1])])
        } else {
            Value::Array(tools)
        };
        fields.push(("tools", tools));
    }
    if generator.rng.chance(80) {
        fields.push(("model", json!("claude-sonnet-4-5")));
    }
    fields.push(("messages", json!([{ "role": "user", "content": "Hi" }])));
    match generator.rng.below(20) {
        0..=9 => fields.push(("stream", json!(true))),
        10..=13 => fields.push(("stream", json!(false))),
        14 => fields.push(("stream", Value::Null)),
        15 => fields.push(("stream", json!("true"))),
        16 => fields.push(("stream", json!(0))),
        _ => {}
    }
    let request = generator.object(fields);
    let text = match generator.rng.below(100) {
        0..=9 => String::new(),
        10 | 11 => "{not json".to_owned(),
        12 => "[]".to_owned(),
        _ => generator.render(&request),
    };

    let mut calls: Vec<Call> = Vec::new();
    for name in declared {
        let arguments = if generator.rng.chance(20) {
            QUOTED_ARGUMENTS
        } else {
            ARGUMENTS
        };
        // As declared, then as a model might write it back.
        calls.push((name.to_owned(), arguments));
        let variant = match generator.rng.below(6) {
            0 => name.to_lowercase(),
            1 => name.to_uppercase(),
            2 => format!(" {name} "),
            3 => format!("_{name}"),
            4 => format!("__{}", name.trim().to_lowercase()),
            _ => name.trim().to_owned(),
        };
        calls.push((variant, arguments));
    }
    for name in ["Glob", "undeclared_tool"] {
        if generator.rng.chance(30) {
            calls.push((name.to_owned(), ARGUMENTS));
        }
    }
    (text, calls)
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
            assert_eq!(a.options, b.options);
            serde_json::from_str::<Value>(&a.request).expect("generated request is valid JSON");
        }
        let (streams, finals) = event_cases(7, 200);
        let (again, finals_again) = event_cases(7, 200);
        for (a, b) in streams.iter().zip(&again) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.events, b.events);
        }
        for (a, b) in finals.iter().zip(&finals_again) {
            assert_eq!(a.events, b.events);
        }
    }

    /// Guards against a generator that never reaches the translators' branches.
    #[test]
    fn cases_cover_the_translators_branches() {
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
        let check = |outputs: &[String], needles: &[&str]| {
            for needle in needles {
                let count = outputs
                    .iter()
                    .filter(|output| output.contains(needle))
                    .count();
                assert!(count >= 20, "{needle} in {count} outputs");
            }
        };

        let requests = outputs(Translator::OpenAIClaudeRequest, &request_cases(1, 2000));
        check(
            &requests,
            &[
                r#""top_p":"#,
                r#""stop":["#,
                r#""user":"#,
                r#""parallel_tool_calls":false"#,
                r#""stream":true"#,
                r#""tool_calls":["#,
                r#""role":"tool""#,
            ],
        );

        let (streams, finals) = event_cases(1, 2000);
        let streams = outputs(Translator::OpenAIClaudeStream, &streams);
        check(
            &streams,
            &[
                r#""type":"tool_use""#,
                r#""name":"Bash""#,
                r#""name":"Ölçü""#,
                r#""type":"input_json_delta""#,
                r#""type":"thinking_delta""#,
                r#""stop_reason":"tool_use""#,
                r#""stop_reason":"max_tokens""#,
                r#""cache_read_input_tokens":"#,
                r#""type":"message_stop""#,
                r#"{"json":{"#,
            ],
        );
        let finals = outputs(Translator::OpenAIClaudeNonStream, &finals);
        check(
            &finals,
            &[
                r#""type":"tool_use""#,
                r#""type":"thinking""#,
                r#""stop_reason":"max_tokens""#,
                r#""cache_read_input_tokens":"#,
            ],
        );
    }
}
