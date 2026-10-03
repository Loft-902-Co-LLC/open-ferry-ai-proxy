use serde_json::{Value, json};

use super::*;

/// What a request with no fields becomes.
const EMPTY_REQUEST: &str =
    r#"{"model":"","messages":[{"role":"user","content":"Complete this:"}]}"#;

/// What a response with no fields becomes.
const EMPTY_RESPONSE: &str =
    r#"{"id":"","object":"text_completion","created":0,"model":"","choices":[]}"#;

/// Reads `text` as JSON, keeping its number text.
fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// The translated request for the request `text`, as JSON text.
fn request(text: &str) -> String {
    convert_completions_request_to_chat_completions(&parse(text)).to_string()
}

/// The translated response for the body `text`, as JSON text.
fn response(text: &str) -> String {
    convert_chat_completions_response_to_completions(text.as_bytes()).to_string()
}

/// The translated chunk for the chunk `text`, as JSON text, or `None` if it
/// is skipped.
fn chunk(text: &str) -> Option<String> {
    convert_chat_completions_stream_chunk_to_completions(text.as_bytes())
        .map(|chunk| chunk.to_string())
}

/// The first choice of a translated response or chunk.
fn first_choice(output: &str) -> Value {
    parse(output)["choices"][0].clone()
}

/// The JSON text written for `key` in the translated request for `text`.
fn request_field(text: &str, key: &str) -> Option<String> {
    let out = parse(&request(text));
    out.get(key).map(Value::to_string)
}

#[test]
fn prompt_becomes_the_user_message() {
    // Shared parameters come out in upstream's order, whatever the client's;
    // the rest, such as `n` and `suffix`, are dropped.
    let out = request(
        r#"{"echo":false,"prompt":"Say hi","model":"gpt-3.5-turbo-instruct","n":2,
            "suffix":"!","max_tokens":16,"stop":["\n"],"temperature":0.5,"top_p":1,
            "frequency_penalty":0,"presence_penalty":-0.5,"stream":true,"logprobs":2,
            "top_logprobs":3}"#,
    );
    assert_eq!(
        out,
        concat!(
            r#"{"model":"gpt-3.5-turbo-instruct","#,
            r#""messages":[{"role":"user","content":"Say hi"}],"#,
            r#""max_tokens":16,"temperature":0.5,"top_p":1,"frequency_penalty":0,"#,
            r#""presence_penalty":-0.5,"stop":["\n"],"stream":true,"logprobs":true,"#,
            r#""top_logprobs":3,"echo":false}"#,
        )
    );
}

#[test]
fn an_empty_or_missing_prompt_asks_to_complete() {
    for text in ["{}", r#"{"prompt":""}"#, r#"{"prompt":null}"#] {
        assert_eq!(request(text), EMPTY_REQUEST, "{text}");
    }
}

#[test]
fn a_request_that_is_not_an_object_has_no_fields() {
    for text in [
        "[]",
        r#"[{"prompt":"x"}]"#,
        r#""prompt""#,
        "5",
        "null",
        "true",
    ] {
        assert_eq!(request(text), EMPTY_REQUEST, "{text}");
    }
}

#[test]
fn a_prompt_or_model_that_is_not_a_string_is_read_as_text() {
    let content = |prompt: &str| {
        let out = parse(&request(&format!(r#"{{"prompt":{prompt}}}"#)));
        out["messages"][0]["content"].as_str().unwrap().to_owned()
    };
    // A batch of prompts isn't split up: the array is the text.
    assert_eq!(content(r#"["a", "b"]"#), r#"["a","b"]"#);
    assert_eq!(content(r#"{"x": 1}"#), r#"{"x":1}"#);
    assert_eq!(content("42"), "42");
    assert_eq!(content("1.50"), "1.5");
    assert_eq!(content("1e3"), "1000");
    assert_eq!(content("true"), "true");
    assert_eq!(content("false"), "false");

    assert_eq!(
        request_field(r#"{"model":7,"prompt":"x"}"#, "model").unwrap(),
        r#""7""#
    );
    assert_eq!(
        request_field(r#"{"model":null,"prompt":"x"}"#, "model").unwrap(),
        r#""""#
    );
}

#[test]
fn integer_parameters_are_coerced_as_gjson_does() {
    let max_tokens =
        |value: &str| request_field(&format!(r#"{{"max_tokens":{value}}}"#), "max_tokens").unwrap();
    assert_eq!(max_tokens("16"), "16");
    assert_eq!(max_tokens("16.9"), "16");
    assert_eq!(max_tokens("-3"), "-3");
    assert_eq!(max_tokens(r#""16""#), "16");
    assert_eq!(max_tokens(r#""16.5""#), "0");
    assert_eq!(max_tokens(r#""+16""#), "0");
    assert_eq!(max_tokens("true"), "1");
    assert_eq!(max_tokens("false"), "0");
    assert_eq!(max_tokens("null"), "0");
    assert_eq!(max_tokens("[16]"), "0");
    assert_eq!(max_tokens("1e30"), i64::MAX.to_string());
    assert_eq!(max_tokens("-1e30"), i64::MIN.to_string());
    assert_eq!(
        request_field(r#"{"top_logprobs":"5"}"#, "top_logprobs").unwrap(),
        "5"
    );
}

#[test]
fn float_parameters_are_coerced_as_gjson_does() {
    let temperature = |value: &str| {
        request_field(&format!(r#"{{"temperature":{value}}}"#), "temperature").unwrap()
    };
    assert_eq!(temperature("0.7"), "0.7");
    assert_eq!(temperature("1.50"), "1.5");
    assert_eq!(temperature("2"), "2");
    // sjson writes floats without an exponent.
    assert_eq!(temperature("1e21"), "1000000000000000000000");
    assert_eq!(temperature("1e-7"), "0.0000001");
    assert_eq!(temperature(r#""0.25""#), "0.25");
    // Go reads hexadecimal floats and digit separators.
    assert_eq!(temperature(r#""0x1p-1""#), "0.5");
    assert_eq!(temperature(r#""0x1_0p0""#), "16");
    assert_eq!(temperature(r#""1_000.5""#), "1000.5");
    assert_eq!(temperature(r#""1__0""#), "0");
    assert_eq!(temperature(r#"" 1""#), "0");
    assert_eq!(temperature(r#""abc""#), "0");
    assert_eq!(temperature("true"), "1");
    assert_eq!(temperature("false"), "0");
    assert_eq!(temperature("null"), "0");
    assert_eq!(temperature("{}"), "0");
    // Go writes -0.
    assert_eq!(temperature("-0.0"), "0");
}

#[test]
fn float_parameters_that_are_not_finite_are_left_out() {
    let out = request(
        r#"{"temperature":1e400,"top_p":"NaN","frequency_penalty":"-Inf",
            "presence_penalty":"1e999","max_tokens":1}"#,
    );
    assert_eq!(
        out,
        r#"{"model":"","messages":[{"role":"user","content":"Complete this:"}],"max_tokens":1}"#
    );
}

#[test]
fn stop_is_copied_as_sent() {
    let stop = |value: &str| request_field(&format!(r#"{{"stop":{value}}}"#), "stop").unwrap();
    assert_eq!(stop(r#""\n\n""#), r#""\n\n""#);
    assert_eq!(stop("null"), "null");
    assert_eq!(stop("1.50"), "1.50");
    assert_eq!(stop(r#"[1, {"a": "b"}]"#), r#"[1,{"a":"b"}]"#);
}

#[test]
fn boolean_parameters_are_coerced_as_gjson_does() {
    let flags = |value: &str| {
        let out = parse(&request(&format!(
            r#"{{"stream":{value},"logprobs":{value},"echo":{value}}}"#
        )));
        [&out["stream"], &out["logprobs"], &out["echo"]].map(|flag| flag.as_bool().unwrap())
    };
    for value in ["true", "1", "-2", "0.5", r#""TRUE""#, r#""t""#, r#""1""#] {
        assert_eq!(flags(value), [true; 3], "{value}");
    }
    for value in ["false", "0", "null", r#""yes""#, r#""""#, "[]", "{}"] {
        assert_eq!(flags(value), [false; 3], "{value}");
    }
}

#[test]
fn response_choices_become_text_completions() {
    let out = response(
        r#"{"id":"chatcmpl-1","object":"chat.completion","created":1700000000,
            "model":"gpt-4o","system_fingerprint":"fp",
            "choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},
                        "logprobs":null,"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#,
    );
    assert_eq!(
        out,
        concat!(
            r#"{"id":"chatcmpl-1","object":"text_completion","created":1700000000,"#,
            r#""model":"gpt-4o","choices":[{"finish_reason":"stop","index":0,"#,
            r#""logprobs":null,"text":"Hello"}],"#,
            r#""usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#,
        )
    );
}

#[test]
fn a_choice_keeps_only_the_fields_it_had() {
    assert_eq!(
        first_choice(&response(r#"{"choices":[{}]}"#)),
        json!({"index": 0})
    );
    // A message without content gives no text, and the delta isn't read
    // when there is a message, even a null one.
    for choice in [
        r#"{"message":{"role":"assistant"}}"#,
        r#"{"message":null,"delta":{"content":"x"}}"#,
        r#"{"message":"hi","delta":{"content":"x"}}"#,
    ] {
        let out = response(&format!(r#"{{"choices":[{choice}]}}"#));
        assert_eq!(first_choice(&out), json!({"index": 0}), "{choice}");
    }
    let out = response(r#"{"choices":[{"delta":{"content":"x"}}]}"#);
    assert_eq!(first_choice(&out), json!({"index": 0, "text": "x"}));
    let out = response(r#"{"choices":[{"message":{"content":null}}]}"#);
    assert_eq!(first_choice(&out), json!({"index": 0, "text": ""}));
    let out = response(r#"{"choices":[{"message":{"content":[{"type":"text"}]}}]}"#);
    assert_eq!(first_choice(&out)["text"], r#"[{"type":"text"}]"#);
}

#[test]
fn a_response_finish_reason_is_read_as_text() {
    let reason = |value: &str| {
        let out = response(&format!(r#"{{"choices":[{{"finish_reason":{value}}}]}}"#));
        first_choice(&out)["finish_reason"].clone()
    };
    assert_eq!(reason("null"), "");
    assert_eq!(reason(r#""null""#), "null");
    assert_eq!(reason(r#""length""#), "length");
    assert_eq!(reason("0"), "0");
    assert_eq!(reason("false"), "false");
}

#[test]
fn logprobs_are_written_as_go_marshals_them() {
    let out = response(
        r#"{"choices":[{"logprobs":{"z":1,"a":[1.50,1e21,1e-7,100],"m":{"y":"<b>","x":null}}}]}"#,
    );
    assert_eq!(
        first_choice(&out)["logprobs"].to_string(),
        r#"{"a":[1.5,1e+21,1e-7,100],"m":{"x":null,"y":"<b>"},"z":1}"#
    );
    let logprobs = |value: &str| {
        let out = response(&format!(r#"{{"choices":[{{"logprobs":{value}}}]}}"#));
        first_choice(&out)["logprobs"].to_string()
    };
    // Unlike sjson, json.Marshal writes large and small floats with exponents.
    assert_eq!(logprobs("0.5"), "0.5");
    assert_eq!(logprobs("1e21"), "1e+21");
    assert_eq!(logprobs("12345678901234567890"), "12345678901234567000");
    assert_eq!(logprobs(r#""x""#), r#""x""#);
    assert_eq!(logprobs("true"), "true");
    assert_eq!(logprobs("null"), "null");
}

#[test]
fn choices_that_are_not_an_array_leave_the_list_empty() {
    for choices in [
        "",
        r#","choices":null"#,
        r#","choices":{}"#,
        r#","choices":"x""#,
    ] {
        let out = response(&format!(r#"{{"id":"r"{choices}}}"#));
        assert_eq!(
            out, r#"{"id":"r","object":"text_completion","created":0,"model":"","choices":[]}"#,
            "{choices}"
        );
    }
    // Each entry of an array counts, even one that isn't an object.
    let out = parse(&response(r#"{"choices":[1,"s",null,[{"index":3}]]}"#));
    assert_eq!(out["choices"], Value::Array(vec![json!({"index": 0}); 4]));
}

#[test]
fn response_fields_are_coerced_as_gjson_does() {
    let out = parse(&response(
        r#"{"id":12,"created":"1700000000","model":{"name": "m"},
            "choices":[{"index":"2"},{"index":1.9},{"index":-1},{"index":1e30}]}"#,
    ));
    assert_eq!(out["id"], "12");
    assert_eq!(out["created"].to_string(), "1700000000");
    assert_eq!(out["model"], r#"{"name":"m"}"#);
    let indexes: Vec<String> = out["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|choice| choice["index"].to_string())
        .collect();
    let max = i64::MAX.to_string();
    assert_eq!(indexes, ["2", "1", "-1", max.as_str()]);
    let out = parse(&response(r#"{"created":null}"#));
    assert_eq!(out["created"].to_string(), "0");
}

#[test]
fn usage_comes_last_even_when_null() {
    assert_eq!(
        response(r#"{"usage":null,"id":"r"}"#),
        r#"{"id":"r","object":"text_completion","created":0,"model":"","choices":[],"usage":null}"#
    );
}

#[test]
fn a_body_that_is_not_a_json_object_has_no_fields() {
    for text in [
        "",
        "not json",
        "[DONE]",
        r#"{"id":"#,
        r#"data: {"id":"r"}"#,
        "[]",
        "1",
    ] {
        assert_eq!(response(text), EMPTY_RESPONSE, "{text}");
    }
}

#[test]
fn a_chunk_with_text_is_sent_on() {
    let out = chunk(
        r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,
            "model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hi"},
            "logprobs":null,"finish_reason":null}]}"#,
    );
    assert_eq!(
        out.unwrap(),
        concat!(
            r#"{"id":"chatcmpl-1","object":"text_completion","created":1700000000,"#,
            r#""model":"gpt-4o","choices":[{"finish_reason":"","index":0,"#,
            r#""logprobs":null,"text":"Hi"}]}"#,
        )
    );
}

#[test]
fn a_chunk_without_text_finish_reason_or_usage_is_skipped() {
    for text in [
        r#"{"choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
        r#"{"choices":[{"delta":{"content":null},"finish_reason":null}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"null"}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":""}]}"#,
        r#"{"choices":[{"message":{"content":"x"}}]}"#,
        r#"{"choices":[],"id":"c"}"#,
        r#"{"choices":{"0":{"delta":{"content":"x"}}}}"#,
        "{}",
        "[]",
        "",
        "[DONE]",
        r#"data: {"usage":{}}"#,
        r#"{"usage":{}"#,
    ] {
        assert_eq!(chunk(text), None, "{text}");
    }
}

#[test]
fn a_finish_reason_alone_sends_the_chunk() {
    let out = chunk(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#).unwrap();
    assert_eq!(
        first_choice(&out),
        json!({"finish_reason": "stop", "index": 0, "text": ""})
    );
    // Without a delta at all, too.
    let out = chunk(r#"{"choices":[{"finish_reason":"length"}]}"#).unwrap();
    assert_eq!(
        first_choice(&out),
        json!({"finish_reason": "length", "index": 0, "text": ""})
    );
    // A finish reason that isn't a string counts when its text isn't empty.
    let out = chunk(r#"{"choices":[{"finish_reason":0}]}"#).unwrap();
    assert_eq!(first_choice(&out)["finish_reason"], "0");
}

#[test]
fn usage_alone_sends_the_chunk() {
    let out = chunk(r#"{"id":"c","choices":[],"usage":{"total_tokens":4}}"#).unwrap();
    assert_eq!(
        out,
        concat!(
            r#"{"id":"c","object":"text_completion","created":0,"model":"","#,
            r#""choices":[],"usage":{"total_tokens":4}}"#,
        )
    );
    // A null usage counts as usage.
    let out = chunk(r#"{"usage":null}"#).unwrap();
    assert_eq!(parse(&out)["usage"], Value::Null);
    // Every choice is converted, with or without content.
    let out = chunk(r#"{"choices":[{"index":1,"delta":{"role":"assistant"}}],"usage":{}}"#);
    assert_eq!(first_choice(&out.unwrap()), json!({"index": 1, "text": ""}));
}

#[test]
fn every_choice_is_sent_once_one_has_content() {
    let out = chunk(
        r#"{"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":"null"},
                       {"index":1,"delta":{"content":"b"},"finish_reason":null},
                       "x"]}"#,
    )
    .unwrap();
    assert_eq!(
        parse(&out)["choices"],
        json!([
            {"index": 0, "text": ""},
            {"finish_reason": "", "index": 1, "text": "b"},
            {"index": 0, "text": ""}
        ])
    );
}

#[test]
fn chunk_content_that_is_not_a_string_is_read_as_text() {
    let text = |content: &str| {
        let out = chunk(&format!(
            r#"{{"choices":[{{"delta":{{"content":{content}}}}}]}}"#
        ));
        out.map(|out| first_choice(&out)["text"].clone())
    };
    assert_eq!(text("[]"), Some(json!("[]")));
    assert_eq!(text(r#"{"a": 1}"#), Some(json!(r#"{"a":1}"#)));
    assert_eq!(text("0"), Some(json!("0")));
    assert_eq!(text("false"), Some(json!("false")));
    assert_eq!(text("null"), None);
}

#[test]
fn a_chunk_keeps_logprobs_and_drops_a_null_string_finish_reason() {
    let out = chunk(
        r#"{"choices":[{"delta":{"content":"a"},"finish_reason":"null",
            "logprobs":{"content":[{"token":"a","logprob":-1e-7}]}}]}"#,
    )
    .unwrap();
    assert_eq!(
        first_choice(&out).to_string(),
        r#"{"index":0,"logprobs":{"content":[{"logprob":-1e-7,"token":"a"}]},"text":"a"}"#
    );
}
