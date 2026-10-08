// Ported from CLIProxyAPI internal/translator/common/claude_native_sse_test.go
// and claude_native_response_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;
use crate::claude::openai::chat_completions::convert_claude_response_to_openai_chat_completions_non_stream;
use crate::claude::openai::responses::convert_claude_response_to_openai_responses_non_stream;

/// gjson's `Get` for a path of keys and array indexes.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    value.pointer(&format!("/{}", path.replace('.', "/")))
}

/// gjson's `Get(path).String()`.
fn text_at(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// The events of `response` adapted, and its model.
fn events(response: &str) -> (Vec<Value>, String) {
    let Native::Events { sse, model } = messages_json_to_sse(response.as_bytes()) else {
        panic!("not adapted: {response}");
    };
    let events = sse
        .split('\n')
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("valid SSE JSON"))
        .collect();
    (events, model)
}

// Ports TestClaudeMessagesJSONToSSEPassthrough.
#[test]
fn passthrough() {
    for raw in [
        "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"sse-model\"}}\r\n\r\n: keepalive\r\n",
        "data: {\"type\":\"message_stop\"}\n\n",
        r#"{"type":"error","error":{"message":"bad request"}}"#,
        r#"{"type":"message","content":"#,
        r#"{"type":"message","content":"not an array"}"#,
        "",
    ] {
        assert_eq!(
            messages_json_to_sse(raw.as_bytes()),
            Native::Other,
            "{raw:?}"
        );
    }
}

// Ports TestClaudeMessagesJSONToSSEEvents.
#[test]
fn events_build_the_message() {
    let raw = r#"{
		"id":"msg_native","type":"message","role":"assistant","model":"claude-native",
		"content":[
			{"type":"text","text":"line 1\n\"quoted\" \\ 雪 <tag>"},
			{"type":"tool_use","id":"tool_nested","name":"lookup","input":{"nested":{"text":"line\n\"quoted\"","items":[true,null,2]},"empty":{}}},
			{"type":"tool_use","id":"tool_empty","name":"clock","input":{}},
			{"type":"thinking","thinking":"plan\nnext","signature":"sig\"native"},
			{"type":"redacted_thinking","data":"opaque"}
		],
		"stop_reason":"stop_sequence","stop_sequence":"END\n",
		"usage":{"input_tokens":13,"output_tokens":5,"cache_read_input_tokens":7,"cache_creation_input_tokens":3}
	}"#;
    let (events, model) = events(raw);
    assert_eq!(model, "claude-native");
    let want_types = [
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "content_block_start",
        "content_block_delta",
        "content_block_delta",
        "content_block_stop",
        "content_block_start",
        "content_block_stop",
        "message_delta",
        "message_stop",
    ];
    let types: Vec<String> = events.iter().map(|event| text_at(event, "type")).collect();
    assert_eq!(types, want_types);
    let root: Value = serde_json::from_str(raw).expect("valid JSON");
    for path in ["id", "model", "role", "usage"] {
        assert_eq!(
            at(&events[0], &format!("message.{path}")),
            at(&root, path),
            "message_start {path}"
        );
    }
    assert_eq!(at(&events[0], "message.content"), Some(&json!([])));
    for (start, stop, index) in [(1, 3, 0), (4, 6, 1), (7, 9, 2), (10, 13, 3), (14, 15, 4)] {
        for event in &events[start..=stop] {
            assert_eq!(at(event, "index"), Some(&json!(index)), "{event}");
        }
    }
    for (event, path, want) in [
        (2, "delta.type", "text_delta"),
        (2, "delta.text", "line 1\n\"quoted\" \\ 雪 <tag>"),
        (4, "content_block.id", "tool_nested"),
        (4, "content_block.name", "lookup"),
        (5, "delta.type", "input_json_delta"),
        (8, "delta.type", "input_json_delta"),
        (11, "delta.type", "thinking_delta"),
        (11, "delta.thinking", "plan\nnext"),
        (12, "delta.type", "signature_delta"),
        (12, "delta.signature", "sig\"native"),
        (14, "content_block.type", "redacted_thinking"),
        (14, "content_block.data", "opaque"),
        (16, "delta.stop_reason", "stop_sequence"),
        (16, "delta.stop_sequence", "END\n"),
    ] {
        assert_eq!(text_at(&events[event], path), want, "event {event} {path}");
    }
    for (event, block) in [(5, 1), (8, 2)] {
        let input: Value =
            serde_json::from_str(&text_at(&events[event], "delta.partial_json")).expect("JSON");
        assert_eq!(Some(&input), at(&root, &format!("content.{block}.input")));
    }
    assert_eq!(at(&events[16], "usage"), at(&root, "usage"));
}

// Ports TestClaudeMessagesJSONToSSECitations.
#[test]
fn citations() {
    let raw = r#"{"type":"message","content":[{"type":"text","text":"Answer.","citations":[{"type":"web_search_result_location","url":"https://example.com","title":"Source","cited_text":"Answer.","encrypted_index":"IDX"}]}]}"#;
    let (events, _) = events(raw);
    let citations: Vec<&Value> = events
        .iter()
        .filter(|event| text_at(event, "delta.type") == "citations_delta")
        .collect();
    assert_eq!(citations.len(), 1, "{events:?}");
    assert_eq!(at(citations[0], "index"), Some(&json!(0)));
    assert_eq!(
        text_at(citations[0], "delta.citation.encrypted_index"),
        "IDX"
    );
}

// Ports TestClaudeNativeResponseNonStreamModelAndContent.
#[test]
fn non_stream_model_and_content() {
    let text = "line 1\n\"quoted\" \\ 雪 <tag>";
    let input = json!({"nested": {"text": text, "items": [true, null, 2.0]}, "empty": {}});
    let raw = json!({
        "id": "msg_native", "type": "message", "role": "assistant", "model": "claude-native",
        "content": [
            {"type": "thinking", "thinking": text, "signature": "sig\n\"native"},
            {"type": "redacted_thinking", "data": "opaque-data"},
            {"type": "text", "text": text},
            {"type": "tool_use", "id": "tool_native", "name": "lookup", "input": input},
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 11, "output_tokens": 9},
    })
    .to_string();
    for request in ["{}", r#"{"model":"request-alias"}"#] {
        let request: Value = serde_json::from_str(request).expect("valid JSON");
        let chat = convert_claude_response_to_openai_chat_completions_non_stream(raw.as_bytes());
        let responses = convert_claude_response_to_openai_responses_non_stream(
            &request,
            &request,
            raw.as_bytes(),
        );
        for (out, text_path, think_path, args_path) in [
            (
                &chat,
                "choices.0.message.content",
                "choices.0.message.reasoning_content",
                "choices.0.message.tool_calls.0.function.arguments",
            ),
            (
                &responses,
                "output.2.content.0.text",
                "output.0.summary.0.text",
                "output.3.arguments",
            ),
        ] {
            for (path, want) in [
                ("model", "claude-native"),
                ("id", "msg_native"),
                (text_path, text),
                (think_path, text),
            ] {
                assert_eq!(text_at(out, path), want, "{request}: {path} in {out}");
            }
            let arguments: Value =
                serde_json::from_str(&text_at(out, args_path)).expect("valid tool arguments");
            assert_eq!(arguments, input, "{request}: {out}");
        }
        for (path, want) in [
            ("output.0.type", "reasoning"),
            ("output.0.encrypted_content", "sig\n\"native"),
            ("output.1.type", "reasoning"),
            // `ClaudeResponsesRedactedThinkingPrefix`
            (
                "output.1.encrypted_content",
                "claude-redacted-thinking:opaque-data",
            ),
            ("output.3.call_id", "tool_native"),
            ("output.3.name", "lookup"),
        ] {
            assert_eq!(
                text_at(&responses, path),
                want,
                "{request}: {path} in {responses}"
            );
        }
    }
}

// Not upstream's: Go's `json.Marshal` writes a block without a delta
// compacted, with `<`, `>`, `&`, U+2028 and U+2029 escaped, and a tool
// call's input goes on as written.
#[test]
fn blocks_are_written_as_go_writes_them() {
    let raw = "{\"type\":\"message\",\"content\":[\
        {\"type\": \"server_tool_use\", \"input\": {\"query\": \"a <b> & c\u{2028}\"}},\
        {\"type\":\"tool_use\",\"input\": { \"a\" : 1.50 }}]}";
    let Native::Events { sse, .. } = messages_json_to_sse(raw.as_bytes()) else {
        panic!("not adapted");
    };
    let escape = |code: &str| format!("{}u{code}", '\\');
    let compact = format!(
        r#"{{"type":"server_tool_use","input":{{"query":"a {}b{} {} c{}"}}}}"#,
        escape("003c"),
        escape("003e"),
        escape("0026"),
        escape("2028"),
    );
    assert!(sse.contains(&compact), "{sse}");
    let (events, model) = events(raw);
    assert_eq!(model, "");
    let deltas: Vec<String> = events
        .iter()
        .filter(|event| text_at(event, "delta.type") == "input_json_delta")
        .map(|event| text_at(event, "delta.partial_json"))
        .collect();
    assert_eq!(deltas, [r#"{ "a" : 1.50 }"#]);
}

// Not upstream's: what gjson reads but serde_json can't, and values read as
// gjson reads them.
#[test]
fn unreadable_and_loose_values() {
    let surrogate = format!(
        r#"{{"type":"message","content":[{{"type":"text","text":"{}ud800"}}]}}"#,
        '\\'
    );
    assert_eq!(
        messages_json_to_sse(surrogate.as_bytes()),
        Native::Unreadable
    );
    let mut invalid = br#"{"type":"message","content":[],"model":""#.to_vec();
    invalid.extend_from_slice(b"\xff\"}");
    assert_eq!(messages_json_to_sse(&invalid), Native::Unreadable);

    let raw = r#"{"type":"message","type":"other","model":7,"content":[
        "loose",
        {"type":"text","text":{"b": 1, "a": 2},"citations":{"url":"u"}},
        {"type":"thinking","thinking":1.50,"signature":null}
    ],"stop_reason":1.50}"#;
    let (events, model) = events(raw);
    assert_eq!(model, "7");
    assert_eq!(at(&events[1], "content_block"), Some(&json!("loose")));
    assert_eq!(text_at(&events[4], "delta.text"), r#"{"b": 1, "a": 2}"#);
    assert_eq!(text_at(&events[5], "delta.citation.url"), "u");
    assert_eq!(text_at(&events[8], "delta.thinking"), "1.5");
    assert_eq!(text_at(&events[9], "delta.type"), "signature_delta");
    assert_eq!(text_at(&events[9], "delta.signature"), "");
    let stop = events
        .iter()
        .find(|event| text_at(event, "type") == "message_delta");
    assert_eq!(
        stop.and_then(|event| at(event, "delta.stop_reason")),
        Some(&json!(1.5))
    );
    assert_eq!(stop.and_then(|event| at(event, "usage")), Some(&json!({})));
}
