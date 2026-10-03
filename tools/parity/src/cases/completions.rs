//! Hand-written cases for the legacy Completions conversions.

use serde_json::{Value, json};

use super::{Case, escaped};

/// Legacy Completions requests.
pub fn requests() -> Vec<Case> {
    let request = |name: &str, text: &str| Case::new(name, "", text);
    vec![
        request(
            "typical-request",
            r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say this is a test","max_tokens":7,"temperature":0,"top_p":1,"n":1,"stream":false,"logprobs":null,"stop":"\n","echo":false,"presence_penalty":0,"frequency_penalty":0,"best_of":1,"logit_bias":{"50256":-100},"user":"user-1","suffix":"!"}"#,
        ),
        request(
            "minimal",
            r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say this is a test"}"#,
        ),
        request(
            "streaming",
            r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Count to 3","stream":true,"logprobs":2,"top_logprobs":2}"#,
        ),
        request("empty-object", "{}"),
        request("array-body", "[]"),
        request("array-of-requests", r#"[{"prompt":"x"}]"#),
        request("null-body", "null"),
        request("string-body", r#""Say this is a test""#),
        request("number-body", "5"),
        request("empty-prompt", r#"{"prompt":"","model":"m"}"#),
        request("null-prompt", r#"{"prompt":null}"#),
        request("batch-prompt", r#"{"prompt": ["first", "second"]}"#),
        request("token-prompt", r#"{"prompt":[1212, 318, 257]}"#),
        request("nested-token-prompt", r#"{"prompt":[[1212,318],[257]]}"#),
        request("object-prompt", r#"{"prompt": { "text" : "x" }}"#),
        request("number-prompts", r#"{"prompt":1.50,"model":1E+2}"#),
        request("negative-zero-prompt", r#"{"prompt":-0}"#),
        request("boolean-prompt", r#"{"prompt":false}"#),
        request(
            "escaped-prompt",
            &r#"{"prompt":"café 🚀 a/b <tag> & more","model":"modèle"}"#
                .replace('é', &escaped('é'))
                .replace('🚀', &escaped('🚀'))
                .replace('/', "\\/")
                .replace('<', &escaped('<')),
        ),
        request(
            "escaped-keys",
            &r#"{"prompt":"x","max_tokens":5,"stream":true}"#
                .replace("prompt", &format!("pr{}mpt", escaped('o')))
                .replace("max_tokens", &format!("max{}tokens", escaped('_'))),
        ),
        request(
            "pretty-printed",
            "{\n  \"prompt\": \"x\",\n  \"stop\": [\n    \"\\n\",\n    \"END\"\n  ],\n  \"temperature\": 0.50\n}",
        ),
        request(
            "string-numbers",
            r#"{"max_tokens":"16","temperature":"0.5","top_p":"0x1p-1","frequency_penalty":"1_0.5","presence_penalty":".5","top_logprobs":"3"}"#,
        ),
        request(
            "unreadable-numbers",
            r#"{"max_tokens":"16.5","temperature":"warm","top_p":" 1","frequency_penalty":"","presence_penalty":"1e","top_logprobs":"+3"}"#,
        ),
        request(
            "other-types-as-numbers",
            r#"{"max_tokens":true,"temperature":true,"top_p":null,"frequency_penalty":[1],"presence_penalty":{"v":1},"top_logprobs":false}"#,
        ),
        request(
            "loose-flags",
            r#"{"stream":"TRUE","logprobs":2,"echo":"1"}"#,
        ),
        request(
            "false-flags",
            r#"{"stream":"yes","logprobs":0,"echo":null}"#,
        ),
        request(
            "float-forms",
            r#"{"temperature":1e-7,"top_p":1e21,"frequency_penalty":-0.0,"presence_penalty":5e-324}"#,
        ),
        request(
            "float-extremes",
            r#"{"temperature":1.7976931348623157e308,"top_p":1e-400,"frequency_penalty":"1e308","presence_penalty":"0x1p1023"}"#,
        ),
        request(
            "integer-forms",
            r#"{"max_tokens":16.9,"top_logprobs":-1.9}"#,
        ),
        request(
            "integer-overflow",
            r#"{"max_tokens":9223372036854775808,"top_logprobs":"18446744073709551616"}"#,
        ),
        request("huge-max-tokens", r#"{"max_tokens":1e30}"#),
        request("stop-string", r#"{"stop":"\n\n"}"#),
        request("stop-null", r#"{"stop":null}"#),
        request("stop-list", r#"{"stop": [ "\n", "END" , 1.50 ]}"#),
        request("stop-object", r#"{"stop":{"sequence":"x"}}"#),
        request("stop-number", r#"{"stop":1E+2}"#),
        request("temperature-beyond-f64", r#"{"temperature":1e400}"#).known_difference(
            "upstream writes temperature as +Inf, which isn't JSON; we leave it out",
        ),
        request("nan-top-p", r#"{"top_p":"NaN"}"#)
            .known_difference("upstream writes top_p as NaN, which isn't JSON; we leave it out"),
        request("infinite-penalty", r#"{"presence_penalty":"-inf"}"#).known_difference(
            "upstream writes presence_penalty as -Inf, which isn't JSON; we leave it out",
        ),
        request("duplicate-keys", r#"{"prompt":"first","prompt":"second"}"#)
            .known_difference("gjson reads the first duplicate key; serde_json keeps the last"),
    ]
}

/// A Chat Completions response body.
fn response(name: &str, body: &str) -> Case {
    Case::response(name, "", vec![body.to_owned()])
}

/// Chat Completions response bodies.
pub fn responses() -> Vec<Case> {
    let typical = json!({
        "id": "chatcmpl-123",
        "object": "chat.completion",
        "created": 1_677_652_288,
        "model": "gpt-4o-mini",
        "system_fingerprint": "fp_44709d6fcb",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hello there, how may I assist you today?" },
            "logprobs": null,
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 9,
            "completion_tokens": 12,
            "total_tokens": 21,
            "completion_tokens_details": { "reasoning_tokens": 0 }
        }
    });
    let logprobs = json!({
        "content": [
            { "token": "Hello", "logprob": -0.31725305, "bytes": [72, 101, 108, 108, 111], "top_logprobs": [
                { "token": "Hello", "logprob": -0.31725305, "bytes": [72, 101, 108, 108, 111] },
                { "token": "Hi", "logprob": -1.3190403, "bytes": [72, 105] }
            ]}
        ],
        "refusal": null
    });
    vec![
        response("typical-response", &typical.to_string()),
        response(
            "pretty-printed",
            &serde_json::to_string_pretty(&typical).expect("a Value serializes"),
        ),
        response(
            "with-logprobs",
            &json!({ "id": "chatcmpl-1", "choices": [{ "index": 0, "message": { "content": "Hello" }, "logprobs": logprobs, "finish_reason": "stop" }] }).to_string(),
        ),
        response(
            "logprob-number-forms",
            r#"{"choices":[{"logprobs":{"z":1.50,"a":[1e21,1e-7,-0.0,5e-324,123456789012345678901234567890,9007199254740993],"m":{"y":"<b>","x":null}}}]}"#,
        ),
        response("logprobs-number", r#"{"choices":[{"logprobs":1E+2}]}"#),
        response(
            "multiple-choices",
            r#"{"id":"c","choices":[{"index":0,"message":{"content":"a"},"finish_reason":"stop"},{"index":1,"message":{"content":"b"},"finish_reason":"length"}]}"#,
        ),
        response(
            "tool-call-response",
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ),
        response(
            "refusal",
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"refusal":"I can't help with that."},"finish_reason":"stop"}]}"#,
        ),
        response(
            "message-without-content",
            r#"{"choices":[{"index":0,"message":{"role":"assistant"},"delta":{"content":"ignored"}}]}"#,
        ),
        response(
            "null-message",
            r#"{"choices":[{"message":null,"delta":{"content":"ignored"}}]}"#,
        ),
        response(
            "delta-only",
            r#"{"choices":[{"index":0,"delta":{"content":"from a delta"},"finish_reason":"stop"}]}"#,
        ),
        response(
            "content-parts",
            r#"{"choices":[{"message":{"content":[ {"type": "text", "text": "a"} ]}}]}"#,
        ),
        response(
            "content-other-types",
            r#"{"choices":[{"message":{"content":1.50}},{"message":{"content":true}},{"message":{"content":{ "a" : 1 }}}]}"#,
        ),
        response(
            "finish-reasons",
            r#"{"choices":[{"finish_reason":null},{"finish_reason":"null"},{"finish_reason":""},{"finish_reason":0},{"finish_reason":{"type": "stop"}}]}"#,
        ),
        response("empty-choices", r#"{"id":"c","choices":[]}"#),
        response("choices-object", r#"{"choices":{"0":{"message":{"content":"x"}}}}"#),
        response("choices-null", r#"{"choices":null}"#),
        response(
            "choices-not-objects",
            r#"{"choices":[1,"choice",null,[],[{"index":1,"message":{"content":"x"}}]]}"#,
        ),
        response(
            "loose-fields",
            r#"{"id":12,"created":"1677652288","model":{"name": "m"},"choices":[{"index":"2"},{"index":1.9},{"index":-1},{"index":true}]}"#,
        ),
        response(
            "integer-overflow",
            r#"{"created":9223372036854775808,"choices":[{"index":"18446744073709551616"}]}"#,
        ),
        response("huge-created", r#"{"created":1e30}"#),
        response("usage-null", r#"{"usage":null}"#),
        response(
            "usage-spacing",
            r#"{"usage": { "prompt_tokens" : 1.50 , "total_tokens":2 } ,"id":"c"}"#,
        ),
        response("usage-string", r#"{"usage":"none"}"#),
        response(
            "escaped-body",
            &r#"{"id":"chatcmpl-1","choices":[{"message":{"content":"café 🚀 a/b"},"finish_reason":"stop"}]}"#
                .replace("content", &format!("c{}ntent", escaped('o')))
                .replace('é', &escaped('é'))
                .replace('🚀', &escaped('🚀'))
                .replace('/', "\\/"),
        ),
        response("not-json", "not json"),
        response("empty", ""),
        response("done", "[DONE]"),
        response("sse-line", r#"data: {"id":"c","choices":[]}"#),
        response("array-body", r#"[{"id":"c"}]"#),
        response("string-body", r#""text""#),
        response("truncated", r#"{"id":"chatcmpl-1","choices":[{"message":{"content":"Hel"#)
            .known_difference("gjson reads what it can from malformed JSON"),
        response("trailing-text", r#"{"id":"chatcmpl-1"} and more"#)
            .known_difference("gjson reads what it can from malformed JSON"),
        response("logprob-beyond-f64", r#"{"choices":[{"logprobs":{"x":1e400}}]}"#)
            .known_difference("Go can't marshal +Inf, so upstream's choices aren't JSON; we keep the number"),
        response("duplicate-keys", r#"{"id":"first","id":"second"}"#)
            .known_difference("gjson reads the first duplicate key; serde_json keeps the last"),
    ]
}

/// A run of Chat Completions stream chunks, each the JSON of a `data:` line.
fn chunks(name: &str, chunks: &[Value]) -> Case {
    Case::response(name, "", chunks.iter().map(Value::to_string).collect())
}

/// A chunk with one choice, holding `delta` and `finish_reason`.
fn chunk(delta: Value, finish_reason: Value) -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1_694_268_190,
        "model": "gpt-4o-mini",
        "system_fingerprint": "fp_44709d6fcb",
        "choices": [{ "index": 0, "delta": delta, "logprobs": null, "finish_reason": finish_reason }]
    })
}

/// Chat Completions stream chunks.
pub fn stream_chunks() -> Vec<Case> {
    let usage = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1_694_268_190,
        "model": "gpt-4o-mini",
        "choices": [],
        "usage": { "prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11 }
    });
    let raw = |name: &str, lines: &[&str]| {
        Case::response(
            name,
            "",
            lines.iter().map(|&line| line.to_owned()).collect(),
        )
    };
    vec![
        chunks(
            "typical-stream",
            &[
                chunk(json!({ "role": "assistant", "content": "" }), Value::Null),
                chunk(json!({ "content": "Hello" }), Value::Null),
                chunk(json!({ "content": " there" }), Value::Null),
                chunk(json!({}), json!("stop")),
                usage.clone(),
            ],
        ),
        chunks(
            "role-only",
            &[chunk(json!({ "role": "assistant" }), Value::Null)],
        ),
        chunks(
            "null-content",
            &[chunk(json!({ "content": null }), Value::Null)],
        ),
        chunks(
            "null-string-finish-reason",
            &[
                chunk(json!({}), json!("null")),
                chunk(json!({ "content": "x" }), json!("null")),
            ],
        ),
        chunks("empty-finish-reason", &[chunk(json!({}), json!(""))]),
        chunks(
            "finish-reasons-not-strings",
            &[
                chunk(json!({}), json!(0)),
                chunk(json!({}), json!(false)),
                chunk(json!({}), json!({ "type": "stop" })),
            ],
        ),
        chunks(
            "content-not-strings",
            &[
                chunk(json!([]), Value::Null),
                chunk(json!({ "content": [] }), Value::Null),
                chunk(json!({ "content": 0 }), Value::Null),
                chunk(json!({ "content": { "a": 1 } }), Value::Null),
            ],
        ),
        chunks(
            "usage-only",
            &[
                usage.clone(),
                json!({ "usage": null }),
                json!({ "usage": {} }),
            ],
        ),
        chunks(
            "usage-with-choices",
            &[json!({
                "choices": [{ "index": 0, "delta": { "role": "assistant" } }],
                "usage": { "total_tokens": 1 }
            })],
        ),
        chunks(
            "multiple-choices",
            &[json!({
                "id": "c",
                "choices": [
                    { "index": 0, "delta": { "role": "assistant" }, "finish_reason": "null" },
                    { "index": 1, "delta": { "content": "b" }, "finish_reason": null },
                    "not a choice"
                ]
            })],
        ),
        chunks(
            "message-in-chunk",
            &[json!({ "choices": [{ "index": 0, "message": { "content": "x" } }] })],
        ),
        chunks(
            "logprobs-chunk",
            &[json!({
                "choices": [{
                    "index": 0,
                    "delta": { "content": "Hi" },
                    "logprobs": { "content": [{ "token": "Hi", "logprob": -1e-7, "bytes": [72, 105], "top_logprobs": [] }] },
                    "finish_reason": null
                }]
            })],
        ),
        chunks(
            "choices-not-a-list",
            &[
                json!({ "choices": { "0": { "delta": { "content": "x" } } } }),
                json!({ "choices": "x", "usage": null }),
            ],
        ),
        raw(
            "escaped-chunk",
            &[
                &r#"{"id":"c","choices":[{"index":0,"delta":{"content":"café 🚀 a/b"}}]}"#
                    .replace("content", &format!("c{}ntent", escaped('o')))
                    .replace('é', &escaped('é'))
                    .replace('🚀', &escaped('🚀'))
                    .replace('/', "\\/"),
            ],
        ),
        raw(
            "pretty-printed-chunk",
            &[
                "{\n  \"id\": \"c\",\n  \"choices\": [\n    {\n      \"index\": 0,\n      \"delta\": { \"content\": { \"a\" : 1 } },\n      \"finish_reason\": { \"type\" : \"stop\" }\n    }\n  ]\n}",
            ],
        ),
        raw(
            "not-json-chunks",
            &[
                "[DONE]",
                "",
                " ",
                "not json",
                ": ping",
                "data: [DONE]",
                "{",
                "null",
                "[]",
            ],
        ),
        raw(
            "sse-line-chunk",
            &[r#"data: {"choices":[{"delta":{"content":"x"}}]}"#],
        ),
        raw(
            "truncated-chunk",
            &[r#"{"choices":[{"delta":{"content":"x"}}]"#],
        )
        .known_difference("gjson reads what it can from malformed JSON"),
        raw(
            "duplicate-keys-chunk",
            &[r#"{"choices":[{"delta":{"content":"x","content":""}}]}"#],
        )
        .known_difference("gjson reads the first duplicate key; serde_json keeps the last"),
    ]
}
