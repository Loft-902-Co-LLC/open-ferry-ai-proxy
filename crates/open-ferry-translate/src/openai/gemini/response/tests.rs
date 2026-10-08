// Ported from CLIProxyAPI internal/translator/openai/gemini/openai_gemini_response_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::*;

fn non_stream(body: &str) -> Value {
    convert_openai_response_to_gemini_non_stream(body.as_bytes())
}

fn run_stream(lines: &[&str]) -> Vec<Value> {
    let mut translator = OpenAIToGeminiStream::new();
    lines
        .iter()
        .flat_map(|line| translator.translate_line(line.as_bytes()))
        .collect()
}

/// Whether any chunk sets a finish reason.
fn finishes(chunks: &[Value]) -> bool {
    chunks
        .iter()
        .any(|chunk| !str_of(chunk["candidates"][0].get("finishReason")).is_empty())
}

#[test]
fn non_stream_preserves_tool_call_id() {
    let out = non_stream(
        r#"{"choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_chat_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]}}]}"#,
    );
    let call = &out["candidates"][0]["content"]["parts"][0]["functionCall"];
    assert_eq!(call["id"], "call_chat_1");
    assert_eq!(call["args"]["q"], "x");
}

#[test]
fn stream_preserves_tool_call_id() {
    let mut translator = OpenAIToGeminiStream::new();
    translator.translate_line(
        br#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_stream_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]}}]}"#,
    );
    let out = translator
        .translate_line(br#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#);
    let call = &out.last().unwrap()["candidates"][0]["content"]["parts"][0]["functionCall"];
    assert_eq!(call["id"], "call_stream_1");
    assert_eq!(call["args"]["q"], "x");
}

#[test]
fn non_stream_multi_choice_parts_overlay() {
    // A later choice's text goes into the earlier choice's tool call part.
    let out = non_stream(
        r#"{"choices":[
            {"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}]}},
            {"index":1,"message":{"role":"assistant","content":"choice 1 text"}}
        ]}"#,
    );
    assert_eq!(
        out["candidates"][0]["content"]["parts"],
        json!([{"functionCall": {"id": "call_1", "name": "lookup", "args": {}}, "text": "choice 1 text"}])
    );

    // The thought mark stays, and the text is replaced.
    let out = non_stream(
        r#"{"choices":[
            {"index":0,"message":{"role":"assistant","reasoning_content":"initial thought"}},
            {"index":1,"message":{"role":"assistant","content":"final text"}}
        ]}"#,
    );
    assert_eq!(
        out["candidates"][0]["content"]["parts"],
        json!([{"thought": true, "text": "final text"}])
    );

    // The text stays, and the function call is added.
    let out = non_stream(
        r#"{"choices":[
            {"index":0,"message":{"role":"assistant","content":"original text"}},
            {"index":1,"message":{"role":"assistant","tool_calls":[{"id":"call_2","type":"function","function":{"name":"search","arguments":"{}"}}]}}
        ]}"#,
    );
    assert_eq!(
        out["candidates"][0]["content"]["parts"],
        json!([{"text": "original text", "functionCall": {"id": "call_2", "name": "search", "args": {}}}])
    );
}

#[test]
fn stream_null_finish_reason_ignored() {
    let mut translator = OpenAIToGeminiStream::new();
    let mut chunk = |line: &str| translator.translate_line(line.as_bytes());

    let out = chunk(
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
    );
    assert!(!finishes(&out));

    let out = chunk(
        r#"{"choices":[{"index":0,"delta":{"reasoning_content":"thinking..."},"finish_reason":null}]}"#,
    );
    assert_eq!(
        out[0]["candidates"][0]["content"]["parts"][0],
        json!({"thought": true, "text": "thinking..."})
    );
    assert!(!finishes(&out));

    let out = chunk(r#"{"choices":[{"index":0,"delta":{},"finish_reason":""}]}"#);
    assert!(!finishes(&out));

    let out = chunk(
        r#"{"choices":[{"index":0,"delta":{"content":"hello world"},"finish_reason":null}]}"#,
    );
    assert_eq!(
        out[0]["candidates"][0]["content"]["parts"][0]["text"],
        "hello world"
    );
    assert!(!finishes(&out));

    let out = chunk(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#);
    assert_eq!(out.last().unwrap()["candidates"][0]["finishReason"], "STOP");
}

#[test]
fn non_stream_null_finish_reason_ignored() {
    for reason in ["null", r#""""#] {
        let out = non_stream(&format!(
            r#"{{"choices":[{{"index":0,"message":{{"role":"assistant","content":"hello"}},"finish_reason":{reason}}}]}}"#
        ));
        assert!(out["candidates"][0].get("finishReason").is_none(), "{out}");
    }
}

/// Asserts `out` is the JSON text `want`, key order included.
fn assert_json(out: &Value, want: &str) {
    let want: Value = serde_json::from_str(want).unwrap();
    assert_eq!(out.to_string(), want.to_string());
}

#[test]
fn stream_gathers_tool_calls_until_the_finish_reason() {
    // Upstream's output for these lines.
    let out = run_stream(&[
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"f","arguments":"{\"a\":"}}]}}],"model":"m"}"#,
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}],"model":"m"}"#,
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"model":"m"}"#,
        r#"data: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":2},"prompt_tokens_details":{"cached_tokens":1}},"model":"m"}"#,
        "data: [DONE]",
    ]);
    assert_eq!(out.len(), 2, "{out:?}");
    assert_json(
        &out[0],
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_1","name":"f","args":{"a":1}}}],"role":"model"},"index":0,"finishReason":"STOP"}],"model":"m"}"#,
    );
    assert_json(
        &out[1],
        r#"{"candidates":[],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":7,"thoughtsTokenCount":2,"cachedContentTokenCount":1},"model":"m"}"#,
    );
}

#[test]
fn stream_tool_calls_come_out_by_index() {
    let out = run_stream(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":2,"function":{"name":"b","arguments":""}},{"index":1,"type":"function","function":{"name":"a"}},{"index":3,"type":"other","function":{"name":"skipped"}},{"index":4,"type":"function"}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_a","function":{"arguments":"{\"x\":true}"}},{"index":2,"function":{"name":"","arguments":"oops"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0]["candidates"][0]["finishReason"], "MAX_TOKENS");
    assert_eq!(
        out[0]["candidates"][0]["content"]["parts"],
        json!([
            {"functionCall": {"id": "call_a", "name": "a", "args": {"x": true}}},
            {"functionCall": {"name": "b", "args": {}}}
        ])
    );
    // The calls went with the first finish reason.
    assert_eq!(out[1]["candidates"][0]["content"]["parts"], json!([]));
}

#[test]
fn stream_text_and_reasoning_chunks() {
    let out = run_stream(&[
        r#"data: {"model":"m","choices":[{"delta":{"reasoning_content":["a",{"text":"b"},{"x":1},"",7],"content":"c","tool_calls":[{"index":0,"function":{"name":"f"}}]},"finish_reason":"stop"}]}"#,
        r#"data: {"choices":[{"delta":{}}],"usage":{"input_tokens":2,"output_tokens_details":{"reasoning_tokens":0}}}"#,
        r#"data: {"choices":[{"delta":{}}]}"#,
        r#"data: {"choices":[{"delta":{}}],"usage":null}"#,
        "event: ping",
        "  [DONE]  ",
        r#"data: {"choices":{}}"#,
        r#"data: {"choices":[]}"#,
        r#"data: {"choices":[{"delta":{}}],"usage":{}}"#,
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    let parts: Vec<&Value> = out
        .iter()
        .map(|chunk| &chunk["candidates"][0]["content"]["parts"])
        .collect();
    assert_eq!(
        parts,
        [
            &json!([{"thought": true, "text": "a"}]),
            &json!([{"thought": true, "text": "b"}]),
            &json!([{"text": "c"}]),
            &json!([]),
            &json!([]),
            &json!([]),
            // The tool call in the chunk with text was ignored too.
            &json!([]),
        ]
    );
    assert_eq!(out[0]["model"], "m");
    // The finish reason in the chunk with text was ignored.
    assert!(!finishes(&out[..3]));
    assert_eq!(
        out[3]["usageMetadata"],
        json!({"promptTokenCount": 2, "totalTokenCount": 2})
    );
    assert!(out[4].get("usageMetadata").is_none());
    assert!(out[5].get("usageMetadata").is_none());
    assert_eq!(out[6]["candidates"][0]["finishReason"], "STOP");
}

#[test]
fn stream_usage_only_chunk() {
    assert_json(
        &run_stream(&[r#"{"choices":[],"model":7,"usage":null}"#])[0],
        r#"{"candidates":[],"usageMetadata":{},"model":"7"}"#,
    );
    assert_json(
        &run_stream(&[r#"{"choices":[],"usage":{"total_tokens":9}}"#])[0],
        r#"{"candidates":[],"usageMetadata":{"totalTokenCount":9}}"#,
    );
}

#[test]
fn non_stream_merges_choices() {
    // Upstream's output for this response.
    let out = non_stream(
        r#"{"model":"m","choices":[{"index":0,"message":{"role":"assistant","reasoning_content":[{"text":"r1"},"r2",{"x":1},3],"content":"hello","tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}]},"finish_reason":"length"},{"index":1,"message":{"content":"second","tool_calls":[{"type":"function","function":{"name":"g","arguments":""}},{"type":"other","function":{"name":"h"}}]},"finish_reason":"content_filter"}],"usage":{"input_tokens":5,"output_tokens":6,"output_tokens_details":{"reasoning_tokens":0}}}"#,
    );
    assert_json(
        &out,
        r#"{"candidates":[{"content":{"parts":[{"thought":true,"text":"second"},{"thought":true,"text":"r2","functionCall":{"name":"g","args":{}}},{"text":"hello"},{"functionCall":{"id":"c1","name":"f","args":{"a":1}}}],"role":"model"},"index":1,"finishReason":"SAFETY"}],"model":"m","usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":6,"totalTokenCount":11}}"#,
    );
}

#[test]
fn non_stream_without_choices() {
    assert_json(
        &non_stream("not json"),
        r#"{"candidates":[{"content":{"parts":[],"role":"model"},"index":0}]}"#,
    );
    assert_json(
        &non_stream(
            r#"{"choices":[{"index":"4"}],"usage":{"prompt_tokens_details":{"cached_tokens":3}}}"#,
        ),
        r#"{"candidates":[{"content":{"parts":[],"role":"model"},"index":4}],"usageMetadata":{"cachedContentTokenCount":3}}"#,
    );
}

#[test]
fn arguments_read_leniently() {
    // Upstream's output for each.
    for (arguments, want) in [
        ("", "{}"),
        ("  {}  ", "{}"),
        (r#" {"a": [1, {"b": null}]} "#, r#"{"a":[1,{"b":null}]}"#),
        (r#"{"a": 1, "b": "x"} trailing"#, r#"{"a":1,"b":"x"}"#),
        (
            r#"{"k.dot": 1, ":colon": 3, "q?": 4, "": 5, "n": -0.0, "u": 18446744073709551615, "big": 1e400, "o": {"x": [1, }, "s": "a\"b"}"#,
            r#"{"k.dot":1,"colon":3,"n":-0,"u":18446744073709551615,"big":"1e400","o":"{\"x\": [1, }","s":"a\"b"}"#,
        ),
        (r#"{"a": [1, 2"#, "{}"),
        ("[1,2]", "{}"),
        // Copied as written, numbers and all.
        (
            r#"{"a": -0, "b": [1E20, 1e3]}"#,
            r#"{"a":-0,"b":[1E20,1e3]}"#,
        ),
        (r#"{"a": [-0, 1E20], "b": c}"#, r#"{"a":[-0,1E20],"b":"c"}"#),
        (
            r#"{"a": 1, "a": 2, "x": 0x10, "y": +5, "z": .5}"#,
            r#"{"a":2,"x":"0x10","y":5,"z":0.5}"#,
        ),
        (
            r#"{"a": 1_000, "b": 1_0.5, "c": 0x1p3, "d": _1, "e": 1__0, "f": 1_.5, "g": 0x_1p-1, "h": 1e_5, "k": 1e-400, "l": 9223372036854775808, "m": -9223372036854775809}"#,
            r#"{"a":1000,"b":10.5,"c":8,"d":"_1","e":"1__0","f":"1_.5","g":0.5,"h":"1e_5","k":0,"l":9223372036854775808,"m":-9223372036854776000}"#,
        ),
    ] {
        let want = crate::json::exact::from_str(want).unwrap();
        assert_eq!(
            args_object(arguments).to_string(),
            want.to_string(),
            "{arguments}"
        );
    }
}

#[test]
fn arguments_read_leniently_by_their_shape() {
    for (arguments, want) in [
        // Junk without a quote is skipped up to the next comma.
        (
            r#"{junk, "a": tru, "b": 1e3, "c": }"#,
            json!({"a": "tru", "b": 1000}),
        ),
        // A key without a colon ends it.
        (r#"{"a": 1, "b" 2, "c": 3}"#, json!({"a": 1})),
        // An unterminated string is set empty and ends it.
        (r#"{"a": "x, "b": 1}"#, json!({"a": "x, "})),
        (r#"{"a": 1, "b": "x}"#, json!({"a": 1, "b": ""})),
        // An unterminated object or array ends it.
        (r#"{"a": {"b": 1, "c": 2}"#, json!({})),
        // Brackets in strings don't count.
        (
            r#"{"a": ["]", "\"["], "b": 2,}"#,
            json!({"a": ["]", "\"["], "b": 2}),
        ),
        (r#"x{"a":null}y"#, json!({"a": null})),
        (r#"}{"a":1"#, json!({})),
    ] {
        assert_eq!(args_object(arguments), want, "{arguments}");
    }
}

#[test]
fn token_count() {
    assert_eq!(
        gemini_token_count(5),
        json!({"totalTokens": 5, "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 5}]})
    );
}
