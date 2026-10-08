//! Ported from openai_interactions_file_data_test.go, and the tests of
//! `ConvertOpenAIRequestToInteractions` and `ConvertOpenAIResponseToInteractions`
//! in interactions_openai_request_test.go and interactions_openai_response_test.go,
//! and from openai_interactions_user_turn_test.go.
//!
//! Dropped or changed tests, as the Antigravity branches are not ported:
//! - `TestConvertOpenAIRequestToInteractions_AntigravitySanitizesGenerationConfigAndSetsAgentConfig`
//!   and `TestConvertOpenAIRequestToInteractionsRenamesConflictingAntigravityToolCallsAndResultsInHistory`
//!   are dropped: they assert only the Antigravity rewrites.
//! - `TestConvertOpenAIRequestToInteractionsRenamesConflictingAntigravityTools`
//!   and `TestConvertOpenAIRequestToInteractionsRenamesConflictingToolChoice`
//!   keep only their halves for a Gemini model, where nothing is renamed.
//! - `TestConvertOpenAIRequestToInteractions_PreservesEnvironmentIDAndPreviousInteractionID`
//!   uses a Gemini model in place of the Antigravity one.

use serde_json::{Value, json};

use super::*;

/// The translator's body, which must not be refused.
#[track_caller]
fn convert_openai_request_to_interactions(model: &str, body: &Value, stream: bool) -> Value {
    let (out, err) = super::convert_openai_request_to_interactions(model, body, stream);
    assert_eq!(err, None, "{out}");
    out
}

/// Each frame's event name and data, checking each is one well-formed frame.
fn frames(out: &str) -> Vec<(String, Value)> {
    out.split_inclusive("\n\n")
        .map(|frame| {
            let (event, data) = frame
                .strip_prefix("event: ")
                .and_then(|frame| frame.strip_suffix("\n\n"))
                .and_then(|frame| frame.split_once("\ndata: "))
                .unwrap_or_else(|| panic!("not a frame: {frame:?}"));
            let data =
                serde_json::from_str(data).unwrap_or_else(|_| Value::String(data.to_owned()));
            (event.to_owned(), data)
        })
        .collect()
}

fn count(out: &str, event: &str) -> usize {
    frames(out).iter().filter(|(name, _)| name == event).count()
}

fn find(out: &str, event: &str) -> Value {
    frames(out)
        .into_iter()
        .find(|(name, _)| name == event)
        .unwrap_or_else(|| panic!("no {event} in {out}"))
        .1
}

fn request(model: &str, raw: &str) -> Value {
    let body: Value = serde_json::from_str(raw).expect("valid JSON");
    convert_openai_request_to_interactions(model, &body, false)
}

/// The keys of an object, in order.
fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .map_or_else(Vec::new, |value| value.keys().map(String::as_str).collect())
}

// TestConvertOpenAIRequestToInteractionsMapsMessagesToolsAndStream
#[test]
fn maps_messages_tools_and_stream() {
    let out = request(
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","stream":true,"messages":[{"role":"system","content":"be brief"},{"role":"user","content":"今天北京的天气怎么样？"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"weather","parameters":{"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}}}],"tool_choice":"auto","max_completion_tokens":128}"#,
    );
    assert_eq!(out["model"], "gemini-3.1-flash-lite");
    assert_eq!(out["stream"], true);
    assert_eq!(out["system_instruction"], "be brief");
    assert_eq!(out["input"][0]["type"], "user_input");
    assert_eq!(
        out["input"][0]["content"][0]["text"],
        "今天北京的天气怎么样？"
    );
    assert_eq!(out["tools"][0]["type"], "function");
    assert_eq!(out["tools"][0]["name"], "get_weather");
    assert_eq!(
        out["tools"][0]["parameters"]["properties"]["location"]["type"],
        "string"
    );
    assert_eq!(out["generation_config"]["tool_choice"], "auto");
    assert_eq!(out["generation_config"]["max_output_tokens"], 128);
}

// TestConvertOpenAIRequestToInteractionsMapsToolCallsAndResults
#[test]
fn maps_tool_calls_and_results() {
    let out = request(
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},{"role":"tool","tool_call_id":"call_1","content":"ok"}]}"#,
    );
    let call = &out["input"][0];
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["id"], "call_1");
    assert!(call.get("call_id").is_none(), "{out}");
    assert_eq!(call["arguments"]["q"], "x");
    let result = &out["input"][1];
    assert_eq!(result["type"], "function_result");
    assert_eq!(result["name"], "lookup");
    assert_eq!(result["call_id"], "call_1");
    assert!(result.get("id").is_none(), "{out}");
    assert_eq!(result["result"], "ok");
}

// TestConvertOpenAIRequestToInteractionsInfersToolNamesForOutOfOrderResults
#[test]
fn infers_tool_names_for_out_of_order_results() {
    let out = request(
        "gemini-3.1-flash-lite",
        r#"{
            "model": "gemini-3.1-flash-lite",
            "messages": [
                {
                    "role": "assistant",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"bj\"}"}}
                    ]
                },
                {"role": "tool", "tool_call_id": "call_2", "content": "sunny"},
                {"role": "tool", "tool_call_id": "call_1", "content": "found"}
            ]
        }"#,
    );
    assert_eq!(out["input"][2]["call_id"], "call_2");
    assert_eq!(out["input"][2]["name"], "weather");
    assert_eq!(out["input"][3]["call_id"], "call_1");
    assert_eq!(out["input"][3]["name"], "lookup");
}

// TestConvertOpenAIRequestToInteractions_PreservesEnvironmentIDAndPreviousInteractionID,
// with a Gemini model.
#[test]
fn preserves_environment_id_and_previous_interaction_id() {
    let out = request(
        "gemini-3.1-flash-lite",
        r#"{
            "model":"gemini-3.1-flash-lite",
            "messages":[{"role":"user","content":"continue"}],
            "previous_response_id":"v1_prev123",
            "environment_id":"env_456"
        }"#,
    );
    assert_eq!(out["previous_interaction_id"], "v1_prev123");
    assert_eq!(out["environment_id"], "env_456");
}

// TestConvertOpenAIRequestToInteractionsRenamesConflictingAntigravityTools,
// its half for a Gemini model: tool names pass through.
#[test]
fn keeps_tool_names() {
    let out = request(
        "gemini-3.1-flash-lite",
        r#"{"model":"gemini-3.1-flash-lite","messages":[{"role":"user","content":"read it"}],"tools":[
            {"type":"function","function":{"name":"read_file","description":"r","parameters":{"type":"object"}}},
            {"type":"function","function":{"name":"write_file","description":"w","parameters":{"type":"object"}}},
            {"type":"function","function":{"name":"execute_code","description":"e","parameters":{"type":"object"}}},
            {"type":"function","function":{"name":"web_search","description":"s","parameters":{"type":"object"}}}
        ]}"#,
    );
    let names: Vec<&Value> = (0..4).map(|index| &out["tools"][index]["name"]).collect();
    assert_eq!(
        names,
        [
            &json!("read_file"),
            &json!("write_file"),
            &json!("execute_code"),
            &json!("web_search")
        ]
    );
}

// TestConvertOpenAIRequestToInteractionsRenamesConflictingToolChoice, its
// half for a Gemini model: the tool choice passes through.
#[test]
fn keeps_tool_choice() {
    let out = request(
        "gemini-3.1-flash-lite",
        r#"{
            "model":"gemini-3.1-flash-lite",
            "messages":[{"role":"user","content":"read it"}],
            "tools":[{"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"read_file"}}
        }"#,
    );
    assert_eq!(
        out["generation_config"]["tool_choice"],
        json!({"type": "function", "function": {"name": "read_file"}})
    );
}

// TestConvertOpenAIRequestToInteractionsNormalizesFileDataURL
#[test]
fn normalizes_file_data_url() {
    let out = request(
        "gemini-3.5-flash",
        r#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"file","file":{"filename":"test.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}}]}]}"#,
    );
    let document = &out["input"][0]["content"][0];
    assert_eq!(document["mime_type"], "application/pdf");
    assert_eq!(document["data"], "JVBERi0xLjQK");
}

// TestConvertOpenAIRequestToInteractionsPreservesRawFileDataWithMIMEType
#[test]
fn preserves_raw_file_data_with_mime_type() {
    let out = request(
        "gemini-3.5-flash",
        r#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"document","mime_type":"application/pdf","data":"JVBERi0xLjQK"}]}]}"#,
    );
    let document = &out["input"][0]["content"][0];
    assert_eq!(document["mime_type"], "application/pdf");
    assert_eq!(document["data"], "JVBERi0xLjQK");
}

/// A request using most of what the translator reads.
const WHOLE_REQUEST: &str = r#"{"model":"gemini-3-pro","n":2,"stop":["x"],"reasoning_effort":" High ","modalities":["text"],"service_tier":"flex","response_format":{"type":"json_object"},"previous_interaction_id":"p1","environment":{"id":"e1"},"agent_config":{"a":1},"temperature":0.5,"messages":[
    {"role":"developer","content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]},
    {"role":" System ","content":{"text":"three"}},
    {"role":"user","content":[{"type":"input_text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aGk="}},{"type":"image_url","image_url":"https://x/y.png"},{"type":"input_audio","input_audio":{"data":"UklG","format":"WAV"}},{"type":"audio"},{"type":"input_file","filename":"a.txt","file_data":"aGk="},{"type":"file","file":{"file_url":"https://x/f"}},{"type":"file"},{"type":"refusal"}]},
    {"role":"assistant","reasoning_content":[{"text":"hmm"},{"content":" "},{"content":"ok"}],"content":"sure","tool_calls":[{"id":"c1","function":{"name":"f","arguments":"not json"}},{"type":"custom","function":{"name":"g"}},{"id":"c3","type":"function"}]},
    {"role":"function","id":"c1","content":{"x":1}}
],"tools":[{"type":"function","name":"flat","description":7},{"type":"web_search"},{"function":{}}]}"#;

// Not upstream's: a request using most of what the translator reads, whole,
// as upstream writes it (the same case is in the parity suite).
#[test]
fn translates_a_whole_request() {
    let out = request("", WHOLE_REQUEST);
    assert_eq!(
        out,
        json!({
            "model": "gemini-3-pro",
            "input": [
                {"type": "user_input", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image", "mime_type": "image/png", "data": "aGk="},
                    {"type": "image", "image_url": "https://x/y.png"},
                    {"type": "audio", "data": "UklG", "mime_type": "audio/wav"},
                    {"type": "document", "filename": "a.txt", "mime_type": "text/plain", "data": "aGk="},
                    {"type": "document", "file_url": "https://x/f"}
                ]},
                {"type": "thought", "content": [{"type": "text", "text": "hmm"}]},
                {"type": "thought", "content": [{"type": "text", "text": "ok"}]},
                {"type": "model_output", "content": [{"type": "text", "text": "sure"}]},
                {"type": "function_call", "name": "f", "arguments": "not json", "id": "c1"},
                {"type": "function_result", "result": {"x": 1}, "call_id": "c1", "name": "f"}
            ],
            "previous_interaction_id": "p1",
            "environment_id": "e1",
            "agent_config": {"a": 1},
            "system_instruction": "onetwo\nthree",
            "generation_config": {
                "temperature": 0.5,
                "candidate_count": 2,
                "stop_sequences": ["x"],
                "thinking_level": "high"
            },
            "response_format": {"type": "json_object"},
            "response_modalities": ["text"],
            "service_tier": "flex",
            "tools": [{"type": "function", "name": "flat", "description": "7"}]
        })
    );
    assert_eq!(
        keys(&out),
        [
            "model",
            "input",
            "previous_interaction_id",
            "environment_id",
            "agent_config",
            "system_instruction",
            "generation_config",
            "response_format",
            "response_modalities",
            "service_tier",
            "tools"
        ]
    );
    assert_eq!(keys(&out["input"][4]), ["type", "name", "arguments", "id"]);
    assert_eq!(
        keys(&out["input"][5]),
        ["type", "result", "call_id", "name"]
    );
}

// Not upstream's: the `stream` flag only fills in a missing `stream`.
#[test]
fn stream_flag_fills_in_a_missing_stream() {
    let body = json!({"stream": "f", "messages": "x"});
    let out = convert_openai_request_to_interactions("m", &body, true);
    assert_eq!(out, json!({"model": "m", "input": [], "stream": false}));
    let out = convert_openai_request_to_interactions("m", &json!({}), true);
    assert_eq!(out, json!({"model": "m", "input": [], "stream": true}));
}

const FINISH: &[u8] = br#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;

// TestConvertOpenAIResponseToInteractionsStreamUsageOnlyTerminalChunk
#[test]
fn stream_usage_only_terminal_chunk() {
    let mut stream = OpenAIToInteractionsStream::new("gpt-test");
    let finish = stream.translate(FINISH);
    let usage = stream.translate(br#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","model":"gpt-test","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}"#);
    let done = stream.translate(b"data: [DONE]");
    assert_eq!(count(&finish, "interaction.completed"), 0);
    assert_eq!(count(&usage, "interaction.completed"), 1);
    assert_eq!(count(&done, "interaction.completed"), 0);
    assert_eq!(count(&done, "done"), 1);
    let payload = find(&usage, "interaction.completed");
    assert_eq!(payload["interaction"]["usage"]["total_input_tokens"], 3);
    assert_eq!(payload["interaction"]["usage"]["total_output_tokens"], 4);
    assert_eq!(payload["interaction"]["usage"]["total_tokens"], 7);
}

// TestConvertOpenAIResponseToInteractionsCompletesOnDoneWithoutUsage
#[test]
fn completes_on_done_without_usage() {
    let mut stream = OpenAIToInteractionsStream::new("gpt-test");
    let finish = stream.translate(FINISH);
    let done = stream.translate(b"data: [DONE]");
    assert_eq!(count(&finish, "interaction.completed"), 0);
    assert_eq!(count(&done, "interaction.completed"), 1);
    assert_eq!(count(&done, "done"), 1);
}

// TestConvertOpenAIResponseToInteractionsStreamCreatedUsesChunkIdentity
#[test]
fn stream_created_uses_chunk_identity() {
    let mut stream = OpenAIToInteractionsStream::new("");
    let out = stream.translate(br#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","model":"gpt-test","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#);
    let payload = find(&out, "interaction.created");
    assert_eq!(payload["interaction"]["id"], "chatcmpl_1");
    assert_eq!(payload["interaction"]["model"], "gpt-test");
}

// TestConvertOpenAIResponseToInteractionsNonStreamDirectToolCall
#[test]
fn non_stream_direct_tool_call() {
    let body = json!({"id":"chatcmpl_1","model":"gpt-test","choices":[{"message":{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}});
    let out = convert_openai_response_to_interactions_non_stream("gpt-test", &body);
    assert_eq!(out["steps"][0]["type"], "function_call");
    assert_eq!(out["steps"][0]["id"], "call_1");
    assert!(out["steps"][0].get("call_id").is_none(), "{out}");
    assert_eq!(out["steps"][0]["arguments"]["q"], "x");
}

// TestConvertOpenAIResponseToInteractionsStreamToolCall
#[test]
fn stream_tool_call() {
    let mut stream = OpenAIToInteractionsStream::new("gpt-test");
    let out = stream.translate(br#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","model":"gpt-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},"finish_reason":null}]}"#);
    let payload = find(&out, "step.start");
    assert_eq!(payload["step"]["type"], "function_call");
    assert_eq!(payload["step"]["id"], "call_1");
    assert!(payload["step"].get("call_id").is_none(), "{payload}");
    assert_eq!(payload["step"]["name"], "lookup");
}

/// A stream of reasoning, text and two tool calls, a chunk each.
const WHOLE_STREAM: &[&str] = &[
    r#"data: {"id":"c1","choices":[{"delta":{"role":"assistant","reasoning_content":"think"}}]}"#,
    r#"{"choices":[{"delta":{"reasoning_content":[{"text":"more"}],"content":"hi"}}]}"#,
    "event: chunk\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}",
    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}},{"index":1,"function":{"name":"g"}}]}}]}"#,
    r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens_details":{"reasoning_tokens":2}}}"#,
    "data: [DONE]",
    "data: [DONE]",
];

// Not upstream's: a whole stream, as upstream frames it but for the time
// stamps (the same case is in the parity suite).
#[test]
fn frames_a_whole_stream() {
    let mut stream = OpenAIToInteractionsStream::new("m");
    let mut out = String::new();
    for chunk in WHOLE_STREAM {
        out.push_str(&stream.translate(chunk.as_bytes()));
    }
    let mut frames = frames(&out);
    for (name, data) in &mut frames {
        if name == "interaction.completed" {
            for key in ["created", "updated"] {
                let time = data["interaction"][key].take();
                assert!(
                    time.as_str().is_some_and(|time| time.ends_with('Z')),
                    "{time}"
                );
            }
        }
    }
    let delta = |index: u64, delta: Value| json!({"index": index, "delta": delta, "event_type": "step.delta"});
    let stop = |index: u64| json!({"index": index, "event_type": "step.stop"});
    let thought =
        |text: &str| json!({"content": {"text": text, "type": "text"}, "type": "thought_summary"});
    let arguments = |text: &str| json!({"arguments": text, "type": "arguments_delta"});
    let expected = [
        (
            "interaction.created",
            json!({"interaction": {"id": "c1", "status": "in_progress", "object": "interaction", "model": "m"}, "event_type": "interaction.created"}),
        ),
        (
            "interaction.status_update",
            json!({"interaction_id": "c1", "status": "in_progress", "event_type": "interaction.status_update"}),
        ),
        (
            "step.start",
            json!({"index": 0, "step": {"type": "thought"}, "event_type": "step.start"}),
        ),
        ("step.delta", delta(0, thought("think"))),
        ("step.delta", delta(0, thought("more"))),
        ("step.stop", stop(0)),
        (
            "step.start",
            json!({"index": 1, "step": {"type": "model_output"}, "event_type": "step.start"}),
        ),
        (
            "step.delta",
            delta(1, json!({"text": "hi", "type": "text"})),
        ),
        ("step.stop", stop(1)),
        (
            "step.start",
            json!({"index": 2, "step": {"type": "function_call", "id": "t1", "name": "f", "arguments": {}}, "event_type": "step.start"}),
        ),
        ("step.delta", delta(2, arguments("{\"a\""))),
        ("step.delta", delta(2, arguments(":1}"))),
        ("step.stop", stop(2)),
        (
            "step.start",
            json!({"index": 3, "step": {"type": "function_call", "id": "call_1", "name": "g", "arguments": {}}, "event_type": "step.start"}),
        ),
        ("step.stop", stop(3)),
        (
            "interaction.completed",
            json!({"interaction": {"id": "c1", "status": "completed", "usage": {"input_tokens": 1, "total_input_tokens": 1, "reasoning_tokens": 2, "total_thought_tokens": 2}, "created": null, "updated": null, "service_tier": "standard", "object": "interaction", "model": "m"}, "event_type": "interaction.completed"}),
        ),
        ("done", json!("[DONE]")),
    ];
    let expected: Vec<(String, Value)> = expected
        .into_iter()
        .map(|(name, data)| (name.to_owned(), data))
        .collect();
    assert_eq!(frames, expected);
}

// Not upstream's: a whole response, as upstream writes it (the same case is
// in the parity suite).
#[test]
fn translates_a_whole_response() {
    let body = json!({"id":"r1","model":"up","choices":{"a":{"message":{"reasoning_content":"why","content":7,"tool_calls":[{"function":{"arguments":{"k":[1]}}}]},"finish_reason":null},"b":{"finish_reason":"stop"}},"usage":{"total_tokens":"9"}});
    let out = convert_openai_response_to_interactions_non_stream("", &body);
    assert_eq!(
        out,
        json!({
            "id": "r1",
            "status": "completed",
            "object": "interaction",
            "model": "up",
            "steps": [
                {"type": "thought", "content": [{"type": "text", "text": "why"}]},
                {"type": "model_output", "content": [{"type": "text", "text": "7"}]},
                {"type": "function_call", "name": "", "arguments": {"k": [1]}}
            ],
            "finish_reason": "stop",
            "usage": {"total_tokens": 9}
        })
    );
    assert_eq!(
        keys(&out),
        [
            "id",
            "status",
            "object",
            "model",
            "steps",
            "finish_reason",
            "usage"
        ]
    );
}

// Not upstream's: a tool call's arguments keep each number as written, as
// upstream copies them, and an id sent as the number -0 stays "-0", as
// gjson's String() gives it (checked with Go).
#[test]
fn numbers_keep_their_text() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let body = json!({"id":"c1","choices":[{"message":{"role":"assistant","tool_calls":[{"id":"t1","type":"function","function":{"name":"f","arguments":spelled}}]},"finish_reason":"tool_calls"}]});
    let out = convert_openai_response_to_interactions_non_stream("m", &body);
    assert_eq!(out["steps"][0]["arguments"].to_string(), spelled);

    let mut stream = OpenAIToInteractionsStream::new("m");
    let out =
        stream.translate(br#"data: {"id":-0,"choices":[{"index":0,"delta":{"content":"hi"}}]}"#);
    assert_eq!(find(&out, "interaction.created")["interaction"]["id"], "-0");
}

// Not upstream's: review 11b's request, whose tool call arguments, JSON in a
// string, go on with each number as written, as upstream copies them
// (checked with Go).
#[test]
fn request_arguments_keep_their_numbers() {
    let out = request(
        "m",
        r#"{"messages":[{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{\"x\":-0,\"y\":1E20}"}}]}]}"#,
    );
    assert_eq!(
        out["input"][0]["arguments"].to_string(),
        r#"{"x":-0,"y":1E20}"#
    );
}

// The v8.0.20 tests of openai_interactions_user_turn_test.go.

const USER_TURN_FILE_ID: &str = r#"{"type":"file","file":{"file_id":"file-absent"}}"#;
const USER_TURN_EMPTY_AUDIO: &str = r#"{"type":"input_audio","input_audio":{"format":"wav"}}"#;
const USER_TURN_TEXT: &str = r#"{"type":"text","text":"keep me"}"#;
const USER_TURN_EMPTY_TEXT: &str = r#"{"type":"text","text":""}"#;
const USER_TURN_INLINE: &str = r#"{"type":"file","file":{"filename":"a.pdf","file_data":"data:application/pdf;base64,JVBERi0xLjQK"}}"#;
const USER_TURN_AUDIO: &str =
    r#"{"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}}"#;

/// The translator's body and refusal for `input`, after checking that the
/// registration gives the same refusal.
fn checked_request(input: &str) -> (Value, Option<UnsupportedPartError>) {
    let body: Value = serde_json::from_str(input).expect("test request is JSON");
    let registered = crate::registry::Registry::global().translate_request_checked(
        &"openai".into(),
        &"interactions".into(),
        "gemini-3.5-flash",
        body.clone(),
        false,
    );
    let (out, err) =
        super::convert_openai_request_to_interactions("gemini-3.5-flash", &body, false);
    assert_eq!(registered.err(), err, "the registration's refusal");
    (out, err)
}

// TestConvertOpenAIRequestToInteractions_RefusesAnyEmptiedUserTurn
#[test]
fn refuses_any_emptied_user_turn() {
    for (name, input, want) in [
        (
            "file id only",
            format!(
                r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"user","content":[{USER_TURN_FILE_ID}]}}]}}"#
            ),
            "file",
        ),
        (
            "history then file id only",
            format!(
                r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"user","content":"hello"}},{{"role":"assistant","content":"hi"}},{{"role":"user","content":[{USER_TURN_FILE_ID}]}}]}}"#
            ),
            "file",
        ),
        (
            "system and developer prompts do not hide the empty turn",
            format!(
                r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"system","content":"sys"}},{{"role":"developer","content":"dev"}},{{"role":"user","content":[{USER_TURN_FILE_ID}]}}]}}"#
            ),
            "file",
        ),
        (
            "emptied turn before a later text turn",
            format!(
                r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"user","content":[{USER_TURN_FILE_ID}]}},{{"role":"assistant","content":"ok"}},{{"role":"user","content":"next"}}]}}"#
            ),
            "file",
        ),
        (
            "audio without bytes",
            format!(
                r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"user","content":"hello"}},{{"role":"assistant","content":"hi"}},{{"role":"user","content":[{USER_TURN_EMPTY_AUDIO}]}}]}}"#
            ),
            "input_audio",
        ),
        (
            "empty text beside the file id does not count as sendable",
            format!(
                r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"user","content":[{USER_TURN_EMPTY_TEXT},{USER_TURN_FILE_ID}]}}]}}"#
            ),
            "file",
        ),
    ] {
        let (body, err) = checked_request(&input);
        let err = err.unwrap_or_else(|| panic!("{name}: no refusal; body = {body}"));
        assert_eq!(err.part_type, want, "{name}");
        assert_eq!(err.status_code(), 400);
        assert_eq!(err.to_string(), format!("unsupported content part: {want}"));
        assert!(body.is_object(), "{name}: {body}");
    }
}

// TestConvertOpenAIRequestToInteractions_KeepsTurnWithTextBesideAttachment
#[test]
fn keeps_turn_with_text_beside_attachment() {
    let (body, err) = checked_request(&format!(
        r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"user","content":"hello"}},{{"role":"assistant","content":"hi"}},{{"role":"user","content":[{USER_TURN_TEXT},{USER_TURN_FILE_ID}]}}]}}"#
    ));
    assert_eq!(err, None, "{body}");
    assert_eq!(body["input"][2]["content"][0]["text"], "keep me", "{body}");
}

// TestConvertOpenAIRequestToInteractions_InlineFileAndAudioAfterHistoryStayBytes
#[test]
fn inline_file_and_audio_after_history_stay_bytes() {
    for (part, want_type, want_data) in [
        (USER_TURN_INLINE, "document", "JVBERi0xLjQK"),
        (USER_TURN_AUDIO, "audio", "UklGRg=="),
    ] {
        let (body, err) = checked_request(&format!(
            r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"user","content":"hello"}},{{"role":"assistant","content":"hi"}},{{"role":"user","content":[{part}]}}]}}"#
        ));
        assert_eq!(err, None, "{body}");
        let part = &body["input"][2]["content"][0];
        assert_eq!(part["type"], want_type, "{body}");
        assert_eq!(part["data"], want_data, "{body}");
    }
}

// TestConvertOpenAIRequestToInteractions_FileURLStaysAFile
#[test]
fn file_url_stays_a_file() {
    let (body, err) = checked_request(
        r#"{"model":"gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"file","file":{"file_url":"https://example.test/a.pdf"}}]}]}"#,
    );
    assert_eq!(err, None, "{body}");
    assert_eq!(
        body["input"][0]["content"][0]["file_url"], "https://example.test/a.pdf",
        "{body}"
    );
}

// TestConvertOpenAIRequestToInteractions_ExportedWrapperKeepsAJSONBody
#[test]
fn refusal_keeps_a_json_body() {
    let (body, err) = checked_request(&format!(
        r#"{{"model":"gemini-3.5-flash","messages":[{{"role":"user","content":[{USER_TURN_FILE_ID}]}}]}}"#
    ));
    assert!(err.is_some() && body.is_object(), "{body}");
}

// Not upstream's: user messages in detail. Any role but the assistant's,
// a tool's and the system prompts' is a user turn; an image with nothing in
// it is still sent; a part of another type isn't an attachment. The
// expected output comes from upstream.
#[test]
fn user_turns_in_detail() {
    for (input, want_body, want_err) in [
        (
            r#"{"messages":[{"role":"user","content":{"type":"input_audio","input_audio":{}}}]}"#,
            r#"{"model":"m","input":[]}"#,
            Some("input_audio"),
        ),
        (
            r#"{"messages":[{"role":"model","content":[{"type":"file","file":{}}]},{"role":"user","content":"x"}]}"#,
            r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"text","text":"x"}]}]}"#,
            Some("file"),
        ),
        (
            r#"{"messages":[{"role":"user","content":[{"type":"image_url"},{"type":"file"}]},{"role":"assistant","content":[{"type":"file","file":{"file_id":"f"}}]}]}"#,
            r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"image"}]}]}"#,
            None,
        ),
        (
            r#"{"messages":[{"role":"user","content":[{"type":"text"},{"text":""},{"type":" INPUT_FILE ","file":{"file_id":"f"}}]}]}"#,
            r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"text","text":""},{"type":"text","text":""}]}]}"#,
            Some("input_file"),
        ),
        (
            r#"{"messages":[{"role":"user","content":""},{"role":"user","content":[{"type":"video"}]},{"role":"tool","content":"r"},{"role":"user","content":[{"type":"text","text":" "},{"type":"audio","data":""}]}]}"#,
            r#"{"model":"m","input":[{"type":"function_result","result":"r"},{"type":"user_input","content":[{"type":"text","text":" "}]}]}"#,
            None,
        ),
        (
            r#"{"messages":[{"role":"user","content":[{"type":"document","file_url":"u"}]},{"role":"user","content":[{"type":"audio"}]},{"role":"user","content":[{"type":"file","file":{"filename":"a.txt","file_data":"aGk="}}]}]}"#,
            r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"document","file_url":"u"}]},{"type":"user_input","content":[{"type":"document","filename":"a.txt","mime_type":"text/plain","data":"aGk="}]}]}"#,
            Some("audio"),
        ),
    ] {
        let body: Value = serde_json::from_str(input).expect("test request is JSON");
        let (out, err) = super::convert_openai_request_to_interactions("m", &body, false);
        assert_eq!(out.to_string(), want_body, "{input}");
        assert_eq!(
            err.map(|err| err.part_type),
            want_err.map(str::to_owned),
            "{input}"
        );
    }
}
