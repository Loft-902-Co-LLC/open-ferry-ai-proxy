//! Ported from the tests of `ConvertInteractionsRequestToOpenAI` and
//! `ConvertInteractionsResponseToOpenAI` in interactions_openai_request_test.go
//! and interactions_openai_response_test.go.
//!
//! Dropped or changed tests, as the Antigravity branches are not ported:
//! - `TestConvertInteractionsResponseToOpenAINonStreamRestoresAntigravityToolName`
//!   and `TestConvertInteractionsResponseToOpenAIStreamRestoresAntigravityToolName`
//!   are dropped: they assert only the Antigravity renames.
//! - `TestConvertInteractionsResponseToOpenAIPreservesNonCollidingAndNonAntigravityNames`
//!   keeps only its half for a Gemini model.
//! - `TestConvertInteractionsResponseToOpenAINonStream_PreservesEnvironmentID`
//!   and `TestConvertInteractionsResponseToOpenAIStream_PreservesEnvironmentID`
//!   use a Gemini model in place of the Antigravity one.

use serde_json::{Value, json};

use super::*;

fn request(model: &str, raw: &str) -> Value {
    let body: Value = serde_json::from_str(raw).expect("valid JSON");
    convert_interactions_request_to_openai(model, &body, false)
}

fn non_stream(model: &str, raw: &str) -> Value {
    let body: Value = serde_json::from_str(raw).expect("valid JSON");
    convert_interactions_response_to_openai_non_stream(model, &body)
}

/// The chunks a stream of events gives.
fn stream(model: &str, events: &[&str]) -> Vec<Value> {
    let mut stream = InteractionsToOpenAIStream::new(model);
    events
        .iter()
        .flat_map(|event| stream.translate(event.as_bytes()))
        .collect()
}

/// The first chunk with a value at `at`, a JSON pointer.
fn find<'a>(chunks: &'a [Value], at: &str) -> &'a Value {
    chunks
        .iter()
        .find(|chunk| chunk.pointer(at).is_some())
        .unwrap_or_else(|| panic!("no {at} in {chunks:?}"))
}

/// The first chunk whose value at `at`, a JSON pointer, is `want`.
fn find_value<'a>(chunks: &'a [Value], at: &str, want: &str) -> &'a Value {
    chunks
        .iter()
        .find(|chunk| chunk.pointer(at).is_some_and(|value| value == want))
        .unwrap_or_else(|| panic!("no {at} = {want} in {chunks:?}"))
}

/// The keys of an object, in order.
fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .map_or_else(Vec::new, |value| value.keys().map(String::as_str).collect())
}

// TestConvertInteractionsRequestToOpenAIPreservesExpressibleFields
#[test]
fn preserves_expressible_fields() {
    let out = request(
        "gpt-test",
        r#"{"model":"gpt-test","tool_choice":{"type":"function","function":{"name":"lookup"}},"response_modalities":["text","image"],"service_tier":"priority","input":"hi"}"#,
    );
    assert_eq!(out["tool_choice"]["type"], "function");
    assert_eq!(out["tool_choice"]["function"]["name"], "lookup");
    assert_eq!(out["modalities"], json!(["text", "image"]));
    assert_eq!(out["service_tier"], "priority");
}

// TestConvertInteractionsRequestToOpenAIAcceptsImageContent
#[test]
fn accepts_image_content() {
    let out = request(
        "gpt-test",
        r#"{"model":"gpt-test","input":[{"type":"user_input","content":[{"type":"image","mime_type":"image/png","data":"aGVsbG8="}]}]}"#,
    );
    let part = &out["messages"][0]["content"][0];
    assert_eq!(part["type"], "image_url");
    assert_eq!(part["image_url"]["url"], "data:image/png;base64,aGVsbG8=");
}

// TestConvertInteractionsRequestToOpenAIPreservesNonImageMediaContent
#[test]
fn preserves_non_image_media_content() {
    let out = request(
        "gpt-test",
        r#"{"model":"gpt-test","input":[{"type":"user_input","content":[{"type":"audio","mime_type":"audio/wav","data":"UklGRg=="},{"type":"video","mime_type":"video/mp4","data":"AAAAIGZ0eXA="},{"type":"document","mime_type":"application/pdf","data":"JVBERi0="}]}]}"#,
    );
    let content = &out["messages"][0]["content"];
    assert_eq!(content[0]["type"], "input_audio");
    assert_eq!(content[0]["input_audio"]["format"], "wav");
    assert_eq!(content[1]["type"], "video_url");
    assert_eq!(content[2]["type"], "file");
}

// TestConvertInteractionsRequestToOpenAIWithToolMessagesDirect
#[test]
fn with_tool_messages_direct() {
    let out = request(
        "gpt-test",
        r#"{"model":"gpt-test","input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]},{"type":"function_call","name":"lookup","call_id":"call_1","arguments":{"q":"x"}},{"type":"function_result","name":"lookup","call_id":"call_1","result":{"ok":true}}]}"#,
    );
    let call = &out["messages"][1]["tool_calls"][0]["function"];
    assert_eq!(call["name"], "lookup");
    assert_eq!(call["arguments"], r#"{"q":"x"}"#);
    assert_eq!(out["messages"][2]["tool_call_id"], "call_1");
}

/// An Interactions request using most of what the translator reads.
const WHOLE_REQUEST: &str = r#"{"model":"m0","stream":"true","system_instruction":{"parts":[{"text":"a"},{"content":{"text":"b"}}]},"input":[
    "plain",
    {"type":"user_input","content":[{"type":"text","text":"x"},{"text":"y"}]},
    {"type":"user_input","content":[{"type":"text","text":"x"},{"type":"image","url":"https://i"},{"type":"audio","data":"QQ==","mime_type":"audio/ogg"},{"type":"video","data":"Vg=="},{"type":"document","mime_type":"image/svg+xml","data":"PHN2Zz4="},{"type":"file","url":"https://f","filename":"f.bin"},{"type":"unknown"}]},
    {"type":"model_output","content":"done"},
    {"type":"thought","content":{"text":"t"}},
    {"type":"function_call","id":"fc","name":"f","arguments":{"a":1}},
    {"type":"function_result","call_id":"fc","output":[1]},
    {"type":"user_input"}
],"tools":[{"name":"t1","parameters":{"type":"object"}},{"function_declarations":[{"name":"d1","description":"dd","parametersJsonSchema":{"type":"string"}}]},{"type":"function","function":{"name":"t2","description":null}}],
"generation_config":{"temperature":0.2,"maxOutputTokens":10,"thinking_config":{"thinking_level":" LOW "},"stop_sequences":"s"},
"top_p":0.3,"n":3,"response_modalities":["text"],"response_format":{"type":"text"},"service_tier":5,"previous_response_id":"pr","environment_id":"env","agent_config":null,"parallel_tool_calls":false,"seed":7,"user":"u"}"#;

// Not upstream's: a request using most of what the translator reads, whole,
// as upstream writes it (the same case is in the parity suite).
#[test]
fn translates_a_whole_request() {
    let out = request("gpt-x", WHOLE_REQUEST);
    assert_eq!(
        out,
        json!({
            "model": "gpt-x",
            "messages": [
                {"role": "system", "content": "ab"},
                {"role": "user", "content": "plain"},
                {"role": "user", "content": "xy"},
                {"role": "user", "content": [
                    {"type": "text", "text": "x"},
                    {"type": "image_url", "image_url": {"url": "https://i"}},
                    {"type": "input_audio", "input_audio": {"data": "QQ==", "format": "opus"}},
                    {"type": "video_url", "video_url": {"url": "data:video/mp4;base64,Vg=="}},
                    {"type": "file", "file": {"filename": "document.svg.xml", "file_data": "PHN2Zz4="}},
                    {"type": "file", "file": {"filename": "f.bin", "file_url": "https://f"}}
                ]},
                {"role": "assistant", "content": "done"},
                {"role": "assistant", "content": "", "reasoning_content": "t"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "fc", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}
                ]},
                {"role": "tool", "tool_call_id": "fc", "content": "[1]"},
                {"role": "user", "content": ""}
            ],
            "stream": true,
            "tools": [
                {"type": "function", "function": {"name": "t1", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "d1", "description": "dd", "parameters": {"type": "string"}}},
                {"type": "function", "function": {"name": "t2", "description": ""}}
            ],
            "temperature": 0.2,
            "max_tokens": 10,
            "top_p": 0.3,
            "n": 3,
            "stop": "s",
            "reasoning_effort": "low",
            "modalities": ["text"],
            "response_format": {"type": "text"},
            "previous_response_id": "pr",
            "environment_id": "env",
            "agent_config": null,
            "parallel_tool_calls": false,
            "seed": 7,
            "user": "u"
        })
    );
    assert_eq!(
        keys(&out),
        [
            "model",
            "messages",
            "stream",
            "tools",
            "temperature",
            "max_tokens",
            "top_p",
            "n",
            "stop",
            "reasoning_effort",
            "modalities",
            "response_format",
            "previous_response_id",
            "environment_id",
            "agent_config",
            "parallel_tool_calls",
            "seed",
            "user"
        ]
    );
}

// Not upstream's: the first reasoning setting that is a string wins, even a
// blank one.
#[test]
fn a_blank_reasoning_setting_stops_the_search() {
    let out = request(
        "m",
        r#"{"generationConfig":{"thinkingLevel":7,"thinking_level":" "},"reasoning_effort":"high"}"#,
    );
    assert_eq!(out, json!({"model": "m", "messages": []}));
    let out = request(
        "m",
        r#"{"generation_config":null,"generationConfig":{"temperature":1},"reasoning_effort":"HIGH"}"#,
    );
    assert_eq!(
        out,
        json!({"model": "m", "messages": [], "reasoning_effort": "high"})
    );
}

// TestConvertInteractionsResponseToOpenAIStreamToolCall
#[test]
fn stream_tool_call() {
    let out = stream(
        "gemini-3.1-flash-lite",
        &[
            r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"gemini-3.1-flash-lite"}}"#,
            r#"data: {"event_type":"step.start","index":0,"step":{"type":"function_call","id":"call_1","name":"get_weather","arguments":{}}}"#,
            r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":"{\"location\":\"北京\"}"}}"#,
            r#"data: {"event_type":"step.stop","index":0}"#,
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"requires_action","usage":{"total_input_tokens":2,"total_output_tokens":3,"total_tokens":5}}}"#,
        ],
    );
    let start = find(&out, "/choices/0/delta/tool_calls/0/function/name");
    assert_eq!(
        start["choices"][0]["delta"]["tool_calls"][0]["id"],
        "call_1"
    );
    assert_eq!(
        start["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    find_value(
        &out,
        "/choices/0/delta/tool_calls/0/function/arguments",
        r#"{"location":"北京"}"#,
    );
    let completed = find_value(&out, "/choices/0/finish_reason", "tool_calls");
    assert_eq!(completed["usage"]["prompt_tokens"], 2);
}

// TestConvertInteractionsResponseToOpenAIStreamFinishMetadataUsage
#[test]
fn stream_finish_metadata_usage() {
    let out = stream(
        "gpt-test",
        &[
            r#"data: {"event_type":"finish","metadata":{"total_usage":{"total_input_tokens":2,"total_output_tokens":6,"total_thought_tokens":3,"total_cached_tokens":1,"total_tokens":11}}}"#,
        ],
    );
    let completed = find_value(&out, "/choices/0/finish_reason", "stop");
    let usage = &completed["usage"];
    assert_eq!(usage["prompt_tokens"], 2);
    assert_eq!(usage["completion_tokens"], 6);
    assert_eq!(usage["completion_tokens_details"]["reasoning_tokens"], 3);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 1);
    assert_eq!(usage["total_tokens"], 11);
}

// TestConvertInteractionsResponseToOpenAINonStreamToolCall
#[test]
fn non_stream_tool_call() {
    let out = non_stream(
        "gemini-3.1-flash-lite",
        r#"{"id":"i1","model":"gemini-3.1-flash-lite","steps":[{"type":"function_call","id":"call_1","name":"get_weather","arguments":{"location":"北京"}}],"usage":{"total_input_tokens":2,"total_output_tokens":3,"total_tokens":5}}"#,
    );
    let call = &out["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], "call_1");
    assert_eq!(call["function"]["name"], "get_weather");
    assert_eq!(call["function"]["arguments"], r#"{"location":"北京"}"#);
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
}

// TestConvertInteractionsResponseToOpenAINonStream_PreservesEnvironmentID,
// with a Gemini model.
#[test]
fn non_stream_preserves_environment_id() {
    let out = non_stream(
        "gemini-3.1-flash-lite",
        r#"{"id":"i1","model":"gemini-3.1-flash-lite","environment_id":"env_chat123","steps":[{"type":"model_output","content":[{"type":"text","text":"hello"}]}],"usage":{"total_tokens":5}}"#,
    );
    assert_eq!(out["environment_id"], "env_chat123");
}

// TestConvertInteractionsResponseToOpenAIStream_PreservesEnvironmentID, with
// a Gemini model.
#[test]
fn stream_preserves_environment_id() {
    let out = stream(
        "gemini-3.1-flash-lite",
        &[
            r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"gemini-3.1-flash-lite","environment_id":"env_chat_stream456"}}"#,
        ],
    );
    assert_eq!(out[0]["environment_id"], "env_chat_stream456");
}

// TestConvertInteractionsResponseToOpenAIPreservesNonCollidingAndNonAntigravityNames,
// its half for a Gemini model.
#[test]
fn preserves_tool_names() {
    let out = non_stream(
        "gemini-3.1-flash-lite",
        r#"{"id":"i2","model":"gemini-3.1-flash-lite","steps":[{"type":"function_call","id":"call_2","name":"external_read_file","arguments":{"path":"/etc/hosts"}}]}"#,
    );
    assert_eq!(
        out["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "external_read_file"
    );
}

// TestConvertInteractionsResponseToOpenAIStreamToolCall_ContiguousZeroBasedIndex
#[test]
fn stream_tool_call_contiguous_zero_based_index() {
    let out = stream(
        "devin/swe-2",
        &[
            r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
            r#"data: {"event_type":"step.start","index":0,"step":{"type":"thought"}}"#,
            r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","text":"planning..."}}"#,
            r#"data: {"event_type":"step.stop","index":0}"#,
            r#"data: {"event_type":"step.start","index":1,"step":{"type":"model_output"}}"#,
            r#"data: {"event_type":"step.delta","index":1,"delta":{"type":"text","text":"I will call tool."}}"#,
            r#"data: {"event_type":"step.stop","index":1}"#,
            r#"data: {"event_type":"step.start","index":2,"step":{"type":"function_call","id":"call_first","name":"write_file","arguments":{}}}"#,
            r#"data: {"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{\"path\":\"a\"}"}}"#,
            r#"data: {"event_type":"step.stop","index":2}"#,
            r#"data: {"event_type":"step.start","index":3,"step":{"type":"function_call","id":"call_second","name":"read_file","arguments":{}}}"#,
            r#"data: {"event_type":"step.delta","index":3,"delta":{"type":"arguments_delta","arguments":"{\"path\":\"b\"}"}}"#,
            r#"data: {"event_type":"step.stop","index":3}"#,
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"completed"}}"#,
            "data: [DONE]",
        ],
    );
    let first = find_value(&out, "/choices/0/delta/tool_calls/0/id", "call_first");
    assert_eq!(first["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    let second = find_value(&out, "/choices/0/delta/tool_calls/0/id", "call_second");
    assert_eq!(second["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
}

// TestConvertInteractionsResponseToOpenAIStream_IncompleteFinishReasonLength
#[test]
fn stream_incomplete_finish_reason_length() {
    let out = stream(
        "devin/swe-2",
        &[
            r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
            r#"data: {"event_type":"step.start","index":0,"step":{"type":"function_call","id":"call_1","name":"write_file"}}"#,
            r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":"{\"path\":\"a\""}}"#,
            r#"data: {"event_type":"step.stop","index":0}"#,
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"incomplete","finish_reason":"length"}}"#,
            "data: [DONE]",
        ],
    );
    find_value(&out, "/choices/0/finish_reason", "length");
}

// TestConvertInteractionsResponseToOpenAI_ContentFilterFinishReason
#[test]
fn content_filter_finish_reason() {
    let out = stream(
        "devin/swe-2",
        &[
            r#"data: {"event_type":"interaction.created","interaction":{"id":"i1","model":"devin/swe-2"}}"#,
            r#"data: {"event_type":"step.start","index":0,"step":{"type":"model_output"}}"#,
            r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"blocked"}}"#,
            r#"data: {"event_type":"step.stop","index":0}"#,
            r#"data: {"event_type":"interaction.completed","interaction":{"id":"i1","status":"incomplete","finish_reason":"content_filter"}}"#,
            "data: [DONE]",
        ],
    );
    find_value(&out, "/choices/0/finish_reason", "content_filter");

    let out = non_stream(
        "devin/swe-2",
        r#"{"id":"i1","model":"devin/swe-2","status":"incomplete","finish_reason":"content_filter","steps":[{"type":"model_output","content":[{"type":"text","text":"blocked"}]}]}"#,
    );
    assert_eq!(out["choices"][0]["finish_reason"], "content_filter");
}

// TestConvertInteractionsResponseToOpenAI_ResponseFailed
#[test]
fn response_failed() {
    let cases = [
        (
            "response_failed_top_level",
            r#"data: {"event_type":"response.failed","error":{"message":"devin upstream error (permission_denied): Unable to process request due to an MCP configuration issue.","code":"403"}}"#,
            "permission_denied",
            "403",
        ),
        (
            "interaction_failed_nested",
            r#"data: {"event_type":"interaction.failed","interaction":{"error":{"message":"rate limit exceeded","code":"429"}}}"#,
            "rate limit exceeded",
            "429",
        ),
        (
            "fallback_defaults",
            r#"data: {"event_type":"response.failed"}"#,
            "upstream error occurred",
            "",
        ),
    ];
    for (name, payload, message, code) in cases {
        let out = stream("devin/kimi-k3", &[payload]);
        let error = &out.first().unwrap_or_else(|| panic!("{name}: no events"))["error"];
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|got| got.contains(message)),
            "{name}: {error}"
        );
        if code.is_empty() {
            assert!(error.get("code").is_none(), "{name}: {error}");
        } else {
            assert_eq!(error["code"], code, "{name}");
        }
    }
}

/// An Interactions event stream using most of what the translator reads.
const WHOLE_STREAM: &[&str] = &[
    r#"data: {"event_type":"interaction.created","interaction":{"id":"i9","model":"gm","environment":{"id":"env1"}}}"#,
    "event: step.start\ndata: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"thought\"}}",
    r#"{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"text":"plan"}}}"#,
    r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","text":"  "}}"#,
    r#"data: {"event_type":"step.delta","index":1,"delta":{"type":"text","text":" "}}"#,
    r#"data: {"event_type":"step.start","index":5,"step":{"type":"function_call","call_id":"cc","id":"ii","name":"f"}}"#,
    r#"data: {"event_type":"step.delta","index":5,"delta":{"type":"arguments_delta","arguments":{"x":1}}}"#,
    r#"data: {"event_type":"step.delta","index":9,"delta":{"type":"arguments_delta"}}"#,
    r#"data: {"event_type":"step.start","index":7,"step":{"type":"function_call"}}"#,
    r#"data: {"event_type":"done"}"#,
    r#"data: {"event_type":"interaction.completed","interaction":{"status":"incomplete","usage":{"input_tokens":4,"total_input_tokens":9,"total_output_tokens":2,"total_cached_tokens":1}},"environment_id":"env2"}"#,
    r#"data: {"event_type":"finish"}"#,
    r#"data: {"event_type":"interaction.failed","error":{"message":"bad","type":"t","code":503}}"#,
    "data: [DONE]",
];

// Not upstream's: a whole stream, as upstream translates it but for the
// time stamps (the same case is in the parity suite).
#[test]
fn translates_a_whole_stream() {
    let mut out = stream("m", WHOLE_STREAM);
    let created = out[0]["created"].as_i64().unwrap_or_default();
    assert!(created > 0, "{}", out[0]);
    for chunk in &mut out {
        if let Some(chunk) = chunk
            .as_object_mut()
            .filter(|chunk| chunk.contains_key("id"))
        {
            assert_eq!(chunk.shift_remove("created"), Some(created.into()));
        }
    }
    let chunk = |delta: Value| json!({"id": "i9", "object": "chat.completion.chunk", "model": "gm", "choices": [{"index": 0, "delta": delta, "finish_reason": null}], "environment_id": "env1"});
    let start = |index: u64, id: &str, name: &str| {
        chunk(
            json!({"tool_calls": [{"index": index, "id": id, "type": "function", "function": {"name": name, "arguments": ""}}]}),
        )
    };
    let arguments = |index: u64, arguments: &str| {
        chunk(json!({"tool_calls": [{"index": index, "function": {"arguments": arguments}}]}))
    };
    assert_eq!(
        out,
        [
            chunk(json!({"role": "assistant"})),
            chunk(json!({"reasoning_content": "plan"})),
            chunk(json!({"content": " "})),
            start(0, "cc", "f"),
            arguments(0, r#"{"x":1}"#),
            arguments(9, ""),
            start(1, "call_1", ""),
            json!({"id": "i9", "object": "chat.completion.chunk", "model": "gm", "choices": [{"index": 0, "delta": {}, "finish_reason": "length"}], "environment_id": "env2", "usage": {"prompt_tokens": 4, "completion_tokens": 2, "prompt_tokens_details": {"cached_tokens": 1}}}),
            json!({"error": {"message": "bad", "type": "t", "code": "503"}}),
        ]
    );
}

/// An Interactions event holding a response, using most of what the
/// translator reads.
const WHOLE_RESPONSE: &str = r#"{"interaction":{"id":"","model":"","steps":[{"type":"thought","content":[{"text":"r1"},{"content":{"text":"r2"}}]},{"type":"model_output","content":"a"},{"type":"model_output","content":[{"type":"text","text":"b"},"junk"]},{"type":"function_call","name":"f","arguments":"{}"},{"type":"function_call","id":"x2","arguments":null}],"environment":{"id":"e"},"finish_reason":"max_tokens"},"id":"outer","usage":{"total_tokens":3,"reasoning_tokens":1}}"#;

// Not upstream's: a whole response, as upstream writes it but for the time
// stamp (the same case is in the parity suite).
#[test]
fn translates_a_whole_response() {
    let mut out = non_stream("mm", WHOLE_RESPONSE);
    let created = out["created"].take();
    assert!(
        created.as_i64().is_some_and(|created| created > 0),
        "{created}"
    );
    assert_eq!(
        out,
        json!({
            "id": "outer",
            "object": "chat.completion",
            "created": null,
            "model": "mm",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "r1r2",
                    "tool_calls": [
                        {"id": "call_0", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                        {"id": "x2", "type": "function", "function": {"name": "", "arguments": "null"}}
                    ]
                },
                "finish_reason": "length"
            }],
            "environment_id": "e",
            "usage": {"total_tokens": 3, "completion_tokens_details": {"reasoning_tokens": 1}}
        })
    );
    assert_eq!(
        keys(&out),
        [
            "id",
            "object",
            "created",
            "model",
            "choices",
            "environment_id",
            "usage"
        ]
    );
    assert_eq!(
        keys(&out["choices"][0]["message"]),
        ["role", "content", "reasoning_content", "tool_calls"]
    );
}

// Not upstream's: an interaction id sent as the number -0 stays "-0", as
// gjson's String() gives it (checked with Go).
#[test]
fn stream_keeps_a_number_id_as_written() {
    let chunks = stream(
        "m",
        &[
            r#"data: {"event_type":"interaction.created","interaction":{"id":-0,"model":"m"}}"#,
            r#"data: {"event_type":"step.delta","index":0,"delta":{"type":"text","text":"hi"}}"#,
        ],
    );
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    assert!(chunks.iter().all(|chunk| chunk["id"] == "-0"), "{chunks:?}");
}

// Not upstream's: a call's arguments object, read with each number as
// written, goes on as that text, as upstream copies it (checked with Go).
#[test]
fn request_arguments_keep_their_numbers() {
    let spelled = r#"{"x":-0,"y":1E20,"z":[1e5,0.10]}"#;
    let body = crate::json::exact::from_str(&format!(
        r#"{{"input":[{{"type":"function_call","id":"a","name":"f","arguments":{spelled}}}]}}"#
    ))
    .unwrap();
    let out = convert_interactions_request_to_openai("m", &body, false);
    assert_eq!(
        out["messages"][0]["tool_calls"][0]["function"]["arguments"],
        spelled
    );
}
