// Ported from CLIProxyAPI
// internal/translator/gemini/openai/chat-completions/gemini_openai_response_test.go
// and the response tests in noop_optimization_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// All tests are ported. The tests after them are new; their expected output
// comes from upstream, with the time and counter in tool call IDs masked.

use serde_json::{Value, json};

use super::*;

/// Replaces the time and counter in generated tool call IDs with `ID`.
fn mask_ids(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if key == "id"
                    && let Value::String(id) = field
                {
                    let mut pieces = id.rsplitn(3, '-');
                    if let (Some(counter), Some(nanos), Some(name)) =
                        (pieces.next(), pieces.next(), pieces.next())
                        && nanos.len() >= 10
                        && nanos.bytes().all(|byte| byte.is_ascii_digit())
                        && !counter.is_empty()
                        && counter.bytes().all(|byte| byte.is_ascii_digit())
                    {
                        *id = format!("{name}-ID");
                    }
                } else {
                    mask_ids(field);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_ids),
        _ => {}
    }
}

/// Translates one chunk and returns the chunks to send as a compact JSON
/// array, with tool call IDs masked.
fn translate(stream: &mut GeminiToOpenAIStream, chunk: &str) -> String {
    let mut chunks = Value::Array(stream.translate(chunk.as_bytes()));
    mask_ids(&mut chunks);
    chunks.to_string()
}

/// Translates a whole response, as compact JSON with tool call IDs masked.
fn non_stream(original_request: &str, body: &str) -> String {
    let original_request: Value = serde_json::from_str(original_request).unwrap();
    let body: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let mut response = convert_gemini_response_to_openai_non_stream(&original_request, &body);
    mask_ids(&mut response);
    response.to_string()
}

fn non_stream_value(body: &str) -> Value {
    convert_gemini_response_to_openai_non_stream(&Value::Null, &serde_json::from_str(body).unwrap())
}

#[test]
fn completion_tokens_include_thoughts() {
    let mut stream = GeminiToOpenAIStream::new(&Value::Null);
    let result = stream.translate(
        br#"{"usageMetadata":{"promptTokenCount":16,"candidatesTokenCount":5,"thoughtsTokenCount":42,"totalTokenCount":63}}"#,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0]["usage"]["completion_tokens"], json!(47));
}

#[test]
fn non_stream_completion_tokens_include_thoughts() {
    let result = non_stream_value(
        r#"{"usageMetadata":{"promptTokenCount":16,"thoughtsTokenCount":42,"totalTokenCount":58}}"#,
    );
    assert_eq!(result["usage"]["completion_tokens"], json!(42));
}

#[test]
fn finish_reason_only_on_final_chunk() {
    let mut stream = GeminiToOpenAIStream::new(&Value::Null);
    let first = stream.translate(
        br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"list_dir","args":{"path":"C:/"}}}]}}],"usageMetadata":{"trafficType":"ON_DEMAND"}}"#,
    );
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["choices"][0]["finish_reason"], Value::Null);

    stream.translate(
        br#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"list_dir","args":{"path":"D:/"}}}]}}],"usageMetadata":{"trafficType":"ON_DEMAND"}}"#,
    );

    let last = stream.translate(
        br#"{"candidates":[{"content":{"parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
    );
    assert_eq!(last.len(), 1);
    assert_eq!(last[0]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(last[0]["choices"][0]["native_finish_reason"], "stop");
}

#[test]
fn non_stream_empty_text_produces_empty_string() {
    let result = non_stream_value(
        r#"{"candidates":[{"content":{"parts":[{"text":""},{"text":"","thought":true}]},"finishReason":"STOP"}]}"#,
    );
    let message = &result["choices"][0]["message"];
    assert_eq!(message["content"], "");
    assert_eq!(message["reasoning_content"], "");
}

#[test]
fn non_stream_audio_transcription_part_produces_content() {
    let result = non_stream_value(
        r#"{"candidates":[{"content":{"parts":[{"text":""},{"audioTranscription":{"text":"Hello world"}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":185,"totalTokenCount":185}}"#,
    );
    assert_eq!(result["choices"][0]["message"]["content"], "Hello world");
}

#[test]
fn non_stream_single_audio_transcription_part() {
    let result = non_stream_value(
        r#"{"candidates":[{"content":{"parts":[{"audioTranscription":{"text":"Single transcription part"}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":185,"totalTokenCount":185}}"#,
    );
    assert_eq!(
        result["choices"][0]["message"]["content"],
        "Single transcription part"
    );
}

#[test]
fn audio_transcription_part_streams_content() {
    let mut stream = GeminiToOpenAIStream::new(&Value::Null);
    let result = stream.translate(
        br#"{"candidates":[{"content":{"parts":[{"text":""},{"audioTranscription":{"text":"Testing one two three."}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":185,"candidatesTokenCount":8,"totalTokenCount":193}}"#,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0]["choices"][0]["delta"]["content"],
        "Testing one two three."
    );
}

#[test]
fn non_stream_text_precedence_over_audio_transcription() {
    let result = non_stream_value(
        r#"{"candidates":[{"content":{"parts":[{"text":"explicit text","audioTranscription":{"text":"ignored transcription"}}]},"finishReason":"STOP"}]}"#,
    );
    assert_eq!(result["choices"][0]["message"]["content"], "explicit text");
}

#[test]
fn non_stream_keeps_assistant_role() {
    let result = non_stream_value(
        r#"{"candidates":[{"index":0,"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}]}"#,
    );
    assert_eq!(result["choices"][0]["message"]["role"], "assistant");
}

#[test]
fn streaming_sets_assistant_role_once() {
    let mut stream = GeminiToOpenAIStream::new(&Value::Null);
    let outputs = stream.translate(
        br#"{"candidates":[{"index":0,"content":{"parts":[{"text":"hello"},{"functionCall":{"name":"lookup","args":{}}},{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}}]}"#,
    );
    assert_eq!(outputs.len(), 1);
    let delta = &outputs[0]["choices"][0]["delta"];
    assert_eq!(delta["role"], "assistant");
    assert_eq!(delta["content"], "hello");
    assert!(delta["tool_calls"].get(0).is_some(), "{delta}");
    assert!(delta["images"].get(0).is_some(), "{delta}");
}

#[test]
fn tool_call_ids_are_unique() {
    let first = function_call_id("f");
    let second = function_call_id("f");
    assert_ne!(first, second);
    let mut masked = json!({"id": first});
    mask_ids(&mut masked);
    assert_eq!(masked, json!({"id": "f-ID"}));
}

#[test]
fn create_time_parses_as_go_does() {
    for (text, want) in [
        ("2024-05-06T07:08:09.123456789Z", Some(1_714_979_289)),
        ("2024-02-29T23:59:59.5-01:00", Some(1_709_254_799)),
        ("0001-01-01T00:00:00Z", Some(-62_135_596_800)),
        ("2024-01-01T00:00:00,5+24:00", Some(1_703_980_800)),
        ("2024-01-01T00:00:00+24:01", Some(1_703_980_740)),
        ("9999-12-31T23:59:59.999999999999Z", Some(253_402_300_799)),
        ("2024-06-30T12:00:00-00:30", Some(1_719_750_600)),
        ("bad", None),
        ("5", None),
        ("2024-02-29T23:59:60+05:30", None),
        ("2023-02-29T00:00:00Z", None),
        ("2024-1-01T00:00:00Z", None),
        ("2024-12-31T24:00:00Z", None),
        ("2024-06-30T12:00:00z", None),
        ("2024-06-30T12:00:00.Z", None),
    ] {
        assert_eq!(unix_seconds(text), want, "{text}");
    }
}

#[test]
fn stream_in_detail() {
    let mut stream = GeminiToOpenAIStream::new(
        &serde_json::from_str(r#"{"tools":[{"name":"a b"},{"name":" c.d "}]}"#).unwrap(),
    );
    for (chunk, expected) in [
        (
            concat!(
                r#"data: {"responseId":"r1","modelVersion":"gm","createTime":"2024-05-06T07:08:09.123456789Z","#,
                r#""candidates":[{"content":{"parts":[{"text":"think","thought":true},{"text":"hi"},"#,
                r#"{"functionCall":{"name":"a_b","args":{"x":1}}},{"functionCall":{"name":"c_d"}},"#,
                r#"{"inlineData":{"mimeType":"image/jpeg","data":"QQ"}},{"inline_data":{"mime_type":"","#,
                r#""data":"Qg"}},{"inlineData":{"data":""}},{"thoughtSignature":"sig"}]}}]}"#
            ),
            concat!(
                r#"[{"id":"r1","object":"chat.completion.chunk","created":1714979289,"model":"gm","#,
                r#""choices":[{"index":0,"delta":{"role":"assistant","content":"hi","reasoning_content":"think","#,
                r#""tool_calls":[{"id":"a b-ID","index":0,"type":"function","function":{"name":"a b","#,
                r#""arguments":"{\"x\":1}"}},{"id":"c_d-ID","index":1,"type":"function","function":{"name":"c_d","#,
                r#""arguments":""}}],"images":[{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,"#,
                r#"QQ"},"index":0},{"type":"image_url","image_url":{"url":"data:image/png;base64,"#,
                r#"Qg"},"index":1}]},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            concat!(
                r#"{"candidates":[{"index":1,"content":{"parts":[{"functionCall":{"name":"e","args":"str"}}]},"#,
                r#""finishReason":"max_tokens"}],"usageMetadata":{"promptTokenCount":3}}"#
            ),
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1714979289,"model":"model","#,
                r#""choices":[{"index":1,"delta":{"role":"assistant","content":null,"reasoning_content":null,"#,
                r#""tool_calls":[{"id":"e-ID","index":0,"type":"function","function":{"name":"e","#,
                r#""arguments":"\"str\""}}]},"finish_reason":"tool_calls","native_finish_reason":"max_tokens"}],"#,
                r#""usage":{"completion_tokens":0,"prompt_tokens":3}}]"#
            ),
        ),
        (
            concat!(
                r#"{"createTime":"bad","candidates":[{"content":{"parts":[{"audioTranscription":{"text":"t"}}]},"#,
                r#""finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"#,
                r#""thoughtsTokenCount":2,"totalTokenCount":17,"cachedContentTokenCount":4}}"#
            ),
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1714979289,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":"assistant","content":"t","reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":"tool_calls","native_finish_reason":"stop"}],"#,
                r#""usage":{"completion_tokens":7,"total_tokens":17,"prompt_tokens":10,"completion_tokens_details":{"reasoning_tokens":2},"#,
                r#""prompt_tokens_details":{"cached_tokens":4}}}]"#
            ),
        ),
        (r#"[DONE]"#, r#"[]"#),
        (r#"data: [DONE]"#, r#"[]"#),
        (r#"not json"#, r#"[]"#),
        (
            r#"{"usageMetadata":{"promptTokenCount":1}}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1714979289,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}],"usage":{"completion_tokens":0,"#,
                r#""prompt_tokens":1}}]"#
            ),
        ),
        (r#"{"candidates":"x"}"#, r#"[]"#),
        (r#"{"candidates":[]}"#, r#"[]"#),
    ] {
        assert_eq!(translate(&mut stream, chunk), expected, "{chunk}");
    }
}

#[test]
fn stream_create_time_and_finish_reasons_in_detail() {
    let mut stream = GeminiToOpenAIStream::new(&serde_json::from_str(r#"{}"#).unwrap());
    for (chunk, expected) in [
        (
            concat!(
                r#"{"createTime":"2024-02-29T23:59:60+05:30","candidates":[{"index":2,"content":{"parts":[{"text":"x","#,
                r#""thought":"yes"},{"text":5},{"functionCall":{"name":"f","args":[1,2]}},{"functionCall":{"name":"f"}}]},"#,
                r#""finishReason":"SAFETY"}]}"#
            ),
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":2,"#,
                r#""delta":{"role":"assistant","content":"5","reasoning_content":null,"tool_calls":[{"id":"f-ID","#,
                r#""index":0,"type":"function","function":{"name":"f","arguments":"[1,2]"}},{"id":"f-ID","#,
                r#""index":1,"type":"function","function":{"name":"f","arguments":""}}]},"finish_reason":null,"#,
                r#""native_finish_reason":null}]}]"#
            ),
        ),
        (
            concat!(
                r#"{"createTime":"2024-02-29T23:59:59.5-01:00","candidates":[{"index":2,"content":{"parts":[{"text":"y"}]},"#,
                r#""finishReason":"MAX_TOKENS"},{"index":3,"finishReason":"MAX_TOKENS"},{"index":4,"#,
                r#""finishReason":"stop"},{"index":5}],"usageMetadata":{"candidatesTokenCount":1}}"#
            ),
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1709254799,"model":"model","#,
                r#""choices":[{"index":2,"delta":{"role":"assistant","content":"y","reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":"tool_calls","native_finish_reason":"max_tokens"}],"#,
                r#""usage":{"completion_tokens":1,"prompt_tokens":0}},{"id":"","object":"chat.completion.chunk","#,
                r#""created":1709254799,"model":"model","choices":[{"index":3,"delta":{"role":null,"#,
                r#""content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"max_tokens","#,
                r#""native_finish_reason":"max_tokens"}],"usage":{"completion_tokens":1,"prompt_tokens":0}},"#,
                r#"{"id":"","object":"chat.completion.chunk","created":1709254799,"model":"model","#,
                r#""choices":[{"index":4,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":"stop","native_finish_reason":"stop"}],"usage":{"completion_tokens":1,"#,
                r#""prompt_tokens":0}},{"id":"","object":"chat.completion.chunk","created":1709254799,"#,
                r#""model":"model","choices":[{"index":5,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}],"usage":{"completion_tokens":1,"#,
                r#""prompt_tokens":0}}]"#
            ),
        ),
        (
            r#"{"createTime":"2023-02-29T00:00:00Z","candidates":[{"content":{}}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1709254799,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":"0001-01-01T00:00:00Z","candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":-62135596800,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":"2024-01-01T00:00:00,5+24:00","candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1703980800,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":"2024-01-01T00:00:00+24:01","candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1703980740,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":"2024-1-01T00:00:00Z","candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1703980740,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":"2024-12-31T24:00:00Z","candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1703980740,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":"9999-12-31T23:59:59.999999999999Z","candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":253402300799,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":"2024-06-30T12:00:00z","candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":253402300799,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":"2024-06-30T12:00:00.Z","candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":253402300799,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":"2024-06-30T12:00:00-00:30","candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1719750600,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
        (
            r#"{"createTime":5,"candidates":[{}]}"#,
            concat!(
                r#"[{"id":"","object":"chat.completion.chunk","created":1719750600,"model":"model","#,
                r#""choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"#,
                r#""tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}]"#
            ),
        ),
    ] {
        assert_eq!(translate(&mut stream, chunk), expected, "{chunk}");
    }
}

#[test]
fn non_stream_in_detail() {
    assert_eq!(
        non_stream(
            r#"{"tools":[{"name":"a b"}]}"#,
            concat!(
                r#"{"responseId":"r","modelVersion":"v","createTime":"2024-05-06T07:08:09Z","candidates":[{"index":0,"#,
                r#""content":{"parts":[{"text":"t1","thought":true},{"text":"a"},{"text":"b"},{"functionCall":{"name":"a_b","#,
                r#""args":{"k":"v"}}},{"inlineData":{"mimeType":"image/webp","data":"QQ"}},{"inlineData":{"data":"Qg"}},"#,
                r#"{"text":"c","thought":true}]},"finishReason":"STOP"},{"index":1,"content":{"parts":[{"audioTranscription":{"text":"x"}},"#,
                r#"{"text":""}]},"finishReason":"OTHER"}],"usageMetadata":{"promptTokenCount":1,"#,
                r#""candidatesTokenCount":2,"totalTokenCount":3,"thoughtsTokenCount":0,"cachedContentTokenCount":1}}"#
            )
        ),
        concat!(
            r#"{"id":"r","object":"chat.completion","created":1714979289,"model":"v","choices":[{"index":0,"#,
            r#""message":{"role":"assistant","content":"ab","reasoning_content":"t1c","tool_calls":[{"id":"a b-ID","#,
            r#""type":"function","function":{"name":"a b","arguments":"{\"k\":\"v\"}"}}],"images":[{"type":"image_url","#,
            r#""image_url":{"url":"data:image/webp;base64,QQ"},"index":0},{"type":"image_url","#,
            r#""image_url":{"url":"data:image/png;base64,Qg"},"index":1}]},"finish_reason":"tool_calls","#,
            r#""native_finish_reason":"tool_calls"},{"index":1,"message":{"role":"assistant","#,
            r#""content":"x","reasoning_content":null,"tool_calls":null},"finish_reason":"other","#,
            r#""native_finish_reason":"other"}],"usage":{"completion_tokens":2,"total_tokens":3,"#,
            r#""prompt_tokens":1,"prompt_tokens_details":{"cached_tokens":1}}}"#
        )
    );
}

#[test]
fn non_stream_invalid_json_gives_the_template() {
    assert_eq!(
        non_stream(r#"{}"#, r#"not json"#),
        r#"{"id":"","object":"chat.completion","created":0,"model":"model","choices":[]}"#
    );
}

#[test]
fn non_stream_non_array_candidates() {
    assert_eq!(
        non_stream(r#"{}"#, r#"{"candidates":"x","createTime":"bad"}"#),
        r#"{"id":"","object":"chat.completion","created":0,"model":"model","choices":[]}"#
    );
}

#[test]
fn non_stream_tool_call_without_finish_reason() {
    assert_eq!(
        non_stream(
            r#"{}"#,
            concat!(
                r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f"}},{"inline_data":{"mime_type":"a/b","#,
                r#""data":"QQ"}},{"text":"z","audioTranscription":{"text":"w"}}]}}]}"#
            )
        ),
        concat!(
            r#"{"id":"","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"#,
            r#""message":{"role":"assistant","content":"z","reasoning_content":null,"tool_calls":[{"id":"f-ID","#,
            r#""type":"function","function":{"name":"f","arguments":""}}],"images":[{"type":"image_url","#,
            r#""image_url":{"url":"data:a/b;base64,QQ"},"index":0}]},"finish_reason":"tool_calls","#,
            r#""native_finish_reason":"tool_calls"}]}"#
        )
    );
}

#[test]
fn non_stream_odd_index_parts_and_usage() {
    assert_eq!(
        non_stream(
            r#"{}"#,
            r#"{"candidates":[{"index":"7","content":{"parts":"x"}}],"usageMetadata":{"promptTokenCount":"5"}}"#
        ),
        concat!(
            r#"{"id":"","object":"chat.completion","created":0,"model":"model","choices":[{"index":7,"#,
            r#""message":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":null},"#,
            r#""finish_reason":null,"native_finish_reason":null}],"usage":{"completion_tokens":0,"#,
            r#""prompt_tokens":5}}"#
        )
    );
}
