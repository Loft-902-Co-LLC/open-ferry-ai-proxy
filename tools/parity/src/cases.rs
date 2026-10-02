//! Comparison cases, and hand-written ones for inputs the generator is unlikely to build.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use open_ferry_translate::codex::claude::convert_claude_request_to_codex;
use serde_json::{Value, json};

pub mod chat;
pub mod claude_chat;
pub mod claude_responses;
pub mod responses;
pub mod signature;

pub struct Case {
    pub name: String,
    /// The model name passed to the translator.
    pub model: String,
    /// The request body exactly as sent, so formatting and escapes are tested too.
    /// For a response translator, the client's original request.
    pub request: String,
    /// For a response translator, the request as sent to Codex. Empty if not given.
    pub translated_request: String,
    /// For a response translator, the Codex event stream lines, or the final
    /// event alone for the non-streaming one.
    pub events: Vec<String>,
    /// Inputs that aren't part of the request, as a JSON object for the
    /// harness entry to read. Null if the entry takes none.
    pub options: Value,
    /// Why the outputs are expected to differ: behaviour not ported yet, or a
    /// difference between Go and Rust we accept (see UPSTREAM.md).
    pub known_difference: Option<&'static str>,
}

impl Case {
    pub fn new(
        name: impl Into<String>,
        model: impl Into<String>,
        request: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            model: model.into(),
            request: request.into(),
            translated_request: String::new(),
            events: Vec::new(),
            options: Value::Null,
            known_difference: None,
        }
    }

    /// A case for a response translator.
    pub fn response(
        name: impl Into<String>,
        request: impl Into<String>,
        events: Vec<String>,
    ) -> Self {
        Self {
            events,
            ..Self::new(name, "", request)
        }
    }

    pub fn with_options(mut self, options: Value) -> Self {
        self.options = options;
        self
    }

    fn with_events(mut self, events: Vec<String>) -> Self {
        self.events = events;
        self
    }

    fn known_difference(mut self, reason: &'static str) -> Self {
        self.known_difference = Some(reason);
        self
    }
}

/// A Grok reasoning signature, from upstream's tests.
const GROK_SIGNATURE: &str = "HmlYdr2aCAqCYP/m9mr8PS6KOsdMs72FGDigmydR+Jsmuv8KX97yWPlbOwmXJgWn0CbHaCacdQD3+n5EvpgLfPNmafS3kdICBjRuDf4bzHy7uBiUhNVhqPtp/ee1y9q4imPE4LYgD1VZ4J+bp9mTeqA1+nC9Oue58CiNEMV9SVaGenCD+aBnVuSTzQhD32Y+68i6HLJW0Dx6ifaRfb8hxYtA/sPM+/FTvAMW11nRho5a2BBSkpnzfqqAz/e/vGJ77/bygpXM823QA9wL9i0X";

/// The smallest well-formed GPT reasoning signature, built as upstream's tests build it.
fn gpt_signature() -> String {
    let mut raw = [0u8; 1 + 8 + 16 + 16 + 32];
    raw[0] = 0x80;
    raw[8] = 1;
    URL_SAFE.encode(raw)
}

/// `c` as a JSON `\u` escape, or a surrogate pair of them.
fn escaped(c: char) -> String {
    c.encode_utf16(&mut [0; 2])
        .iter()
        .map(|unit| format!("{}u{unit:04x}", '\\'))
        .collect()
}

fn with_thinking_signature(signature: &str) -> String {
    json!({
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "summary", "signature": signature},
                {"type": "text", "text": "answer"}
            ]},
            {"role": "user", "content": "next"}
        ]
    })
    .to_string()
}

pub fn hand_written() -> Vec<Case> {
    let deep_schema = (0..100).fold(
        json!({ "type": "string" }),
        |schema, _| json!({ "type": "array", "items": schema }),
    );
    let large_image = "A".repeat(2 << 20);

    vec![
        Case::new(
            "minimal",
            "gpt-5",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        ),
        Case::new("empty-object", "gpt-5", "{}"),
        Case::new("array-body", "gpt-5", "[]"),
        Case::new("null-body", "gpt-5", "null"),
        Case::new(
            "escaped-keys",
            "gpt-5",
            r#"{"messages":[{"role":"user","content":"café 🚀"}]}"#
                .replace("role", &format!("r{}le", escaped('o')))
                .replace('é', &escaped('é'))
                .replace('🚀', &escaped('🚀')),
        ),
        Case::new(
            "duplicate-keys",
            "gpt-5",
            r#"{"thinking":{"type":"enabled","budget_tokens":1024},"thinking":{"type":"disabled"}}"#,
        )
        .known_difference("gjson reads the first duplicate key; serde_json keeps the last"),
        Case::new(
            "pretty-tool-input",
            "gpt-5",
            serde_json::to_string_pretty(&json!({
                "tools": [{"name": "get_weather", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}],
                "messages": [
                    {"role": "user", "content": "Weather?"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Zürich", "days": 1.50}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "document"}]}
                    ]}
                ]
            }))
            .expect("serializable"),
        ),
        Case::new(
            "huge-float-budget",
            "gpt-5",
            r#"{"thinking":{"type":"enabled","budget_tokens":1e30}}"#,
        )
        .known_difference("Go's int64(1e30) depends on the CPU; Rust saturates"),
        Case::new(
            "infinite-number-as-text",
            "gpt-5",
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":1e400}]}]}"#,
        ),
        // Go lowercases İ to i and has no final-sigma rule; Rust does both.
        Case::new(
            "dotted-capital-i-effort",
            "gpt-5",
            r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"MAXİMUM"}}"#,
        ),
        Case::new(
            "final-sigma-effort",
            "gpt-5",
            r#"{"thinking":{"type":"adaptive"},"output_config":{"effort":"ΑΣ"}}"#,
        ),
        Case::new(
            "dotted-capital-i-service-tier",
            "gpt-5",
            r#"{"service_tier":"PRİORİTY"}"#,
        ),
        Case::new(
            "dotted-capital-i-signature-prefix",
            "gpt-5",
            with_thinking_signature(&format!("OPENAİ#{}", gpt_signature())),
        ),
        Case::new(
            "deep-schema",
            "gpt-5",
            json!({ "tools": [{ "name": "deep", "input_schema": deep_schema }] }).to_string(),
        ),
        Case::new(
            "large-image",
            "gpt-5",
            json!({ "messages": [{ "role": "user", "content": [
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": large_image } }
            ]}]})
            .to_string(),
        ),
        Case::new(
            "grok-signature",
            "grok-4",
            with_thinking_signature(GROK_SIGNATURE),
        ),
    ]
}

/// A request declaring a short tool, one whose name Codex sees shortened, and
/// web search.
fn request_with_tools() -> Value {
    json!({
        "tools": [
            { "name": "get_weather", "input_schema": { "type": "object" } },
            { "name": format!("get_weather_{}", "x".repeat(70)), "input_schema": { "type": "object" } },
            { "type": "web_search_20250305", "name": "web_search" }
        ],
        "messages": [{ "role": "user", "content": "hi" }]
    })
}

/// The long tool's name as Codex sees it.
fn shortened_tool_name() -> String {
    let codex = convert_claude_request_to_codex("gpt-5", &request_with_tools());
    codex["tools"][1]["name"]
        .as_str()
        .expect("tool is named")
        .to_owned()
}

fn message_item(id: &str, text: &str) -> Value {
    json!({ "type": "message", "id": id, "status": "completed", "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }] })
}

fn call_item(id: &str, call_id: &str, name: &str, arguments: &str) -> Value {
    json!({ "type": "function_call", "id": id, "call_id": call_id, "name": name,
            "arguments": arguments, "status": "completed" })
}

fn reasoning_item(id: &str, summary: &[&str], signature: &str) -> Value {
    let summary: Vec<Value> = summary
        .iter()
        .map(|text| json!({ "type": "summary_text", "text": text }))
        .collect();
    json!({ "type": "reasoning", "id": id, "summary": summary, "encrypted_content": signature })
}

/// The events streaming one message.
fn message_events(index: usize, id: &str, text: &str) -> Vec<Value> {
    let at = json!({ "output_index": index, "item_id": id, "content_index": 0 });
    let event = |kind: &str, extra: Value| {
        let mut event = json!({ "type": kind });
        merge(&mut event, &at);
        merge(&mut event, &extra);
        event
    };
    vec![
        json!({ "type": "response.output_item.added", "output_index": index,
                "item": { "type": "message", "id": id, "status": "in_progress", "role": "assistant", "content": [] } }),
        event(
            "response.content_part.added",
            json!({ "part": { "type": "output_text", "text": "" } }),
        ),
        event("response.output_text.delta", json!({ "delta": text })),
        event("response.output_text.done", json!({ "text": text })),
        event(
            "response.content_part.done",
            json!({ "part": { "type": "output_text", "text": text } }),
        ),
        json!({ "type": "response.output_item.done", "output_index": index, "item": message_item(id, text) }),
    ]
}

/// The events streaming one function call, its arguments in `deltas`.
fn call_events(index: usize, id: &str, call_id: &str, name: &str, deltas: &[&str]) -> Vec<Value> {
    let arguments = deltas.concat();
    let mut events = vec![json!({
        "type": "response.output_item.added", "output_index": index,
        "item": { "type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": "", "status": "in_progress" }
    })];
    for delta in deltas {
        events.push(json!({ "type": "response.function_call_arguments.delta", "output_index": index, "item_id": id, "delta": delta }));
    }
    events.push(json!({ "type": "response.function_call_arguments.done", "output_index": index, "item_id": id, "arguments": arguments }));
    events.push(json!({ "type": "response.output_item.done", "output_index": index, "item": call_item(id, call_id, name, &arguments) }));
    events
}

fn finished(kind: &str, output: Vec<Value>, extra: Value) -> Value {
    let mut response = json!({
        "id": "resp_1", "object": "response", "model": "gpt-5", "output": output,
        "usage": {
            "input_tokens": 120, "input_tokens_details": { "cached_tokens": 100 },
            "output_tokens": 40, "output_tokens_details": { "reasoning_tokens": 12 },
            "total_tokens": 160
        }
    });
    merge(&mut response, &extra);
    json!({ "type": kind, "response": response })
}

fn completed(output: Vec<Value>) -> Value {
    finished(
        "response.completed",
        output,
        json!({ "status": "completed" }),
    )
}

fn merge(target: &mut Value, extra: &Value) {
    for (key, value) in extra.as_object().into_iter().flatten() {
        target[key] = value.clone();
    }
}

fn stream_case(name: &str, request: &Value, events: Vec<Value>) -> Case {
    let lines = events
        .iter()
        .map(|event| format!("data: {event}"))
        .collect();
    Case::response(name, request.to_string(), lines)
}

/// Streams the generator is unlikely to build in one piece.
pub fn hand_written_streams() -> Vec<Case> {
    let plain = json!({ "messages": [{ "role": "user", "content": "hi" }] });
    let tools = request_with_tools();
    let long_name = shortened_tool_name();
    let created = json!({ "type": "response.created", "response": { "id": "resp_1", "status": "in_progress", "output": [] } });

    let text = {
        let mut events = vec![created.clone()];
        events.extend(message_events(0, "msg_1", "Hello, world!"));
        events.push(completed(vec![message_item("msg_1", "Hello, world!")]));
        events
    };

    let reasoning_then_tool = {
        let at = |summary_index: usize| json!({ "output_index": 0, "item_id": "rs_1", "summary_index": summary_index });
        let event = |kind: &str, summary_index: usize, extra: Value| {
            let mut event = json!({ "type": kind });
            merge(&mut event, &at(summary_index));
            merge(&mut event, &extra);
            event
        };
        let mut events = vec![
            created.clone(),
            json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "reasoning", "id": "rs_1", "summary": [] } }),
        ];
        for (index, part) in ["Checking the city.", "Calling the tool."]
            .into_iter()
            .enumerate()
        {
            events.push(event(
                "response.reasoning_summary_part.added",
                index,
                json!({ "part": { "type": "summary_text", "text": "" } }),
            ));
            events.push(event(
                "response.reasoning_summary_text.delta",
                index,
                json!({ "delta": part }),
            ));
            events.push(event(
                "response.reasoning_summary_text.done",
                index,
                json!({ "text": part }),
            ));
            events.push(event(
                "response.reasoning_summary_part.done",
                index,
                json!({ "part": { "type": "summary_text", "text": part } }),
            ));
        }
        let reasoning = reasoning_item(
            "rs_1",
            &["Checking the city.", "Calling the tool."],
            "gAAAAABsignature",
        );
        events.push(
            json!({ "type": "response.output_item.done", "output_index": 0, "item": reasoning }),
        );
        events.extend(call_events(
            1,
            "fc_1",
            "call_1",
            &long_name,
            &["{\"city\":", "\"Paris\"}"],
        ));
        events.push(completed(vec![
            reasoning,
            call_item("fc_1", "call_1", &long_name, "{\"city\":\"Paris\"}"),
        ]));
        events
    };

    // Two calls streaming at once, with text arriving while they do.
    let parallel_calls = {
        let first = call_events(
            0,
            "fc_1",
            "call_1",
            "get_weather",
            &["{\"city\":", "\"Paris\"}"],
        );
        let second = call_events(
            1,
            "fc_2",
            "call_2",
            &long_name,
            &["{\"city\":", "\"Lima\"}"],
        );
        let text = message_events(2, "msg_1", "Checking both.");
        let mut events = vec![
            created.clone(),
            first[0].clone(),
            second[0].clone(),
            first[1].clone(),
        ];
        events.extend([
            second[1].clone(),
            text[0].clone(),
            text[1].clone(),
            text[2].clone(),
        ]);
        events.extend([
            first[2].clone(),
            second[2].clone(),
            first[3].clone(),
            first[4].clone(),
        ]);
        events.extend(text[3..].iter().cloned());
        events.extend([second[3].clone(), second[4].clone()]);
        events.push(completed(vec![
            call_item("fc_1", "call_1", "get_weather", "{\"city\":\"Paris\"}"),
            call_item("fc_2", "call_2", &long_name, "{\"city\":\"Lima\"}"),
            message_item("msg_1", "Checking both."),
        ]));
        events
    };

    let web_search = {
        let search = json!({
            "type": "web_search_call", "id": "ws_1", "status": "completed",
            "action": { "type": "search", "query": "weather in Paris" },
            "results": [{ "url": "https://example.com/paris", "title": " Paris weather " }, { "url": " " }]
        });
        let mut events = vec![
            created.clone(),
            json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "web_search_call", "id": "ws_1", "status": "in_progress" } }),
            json!({ "type": "response.web_search_call.in_progress", "output_index": 0, "item_id": "ws_1" }),
            json!({ "type": "response.web_search_call.searching", "output_index": 0, "item_id": "ws_1" }),
            json!({ "type": "response.web_search_call.completed", "output_index": 0, "item_id": "ws_1" }),
            json!({ "type": "response.output_item.done", "output_index": 0, "item": search }),
        ];
        events.extend(message_events(1, "msg_1", "It is sunny."));
        events.push(completed(vec![
            search,
            message_item("msg_1", "It is sunny."),
        ]));
        events
    };

    let cyber_policy = {
        let mut events = vec![created.clone()];
        events.extend(message_events(0, "msg_1", "Let me").into_iter().take(3));
        events.push(json!({ "type": "error", "error": {
            "type": "invalid_request", "code": "cyber_policy",
            "message": "This content was flagged for possible cybersecurity risk.", "param": null
        }}));
        events
    };

    let incomplete = {
        let mut events = vec![created.clone()];
        events.extend(message_events(0, "msg_1", "The answer is"));
        events.push(finished(
            "response.incomplete",
            vec![message_item("msg_1", "The answer is")],
            json!({ "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" }, "stop_sequence": "\nEND" }),
        ));
        events
    };

    let empty_call_id = {
        let mut events = vec![created.clone()];
        events.extend(call_events(0, "fc_1", "", "get_weather", &["{}"]));
        events.push(completed(vec![call_item("fc_1", "", "get_weather", "{}")]));
        events
    };

    let calls_only_in_final_output = vec![
        created.clone(),
        completed(vec![
            call_item("fc_1", "call_1", "get_weather", "{\"city\":\"Paris\"}"),
            call_item("fc_2", "call_2", &long_name, "{\"city\":\"Lima\"}"),
        ]),
    ];

    // Arguments set without deltas, sent, then replaced by a longer value whose
    // first bytes differ; what was already sent ends inside a character.
    let replaced_arguments = vec![
        created.clone(),
        json!({ "type": "response.output_item.added", "output_index": 0,
                "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_weather", "arguments": "" } }),
        json!({ "type": "response.function_call_arguments.done", "output_index": 0, "item_id": "fc_1", "arguments": "abc" }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": call_item("fc_1", "call_1", "get_weather", "αβ") }),
        completed(Vec::new()),
    ];

    let signature_only_reasoning = {
        let reasoning = reasoning_item("rs_1", &[], "gAAAAABsignature");
        let mut events = vec![
            created.clone(),
            json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "reasoning", "id": "rs_1", "summary": [] } }),
            json!({ "type": "response.output_item.done", "output_index": 0, "item": reasoning }),
        ];
        events.extend(message_events(1, "msg_1", "Done."));
        events.push(completed(Vec::new()));
        events
    };

    let mut with_noise = stream_case("event-lines-and-noise", &plain, text.clone());
    with_noise.events = text
        .iter()
        .flat_map(|event| {
            [
                format!("event: {}", event["type"].as_str().unwrap_or_default()),
                format!("data:{event}\r"),
                ": ping".to_owned(),
                String::new(),
            ]
        })
        .chain(["data: [DONE]".to_owned()])
        .collect();

    vec![
        stream_case("text", &plain, text),
        stream_case("reasoning-then-long-tool", &tools, reasoning_then_tool),
        stream_case("parallel-calls-with-text", &tools, parallel_calls),
        stream_case("web-search", &tools, web_search),
        stream_case("cyber-policy-error", &plain, cyber_policy),
        stream_case("incomplete-max-tokens", &plain, incomplete),
        stream_case("empty-call-id", &tools, empty_call_id),
        stream_case(
            "calls-only-in-final-output",
            &tools,
            calls_only_in_final_output,
        ),
        stream_case(
            "arguments-replaced-mid-character",
            &tools,
            replaced_arguments,
        ),
        stream_case("signature-only-reasoning", &plain, signature_only_reasoning),
        with_noise,
        Case::response("no-events", plain.to_string(), Vec::new()),
    ]
}

/// Final events for the non-streaming translator.
pub fn hand_written_finals() -> Vec<Case> {
    let plain = json!({ "messages": [{ "role": "user", "content": "hi" }] });
    let tools = request_with_tools();
    let long_name = shortened_tool_name();
    let final_case = |name: &str, request: &Value, event: Value| {
        Case::response(name, request.to_string(), vec![event.to_string()])
    };
    let search = |id: &str, action: Value| json!({ "type": "web_search_call", "id": id, "status": "completed", "action": action });

    vec![
        final_case(
            "full",
            &tools,
            completed(vec![
                reasoning_item("rs_1", &["Checking.", "Calling."], "gAAAAABsignature"),
                message_item("msg_1", "Let me check."),
                call_item("fc_1", "call_1", &long_name, "{\"city\":\"Paris\"}"),
            ]),
        ),
        final_case(
            "loose-summary-and-content",
            &plain,
            completed(vec![
                json!({ "type": "reasoning", "id": "rs_1", "summary": "plain summary" }),
                json!({ "type": "reasoning", "id": "rs_2", "summary": { "text": "object summary" } }),
                json!({ "type": "reasoning", "id": "rs_3", "summary": [], "content": [{ "type": "reasoning_text", "text": "from content" }] }),
                json!({ "type": "message", "id": "msg_1", "content": "content as a string" }),
                json!({ "type": "message", "id": "msg_2", "content": [{ "type": "refusal", "text": "skipped" }, { "type": "output_text", "text": "kept" }] }),
            ]),
        ),
        final_case(
            "loose-arguments",
            &tools,
            completed(vec![
                call_item("fc_1", "call_1", "get_weather", "not json"),
                call_item("fc_2", "call_2", "get_weather", "[1,2]"),
                call_item("fc_3", "call.3:x y", "get_weather", " {\"padded\": true} "),
                call_item("fc_4", "", "unknown_tool", "{\"n\":1e400}"),
            ]),
        ),
        final_case(
            "web-search-dedupe",
            &tools,
            completed(vec![
                search("ws_1", json!({ "type": "open_page" })),
                search("ws_1", json!({ "type": "search", "query": "weather" })),
                search(
                    "",
                    json!({ "type": "open_page", "url": "https://example.com" }),
                ),
                search(
                    "",
                    json!({ "type": "open_page", "url": "https://example.com" }),
                ),
                message_item("msg_1", "ok"),
            ]),
        ),
        final_case(
            "incomplete-max-tokens",
            &plain,
            finished(
                "response.incomplete",
                vec![message_item("msg_1", "The answer is")],
                json!({ "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" }, "stop_sequence": "\nEND" }),
            ),
        ),
        final_case("output-as-object", &plain, {
            let mut event = completed(Vec::new());
            event["response"]["output"] = json!({ "a": message_item("msg_1", "hidden") });
            event
        }),
        final_case(
            "not-final",
            &plain,
            json!({ "type": "response.created", "response": { "id": "resp_1" } }),
        ),
        final_case(
            "failed",
            &plain,
            finished("response.failed", Vec::new(), json!({ "status": "failed" })),
        ),
        Case::response("empty-event", plain.to_string(), vec![String::new()]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_parse_back_to_the_character() {
        for c in ['b', 'é', '🚀'] {
            let text = format!("\"{}\"", escaped(c));
            assert!(text.is_ascii() && text.contains("\\u"), "{text}");
            assert_eq!(
                serde_json::from_str::<String>(&text).unwrap(),
                c.to_string()
            );
        }
    }

    /// Escaped cases are only useful if the escapes survive editing.
    #[test]
    fn escaped_cases_hold_escapes() {
        let all = hand_written()
            .into_iter()
            .chain(responses::requests())
            .chain(responses::streams())
            .chain(responses::finals());
        let escaped: Vec<Case> = all.filter(|case| case.name.contains("escaped")).collect();
        assert_eq!(escaped.len(), 5);
        for case in escaped {
            let text = case.events.first().unwrap_or(&case.request);
            assert!(text.contains("\\u"), "{} has no \\u escape", case.name);
        }
    }
}
