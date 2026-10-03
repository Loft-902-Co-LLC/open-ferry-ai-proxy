//! Hand-written cases for the translator registry: every other suite's
//! hand-written cases, sent through the registry for their own pair of
//! formats, and cases for what the registry does itself.

use serde_json::{Value, json};

use super::Case;

/// The formats upstream names, as the registry's tables key them.
pub const FORMATS: &[&str] = &[
    "openai",
    "openai-response",
    "claude",
    "gemini",
    "codex",
    "antigravity",
    "interactions",
];

/// `case` sent through the registry from `from` to `to`, with `options` added.
fn through(case: Case, from: &str, to: &str, options: Value) -> Case {
    let mut fields = json!({ "from": from, "to": to });
    if let (Value::Object(fields), Value::Object(options)) = (&mut fields, options) {
        fields.extend(options);
    }
    Case {
        name: format!("{from}-to-{to}-{}", case.name),
        ..case
    }
    .with_options(fields)
}

fn request(name: &str, model: &str, body: Value) -> Case {
    Case::new(name, model, body.to_string())
}

/// A request through a translator that changes nothing, so only the
/// registry's own handling shows.
fn identity(case: Case, from: &str, to: &str) -> Case {
    through(case, from, to, json!({ "identity": true }))
}

pub fn requests() -> Vec<Case> {
    let stream = || json!({ "stream": true });
    let mut cases = Vec::new();
    for ((from, to), source) in [
        (("claude", "codex"), super::hand_written()),
        (("openai-response", "codex"), super::responses::requests()),
        (("openai", "codex"), super::chat::requests()),
        (("openai", "claude"), super::claude_chat::requests()),
        (
            ("openai-response", "claude"),
            super::claude_responses::requests(),
        ),
    ] {
        cases.extend(
            source
                .into_iter()
                .map(|case| through(case, from, to, stream())),
        );
    }

    // No translator: only the model changes, to the one asked for, unless
    // that is empty or the body's already reads the same.
    let fallback = [
        request(
            "prefixed-model",
            "gpt-5",
            json!({ "model": "x/gpt-5", "input": "hi" }),
        ),
        request("empty-model", "", json!({ "model": "x/gpt-5" })),
        request("no-body-model", "gpt-5", json!({ "input": "hi" })),
        request("numeric-body-model", "123", json!({ "model": 123 })),
        request("boolean-body-model", "true", json!({ "model": true })),
        request("null-body-model", "gpt-5", json!({ "model": null })),
        request(
            "object-body-model",
            "gpt-5",
            json!({ "model": { "id": "gpt-5" } }),
        ),
        request("padded-model", "gpt-5", json!({ "model": "gpt-5 " })),
        request("array-body", "gpt-5", json!([1])),
        request("string-body", "gpt-5", json!("text")),
        request("number-body", "gpt-5", json!(5)),
        request("null-body", "gpt-5", Value::Null),
        request("empty-body", "gpt-5", json!({})),
        request(
            "summary-fields-left-alone",
            "gpt-5",
            json!({ "reasoning_effort": "high", "reasoning": { "summary": "auto" } }),
        ),
    ];
    for case in fallback {
        for (from, to) in [
            ("claude", "gemini"),
            ("claude", "claude"),
            ("codex", "openai"),
            ("Claude", "codex"),
        ] {
            cases.push(through(case.clone(), from, to, stream()));
        }
    }
    // An object model named as its compact JSON. Upstream compares the text as
    // written, so it replaces the spaced one.
    let compact = Case::new("compact-object-model", "{}", r#"{"model":{}}"#);
    let spaced = Case::new("spaced-object-model", "{}", r#"{"model":{ }}"#)
        .known_difference("an object model is compared as compact JSON; gjson reads its text");
    for case in [compact, spaced] {
        cases.push(through(case, "claude", "gemini", stream()));
    }

    // Summary settings carried from the client's format to the provider's.
    let summaries = [
        // Chat Completions: reasoning effort counts, but not toward Claude.
        (
            "openai",
            "codex",
            "gpt-5",
            json!({ "reasoning_effort": "high" }),
        ),
        (
            "openai",
            "codex",
            "gpt-5",
            json!({ "reasoning_effort": " None " }),
        ),
        (
            "openai",
            "codex",
            "gpt-5",
            json!({ "reasoning_effort": "", "reasoning": { "summary": "x" } }),
        ),
        (
            "openai",
            "claude",
            "claude-opus-4-6",
            json!({ "reasoning_effort": "high" }),
        ),
        (
            "openai",
            "claude",
            "claude-opus-4-6",
            json!({ "reasoning": { "exclude": false }, "max_tokens": 100 }),
        ),
        (
            "openai",
            "gemini",
            "gemini-2.5-pro",
            json!({ "extra_body": { "google": { "thinking_config": { "include_thoughts": true } } }, "reasoning_effort": "none" }),
        ),
        (
            "openai",
            "openai",
            "gpt-5",
            json!({ "include_reasoning": false, "reasoning": { "exclude": "no" } }),
        ),
        (
            "openai",
            "openai",
            "gpt-5",
            json!({ "reasoning": { "enabled": true, "exclude": true }, "include_reasoning": true }),
        ),
        // Responses and Codex.
        (
            "openai-response",
            "codex",
            "gpt-5",
            json!({ "reasoning": { "summary": " Detailed ", "generate_summary": "auto" } }),
        ),
        (
            "openai-response",
            "codex",
            "gpt-5",
            json!({ "reasoning": { "summary": null, "generate_summary": "concise" } }),
        ),
        (
            "codex",
            "openai-response",
            "gpt-5",
            json!({ "reasoning": { "summary": "none" } }),
        ),
        (
            "codex",
            "codex",
            "gpt-5",
            json!({ "reasoning": { "generate_summary": "detailed", "effort": "low" } }),
        ),
        (
            "codex",
            "codex",
            "gpt-5",
            json!({ "reasoning": { "summary": 1 } }),
        ),
        // Claude: display counts only alongside active thinking.
        (
            "claude",
            "codex",
            "gpt-5",
            json!({ "thinking": { "type": "adaptive", "display": "summarized" } }),
        ),
        (
            "claude",
            "codex",
            "gpt-5",
            json!({ "thinking": { "type": "enabled", "budget_tokens": 0, "display": "summarized" } }),
        ),
        (
            "claude",
            "codex",
            "gpt-5",
            json!({ "thinking": { "type": " Enabled ", "budget_tokens": -1, "display": " Omitted " } }),
        ),
        (
            "claude",
            "codex",
            "gpt-5",
            json!({ "thinking": { "type": "enabled", "budget_tokens": "0", "display": "summarized" } }),
        ),
        (
            "claude",
            "codex",
            "gpt-5",
            json!({ "thinking": { "type": "disabled", "display": "summarized" } }),
        ),
        // Gemini, Antigravity and Interactions.
        (
            "gemini",
            "antigravity",
            "gemini-2.5-pro",
            json!({ "generationConfig": { "thinkingConfig": { "include_thoughts": true } } }),
        ),
        (
            "antigravity",
            "gemini",
            "gemini-2.5-pro",
            json!({ "request": { "generationConfig": { "thinking_config": { "includeThoughts": false } } }, "generation_config": { "thinking_config": { "include_thoughts": true } } }),
        ),
        (
            "interactions",
            "openai-response",
            "gpt-5",
            json!({ "generation_config": { "thinkingSummaries": "auto" }, "reasoning": { "summary": "none" } }),
        ),
        (
            "interactions",
            "interactions",
            "gpt-5",
            json!({ "reasoning": { "summary": "detailed" }, "generation_config": { "thinking_config": { "include_thoughts": false } } }),
        ),
        (
            "openai-response",
            "interactions",
            "gpt-5",
            json!({ "reasoning": { "summary": "concise" }, "generation_config": { "thinkingSummaries": "none" } }),
        ),
        // Claude as the provider: thinking is turned on when the model can
        // think, adaptively if it has levels, else with the smallest budget
        // that fits under max_tokens.
        (
            "openai-response",
            "claude",
            "claude-opus-4-6",
            json!({ "reasoning": { "summary": "auto" } }),
        ),
        (
            "openai-response",
            "claude",
            "claude-opus-5",
            json!({ "reasoning": { "summary": "auto" } }),
        ),
        (
            "openai-response",
            "claude",
            "claude-sonnet-4-5-20250929",
            json!({ "reasoning": { "summary": "auto" }, "max_tokens": 1024 }),
        ),
        (
            "openai-response",
            "claude",
            "claude-sonnet-4-5-20250929",
            json!({ "reasoning": { "summary": "auto" }, "max_tokens": 1025 }),
        ),
        (
            "openai-response",
            "claude",
            "claude-sonnet-4-5-20250929(16000)",
            json!({ "reasoning": { "summary": "auto" }, "max_tokens": "2000" }),
        ),
        (
            "openai-response",
            "claude",
            "claude-3-5-haiku-20241022",
            json!({ "reasoning": { "summary": "auto" } }),
        ),
        (
            "openai-response",
            "claude",
            "",
            json!({ "model": "claude-opus-4-6(high)", "reasoning": { "summary": "auto" } }),
        ),
        (
            "openai-response",
            "claude",
            "unknown-model",
            json!({ "reasoning": { "summary": "auto" } }),
        ),
        (
            "openai-response",
            "claude",
            "claude-opus-4-6",
            json!({ "reasoning": { "summary": "none" }, "thinking": { "type": "adaptive" } }),
        ),
        (
            "openai-response",
            "claude",
            "claude-opus-4-6",
            json!({ "reasoning": { "summary": "auto" }, "thinking": { "type": "disabled" } }),
        ),
        (
            "openai-response",
            "claude",
            "claude-opus-4-6",
            json!({ "reasoning": { "summary": "auto" }, "thinking": { "budget_tokens": 5000 } }),
        ),
        // Formats are read trimmed and lowercased.
        (
            " OpenAI ",
            "CLAUDE",
            "claude-opus-4-6",
            json!({ "reasoning": { "summary": "auto" } }),
        ),
        (
            "Codex",
            " Gemini",
            "gemini-2.5-pro",
            json!({ "reasoning": { "summary": "auto" } }),
        ),
        (
            "openai-response",
            "unknown",
            "gpt-5",
            json!({ "reasoning": { "summary": "auto" } }),
        ),
        // Bodies the summary can't be written into.
        (
            "openai-response",
            "codex",
            "gpt-5",
            json!({ "reasoning": [1], "include": [] }),
        ),
        (
            "openai-response",
            "gemini",
            "gpt-5",
            json!({ "reasoning": { "summary": "auto" }, "generationConfig": [] }),
        ),
        (
            "gemini",
            "codex",
            "gpt-5",
            json!({ "generationConfig": { "thinkingConfig": { "includeThoughts": true } }, "reasoning": "x" }),
        ),
        (
            "openai-response",
            "codex",
            "gpt-5",
            json!({ "reasoning": { "summary": "none", "generate_summary": null } }),
        ),
        (
            "openai-response",
            "codex",
            "gpt-5",
            json!({ "reasoning": {} }),
        ),
    ];
    for (index, (from, to, model, body)) in summaries.into_iter().enumerate() {
        let case = request(&format!("summary-{index}"), model, body);
        cases.push(identity(case, from, to));
    }
    for (index, body) in [json!([1]), json!("text"), json!(5), Value::Null]
        .into_iter()
        .enumerate()
    {
        let case = request(&format!("non-object-{index}"), "claude-opus-4-6", body);
        cases.push(identity(case.clone(), "openai-response", "claude"));
        cases.push(identity(case, "openai", "codex"));
    }
    cases
}

/// Response cases, for the pairs whose translators they were written for,
/// and every fifth also for the `fallback` pairs, which have no translator.
fn responses(sources: [Vec<Case>; 5], fallback: &[(&str, &str)]) -> Vec<Case> {
    let pairs = [
        ("codex", "claude"),
        ("codex", "openai-response"),
        ("codex", "openai"),
        ("claude", "openai"),
        ("claude", "openai-response"),
    ];
    let mut cases = Vec::new();
    for ((from, to), source) in pairs.into_iter().zip(sources) {
        for (index, case) in source.into_iter().enumerate() {
            if index % 5 == 0 {
                for (from, to) in fallback {
                    cases.push(through(case.clone(), from, to, json!({})));
                }
            }
            cases.push(through(case, from, to, json!({})));
        }
    }
    cases
}

pub fn streams() -> Vec<Case> {
    let mut cases = responses(
        [
            super::hand_written_streams(),
            super::responses::streams(),
            super::chat::streams(),
            super::claude_chat::streams(),
            super::claude_responses::streams(),
        ],
        &[("codex", "gemini"), ("Codex", "claude")],
    );
    let lines = vec![
        "data: {\"type\":\"response.created\"}".to_owned(),
        String::new(),
        "event: ping".to_owned(),
        "data: [DONE]".to_owned(),
    ];
    for (from, to) in [
        ("codex", "codex"),
        ("claude", "codex"),
        ("gemini", "openai"),
    ] {
        let case = Case::response("blank-lines", "{}", lines.clone());
        cases.push(through(case, from, to, json!({})));
    }
    cases
}

pub fn finals() -> Vec<Case> {
    let mut cases = responses(
        [
            super::hand_written_finals(),
            super::responses::finals(),
            super::chat::finals(),
            super::claude_chat::finals(),
            super::claude_responses::finals(),
        ],
        &[("claude", "claude")],
    );
    for (name, events) in [
        ("no-events", Vec::new()),
        ("empty-body", vec![String::new()]),
        ("not-json", vec!["not json".to_owned()]),
        ("json", vec!["{\"id\":1}".to_owned()]),
    ] {
        let case = Case::response(name, "{}", events);
        cases.push(through(case.clone(), "codex", "gemini", json!({})));
        cases.push(through(case, "codex", "claude", json!({})));
    }
    // An apply_patch call whose input isn't a patch: the translator fails, and
    // the registry returns nothing.
    let body = [
        r#"data: {"type":"message_start","message":{"id":"m"}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"apply_patch","input":{}}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        r#"data: {"type":"message_stop"}"#,
    ]
    .join("\n");
    let request = r#"{"tools":[{"type":"custom","name":"apply_patch"}]}"#;
    let case = Case::response("apply-patch-failed", request, vec![body]);
    cases.push(through(case, "claude", "openai-response", json!({})));
    cases
}

pub fn lookups() -> Vec<Case> {
    let mut cases = Vec::new();
    let formats = FORMATS.iter().chain(&["Codex", " claude", ""]);
    for from in formats.clone() {
        for to in formats.clone() {
            let case = Case::new("lookup", "", "{\"raw\":true}");
            cases.push(through(case, from, to, json!({ "count": 7 })));
        }
    }
    for (count, body) in [(0, ""), (-1, "raw"), (i64::MAX, "{}"), (i64::MIN, "{}")] {
        for (from, to) in [("codex", "claude"), ("claude", "codex")] {
            let case = Case::new("token-count", "", body);
            cases.push(through(case, from, to, json!({ "count": count })));
        }
    }
    cases
}
