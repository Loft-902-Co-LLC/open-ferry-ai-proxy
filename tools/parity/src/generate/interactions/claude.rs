//! Random input for the Claude and Interactions translators' suites (P4
//! WP4-A, see `crate::interactions::claude`): Claude Messages requests and
//! event streams for the Interactions translators, and Interactions requests
//! and events (from the parent module) for the Claude ones.
//!
//! The Claude requests are the Gemini translators' (see
//! `to_gemini::claude_request_cases`), now and then with a `system` message
//! among the others, which the translator to Interactions holds back until
//! the tool results it would split. The Claude event streams are the Chat
//! Completions translators' (see `claude_chat::event_cases`); the
//! non-streaming cases are their streams as one SSE body, or now and then a
//! whole Claude message.
//!
//! Both response translators read token counts as int64s. Go converts a
//! number out of int64's range by the CPU's rules, and the result then
//! decides the totals too, so the shared pools' two such literals become
//! large numbers within range (see [`in_int64_range`]).

use serde_json::{Value, json};

use super::super::{Rng, TEXTS, to_gemini, to_object};
use crate::cases::Case;

/// `count` Claude Messages requests, each with a `stream` option.
pub fn claude_request_cases(seed: u64, count: usize) -> Vec<Case> {
    to_gemini::claude_request_cases(seed.rotate_left(29), count)
        .into_iter()
        .enumerate()
        .map(|(index, mut case)| {
            let mut rng = rng(seed, index as u64);
            if rng.chance(25)
                && let Ok(mut request) = serde_json::from_str::<Value>(&case.request)
                && let Some(Value::Array(messages)) = request.get_mut("messages")
            {
                let at = rng.below(messages.len() + 1);
                let content = if rng.chance(60) {
                    json!(rng.pick(TEXTS))
                } else {
                    json!([{ "type": "text", "text": rng.pick(TEXTS) }])
                };
                messages.insert(at, json!({ "role": "system", "content": content }));
                case.request = request.to_string();
            }
            case
        })
        .collect()
}

/// `count` Interactions requests, each with a `stream` option.
pub fn interactions_request_cases(seed: u64, count: usize) -> Vec<Case> {
    super::request_cases(seed.rotate_left(31), count)
        .into_iter()
        .enumerate()
        .map(|(index, case)| {
            let stream = rng(seed, !(index as u64)).chance(50);
            case.with_options(json!({ "stream": stream }))
        })
        .collect()
}

/// `count` Interactions event streams, and as many whole interactions, for
/// the translators to Claude.
pub fn interactions_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let (streams, finals) = super::event_cases(seed.rotate_left(37), count);
    let in_range = |mut case: Case| {
        case.events = case.events.into_iter().map(in_int64_range).collect();
        case
    };
    (
        streams.into_iter().map(in_range).collect(),
        finals.into_iter().map(in_range).collect(),
    )
}

/// `count` Claude event streams, and as many non-streaming cases: the same
/// stream as one SSE body, or now and then a whole message.
pub fn claude_event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    let (streams, finals) = super::super::claude_chat::event_cases(seed.rotate_left(41), count);
    let finals = finals
        .into_iter()
        .enumerate()
        .map(|(index, mut case)| {
            let mut rng = rng(seed, index as u64 ^ 0x5A5A);
            if rng.chance(40) {
                let message = message(&mut rng);
                case.events = vec![if rng.chance(30) {
                    serde_json::to_string_pretty(&message).expect("a Value always serializes")
                } else {
                    message.to_string()
                }];
            }
            case
        })
        .collect();
    (streams, finals)
}

/// `text` with the shared pools' number literals out of int64's range
/// replaced by large ones within it. `serde_json` writes `1e30` as `1e+30`.
fn in_int64_range(text: String) -> String {
    text.replace("123456789012345678901234567890", "1234567890123456789")
        .replace("1e+30", "1e18")
        .replace("1e30", "1e18")
}

fn rng(seed: u64, index: u64) -> Rng {
    Rng(seed ^ 0x434C_4155_4445_4958 ^ index.wrapping_mul(0x9FB2_1C65_1E98_DF25))
}

/// Token counts, loosely written, none out of int64's range.
const COUNTS: &[&str] = &[
    "0",
    "-0",
    "1",
    "42",
    "-7",
    "1.50",
    "2.0",
    "1e3",
    "9007199254740993",
    "1234567890123456789",
    "5e-324",
];

fn count(rng: &mut Rng) -> Value {
    if rng.chance(70) {
        json!(rng.below(5000))
    } else {
        serde_json::from_str(rng.pick(COUNTS)).expect("counts are JSON numbers")
    }
}

/// A whole Claude message, as a non-streaming upstream answers.
fn message(rng: &mut Rng) -> Value {
    let mut fields: Vec<(&str, Value)> = Vec::new();
    match rng.below(10) {
        0 => {}
        1 => fields.push(("id", json!(""))),
        _ => fields.push(("id", json!(format!("msg_{}", rng.below(1000))))),
    }
    fields.push(("type", json!("message")));
    fields.push(("role", json!("assistant")));
    if rng.chance(80) {
        let model = rng.pick(&["claude-opus-4-6", "claude-sonnet-4-5-20250929", "", " "]);
        fields.push(("model", json!(model)));
    }
    let content = match rng.below(20) {
        0 => rng.pick(&[
            json!("text"),
            Value::Null,
            json!({ "type": "text", "text": "x" }),
        ]),
        _ => (0..rng.below(5)).map(|_| block(rng)).collect(),
    };
    fields.push(("content", content));
    if rng.chance(70) {
        let reason = rng.pick(&["end_turn", "tool_use", "max_tokens", "refusal"]);
        fields.push(("stop_reason", json!(reason)));
    }
    if rng.chance(80) {
        let mut usage: Vec<(&str, Value)> = Vec::new();
        for key in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "thinking_tokens",
        ] {
            if rng.chance(60) {
                usage.push((key, count(rng)));
            }
        }
        fields.push(("usage", to_object(usage)));
    }
    rng.shuffle(&mut fields);
    to_object(fields)
}

/// A content block of a Claude message.
fn block(rng: &mut Rng) -> Value {
    match rng.below(12) {
        0..=3 => json!({ "type": "text", "text": rng.pick(TEXTS) }),
        4 | 5 => {
            let mut fields = vec![
                ("type", json!("thinking")),
                ("thinking", json!(rng.pick(TEXTS))),
            ];
            if rng.chance(60) {
                fields.push(("signature", json!("EqQBCkYIBxgCKkA=")));
            }
            to_object(fields)
        }
        6..=8 => {
            let mut fields = vec![("type", json!("tool_use"))];
            if rng.chance(85) {
                fields.push(("id", json!(format!("toolu_{}", rng.below(100)))));
            }
            let name = rng.pick(&["get_weather", "lookup.v2", "名前", ""]);
            fields.push(("name", json!(name)));
            let input = match rng.below(8) {
                0..=3 => json!({ "city": rng.pick(TEXTS), "days": count(rng) }),
                4 => json!({}),
                5 => json!("{\"city\":\"Paris\"}"),
                6 => Value::Null,
                _ => json!([1, 2]),
            };
            if rng.chance(90) {
                fields.push(("input", input));
            }
            to_object(fields)
        }
        9 => json!({ "type": "redacted_thinking", "data": "EmwKAhgB" }),
        10 => {
            json!({ "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {} })
        }
        _ => rng.pick(&[
            json!({}),
            json!("text"),
            Value::Null,
            json!({ "type": "image" }),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases_are_reproducible_and_in_range() {
        let requests = claude_request_cases(3, 60);
        assert_eq!(requests.len(), 60);
        assert_eq!(requests[9].request, claude_request_cases(3, 60)[9].request);
        let requests = interactions_request_cases(3, 60);
        assert_eq!(
            requests[9].options,
            interactions_request_cases(3, 60)[9].options
        );
        for case in claude_request_cases(3, 60).iter().chain(&requests) {
            serde_json::from_str::<Value>(&case.request).expect("a request is JSON");
        }
        let (streams, finals) = interactions_event_cases(3, 200);
        let (claude_streams, claude_finals) = claude_event_cases(3, 200);
        assert_eq!((streams.len(), finals.len()), (200, 200));
        assert_eq!((claude_streams.len(), claude_finals.len()), (200, 200));
        assert_eq!(
            claude_finals[5].events,
            claude_event_cases(3, 200).1[5].events
        );
        let events = streams.iter().chain(&finals).flat_map(|case| &case.events);
        for event in events {
            assert!(
                !event.contains("e+30") && !event.contains("1e30"),
                "{event}"
            );
            assert!(!event.contains("123456789012345678901234567890"), "{event}");
        }
    }
}
