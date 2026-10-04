//! Hand-written cases for the OpenAI Responses and Interactions response
//! translators' suites: text, thoughts and their signatures, calls to
//! functions, custom tools and namespaced tools, `apply_patch` streamed in
//! pieces, named late, invalid, failed or cut short, and the ways a stream
//! starts and ends. The tool input error suite runs the same streams as
//! the Interactions → Responses one.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use serde_json::{Value, json};

use crate::cases::Case;

/// `apply_patch` declared in the `functions` namespace, as upstream's tests
/// declare it.
const PATCH_REQUEST: &str = r#"{"model":"gpt-5.1-codex","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}]}"#;

/// `apply_patch` declared directly, with other tools of each kind.
const TOOLS_REQUEST: &str = r#"{"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}},{"type":"function","name":"get_weather","parameters":{"type":"object"}},{"type":"custom","name":"run_sql"},{"type":"namespace","name":"mcp__github","tools":[{"type":"function","name":"search_code"}]}]}"#;

/// Tools other than `apply_patch`.
const PLAIN_REQUEST: &str = r#"{"model":"client-model","tools":[{"type":"function","name":"get_weather"},{"type":"custom","name":"run_sql"},{"type":"namespace","name":"browser","tools":[{"type":"function","name":"open"}]}]}"#;

/// The patch the cases' `apply_patch` calls carry.
const PATCH: &str = "*** Begin Patch\n*** Add File: 中.txt\n+😀\n*** End Patch\n";

/// A GPT reasoning signature, which the translators recognize, built as
/// upstream's tests build it.
fn gpt_signature() -> String {
    let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
    payload[0] = 0x80;
    payload[8] = 1;
    for (i, byte) in payload.iter_mut().enumerate().skip(9) {
        *byte = i as u8;
    }
    URL_SAFE.encode(payload)
}

/// Each event as its JSON text.
fn lines(events: &[Value]) -> Vec<String> {
    events.iter().map(Value::to_string).collect()
}

/// An Interactions stream case for a Responses client's `request`.
fn stream(name: &str, model: &str, request: &str, events: Vec<String>) -> Case {
    Case {
        model: model.to_owned(),
        ..Case::response(name, request, events)
    }
}

fn created(id: &str) -> Value {
    json!({ "event_type": "interaction.created", "interaction": { "id": id, "model": "gemini-3-pro-preview" } })
}

fn start(index: u64, step: Value) -> Value {
    json!({ "event_type": "step.start", "index": index, "step": step })
}

fn delta(index: u64, delta: Value) -> Value {
    json!({ "event_type": "step.delta", "index": index, "delta": delta })
}

fn arguments(index: u64, arguments: &str) -> Value {
    delta(
        index,
        json!({ "type": "arguments_delta", "arguments": arguments }),
    )
}

fn stop(index: u64) -> Value {
    json!({ "event_type": "step.stop", "index": index })
}

fn completed(usage: Value) -> Value {
    json!({ "event_type": "interaction.completed", "interaction": { "id": "interaction_1", "status": "completed", "usage": usage } })
}

/// The patch's arguments in three pieces, cut inside the first escape and
/// right after the first non-ASCII character.
fn patch_fragments() -> Vec<String> {
    let arguments = json!({ "input": PATCH }).to_string();
    let escape = arguments.find('\\').map_or(0, |at| at + 1);
    let character = arguments
        .find('中')
        .map_or(arguments.len(), |at| at + '中'.len_utf8());
    let (head, rest) = arguments.split_at(escape);
    let (middle, tail) = rest.split_at(character - escape);
    vec![head.to_owned(), middle.to_owned(), tail.to_owned()]
}

/// The Interactions → Responses stream cases.
pub fn to_responses_streams() -> Vec<Case> {
    let signature = gpt_signature();
    let patch_step = json!({ "type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch" });
    let mut patch_events = vec![created("interaction_1"), start(0, patch_step.clone())];
    patch_events.extend(patch_fragments().iter().map(|piece| arguments(0, piece)));
    patch_events.push(stop(0));
    let cut_short = patch_events.clone();
    patch_events.push(completed(
        json!({ "total_input_tokens": 3, "total_output_tokens": 4 }),
    ));
    vec![
        stream(
            "text-thought-and-usage",
            "gemini-3-pro-preview",
            "{}",
            lines(&[
                created("interaction_1"),
                json!({ "event_type": "interaction.status_update", "status": "in_progress" }),
                start(0, json!({ "type": "thought" })),
                delta(
                    0,
                    json!({ "type": "thought_summary", "content": { "type": "text", "text": "Thinking" } }),
                ),
                delta(
                    0,
                    json!({ "type": "thought_signature", "signature": signature }),
                ),
                stop(0),
                start(1, json!({ "type": "model_output" })),
                delta(1, json!({ "type": "text", "text": "Hello, " })),
                delta(1, json!({ "type": "text", "text": "<world> & 🚀" })),
                stop(1),
                completed(json!({
                    "total_input_tokens": 10,
                    "total_output_tokens": 5,
                    "total_thought_tokens": 2,
                    "total_cached_tokens": 1,
                    "total_tokens": 18,
                })),
            ]),
        ),
        stream(
            "calls-of-each-kind",
            "",
            TOOLS_REQUEST,
            lines(&[
                created("interaction_1"),
                start(
                    0,
                    json!({ "type": "function_call", "id": "call_a", "name": "get_weather" }),
                ),
                arguments(0, r#"{"city":"#),
                arguments(0, r#""Paris"}"#),
                stop(0),
                start(
                    1,
                    json!({ "type": "function_call", "call_id": "call_b", "name": "run_sql", "arguments": {} }),
                ),
                arguments(1, r#"{"input":"SELECT 1"}"#),
                stop(1),
                start(
                    2,
                    json!({ "type": "function_call", "id": "call_c", "name": "mcp__github__search_code" }),
                ),
                arguments(2, r#"{"q":"fn main"}"#),
                stop(2),
                completed(json!({ "total_input_tokens": 1 })),
            ]),
        ),
        stream(
            "apply-patch-in-pieces",
            "devin/swe-2",
            PATCH_REQUEST,
            lines(&patch_events),
        ),
        stream(
            "apply-patch-cut-short",
            "devin/swe-2",
            PATCH_REQUEST,
            lines(&cut_short),
        ),
        stream(
            "apply-patch-invalid-arguments",
            "devin/swe-2",
            TOOLS_REQUEST,
            lines(&[
                created("interaction_1"),
                start(
                    0,
                    json!({ "type": "function_call", "id": "item_1", "name": "apply_patch" }),
                ),
                arguments(0, r#"{"input":5}"#),
                stop(0),
                completed(json!({})),
            ]),
        ),
        stream(
            "apply-patch-named-late",
            "devin/swe-2",
            PATCH_REQUEST,
            lines(&[
                created("interaction_1"),
                start(0, json!({ "type": "function_call", "id": "item_2" })),
                arguments(0, r#"{"input":"*** Begin"#),
                json!({
                    "event_type": "step.delta",
                    "index": 0,
                    "step": { "type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch" },
                    "delta": { "type": "arguments_delta", "arguments": r#" Patch\n*** End Patch\n"}"# },
                }),
                stop(0),
                json!({
                    "event_type": "interaction.completed",
                    "steps": [{ "index": 0, "type": "function_call", "id": "item_2", "call_id": "call_2", "name": "functions__apply_patch", "arguments": { "input": "*** Begin Patch\n*** End Patch\n" } }],
                }),
            ]),
        ),
        stream(
            "apply-patch-never-named",
            "devin/swe-2",
            PATCH_REQUEST,
            lines(&[
                created("interaction_1"),
                start(0, json!({ "type": "function_call" })),
                arguments(0, r#"{"input":"p"}"#),
                stop(0),
                completed(json!({})),
            ]),
        ),
        stream(
            "apply-patch-upstream-failed",
            "devin/swe-2",
            PATCH_REQUEST,
            lines(&[
                created("interaction_1"),
                json!({ "event_type": "interaction.failed", "error": { "code": 500, "message": "boom" } }),
            ]),
        ),
        stream(
            "failed-without-apply-patch",
            "gemini-2.5-pro",
            PLAIN_REQUEST,
            lines(&[
                created("interaction_1"),
                start(0, json!({ "type": "model_output" })),
                delta(0, json!({ "type": "text", "text": "partial" })),
                json!({ "event_type": "interaction.failed", "interaction": { "error": { "code": "RESOURCE_EXHAUSTED", "message": "quota" } } }),
                json!({ "event_type": "step.stop", "index": 0 }),
            ]),
        ),
        stream(
            "environment-and-content-filter",
            "devin/swe-2",
            r#"{"model":"client-model"}"#,
            lines(&[
                json!({ "event_type": "interaction.created", "interaction": { "id": "interaction_9", "environment": { "id": "env_2" } } }),
                start(0, json!({ "type": "model_output" })),
                delta(0, json!({ "type": "text", "text": "Sorry" })),
                stop(0),
                json!({ "event_type": "interaction.completed", "interaction": { "status": "completed", "finish_reason": "content_filter" } }),
            ]),
        ),
        stream(
            "sse-frames-and-done",
            "gemini-2.5-flash",
            PLAIN_REQUEST,
            vec![
                format!(
                    "event: interaction.created\ndata: {}",
                    created("interaction_1")
                ),
                format!("data: {}", start(0, json!({ "type": "model_output" }))),
                format!(
                    "data: {}",
                    delta(0, json!({ "type": "text", "text": "hi" }))
                ),
                "data: [DONE]".to_owned(),
                format!("data: {}", stop(0)),
                "[DONE]".to_owned(),
            ],
        ),
        stream("no-events", "gemini-2.5-flash", PATCH_REQUEST, Vec::new()),
        Case {
            translated_request: PLAIN_REQUEST.to_owned(),
            ..stream(
                "request-only-as-sent-upstream",
                "gemini-2.5-flash",
                "",
                lines(&[
                    created("interaction_1"),
                    start(
                        0,
                        json!({ "type": "function_call", "id": "call_1", "name": "get_weather" }),
                    ),
                    arguments(0, "{}"),
                    completed(json!({})),
                ]),
            )
        },
    ]
}

/// The Interactions → Responses non-streaming cases.
pub fn to_responses_finals() -> Vec<Case> {
    let signature = gpt_signature();
    let body = |name: &str, request: &str, body: String| Case {
        model: "gemini-3-pro-preview".to_owned(),
        ..Case::response(name, request, vec![body])
    };
    let patch_call = |name: &str, arguments: Value| {
        json!({ "id": "interaction_1", "status": "completed", "steps": [{ "type": "function_call", "id": "call_1", "name": name, "arguments": arguments }] })
            .to_string()
    };
    vec![
        body(
            "steps-of-each-kind",
            TOOLS_REQUEST,
            json!({
                "id": "interaction_1",
                "status": "completed",
                "steps": [
                    { "type": "thought", "signature": signature, "content": [{ "type": "text", "text": "plan" }] },
                    { "type": "model_output", "id": "step_a", "content": [{ "type": "text", "text": "Hello" }, { "type": "image", "mime_type": "image/png", "data": "aGVsbG8=" }] },
                    { "type": "model_output", "content": "plain text" },
                    { "type": "function_call", "id": "call_1", "name": "get_weather", "arguments": { "city": "Paris" } },
                    { "type": "function_call", "call_id": "call_2", "name": "run_sql", "arguments": { "input": "SELECT 1" } },
                    { "type": "function_call", "id": "call_3", "name": "mcp__github__search_code", "arguments": "{\"q\":1}" },
                ],
                "usage": { "total_input_tokens": 7, "total_output_tokens": 3, "total_thought_tokens": 1 },
            })
            .to_string(),
        ),
        body(
            "wrapped-and-pretty-printed",
            PLAIN_REQUEST,
            "{\n  \"interaction\": {\n    \"id\": \"interaction_2\",\n    \"status\": \"incomplete\",\n    \"environment_id\": \"env_1\",\n    \"steps\": [\n      {\"type\": \"function_call\", \"id\": \"call_1\", \"name\": \"get_weather\", \"arguments\": { \"city\" : \"Zürich\" }},\n      {\"type\": \"function_call\", \"id\": \"call_2\", \"name\": \"run_sql\", \"arguments\": { \"query\" : [1, 2] }}\n    ]\n  }\n}".to_owned(),
        ),
        body(
            "apply-patch",
            PATCH_REQUEST,
            patch_call("functions__apply_patch", json!({ "input": PATCH })),
        ),
        body(
            "apply-patch-invalid",
            PATCH_REQUEST,
            patch_call("functions__apply_patch", json!({ "input": 5 })),
        ),
        body(
            "apply-patch-unnamed-call",
            PATCH_REQUEST,
            patch_call("", json!({ "input": PATCH })),
        ),
        body(
            "apply-patch-interaction-failed",
            PATCH_REQUEST,
            json!({ "status": "failed", "steps": [] }).to_string(),
        ),
        body(
            "error-without-apply-patch",
            PLAIN_REQUEST,
            json!({ "error": { "code": 429, "message": "slow down" } }).to_string(),
        ),
        body(
            "length",
            "",
            json!({ "id": "interaction_3", "model": "gemini-2.5-pro", "finish_reason": "max_tokens", "steps": [{ "type": "model_output", "content": [{ "type": "text", "text": "cut" }] }] }).to_string(),
        ),
        body("not-json", PLAIN_REQUEST, "not json".to_owned()),
        body("empty", PLAIN_REQUEST, String::new()),
    ]
}

/// A Responses event.
fn event(kind: &str, mut fields: Value) -> Value {
    fields["type"] = kind.into();
    fields
}

/// A Responses stream case for an Interactions client.
fn back(name: &str, model: &str, events: Vec<String>) -> Case {
    Case {
        model: model.to_owned(),
        ..Case::response(
            name,
            r#"{"model":"gemini-3-pro-preview","input":"hi"}"#,
            events,
        )
    }
}

fn response(status: &str, output: Value, usage: Value) -> Value {
    json!({ "id": "resp_1", "object": "response", "status": status, "model": "gpt-5.1-codex", "output": output, "usage": usage })
}

/// The Responses → Interactions stream cases.
pub fn to_interactions_streams() -> Vec<Case> {
    let message = json!({ "id": "msg_1", "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Hello there" }] });
    let call = json!({ "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "search_code", "namespace": "mcp__github", "arguments": "{\"q\":\"x\"}" });
    let usage = json!({ "input_tokens": 9, "output_tokens": 4, "total_tokens": 13, "input_tokens_details": { "cached_tokens": 2 }, "output_tokens_details": { "reasoning_tokens": 1 } });
    vec![
        back(
            "text-reasoning-and-call",
            "",
            lines(&[
                event(
                    "response.created",
                    json!({ "response": response("in_progress", json!([]), Value::Null) }),
                ),
                event(
                    "response.output_item.added",
                    json!({ "output_index": 0, "item": { "id": "rs_1", "type": "reasoning", "summary": [] } }),
                ),
                event(
                    "response.reasoning_summary_text.delta",
                    json!({ "item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "Think" }),
                ),
                event(
                    "response.output_item.done",
                    json!({ "output_index": 0, "item": { "id": "rs_1", "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Think" }] } }),
                ),
                event(
                    "response.output_item.added",
                    json!({ "output_index": 1, "item": { "id": "msg_1", "type": "message", "content": [] } }),
                ),
                event(
                    "response.output_text.delta",
                    json!({ "item_id": "msg_1", "output_index": 1, "content_index": 0, "delta": "Hello " }),
                ),
                event(
                    "response.output_text.delta",
                    json!({ "item_id": "msg_1", "output_index": 1, "content_index": 0, "delta": "there" }),
                ),
                event(
                    "response.output_item.done",
                    json!({ "output_index": 1, "item": message }),
                ),
                event(
                    "response.output_item.added",
                    json!({ "output_index": 2, "item": { "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "search_code", "namespace": "mcp__github", "arguments": "" } }),
                ),
                event(
                    "response.function_call_arguments.delta",
                    json!({ "item_id": "fc_1", "output_index": 2, "delta": "{\"q\":" }),
                ),
                event(
                    "response.function_call_arguments.delta",
                    json!({ "item_id": "fc_1", "output_index": 2, "delta": "\"x\"}" }),
                ),
                event(
                    "response.output_item.done",
                    json!({ "output_index": 2, "item": call }),
                ),
                event(
                    "response.completed",
                    json!({ "response": response("completed", json!([message, call]), usage) }),
                ),
            ]),
        ),
        back(
            "items-without-deltas",
            "gemini-model",
            lines(&[
                event(
                    "response.output_item.added",
                    json!({ "output_index": 0, "item": { "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "" } }),
                ),
                event(
                    "response.output_item.done",
                    json!({ "output_index": 0, "item": { "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": " {\"city\": \"Paris\"} " } }),
                ),
                event(
                    "response.incomplete",
                    json!({ "response": response("incomplete", json!([message]), json!({ "output_tokens": 1 })) }),
                ),
            ]),
        ),
        back(
            "unkeyed-text-then-done",
            "",
            lines(&[
                event(
                    "response.created",
                    json!({ "response": { "id": "resp_2" } }),
                ),
                event(
                    "response.output_text.delta",
                    json!({ "delta": "Hello there" }),
                ),
                event(
                    "response.output_item.done",
                    json!({ "output_index": 0, "item": message }),
                ),
                event(
                    "response.completed",
                    json!({ "response": { "status": "completed", "output": [message] } }),
                ),
            ]),
        ),
        back(
            "sse-frames-and-done",
            "",
            vec![
                format!(
                    "event: response.created\ndata: {}",
                    event(
                        "response.created",
                        json!({ "response": { "id": "resp_3", "model": "gpt-5" } })
                    )
                ),
                ": ping".to_owned(),
                format!(
                    "data: {}",
                    event(
                        "response.output_text.delta",
                        json!({ "output_index": 0, "content_index": 0, "delta": "<b>&</b>" })
                    )
                ),
                "data: [DONE]".to_owned(),
                format!(
                    "data: {}",
                    event("response.output_text.delta", json!({ "delta": "late" }))
                ),
            ],
        ),
        back("done-alone", "", vec!["[DONE]".to_owned()]),
    ]
}

/// The Responses → Interactions non-streaming cases.
pub fn to_interactions_finals() -> Vec<Case> {
    let body = |name: &str, model: &str, body: String| Case {
        model: model.to_owned(),
        ..Case::response(name, "{}", vec![body])
    };
    vec![
        body(
            "items-of-each-kind",
            "",
            response(
                "completed",
                json!([
                    { "id": "rs_1", "type": "reasoning", "summary": [{ "type": "summary_text", "text": "plan" }, { "type": "summary_text", "text": "" }] },
                    { "id": "msg_1", "type": "message", "content": [
                        { "type": "output_text", "text": "Hello" },
                        { "type": "output_image", "image_url": "data:image/png;base64,aGVsbG8=" },
                        { "type": "output_image", "image_url": "https://example.com/a.png" },
                        { "type": "refusal", "refusal": "no" },
                    ] },
                    { "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "open", "namespace": "browser", "arguments": "{\"url\":\"https://example.com\"}" },
                    { "type": "function_call", "id": "fc_2", "name": "get_weather", "arguments": "not json" },
                    { "type": "web_search_call", "id": "ws_1" },
                ]),
                json!({ "input_tokens": 5, "output_tokens": 6, "input_tokens_details": { "cached_tokens": 1 } }),
            )
            .to_string(),
        ),
        body(
            "incomplete-without-id",
            "gemini-2.5-pro",
            json!({ "status": "incomplete", "output": [] }).to_string(),
        ),
        body("not-json", "", "not json".to_owned()),
        body("empty", "", String::new()),
    ]
}
