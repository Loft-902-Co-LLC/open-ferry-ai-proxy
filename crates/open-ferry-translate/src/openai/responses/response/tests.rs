// Ported from CLIProxyAPI internal/translator/openai/openai/responses/openai_openai-responses_response_test.go
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

// All 44 tests are ported, with the response-side test of
// custom_tool_namespace_recovery_test.go and the one in
// responses_request_state_test.go. The full responses_compatibility_digest_test.go
// is in the request's tests, since it hashes requests as well.
// responses_perf_test.go holds only benchmarks and is not ported. Tests of
// our own, marked as such, check finalizing a stream that had no lines, and
// the translator side of v8.0.20's issue6381_responses_eof_test.go, whose
// tests drive the executor.
//
// Table-driven tests run their cases in a loop rather than as subtests.
// Where upstream passes no request, or one that isn't valid JSON, these pass
// `Null`, which is what `pickRequestJSON` skips. Where upstream asks the state
// left in its `*any` for `ToolInputError`, these ask the stream, or take the
// error the non-streaming translator returns with its body. A few checks look
// up the item they check by call ID where upstream takes the last one, which
// is the same item.

use serde_json::Value;

use super::super::tools::cap;
use super::*;

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("valid JSON")
}

/// A request as upstream's tests give it: `Null` for none, or for text that
/// isn't valid JSON.
fn request(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// `text` as a JSON string, as Go's `%q` writes these tests' strings.
fn quote(text: &str) -> String {
    Value::from(text).to_string()
}

/// Looks up a dotted path such as `response.output.0.name`, like a plain
/// gjson path.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(map) => map.get(key),
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => None,
    })
}

/// The value at `path` as gjson's `String()` gives it.
fn text_at(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// The value at `path` as gjson's `Int()` gives it.
fn int_at(value: &Value, path: &str) -> i64 {
    at(value, path).map_or(0, int_of)
}

/// `parseOpenAIResponsesSSEEvent`, for each event in `frames`.
fn events(frames: &str) -> Vec<(String, Value)> {
    frames
        .split_terminator("\n\n")
        .map(|frame| {
            let lines: Vec<&str> = frame.split('\n').collect();
            assert!(lines.len() >= 2, "unexpected SSE chunk: {frame:?}");
            let event = lines[0].strip_prefix("event:").unwrap_or(lines[0]).trim();
            let data = lines[1].strip_prefix("data:").unwrap_or(lines[1]).trim();
            let data = serde_json::from_str(data)
                .unwrap_or_else(|_| panic!("invalid SSE data JSON: {data:?}"));
            (event.to_owned(), data)
        })
        .collect()
}

/// One stream, fed a line at a time as upstream's tests feed theirs.
struct Feed(OpenAIToOpenAIResponsesStream);

impl Feed {
    fn new(model: &str, original_request: &str, request_text: &str) -> Self {
        Self(OpenAIToOpenAIResponsesStream::new(
            model,
            &request(original_request),
            &request(request_text),
        ))
    }

    fn line(&mut self, line: &str) -> Vec<(String, Value)> {
        events(&self.0.translate_line(line.as_bytes()))
    }

    fn lines(&mut self, lines: &[&str]) -> Vec<(String, Value)> {
        lines.iter().flat_map(|line| self.line(line)).collect()
    }
}

/// `ConvertOpenAIChatCompletionsResponseToOpenAIResponsesNonStream`, for
/// requests given as text.
fn convert(original_request: &str, request_text: &str, raw: &str) -> Value {
    convert_openai_chat_completions_response_to_openai_responses_non_stream(
        &request(original_request),
        &request(request_text),
        raw.as_bytes(),
    )
}

/// The data of the last `event`, or `Null`.
fn last<'o>(out: &'o [(String, Value)], event: &str) -> &'o Value {
    out.iter()
        .rev()
        .find(|(name, _)| name == event)
        .map_or(&Value::Null, |(_, data)| data)
}

/// The data of the last `event` whose item has type `kind`, or `Null`.
fn last_of<'o>(out: &'o [(String, Value)], event: &str, kind: &str) -> &'o Value {
    out.iter()
        .rev()
        .find(|(name, data)| name == event && text_at(data, "item.type") == kind)
        .map_or(&Value::Null, |(_, data)| data)
}

/// How many `event`s there are.
fn count(out: &[(String, Value)], event: &str) -> usize {
    out.iter().filter(|(name, _)| name == event).count()
}

/// The items of type `kind` in `data`'s `response.output`.
fn output_items<'d>(data: &'d Value, kind: &'d str) -> impl Iterator<Item = &'d Value> {
    at(data, "response.output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |item| text_at(item, "type") == kind)
}

#[test]
fn response_completed_waits_for_done() {
    let request = r#"{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true}"#;

    struct Case {
        name: &'static str,
        lines: &'static [&'static str],
        /// The index of the `[DONE]` line, where `response.completed` must
        /// come.
        done_index: usize,
        /// Input, output and total tokens.
        usage: Option<(i64, i64, i64)>,
    }
    let cases = [
        // A provider may send finish_reason first and only attach usage in a
        // later chunk (e.g. Vertex AI), so response.completed must wait for
        // [DONE] to include that usage.
        Case {
            name: "late usage after finish reason",
            lines: &[
                r#"data: {"id":"resp_late_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_late_usage","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
                r#"data: {"id":"resp_late_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                r#"data: {"id":"resp_late_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#,
                "data: [DONE]",
            ],
            done_index: 3,
            usage: Some((11, 7, 18)),
        },
        // Usage on the finish_reason chunk still gives a single
        // response.completed, deferred until [DONE].
        Case {
            name: "usage on finish reason chunk",
            lines: &[
                r#"data: {"id":"resp_usage_same_chunk","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_usage_same_chunk","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
                r#"data: {"id":"resp_usage_same_chunk","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":13,"completion_tokens":5,"total_tokens":18}}"#,
                "data: [DONE]",
            ],
            done_index: 2,
            usage: Some((13, 5, 18)),
        },
        Case {
            name: "no finish reason",
            lines: &[
                r#"data: {"id":"resp_no_finish_reason","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"}}]}"#,
                "data: [DONE]",
            ],
            done_index: 1,
            usage: None,
        },
        // A buggy server might never send usage: response.completed still
        // waits for [DONE] but leaves usage out.
        Case {
            name: "no usage chunk",
            lines: &[
                r#"data: {"id":"resp_no_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_no_usage","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
                r#"data: {"id":"resp_no_usage","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                "data: [DONE]",
            ],
            done_index: 2,
            usage: None,
        },
    ];

    for case in cases {
        let name = case.name;
        let mut feed = Feed::new("model", request, request);
        let mut completed_count = 0;
        let mut completed_index = None;
        let (mut created, mut in_progress, mut completed) = (Value::Null, Value::Null, Value::Null);
        for (i, line) in case.lines.iter().enumerate() {
            for (event, data) in feed.line(line) {
                match event.as_str() {
                    "response.created" => created = data,
                    "response.in_progress" => in_progress = data,
                    "response.completed" => {
                        completed_count += 1;
                        completed_index = Some(i);
                        completed = data;
                        assert!(
                            i >= case.done_index,
                            "{name}: unexpected early response.completed on input index {i}"
                        );
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(completed_count, 1, "{name}: response.completed events");
        assert_eq!(completed_index, Some(case.done_index), "{name}");
        assert_eq!(text_at(&created, "response.model"), "gpt-5.4", "{name}");
        assert_eq!(text_at(&in_progress, "response.model"), "gpt-5.4", "{name}");

        match case.usage {
            None => assert!(
                at(&completed, "response.usage").is_none(),
                "{name}: expected no usage: {completed}"
            ),
            Some((input, output, total)) => {
                assert_eq!(
                    int_at(&completed, "response.usage.input_tokens"),
                    input,
                    "{name}"
                );
                assert_eq!(
                    int_at(&completed, "response.usage.output_tokens"),
                    output,
                    "{name}"
                );
                assert_eq!(
                    int_at(&completed, "response.usage.total_tokens"),
                    total,
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn finalizes_open_message_at_stream_end() {
    let request = r#"{"model":"gpt-5.4"}"#;
    let cases = [
        (
            "missing finish reason",
            r#"data: {"id":"resp_missing_finish_reason","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"}}]}"#,
        ),
        (
            "null finish reason",
            r#"data: {"id":"resp_null_finish_reason","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}"#,
        ),
    ];

    for (name, chunk) in cases {
        let out = Feed::new("model", request, request).lines(&[chunk, "data: [DONE]"]);
        let names: Vec<&str> = out.iter().map(|(event, _)| event.as_str()).collect();
        assert_eq!(
            names,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ],
            "{name}"
        );
        assert_eq!(
            text_at(last(&out, "response.output_text.done"), "text"),
            "hello",
            "{name}"
        );
        assert_eq!(
            text_at(last(&out, "response.content_part.done"), "part.text"),
            "hello",
            "{name}"
        );
        assert_eq!(
            text_at(
                last(&out, "response.output_item.done"),
                "item.content.0.text"
            ),
            "hello",
            "{name}"
        );
        assert_eq!(
            text_at(last(&out, "response.completed"), "response.status"),
            "completed",
            "{name}"
        );
    }
}

#[test]
fn multiple_tool_calls_remain_separate() {
    let lines = [
        r#"data: {"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_read","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\",\"limit\":400,\"offset\":1}"}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":1,"id":"call_glob","type":"function","function":{"name":"glob","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.{yml,yaml}\"}"}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_test","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}}"#,
        "data: [DONE]",
    ];
    let request = r#"{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true}"#;
    let out = Feed::new("model", request, request).lines(&lines);

    let mut added_names = HashMap::new();
    let mut done_args = HashMap::new();
    let mut done_names = HashMap::new();
    let mut items = HashMap::new();
    for (event, data) in &out {
        match event.as_str() {
            "response.output_item.added" if text_at(data, "item.type") == "function_call" => {
                added_names.insert(text_at(data, "item.call_id"), text_at(data, "item.name"));
            }
            "response.output_item.done" if text_at(data, "item.type") == "function_call" => {
                let call_id = text_at(data, "item.call_id");
                done_args.insert(call_id.clone(), text_at(data, "item.arguments"));
                done_names.insert(call_id, text_at(data, "item.name"));
            }
            "response.completed" => {
                for item in output_items(data, "function_call") {
                    items.insert(text_at(item, "call_id"), item.clone());
                }
            }
            _ => {}
        }
    }

    assert_eq!(added_names.len(), 2, "function_call added events");
    assert_eq!(done_args.len(), 2, "function_call done events");
    assert_eq!(added_names["call_read"], "read");
    assert_eq!(added_names["call_glob"], "glob");
    for call_id in ["call_read", "call_glob"] {
        let args = &done_args[call_id];
        assert!(
            raw::valid(args),
            "invalid JSON args for {call_id}: {args:?}"
        );
        assert!(
            !args.contains("}{"),
            "{call_id} args were concatenated: {args:?}"
        );
    }
    assert_eq!(done_names["call_read"], "read");
    assert_eq!(done_names["call_glob"], "glob");
    assert_eq!(
        text_at(&parse(&done_args["call_read"]), "filePath"),
        r"C:\repo"
    );
    assert_eq!(text_at(&parse(&done_args["call_glob"]), "path"), r"C:\repo");
    assert_eq!(
        text_at(&parse(&done_args["call_glob"]), "pattern"),
        "*.{yml,yaml}"
    );

    assert_eq!(items.len(), 2, "function_call items in response.output");
    assert_eq!(text_at(&items["call_read"], "name"), "read");
    assert_eq!(text_at(&items["call_glob"], "name"), "glob");
}

#[test]
fn multi_choice_tool_calls_use_distinct_output_indexes() {
    let lines = [
        r#"data: {"id":"resp_multi_choice","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_choice0","type":"function","function":{"name":"glob","arguments":""}}]},"finish_reason":null},{"index":1,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_choice1","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_multi_choice","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}"}}]},"finish_reason":null},{"index":1,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_multi_choice","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"tool_calls"},{"index":1,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}}"#,
        "data: [DONE]",
    ];
    let request = r#"{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true}"#;
    let out = Feed::new("model", request, request).lines(&lines);

    // The output index, name and arguments of each call.
    let mut added = HashMap::new();
    let mut done = HashMap::new();
    for (event, data) in &out {
        if text_at(data, "item.type") != "function_call" {
            continue;
        }
        let call = (
            int_at(data, "output_index"),
            text_at(data, "item.name"),
            text_at(data, "item.arguments"),
        );
        match event.as_str() {
            "response.output_item.added" => {
                added.insert(text_at(data, "item.call_id"), call);
            }
            "response.output_item.done" => {
                done.insert(text_at(data, "item.call_id"), call);
            }
            _ => {}
        }
    }

    assert_eq!(added.len(), 2, "function_call added events");
    assert_eq!(done.len(), 2, "function_call done events");
    assert_eq!(added["call_choice0"].1, "glob");
    assert_eq!(added["call_choice1"].1, "read");
    assert_ne!(
        added["call_choice0"].0, added["call_choice1"].0,
        "expected distinct output indexes for different choices"
    );
    for call_id in ["call_choice0", "call_choice1"] {
        assert!(
            raw::valid(&done[call_id].2),
            "invalid JSON args for {call_id}: {:?}",
            done[call_id].2
        );
    }
    assert_ne!(
        done["call_choice0"].0, done["call_choice1"].0,
        "expected distinct done output indexes for different choices"
    );
    assert_eq!(done["call_choice0"].1, "glob");
    assert_eq!(done["call_choice1"].1, "read");
}

#[test]
fn mixed_message_and_tool_use_distinct_output_indexes() {
    let lines = [
        r#"data: {"id":"resp_mixed","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello","reasoning_content":null,"tool_calls":null},"finish_reason":null},{"index":1,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_choice1","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_mixed","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"stop"},{"index":1,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"}}]},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}}"#,
        "data: [DONE]",
    ];
    let request = r#"{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true}"#;
    let out = Feed::new("model", request, request).lines(&lines);

    let mut message_index = None;
    let mut tool_index = None;
    for (event, data) in &out {
        if event != "response.output_item.added" {
            continue;
        }
        match text_at(data, "item.type").as_str() {
            "message" if text_at(data, "item.id") == "msg_resp_mixed_0" => {
                message_index = Some(int_at(data, "output_index"));
            }
            "function_call" if text_at(data, "item.call_id") == "call_choice1" => {
                tool_index = Some(int_at(data, "output_index"));
            }
            _ => {}
        }
    }
    let message_index = message_index.expect("message output index");
    let tool_index = tool_index.expect("tool output index");
    assert_ne!(
        message_index, tool_index,
        "expected distinct output indexes for message and tool call"
    );
}

#[test]
fn completed_omits_top_level_output_text() {
    let lines = [
        r#"data: {"id":"resp_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"hello ","reasoning_content":null,"tool_calls":null},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":"world","reasoning_content":null,"tool_calls":null},"finish_reason":"stop"}],"usage":{"completion_tokens":2,"total_tokens":4,"prompt_tokens":2}}"#,
        "data: [DONE]",
    ];
    let request = r#"{"model":"gpt-5.4"}"#;
    let out = Feed::new("model", request, request).lines(&lines);
    let completed = last(&out, "response.completed");
    assert!(!completed.is_null(), "expected response.completed event");
    assert!(
        at(completed, "response.output_text").is_none(),
        "response.output_text should be omitted to match native Responses output: {completed}"
    );
    assert_eq!(
        text_at(completed, "response.output.0.content.0.text"),
        "hello world"
    );
}

#[test]
fn tool_call_completed_omits_top_level_output_text() {
    let lines = [
        r#"data: {"id":"resp_tool_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"I will call the weather tool.","reasoning_content":null,"tool_calls":null},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_tool_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_weather","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_tool_output_text","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"location\":\"北京\",\"unit\":\"celsius\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}}"#,
        "data: [DONE]",
    ];
    let request = r#"{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true}"#;
    let out = Feed::new("model", request, request).lines(&lines);
    let completed = last(&out, "response.completed");
    assert!(!completed.is_null(), "expected response.completed event");
    assert!(
        at(completed, "response.output_text").is_none(),
        "response.output_text should be omitted to match native Responses output: {completed}"
    );
    assert_eq!(
        text_at(completed, "response.output.0.content.0.text"),
        "I will call the weather tool."
    );
    assert!(
        text_at(completed, "response.output.1.arguments").contains("北京"),
        "response function call arguments want Beijing: {completed}"
    );
}

#[test]
fn function_call_done_and_completed_output_stay_ascending() {
    let lines = [
        r#"data: {"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_glob","type":"function","function":{"name":"glob","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"C:\\\\repo\",\"pattern\":\"*.go\"}"}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":1,"id":"call_read","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":1,"function":{"arguments":"{\"filePath\":\"C:\\\\repo\\\\README.md\",\"limit\":20,\"offset\":1}"}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_order","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":"tool_calls"}],"usage":{"completion_tokens":10,"total_tokens":20,"prompt_tokens":10}}"#,
        "data: [DONE]",
    ];
    let request = r#"{"model":"gpt-5.4","tool_choice":"auto","parallel_tool_calls":true}"#;
    let out = Feed::new("model", request, request).lines(&lines);

    let mut done_indexes = Vec::new();
    let mut completed_order = Vec::new();
    for (event, data) in &out {
        match event.as_str() {
            "response.output_item.done" if text_at(data, "item.type") == "function_call" => {
                done_indexes.push(int_at(data, "output_index"));
            }
            "response.completed" => {
                for item in output_items(data, "function_call") {
                    completed_order.push(text_at(item, "call_id"));
                }
            }
            _ => {}
        }
    }

    assert_eq!(done_indexes.len(), 2, "function_call done indexes");
    assert!(
        done_indexes[0] < done_indexes[1],
        "expected ascending done output indexes, got {done_indexes:?}"
    );
    assert_eq!(completed_order, ["call_glob", "call_read"]);
}

#[test]
fn non_stream_omits_top_level_output_text() {
    let request = r#"{"model":"gpt-5.4"}"#;
    let raw = r#"{"id":"chatcmpl_output_text","object":"chat.completion","created":1773896263,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"ping"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#;
    let out = convert(request, request, raw);
    assert!(
        out.get("output_text").is_none(),
        "output_text should be omitted to match native Responses output: {out}"
    );
    assert_eq!(text_at(&out, "output.0.content.0.text"), "ping", "{out}");
}

const NAMESPACE_REQUEST: &str = r#"{
		"model":"deepseek-v4-flash",
		"tools":[
			{
				"type":"namespace",
				"name":"mcp__test_mcp__",
				"tools":[{"type":"function","name":"add_numbers","parameters":{"type":"object","properties":{}}}]
			}
		]
	}"#;

#[test]
fn restores_namespace_function_call() {
    let lines = [
        r#"data: {"id":"chatcmpl_namespace_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_ns","type":"function","function":{"name":"mcp__test_mcp__add_numbers","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_namespace_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":3,\"b\":5}"}}]},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("model", NAMESPACE_REQUEST, "").lines(&lines);
    for (label, event) in [
        ("added", "response.output_item.added"),
        ("done", "response.output_item.done"),
    ] {
        let data = last_of(&out, event, "function_call");
        assert!(!data.is_null(), "expected function_call {label} event");
        assert_eq!(text_at(data, "item.name"), "add_numbers", "{label}");
        assert_eq!(
            text_at(data, "item.namespace"),
            "mcp__test_mcp__",
            "{label}"
        );
    }
    let completed = last(&out, "response.completed");
    assert!(!completed.is_null(), "expected response.completed event");
    assert_eq!(text_at(completed, "response.output.0.name"), "add_numbers");
    assert_eq!(
        text_at(completed, "response.output.0.namespace"),
        "mcp__test_mcp__"
    );
}

#[test]
fn non_stream_restores_namespace_function_call() {
    let raw = r#"{"id":"chatcmpl_namespace_nonstream","object":"chat.completion","created":1773896263,"model":"model","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_ns","type":"function","function":{"name":"mcp__test_mcp__add_numbers","arguments":"{\"a\":3,\"b\":5}"}}]},"finish_reason":"tool_calls"}]}"#;
    let out = convert(NAMESPACE_REQUEST, "", raw);
    assert_eq!(text_at(&out, "output.0.name"), "add_numbers", "{out}");
    assert_eq!(
        text_at(&out, "output.0.namespace"),
        "mcp__test_mcp__",
        "{out}"
    );
}

const CAPPED_NAMESPACE_REQUEST: &str = r#"{
		"model":"deepseek-v4-flash",
		"tools":[
			{
				"type":"namespace",
				"name":"mcp__codex_apps__codex_document_control",
				"tools":[{"type":"function","name":"_execute_document_command","parameters":{"type":"object"}}]
			}
		]
	}"#;

#[test]
fn restores_capped_namespace_function_call() {
    // The 66-character flattened name is cut to 64.
    let chat_name = cap("mcp__codex_apps__codex_document_control___execute_document_command");
    let start = format!(
        r#"data: {{"id":"chatcmpl_capped_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"call_capped","type":"function","function":{{"name":"{chat_name}","arguments":""}}}}]}},"finish_reason":null}}]}}"#
    );
    let lines = [
        start.as_str(),
        r#"data: {"id":"chatcmpl_capped_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":\"run\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("model", CAPPED_NAMESPACE_REQUEST, "").lines(&lines);
    for (label, event) in [
        ("added", "response.output_item.added"),
        ("done", "response.output_item.done"),
    ] {
        let data = last_of(&out, event, "function_call");
        assert!(!data.is_null(), "expected function_call {label} event");
        assert_eq!(
            text_at(data, "item.name"),
            "_execute_document_command",
            "{label}"
        );
        assert_eq!(
            text_at(data, "item.namespace"),
            "mcp__codex_apps__codex_document_control",
            "{label}"
        );
    }
    let completed = last(&out, "response.completed");
    assert!(!completed.is_null(), "expected response.completed event");
    assert_eq!(
        text_at(completed, "response.output.0.name"),
        "_execute_document_command"
    );
    assert_eq!(
        text_at(completed, "response.output.0.namespace"),
        "mcp__codex_apps__codex_document_control"
    );
}

#[test]
fn non_stream_restores_capped_namespace_function_call() {
    let chat_name = cap("mcp__codex_apps__codex_document_control___execute_document_command");
    let raw = format!(
        r#"{{"id":"chatcmpl_capped_nonstream","object":"chat.completion","created":1773896263,"model":"model","choices":[{{"index":0,"message":{{"role":"assistant","tool_calls":[{{"id":"call_capped","type":"function","function":{{"name":"{chat_name}","arguments":"{{\"cmd\":\"run\"}}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
    );
    let out = convert(CAPPED_NAMESPACE_REQUEST, "", &raw);
    assert_eq!(
        text_at(&out, "output.0.name"),
        "_execute_document_command",
        "{out}"
    );
    assert_eq!(
        text_at(&out, "output.0.namespace"),
        "mcp__codex_apps__codex_document_control",
        "{out}"
    );
}

/// Fails on any function call argument event: custom tool calls have none.
fn assert_no_function_events(out: &[(String, Value)]) {
    for (event, data) in out {
        assert!(
            event != "response.function_call_arguments.delta"
                && event != "response.function_call_arguments.done",
            "unexpected function call event {event:?}: {data}"
        );
    }
}

/// Checks the item of the last added and done events for `call_id`, and the
/// first item of the last `response.completed`: each must be a custom tool
/// call to `name` with that call ID.
fn assert_custom_call(out: &[(String, Value)], call_id: &str, name: &str) {
    let item_of = |event: &str| {
        out.iter()
            .rev()
            .find(|(kind, data)| kind == event && text_at(data, "item.call_id") == call_id)
            .map_or(&Value::Null, |(_, data)| &data["item"])
    };
    for (label, item) in [
        ("added", item_of("response.output_item.added")),
        ("done", item_of("response.output_item.done")),
        (
            "completed",
            &last(out, "response.completed")["response"]["output"][0],
        ),
    ] {
        assert!(!item.is_null(), "expected {label} event");
        assert_eq!(text_at(item, "type"), "custom_tool_call", "{label}");
        assert_eq!(text_at(item, "id"), format!("ctc_{call_id}"), "{label}");
        assert_eq!(text_at(item, "call_id"), call_id, "{label}");
        assert_eq!(text_at(item, "name"), name, "{label}");
    }
}

#[test]
fn custom_tool_name_arrives_late() {
    let original = r#"{
		"model":"gpt-5.4",
		"tools":[{"type":"custom","name":"exec"}]
	}"#;
    let lines = [
        r#"data: {"id":"chatcmpl_custom_late_name","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_exec","type":"function","function":{"arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_custom_late_name","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"exec","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_custom_late_name","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("model", original, "").lines(&lines);
    assert_no_function_events(&out);
    assert_custom_call(&out, "call_exec", "exec");
    let input_done = last(&out, "response.custom_tool_call_input.done");
    assert_eq!(text_at(input_done, "item_id"), "ctc_call_exec");
    assert_eq!(text_at(input_done, "input"), "pwd");
}

#[test]
fn custom_tool_name_and_id_are_missing() {
    let original = r#"{"model":"gpt-5.4","tools":[{"type":"custom","name":"exec"}]}"#;
    let lines = [
        r#"data: {"id":"chatcmpl_custom_missing_fields","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"type":"function","function":{"arguments":"{\"input\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("model", original, "").lines(&lines);
    assert_custom_call(&out, "call_chatcmpl_custom_missing_fields_0_0", "exec");
}

#[test]
fn tool_call_id_may_arrive_late_or_be_missing() {
    let cases: [(&str, &[&str], &str); 2] = [
        (
            "late id",
            &[
                r#"data: {"id":"chatcmpl_late_id","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"type":"function","function":{"name":"read","arguments":"{\"file"}}]},"finish_reason":null}]}"#,
                r#"data: {"id":"chatcmpl_late_id","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_late","function":{"arguments":"Path\":\"README.md\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ],
            "call_late",
        ),
        (
            "missing id",
            &[
                r#"data: {"id":"chatcmpl_missing_id","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"type":"function","function":{"name":"read","arguments":"{\"filePath\":\"README.md\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ],
            "call_chatcmpl_missing_id_0_0",
        ),
    ];

    for (name, lines, call_id) in cases {
        let mut feed = Feed::new("model", "", "");
        let mut out = feed.lines(lines);
        out.extend(feed.line("data: [DONE]"));
        let item_id = format!("fc_{call_id}");
        let added = last(&out, "response.output_item.added");
        let delta = last(&out, "response.function_call_arguments.delta");
        assert_eq!(text_at(added, "item.id"), item_id, "{name}: {out:?}");
        assert_eq!(text_at(added, "item.call_id"), call_id, "{name}");
        assert_eq!(text_at(delta, "item_id"), item_id, "{name}");
        assert_eq!(
            text_at(delta, "delta"),
            r#"{"filePath":"README.md"}"#,
            "{name}: want the whole buffered arguments"
        );
        assert_eq!(
            text_at(
                last(&out, "response.function_call_arguments.done"),
                "item_id"
            ),
            item_id,
            "{name}"
        );
        assert_eq!(
            text_at(last(&out, "response.output_item.done"), "item.id"),
            item_id,
            "{name}"
        );
    }
}

/// The items of the last added and done events, and the first item of the
/// last `response.completed`.
fn last_items(out: &[(String, Value)]) -> [(&'static str, &Value); 3] {
    [
        ("added", &last(out, "response.output_item.added")["item"]),
        ("done", &last(out, "response.output_item.done")["item"]),
        (
            "completed",
            &last(out, "response.completed")["response"]["output"][0],
        ),
    ]
}

const ADDITIONAL_NAMESPACE_REQUEST: &str = r#"{
		"model":"gpt-5.4",
		"input":[{
			"type":"additional_tools",
			"tools":[{
				"type":"namespace",
				"name":"collaboration",
				"tools":[{"type":"function","name":"send_message","parameters":{"type":"object","properties":{}}}]
			}]
		}]
	}"#;

#[test]
fn restores_additional_namespace_function_call() {
    let lines = [
        r#"data: {"id":"chatcmpl_additional_namespace_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_send","type":"function","function":{"name":"collaboration__send_message","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_additional_namespace_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"target\":\"worker\",\"message\":\"ping\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("model", ADDITIONAL_NAMESPACE_REQUEST, "").lines(&lines);
    for (label, item) in last_items(&out) {
        assert_eq!(text_at(item, "name"), "send_message", "{label}");
        assert_eq!(text_at(item, "namespace"), "collaboration", "{label}");
    }
}

#[test]
fn non_stream_restores_additional_namespace_function_call() {
    let raw = r#"{"id":"chatcmpl_additional_namespace_nonstream","object":"chat.completion","created":1773896263,"model":"model","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_send","type":"function","function":{"name":"collaboration__send_message","arguments":"{\"target\":\"worker\",\"message\":\"ping\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let out = convert(ADDITIONAL_NAMESPACE_REQUEST, "", raw);
    assert_eq!(text_at(&out, "output.0.name"), "send_message", "{out}");
    assert_eq!(
        text_at(&out, "output.0.namespace"),
        "collaboration",
        "{out}"
    );
}

const ADDITIONAL_CUSTOM_REQUEST: &str = r#"{
		"model":"gpt-5.4",
		"input":[{
			"type":"additional_tools",
			"tools":[{
				"type":"namespace",
				"name":"functions",
				"tools":[{"type":"custom","name":"exec"}]
			}]
		}]
	}"#;

#[test]
fn restores_additional_namespace_custom_tool_call() {
    let lines = [
        r#"data: {"id":"chatcmpl_additional_namespace_custom_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_exec","type":"function","function":{"name":"functions__exec","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_additional_namespace_custom_stream","object":"chat.completion.chunk","created":1773896263,"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("model", ADDITIONAL_CUSTOM_REQUEST, "").lines(&lines);
    assert_no_function_events(&out);
    for (label, item) in last_items(&out) {
        assert!(!item.is_null(), "expected {label} event");
        assert_eq!(text_at(item, "type"), "custom_tool_call", "{label}");
        assert_eq!(text_at(item, "name"), "exec", "{label}");
        assert_eq!(text_at(item, "namespace"), "functions", "{label}");
    }
    assert_eq!(
        text_at(last(&out, "response.custom_tool_call_input.done"), "input"),
        "pwd"
    );
    assert_eq!(
        text_at(last(&out, "response.output_item.done"), "item.input"),
        "pwd"
    );
    assert_eq!(
        text_at(last(&out, "response.completed"), "response.output.0.input"),
        "pwd"
    );
}

#[test]
fn non_stream_restores_additional_namespace_custom_tool_call() {
    let raw = r#"{"id":"chatcmpl_additional_namespace_custom_nonstream","object":"chat.completion","created":1773896263,"model":"model","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_exec","type":"function","function":{"name":"functions__exec","arguments":"{\"input\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let out = convert(ADDITIONAL_CUSTOM_REQUEST, "", raw);
    assert_eq!(text_at(&out, "output.0.type"), "custom_tool_call", "{out}");
    assert_eq!(text_at(&out, "output.0.name"), "exec", "{out}");
    assert_eq!(text_at(&out, "output.0.namespace"), "functions", "{out}");
    assert_eq!(text_at(&out, "output.0.input"), "pwd", "{out}");
}

#[test]
fn does_not_complete_reasoning_only_stream() {
    let request = r#"{"model":"deepseek-v4-flash"}"#;
    let lines = [
        r#"data: {"id":"resp_reasoning_only","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"still thinking"},"finish_reason":null}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("deepseek-v4-flash", request, request).lines(&lines);
    for (event, data) in &out {
        assert_ne!(
            event, "response.completed",
            "reasoning-only stream was finalized as response.completed: {data}"
        );
    }
    assert!(
        count(&out, "response.reasoning_summary_text.delta") > 0,
        "test stream did not exercise reasoning output"
    );
}

#[test]
fn incomplete_tool_stream_does_not_finalize_as_completed() {
    let request = r#"{"model":"gpt-5.6-terra"}"#;
    let cases = [
        (
            "zero argument bytes without finish reason",
            r#"data: {"id":"resp_interrupted_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-terra","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":{"name":"apply_patch","arguments":""}}]},"finish_reason":null}]}"#,
        ),
        (
            "partial json arguments without finish reason",
            r#"data: {"id":"resp_interrupted_partial","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-terra","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":{"name":"apply_patch","arguments":"{\"filePath\":\"foo"}}]},"finish_reason":null}]}"#,
        ),
    ];

    for (name, chunk) in cases {
        let out = Feed::new("gpt-5.6-terra", request, request).lines(&[chunk, "data: [DONE]"]);
        for (event, data) in &out {
            assert!(
                !matches!(
                    event.as_str(),
                    "response.completed"
                        | "response.output_item.done"
                        | "response.function_call_arguments.done"
                ),
                "{name}: incomplete tool stream emitted {event}: {data}"
            );
        }
    }
}

#[test]
fn finish_reason_length_emits_incomplete() {
    let request = r#"{"model":"gpt-5.6-luna"}"#;
    let lines = [
        r#"data: {"id":"resp_length_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":{"name":"apply_patch","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_length_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("gpt-5.6-luna", request, request).lines(&lines);
    for (event, data) in &out {
        match event.as_str() {
            "response.completed" => panic!(
                "stream with finish_reason=length was finalized as response.completed: {data}"
            ),
            "response.output_item.done" => {
                assert_eq!(text_at(data, "item.status"), "incomplete");
                assert_ne!(
                    text_at(data, "item.arguments"),
                    "{}",
                    "item.arguments synthesized an empty object, want raw args or empty string"
                );
            }
            "response.incomplete" => {
                assert_eq!(text_at(data, "response.status"), "incomplete");
                assert_eq!(
                    text_at(data, "response.incomplete_details.reason"),
                    "max_output_tokens"
                );
                assert_eq!(text_at(data, "response.output.0.status"), "incomplete");
            }
            _ => {}
        }
    }
    assert!(
        count(&out, "response.output_item.done") > 0,
        "expected response.output_item.done for finish_reason=length"
    );
    assert!(
        count(&out, "response.incomplete") > 0,
        "expected response.incomplete for finish_reason=length"
    );
}

#[test]
fn finish_reason_content_filter_emits_incomplete() {
    let request = r#"{"model":"gpt-5.6-luna"}"#;
    let lines = [
        r#"data: {"id":"resp_filter_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":{"name":"apply_patch","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"resp_filter_tool","object":"chat.completion.chunk","created":1773896263,"model":"gpt-5.6-luna","choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("gpt-5.6-luna", request, request).lines(&lines);
    for (event, data) in &out {
        match event.as_str() {
            "response.completed" => panic!(
                "stream with finish_reason=content_filter was finalized as response.completed: {data}"
            ),
            "response.output_item.done" => {
                assert_eq!(text_at(data, "item.status"), "incomplete");
            }
            "response.incomplete" => {
                assert_eq!(text_at(data, "response.status"), "incomplete");
                assert_eq!(
                    text_at(data, "response.incomplete_details.reason"),
                    "content_filter"
                );
            }
            _ => {}
        }
    }
    assert!(
        count(&out, "response.output_item.done") > 0,
        "expected response.output_item.done for finish_reason=content_filter"
    );
    assert!(
        count(&out, "response.incomplete") > 0,
        "expected response.incomplete for finish_reason=content_filter"
    );
}

#[test]
fn non_stream_finish_reason_length() {
    let raw = r#"{"id":"chatcmpl_len","object":"chat.completion","created":1773896263,"model":"gpt-5.6","choices":[{"index":0,"message":{"role":"assistant","content":"truncated text"},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
    let out = convert("", "", raw);
    assert_eq!(text_at(&out, "status"), "incomplete", "{out}");
    assert_eq!(
        text_at(&out, "incomplete_details.reason"),
        "max_output_tokens",
        "{out}"
    );
    assert_eq!(text_at(&out, "output.0.status"), "incomplete", "{out}");
}

#[test]
fn non_stream_finish_reason_content_filter() {
    let raw = r#"{"id":"chatcmpl_filter","object":"chat.completion","created":1773896263,"model":"gpt-5.6","choices":[{"index":0,"message":{"role":"assistant","content":"blocked text"},"finish_reason":"content_filter"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
    let out = convert("", "", raw);
    assert_eq!(text_at(&out, "status"), "incomplete", "{out}");
    assert_eq!(
        text_at(&out, "incomplete_details.reason"),
        "content_filter",
        "{out}"
    );
    assert_eq!(text_at(&out, "output.0.status"), "incomplete", "{out}");
}

#[test]
fn non_stream_reasoning_fallback() {
    // The name, the response, the request, and the reasoning summary wanted,
    // if any.
    let cases = [
        (
            "reasoning_content field present",
            r#"{"id":"chatcmpl_rc","object":"chat.completion","created":1773896263,"model":"o3-mini","choices":[{"index":0,"message":{"role":"assistant","content":"hello","reasoning_content":"thought from reasoning_content"},"finish_reason":"stop"}]}"#,
            "",
            Some("thought from reasoning_content"),
        ),
        (
            "reasoning fallback field present",
            r#"{"id":"chatcmpl_r","object":"chat.completion","created":1773896263,"model":"o3-mini","choices":[{"index":0,"message":{"role":"assistant","content":"hello","reasoning":"thought from reasoning"},"finish_reason":"stop"}]}"#,
            "",
            Some("thought from reasoning"),
        ),
        (
            "both reasoning_content and reasoning present (reasoning_content priority)",
            r#"{"id":"chatcmpl_both","object":"chat.completion","created":1773896263,"model":"o3-mini","choices":[{"index":0,"message":{"role":"assistant","content":"hello","reasoning_content":"priority thought","reasoning":"ignored thought"},"finish_reason":"stop"}]}"#,
            "",
            Some("priority thought"),
        ),
        (
            "empty reasoning_content falls back to reasoning",
            r#"{"id":"chatcmpl_empty_rc","object":"chat.completion","created":1773896263,"model":"o3-mini","choices":[{"index":0,"message":{"role":"assistant","content":"hello","reasoning_content":"","reasoning":"fallback thought"},"finish_reason":"stop"}]}"#,
            "",
            Some("fallback thought"),
        ),
        (
            "neither field present without request reasoning",
            r#"{"id":"chatcmpl_none","object":"chat.completion","created":1773896263,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]}"#,
            "",
            None,
        ),
        (
            "neither field present with request reasoning produces empty summary",
            r#"{"id":"chatcmpl_req_only","object":"chat.completion","created":1773896263,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]}"#,
            r#"{"model":"gpt-4o","reasoning":{"effort":"medium"}}"#,
            Some(""),
        ),
    ];

    for (name, raw, request, want) in cases {
        let out = convert(request, request, raw);
        let reasoning = out
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|item| text_at(item, "type") == "reasoning");
        assert_eq!(reasoning.is_some(), want.is_some(), "{name}: {out}");
        let (Some(item), Some(want)) = (reasoning, want) else {
            continue;
        };
        if want.is_empty() {
            let summary = at(item, "summary").and_then(Value::as_array);
            assert!(
                summary.is_none_or(Vec::is_empty),
                "{name}: want an empty summary: {out}"
            );
        } else {
            assert_eq!(text_at(item, "summary.0.text"), want, "{name}: {out}");
            assert_eq!(
                text_at(item, "summary.0.type"),
                "summary_text",
                "{name}: {out}"
            );
        }
    }
}

#[test]
fn empty_tool_calls_array_does_not_terminate_items() {
    let request = r#"{"model":"codebuddy-hy4"}"#;
    let lines = [
        r#"data: {"id":"chatcmpl_empty_tc","object":"chat.completion.chunk","created":1773896263,"model":"codebuddy-hy4","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_content":"Thinking part 1, ","function_call":null,"refusal":"","tool_calls":[]},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_empty_tc","object":"chat.completion.chunk","created":1773896263,"model":"codebuddy-hy4","choices":[{"index":0,"delta":{"content":"","reasoning_content":"thinking part 2.","function_call":null,"refusal":"","tool_calls":[]},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_empty_tc","object":"chat.completion.chunk","created":1773896263,"model":"codebuddy-hy4","choices":[{"index":0,"delta":{"content":"Hello ","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[]},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_empty_tc","object":"chat.completion.chunk","created":1773896263,"model":"codebuddy-hy4","choices":[{"index":0,"delta":{"content":"world!","reasoning_content":"","function_call":null,"refusal":"","tool_calls":[]},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("codebuddy-hy4", request, request).lines(&lines);
    let items = |event: &str, kind: &str| {
        out.iter()
            .filter(|(name, data)| name == event && text_at(data, "item.type") == kind)
            .count()
    };
    assert_eq!(items("response.output_item.added", "reasoning"), 1);
    assert_eq!(items("response.output_item.done", "reasoning"), 1);
    assert_eq!(
        text_at(
            last_of(&out, "response.output_item.done", "reasoning"),
            "item.summary.0.text"
        ),
        "Thinking part 1, thinking part 2."
    );
    assert_eq!(items("response.output_item.added", "message"), 1);
    assert_eq!(items("response.output_item.done", "message"), 1);
    assert_eq!(
        text_at(
            last_of(&out, "response.output_item.done", "message"),
            "item.content.0.text"
        ),
        "Hello world!"
    );
    assert_eq!(count(&out, "response.completed"), 1);
    let completed = last(&out, "response.completed");
    assert_eq!(
        text_at(completed, "response.output.0.summary.0.text"),
        "Thinking part 1, thinking part 2."
    );
    assert_eq!(
        text_at(completed, "response.output.1.content.0.text"),
        "Hello world!"
    );
}

/// `added:<type>:<index>` and `done:<type>:<index>` for each output item
/// event, in order.
fn item_order(out: &[(String, Value)]) -> Vec<String> {
    out.iter()
        .filter_map(|(event, data)| {
            let stage = match event.as_str() {
                "response.output_item.added" => "added",
                "response.output_item.done" => "done",
                _ => return None,
            };
            Some(format!(
                "{stage}:{}:{}",
                text_at(data, "item.type"),
                int_at(data, "output_index")
            ))
        })
        .collect()
}

#[test]
fn chunk_with_content_and_reasoning_content() {
    let request = r#"{"model":"deepseek-v4-flash"}"#;
    let lines = [
        r#"data: {"id":"chatcmpl_ds","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Thinking part 1,"},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_ds","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"content":"Bien","reasoning_content":" Just professional."},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_ds","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"content":" continues here."},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("deepseek-v4-flash", request, request).lines(&lines);
    assert_eq!(
        text_at(
            last_of(&out, "response.output_item.done", "reasoning"),
            "item.summary.0.text"
        ),
        "Thinking part 1, Just professional."
    );
    assert_eq!(
        text_at(
            last_of(&out, "response.output_item.done", "message"),
            "item.content.0.text"
        ),
        "Bien continues here."
    );
    assert_eq!(count(&out, "response.completed"), 1);

    // The reasoning finishes before the message starts.
    assert_eq!(
        item_order(&out),
        [
            "added:reasoning:0",
            "done:reasoning:0",
            "added:message:1",
            "done:message:1"
        ]
    );

    // The item IDs differ.
    let added_ids: Vec<String> = out
        .iter()
        .filter(|(name, _)| name == "response.output_item.added")
        .map(|(_, data)| text_at(data, "item.id"))
        .collect();
    assert!(
        added_ids.len() == 2 && added_ids[0] != added_ids[1],
        "expected 2 distinct item IDs, got {added_ids:?}"
    );

    // The final response has both, in order.
    let completed = last(&out, "response.completed");
    assert_eq!(
        text_at(completed, "response.output.0.summary.0.text"),
        "Thinking part 1, Just professional."
    );
    assert_eq!(
        text_at(completed, "response.output.1.content.0.text"),
        "Bien continues here."
    );
}

#[test]
fn single_chunk_with_both_content_and_reasoning_content() {
    let request = r#"{"model":"deepseek-v4-flash"}"#;
    let lines = [
        r#"data: {"id":"chatcmpl_single","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"content":"Answer","reasoning_content":"Thought"},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ];
    let out = Feed::new("deepseek-v4-flash", request, request).lines(&lines);
    assert_eq!(
        item_order(&out),
        [
            "added:reasoning:0",
            "done:reasoning:0",
            "added:message:1",
            "done:message:1"
        ]
    );
    let completed = last(&out, "response.completed");
    assert_eq!(
        text_at(completed, "response.output.0.summary.0.text"),
        "Thought"
    );
    assert_eq!(
        text_at(completed, "response.output.1.content.0.text"),
        "Answer"
    );
}

/// `applyPatchChatStart`.
fn patch_start(index: i64, id: &str, name: &str) -> String {
    format!(
        r#"data: {{"id":"r1","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":{index},"id":{},"type":"function","function":{{"name":{},"arguments":""}}}}]}}}}]}}"#,
        quote(id),
        quote(name)
    )
}

/// `applyPatchChatFragment`.
fn patch_fragment(index: i64, fragment: &str) -> String {
    format!(
        r#"data: {{"id":"r1","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":{index},"function":{{"arguments":{}}}}}]}}}}]}}"#,
        quote(fragment)
    )
}

/// `applyPatchChatEnd`.
const PATCH_END: [&str; 2] = [
    r#"data: {"id":"r1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    "data: [DONE]",
];

/// `applyPatchChatRequest`.
const PATCH_REQUEST: &str = r#"{"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}]}"#;

/// A stream for the apply_patch tests, made as theirs are, which gives each
/// event's data (`applyPatchChatEvents`).
struct PatchFeed(Feed);

impl PatchFeed {
    fn new(request: &str) -> Self {
        Self(Feed::new("test", request, ""))
    }

    fn line(&mut self, line: &str) -> Vec<Value> {
        self.0
            .line(line)
            .into_iter()
            .map(|(_, data)| data)
            .collect()
    }

    fn end(&mut self) -> Vec<Value> {
        PATCH_END.iter().flat_map(|line| self.line(line)).collect()
    }

    fn error(&self) -> Option<&(dyn Error + 'static)> {
        self.0.0.tool_input_error()
    }
}

fn kind(event: &Value) -> String {
    text_at(event, "type")
}

/// The first output item of the last `response.completed` in `events`.
fn completed_item(events: &[Value]) -> Value {
    events
        .iter()
        .rev()
        .find(|event| kind(event) == "response.completed")
        .map_or(Value::Null, |event| event["response"]["output"][0].clone())
}

#[test]
fn apply_patch_chat_preview_before_done() {
    let mut feed = PatchFeed::new(PATCH_REQUEST);
    feed.line(&patch_start(0, "c1", "apply_patch"));
    let preview = feed.line(&patch_fragment(
        0,
        r#"{"input":"*** Begin Patch\n*** Add File: a.txt\n+hello\n"#,
    ));
    assert!(
        preview.len() == 1
            && kind(&preview[0]) == "response.custom_tool_call_input.delta"
            && text_at(&preview[0], "delta") == "*** Begin Patch\n*** Add File: a.txt\n+hello\n",
        "missing real decoded preview: {preview:?}"
    );
    assert!(
        text_at(&preview[0], "call_id") == "c1" && text_at(&preview[0], "item_id") == "ctc_c1",
        "preview identity: {}",
        preview[0]
    );
    let mut events = preview;
    events.extend(feed.line(&patch_fragment(0, r#"*** End Patch"}"#)));
    events.extend(feed.end());

    let want = "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch";
    let mut deltas = String::new();
    let (mut seen_done, mut seen_item, mut seen_completed) = (false, false, false);
    let mut last_sequence = 0;
    for event in &events {
        let sequence = int_at(event, "sequence_number");
        assert!(sequence > last_sequence, "non-monotonic sequence: {event}");
        last_sequence = sequence;
        match kind(event).as_str() {
            "response.custom_tool_call_input.delta" => deltas.push_str(&text_at(event, "delta")),
            "response.custom_tool_call_input.done" => {
                seen_done = true;
                assert!(
                    deltas == want
                        && text_at(event, "input") == want
                        && text_at(event, "call_id") == "c1",
                    "input.done mismatch: {event}"
                );
            }
            "response.output_item.done" => {
                seen_item = true;
                assert!(
                    seen_done && text_at(event, "item.input") == want,
                    "item.done mismatch: {event}"
                );
            }
            "response.completed" => {
                seen_completed = true;
                assert!(
                    seen_item && text_at(event, "response.output.0.input") == want,
                    "completed mismatch: {event}"
                );
            }
            _ => {}
        }
    }
    assert!(
        seen_done && seen_item && seen_completed,
        "missing terminal events: {events:?}"
    );
}

#[test]
fn apply_patch_chat_late_identity_and_interleaved_calls() {
    let mut feed = PatchFeed::new(PATCH_REQUEST);
    feed.line(&patch_start(0, "", ""));
    for event in feed.line(&patch_fragment(0, r#"{"input":"first\n"#)) {
        let kind = kind(&event);
        assert!(
            !kind.contains("arguments.delta") && kind != "response.custom_tool_call_input.delta",
            "emitted before identity: {event}"
        );
    }
    let mut events = feed.line(&patch_start(0, "c1", "apply_patch"));
    events.extend(feed.line(&patch_start(1, "c2", "apply_patch")));
    events.extend(feed.line(&patch_fragment(1, r#"{"input":"second"#)));
    events.extend(feed.line(&patch_fragment(0, r#"tail"}"#)));
    events.extend(feed.line(&patch_fragment(1, r#" tail"}"#)));
    events.extend(feed.end());

    let mut inputs: HashMap<String, String> = HashMap::new();
    let mut indices: HashMap<String, i64> = HashMap::new();
    let mut done: HashMap<String, String> = HashMap::new();
    for event in &events {
        match kind(event).as_str() {
            "response.custom_tool_call_input.delta" => {
                let id = text_at(event, "call_id");
                inputs
                    .entry(id.clone())
                    .or_default()
                    .push_str(&text_at(event, "delta"));
                indices.insert(id, int_at(event, "output_index"));
            }
            "response.custom_tool_call_input.done" => {
                done.insert(text_at(event, "call_id"), text_at(event, "input"));
            }
            "response.failed" => panic!("interleaved calls failed: {event}"),
            _ => {}
        }
    }
    let text = |map: &HashMap<String, String>, id: &str| map.get(id).cloned().unwrap_or_default();
    assert!(
        text(&inputs, "c1") == "first\ntail"
            && text(&inputs, "c2") == "second tail"
            && text(&done, "c1") == text(&inputs, "c1")
            && text(&done, "c2") == text(&inputs, "c2")
            && indices.get("c1") != indices.get("c2"),
        "mixed calls: inputs={inputs:?} done={done:?} indices={indices:?}"
    );
}

#[test]
fn apply_patch_chat_invalid_arguments_fail_once() {
    for arguments in [
        "plain patch",
        "{}",
        r#"{"input":42}"#,
        r#"{"input":"x","extra":1}"#,
        r#"{"input":"x","input":"y"}"#,
        r#"{"input":"x"} {}"#,
        r#"{"input":"unfinished"#,
        r#"{"input":"bad\q"}"#,
        r#"{"input":"\ud800"}"#,
    ] {
        let mut feed = PatchFeed::new(PATCH_REQUEST);
        let mut events = feed.line(&patch_start(0, "c1", "apply_patch"));
        events.extend(feed.line(&patch_fragment(0, arguments)));
        events.extend(feed.end());
        events.extend(feed.end());
        let mut failures = 0;
        for event in &events {
            match kind(event).as_str() {
                "response.failed" => {
                    failures += 1;
                    assert_eq!(
                        text_at(event, "response.error.code"),
                        "invalid_tool_arguments",
                        "{arguments}: wrong failure: {event}"
                    );
                }
                "response.completed"
                | "response.incomplete"
                | "response.custom_tool_call_input.done"
                | "response.output_item.done" => {
                    panic!("{arguments}: invalid arguments succeeded: {event}")
                }
                _ => {}
            }
        }
        assert!(
            failures == 1 && feed.error().is_some(),
            "{arguments}: missing retained failure: count={failures}"
        );
    }
}

#[test]
fn apply_patch_chat_winner_and_namespace() {
    // The name, the request, the name the upstream model calls, and the type,
    // name and namespace of the item wanted.
    let cases = [
        (
            "function",
            r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#,
            "apply_patch",
            "function_call",
            "apply_patch",
            "",
        ),
        (
            "function wins",
            r#"{"tools":[{"type":"function","name":"apply_patch"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "apply_patch",
            "function_call",
            "apply_patch",
            "",
        ),
        (
            "custom wins",
            r#"{"tools":[{"type":"custom","name":"apply_patch"}],"input":[{"type":"additional_tools","tools":[{"type":"function","name":"apply_patch"}]}]}"#,
            "apply_patch",
            "custom_tool_call",
            "apply_patch",
            "",
        ),
        (
            "namespace",
            r#"{"tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "editor__apply_patch",
            "custom_tool_call",
            "apply_patch",
            "editor",
        ),
        (
            "flat collision",
            r#"{"tools":[{"type":"function","name":"editor__apply_patch"},{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "editor__apply_patch",
            "function_call",
            "editor__apply_patch",
            "",
        ),
        (
            "same source custom first",
            r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"apply_patch"}]}"#,
            "apply_patch",
            "custom_tool_call",
            "apply_patch",
            "",
        ),
        (
            "same source function first",
            r#"{"tools":[{"type":"function","name":"apply_patch"},{"type":"custom","name":"apply_patch"}]}"#,
            "apply_patch",
            "function_call",
            "apply_patch",
            "",
        ),
        (
            "namespace before flat",
            r#"{"tools":[{"type":"namespace","name":"editor","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"editor__apply_patch"}]}"#,
            "editor__apply_patch",
            "custom_tool_call",
            "apply_patch",
            "editor",
        ),
        (
            "other custom",
            r#"{"tools":[{"type":"custom","name":"edit"}]}"#,
            "edit",
            "custom_tool_call",
            "edit",
            "",
        ),
    ];

    for (name, request, upstream, want_type, want_name, namespace) in cases {
        let mut feed = PatchFeed::new(request);
        feed.line(&patch_start(0, "c1", upstream));
        let args = if want_type == "function_call" || name == "other custom" {
            r#"{"not_input":42}"#
        } else {
            r#"{"input":"x"}"#
        };
        let mut events = feed.line(&patch_fragment(0, args));
        events.extend(feed.end());
        let item = completed_item(&events);
        assert!(
            text_at(&item, "type") == want_type
                && text_at(&item, "name") == want_name
                && text_at(&item, "namespace") == namespace,
            "{name}: wrong winning identity: {item}"
        );
        if want_type == "function_call" {
            assert_eq!(
                text_at(&item, "arguments"),
                args,
                "{name}: ordinary function was unwrapped: {item}"
            );
        }
        if name == "other custom" {
            assert_eq!(
                text_at(&item, "input"),
                args,
                "{name}: other custom changed: {item}"
            );
        }
    }
}

#[test]
fn apply_patch_chat_non_stream_strict_input() {
    // The arguments, and the input wanted, if they are valid.
    let cases = [
        (
            r#"{"input":"*** Begin Patch\n*** End Patch"}"#,
            Some("*** Begin Patch\n*** End Patch"),
        ),
        (r#"{"input":12}"#, None),
        (r#"{"input":"truncated"#, None),
        (r#"{"input":"x","extra":true}"#, None),
        (r#"{"input":"\ud800"}"#, None),
    ];
    for (arguments, want) in cases {
        let raw = format!(
            r#"{{"id":"r1","choices":[{{"index":0,"message":{{"tool_calls":[{{"id":"c1","function":{{"name":"apply_patch","arguments":{}}}}}]}},"finish_reason":"tool_calls"}}]}}"#,
            quote(arguments)
        );
        let (result, error) = non_stream(&parse(PATCH_REQUEST), &Value::Null, raw.as_bytes());
        match want {
            None => assert!(
                text_at(&result, "status") == "failed"
                    && text_at(&result, "error.code") == "invalid_tool_arguments"
                    && error.is_some(),
                "{arguments}: invalid non-stream input succeeded: {result}"
            ),
            Some(want) => assert_eq!(
                text_at(&result, "output.0.input"),
                want,
                "{arguments}: non-stream input mismatch: {result}"
            ),
        }
    }
}

#[test]
fn apply_patch_chat_terminal_validates_truncated_call() {
    let mut feed = PatchFeed::new(PATCH_REQUEST);
    feed.line(&patch_start(0, "c1", "apply_patch"));
    feed.line(&patch_fragment(0, r#"{"input":"unfinished"#));
    let events = feed.line(PATCH_END[1]);
    assert!(
        events.len() == 1 && kind(&events[0]) == "response.failed",
        "truncated terminal did not fail: {events:?}"
    );
}

#[test]
fn apply_patch_chat_unicode_fragments() {
    let arguments = r#"{"input":"line\n\u4f60\u597d \ud83d\ude00 \" \\ \u96ea"}"#;
    for split in 1..arguments.len() {
        let mut feed = PatchFeed::new(PATCH_REQUEST);
        feed.line(&patch_start(0, "c1", "apply_patch"));
        let mut events = feed.line(&patch_fragment(0, &arguments[..split]));
        events.extend(feed.line(&patch_fragment(0, &arguments[split..])));
        events.extend(feed.end());
        let input: String = events
            .iter()
            .filter(|event| kind(event) == "response.custom_tool_call_input.delta")
            .map(|event| text_at(event, "delta"))
            .collect();
        assert_eq!(input, "line\n你好 😀 \" \\ 雪", "split {split}");
    }
}

#[test]
fn apply_patch_chat_identity_fields_arrive_separately() {
    for (id, name) in [("c1", ""), ("", "apply_patch")] {
        let mut feed = PatchFeed::new(PATCH_REQUEST);
        feed.line(&patch_start(0, id, name));
        let events = feed.line(&patch_fragment(0, r#"{"input":"preview"#));
        assert!(
            events.is_empty(),
            "preview without complete identity: {events:?}"
        );
        let events = feed.line(&patch_start(0, "c1", "apply_patch"));
        let found = events
            .iter()
            .rev()
            .find(|event| kind(event) == "response.custom_tool_call_input.delta")
            .is_some_and(|event| {
                text_at(event, "delta") == "preview" && text_at(event, "call_id") == "c1"
            });
        assert!(found, "buffer not released after identity: {events:?}");
    }
}

#[test]
fn apply_patch_chat_conflicting_identity_fails() {
    for (id, name) in [("other", "apply_patch"), ("c1", "other")] {
        let mut feed = PatchFeed::new(PATCH_REQUEST);
        feed.line(&patch_start(0, "c1", "apply_patch"));
        feed.line(&patch_fragment(0, r#"{"input":"preview"#));
        let mut events = feed.line(&patch_start(0, id, name));
        events.extend(feed.end());
        assert!(
            events.len() == 1 && kind(&events[0]) == "response.failed",
            "conflicting identity did not fail once: {events:?}"
        );
    }
}

#[test]
fn apply_patch_chat_non_stream_original_declaration_wins() {
    let original = r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#;
    let raw = r#"{"id":"r1","choices":[{"index":0,"message":{"tool_calls":[{"id":"c1","function":{"name":"apply_patch","arguments":"{\"not_input\":42}"}}]},"finish_reason":"tool_calls"}]}"#;
    let result = convert(original, PATCH_REQUEST, raw);
    assert!(
        text_at(&result, "output.0.type") == "function_call"
            && text_at(&result, "output.0.arguments") == r#"{"not_input":42}"#,
        "converted declaration overrode original: {result}"
    );
}

#[test]
fn apply_patch_chat_pending_identity_conflict_fails() {
    let mut feed = PatchFeed::new(PATCH_REQUEST);
    feed.line(&patch_start(0, "c1", ""));
    feed.line(&patch_fragment(0, r#"{"input":"x"}"#));
    let events = feed.line(&patch_start(0, "c2", "apply_patch"));
    assert!(
        events.len() == 1 && kind(&events[0]) == "response.failed",
        "pending ID silently replaced: {events:?}"
    );
}

#[test]
fn apply_patch_chat_missing_id_synthesized_at_terminal() {
    let mut feed = PatchFeed::new(PATCH_REQUEST);
    feed.line(&patch_start(0, "", "apply_patch"));
    feed.line(&patch_fragment(0, r#"{"input":"x"}"#));
    let item = completed_item(&feed.end());
    assert!(
        text_at(&item, "type") == "custom_tool_call"
            && text_at(&item, "input") == "x"
            && !text_at(&item, "call_id").is_empty(),
        "missing ID lost patch: {item}"
    );
}

#[test]
fn apply_patch_chat_deferred_identity_conflict() {
    // The name, the request, the ID and name the call settles on, the item
    // type wanted, and whether the response fails.
    let cases = [
        (
            "return to first ID",
            PATCH_REQUEST,
            "c1",
            "apply_patch",
            "",
            true,
        ),
        (
            "keep second ID",
            PATCH_REQUEST,
            "c2",
            "apply_patch",
            "",
            true,
        ),
        ("omit final ID", PATCH_REQUEST, "", "apply_patch", "", true),
        ("infer name at terminal", PATCH_REQUEST, "c1", "", "", true),
        (
            "ordinary function",
            r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#,
            "c1",
            "apply_patch",
            "function_call",
            false,
        ),
        (
            "function winner",
            r#"{"tools":[{"type":"function","name":"apply_patch"},{"type":"custom","name":"apply_patch"}]}"#,
            "c1",
            "apply_patch",
            "function_call",
            false,
        ),
        (
            "other custom",
            r#"{"tools":[{"type":"custom","name":"edit"}]}"#,
            "c1",
            "edit",
            "custom_tool_call",
            false,
        ),
    ];

    for (name, request, resolved_id, resolved_name, want_type, invalid) in cases {
        let mut feed = PatchFeed::new(request);
        feed.line(&patch_start(0, "c1", ""));
        let pending = feed.line(
            r#"data: {"id":"r1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"","arguments":"{\"input\":\"x\"}"}}]}}]}"#,
        );
        assert!(
            pending.is_empty(),
            "{name}: unclassified call emitted events: {pending:?}"
        );
        let mut events = feed.line(&patch_start(0, resolved_id, resolved_name));
        events.extend(feed.end());

        let (mut failures, mut completions) = (0, 0);
        let mut item = Value::Null;
        for event in &events {
            match kind(event).as_str() {
                "response.failed" => {
                    failures += 1;
                    assert!(
                        invalid
                            && text_at(event, "response.error.code") == "invalid_tool_arguments",
                        "{name}: unexpected failure: {event}"
                    );
                }
                "response.completed" => {
                    completions += 1;
                    item = event["response"]["output"][0].clone();
                }
                "response.incomplete"
                | "response.custom_tool_call_input.done"
                | "response.output_item.done" => {
                    assert!(
                        !invalid,
                        "{name}: conflicting pending identity succeeded: {event}"
                    );
                }
                _ => {}
            }
        }
        if invalid {
            assert!(
                failures == 1 && completions == 0 && feed.error().is_some(),
                "{name}: lost pending conflict: failures={failures} completions={completions}"
            );
            continue;
        }
        assert!(
            completions == 1
                && feed.error().is_none()
                && text_at(&item, "type") == want_type
                && text_at(&item, "call_id") == "c1",
            "{name}: unrelated tool changed: {item}"
        );
        if want_type == "function_call" {
            assert_eq!(
                text_at(&item, "arguments"),
                r#"{"input":"x"}"#,
                "{name}: ordinary arguments changed: {item}"
            );
        } else {
            assert_eq!(
                text_at(&item, "input"),
                "x",
                "{name}: other custom input changed: {item}"
            );
        }
    }
}

#[test]
fn apply_patch_chat_successful_terminal_seals_state() {
    let late_fragment = patch_fragment(0, r#"{"input":"late"}"#);
    for (name, late) in [
        ("duplicate DONE", PATCH_END[1]),
        ("post-terminal fragment", late_fragment.as_str()),
    ] {
        let mut feed = PatchFeed::new(PATCH_REQUEST);
        feed.line(&patch_start(0, "c1", "apply_patch"));
        feed.line(&patch_fragment(0, r#"{"input":"x"}"#));
        for event in feed.line(PATCH_END[0]) {
            assert_ne!(
                kind(&event),
                "response.completed",
                "{name}: finish_reason prematurely sealed response: {event}"
            );
        }
        feed.line(
            r#"data: {"id":"r1","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#,
        );
        let events = feed.line(PATCH_END[1]);
        assert!(
            events.len() == 1
                && kind(&events[0]) == "response.completed"
                && text_at(&events[0], "response.output.0.input") == "x"
                && int_at(&events[0], "response.usage.total_tokens") == 15,
            "{name}: missing successful completion or late usage: {events:?}"
        );
        for line in [late, PATCH_END[1]] {
            let more = feed.line(line);
            assert!(
                more.is_empty(),
                "{name}: sealed response emitted more events: {more:?}"
            );
        }
        assert!(
            feed.error().is_none(),
            "{name}: successful terminal became a failure"
        );
    }
}

// From custom_tool_namespace_recovery_test.go.

/// `namespaceRecoveryRequest`.
const NAMESPACE_RECOVERY_REQUEST: &str = r#"{"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"},{"type":"function","name":"wait"}]}]}]}"#;

/// `assertRecoveredCustomCall`.
fn assert_recovered_custom_call(item: &Value, with_input: bool) {
    for (key, want) in [
        ("type", "custom_tool_call"),
        ("name", "exec"),
        ("namespace", "functions"),
        ("call_id", "call_fixture"),
    ] {
        assert_eq!(text_at(item, key), want, "{key}: item={item}");
    }
    if with_input {
        assert_eq!(
            text_at(item, "input"),
            "text(\"测试\\n\");",
            "input changed: {item}"
        );
    }
    assert!(
        item.get("arguments").is_none(),
        "custom call retained JSON arguments: {item}"
    );
}

#[test]
fn custom_tool_namespace_recovery_preserves_stream_and_non_stream() {
    for name in ["functions__exec", "exec"] {
        let args = r#"{"input":"text(\"测试\\n\");"}"#;
        let call = format!(
            r#"{{"index":0,"id":"call_fixture","type":"function","function":{{"name":{},"arguments":{}}}}}"#,
            quote(name),
            quote(args)
        );
        let raw = format!(
            r#"{{"id":"fixture","choices":[{{"index":0,"message":{{"tool_calls":[{call}]}},"finish_reason":"tool_calls"}}]}}"#
        );
        let response = convert(NAMESPACE_RECOVERY_REQUEST, "", &raw);
        assert_recovered_custom_call(&response["output"][0], true);

        let mut feed = Feed::new("fixture", NAMESPACE_RECOVERY_REQUEST, "");
        let mut counts: HashMap<String, usize> = HashMap::new();
        for line in [
            format!(
                r#"data: {{"id":"fixture","choices":[{{"index":0,"delta":{{"tool_calls":[{call}]}},"finish_reason":"tool_calls"}}]}}"#
            ),
            "data: [DONE]".to_owned(),
        ] {
            for (event, data) in feed.line(&line) {
                *counts.entry(event.clone()).or_default() += 1;
                match event.as_str() {
                    "response.output_item.added" | "response.output_item.done" => {
                        assert_recovered_custom_call(
                            &data["item"],
                            event == "response.output_item.done",
                        );
                    }
                    "response.completed" => {
                        assert_recovered_custom_call(&data["response"]["output"][0], true);
                    }
                    _ => assert!(
                        !event.starts_with("response.function_call_arguments."),
                        "{name}: custom call downgraded: {data}"
                    ),
                }
            }
        }
        for event in [
            "response.output_item.added",
            "response.output_item.done",
            "response.completed",
            "response.custom_tool_call_input.done",
        ] {
            assert_eq!(counts.get(event), Some(&1), "{name}: {event} count");
        }
    }
}

// From responses_request_state_test.go.

#[test]
fn responses_request_selection_and_stream_isolation() {
    let with_namespace = |namespace: &str| {
        request(&format!(
            r#"{{"tools":[{{"type":"namespace","name":{},"tools":[{{"type":"function","name":"run"}}]}}]}}"#,
            quote(namespace)
        ))
    };
    // The client's request, the translated one, and the namespace wanted.
    let cases = [
        (
            with_namespace("original"),
            with_namespace("translated"),
            "original",
        ),
        (
            request(r#"{"broken":"#),
            with_namespace("fallback"),
            "fallback",
        ),
        (Value::Null, with_namespace("separate"), "separate"),
        (request("invalid"), request("invalid"), ""),
    ];
    let start =
        br#"data: {"id":"r","created":1,"choices":[{"index":0,"delta":{"role":"assistant"}}]}"#;
    let tool = br#"data: {"id":"r","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#;
    let mut streams: Vec<_> = cases
        .iter()
        .map(|(original, translated, _)| {
            OpenAIToOpenAIResponsesStream::new("test", original, translated)
        })
        .collect();
    // Interleave independent requests to catch accidental global cache reuse.
    for stream in &mut streams {
        stream.translate_line(start);
    }
    for (i, (stream, (_, _, namespace))) in streams.iter_mut().zip(&cases).enumerate() {
        let mut all = stream.translate_line(tool);
        all.push_str(&stream.translate_line(b"data: [DONE]"));
        assert!(
            all.contains(r#""name":"run""#),
            "case {i} lost tool identity"
        );
        if namespace.is_empty() {
            assert!(
                !all.contains(r#""namespace":"#),
                "invalid requests acquired another stream's namespace"
            );
        } else {
            assert!(
                all.contains(&format!(r#""namespace":{}"#, quote(namespace))),
                "case {i} lost namespace {namespace:?}"
            );
        }
    }
}

/// Not from upstream: an executor finalizes the state the first line made,
/// so a stream with no lines at all ends with nothing, even for a request
/// that declares `apply_patch`. One empty line is enough to make the state.
#[test]
fn finalize_without_lines_gives_nothing() {
    let request = parse(PATCH_REQUEST);
    let mut stream = OpenAIToOpenAIResponsesStream::new("test", &request, &Value::Null);
    assert_eq!(stream.finalize_tool_input(), "");
    assert!(stream.tool_input_error().is_none());

    let mut stream = OpenAIToOpenAIResponsesStream::new("test", &request, &Value::Null);
    assert_eq!(stream.translate_line(b""), "");
    assert!(
        stream
            .finalize_tool_input()
            .starts_with("event: response.failed\n")
    );
    assert!(stream.tool_input_error().is_some());
}

#[test]
fn stream_echo_keeps_negative_zero() {
    // Not upstream's: upstream writes the repeated `temperature` and `top_p`
    // as float64s and the tools as `Value()` gives them, so negative zero as
    // `-0`; so must the completed event.
    let original_request = crate::json::exact::from_str(
        r#"{"temperature":-0,"top_p":-0.0,"tools":[{"type":"function","name":"f","parameters":{"minimum":-0,"maximum":1E20}}]}"#,
    )
    .unwrap();
    let mut stream = OpenAIToOpenAIResponsesStream::new("m", &original_request, &Value::Null);
    let mut out = stream.translate_line(
        br#"data: {"id":"c","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
    );
    out.push_str(&stream.translate_line(b"data: [DONE]"));
    let completed = out
        .split("\n\n")
        .find(|frame| frame.starts_with("event: response.completed"))
        .expect("the stream completes");
    for want in [
        r#""temperature":-0,"#,
        r#""top_p":-0,"#,
        r#""parameters":{"maximum":100000000000000000000,"minimum":-0}"#,
    ] {
        assert!(completed.contains(want), "{want} in {completed}");
    }
}

/// Not upstream's: the translator side of v8.0.20's
/// `TestOpenAICompatExecutorResponsesEOFAfterFinishReasonCompletesStream`.
/// Once a finish reason closed the message, a stream that ends without
/// `[DONE]` can be finished by sending `[DONE]`, and late usage still counts.
#[test]
fn eof_after_a_finish_reason_can_finish_the_stream() {
    let mut feed = Feed::new("test", r#"{"model":"test"}"#, "");
    feed.line(r#"data: {"id":"chatcmpl-eof","object":"chat.completion.chunk","created":1773896263,"model":"test","choices":[{"index":0,"delta":{"role":"assistant","content":"done"},"finish_reason":null}]}"#);
    assert!(!feed.0.can_finalize_response_stream());
    feed.line(r#"data: {"id":"chatcmpl-eof","object":"chat.completion.chunk","created":1773896263,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#);
    assert!(feed.0.can_finalize_response_stream());
    feed.line(r#"data: {"id":"chatcmpl-eof","object":"chat.completion.chunk","created":1773896263,"model":"test","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#);
    assert!(feed.0.can_finalize_response_stream());

    let out = feed.line("data: [DONE]");
    assert_eq!(count(&out, "response.completed"), 1);
    let completed = last(&out, "response.completed");
    assert_eq!(int_at(completed, "response.usage.input_tokens"), 3);
    assert_eq!(int_at(completed, "response.usage.output_tokens"), 1);
    assert!(!feed.0.can_finalize_response_stream());
}

/// Not upstream's: a stream can't be finished without `[DONE]` before a
/// finish reason, or with nothing but reasoning. A finish reason that closes
/// a tool call can finish it.
#[test]
fn eof_without_a_closing_finish_reason_cannot_finish_the_stream() {
    let mut feed = Feed::new("test", "", "");
    feed.line(
        r#"data: {"id":"r","created":1,"choices":[{"index":0,"delta":{"content":"partial"}}]}"#,
    );
    assert!(!feed.0.can_finalize_response_stream());

    let mut feed = Feed::new("test", "", "");
    feed.line(r#"data: {"id":"r","created":1,"choices":[{"index":0,"delta":{"reasoning_content":"think"},"finish_reason":"stop"}]}"#);
    assert!(!feed.0.can_finalize_response_stream());

    let mut feed = Feed::new("test", "", "");
    feed.line(r#"data: {"id":"r","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"run","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#);
    assert!(feed.0.can_finalize_response_stream());
}

/// Not upstream's: the translator side of v8.0.20's
/// `TestOpenAICompatExecutorResponsesEOFAfterApplyPatchFinishReasonCompletesStream`.
/// An `apply_patch` call that a finish reason closed doesn't fail the stream
/// at its end; one still open does.
#[test]
fn eof_after_a_finished_patch_call_does_not_fail() {
    let request = parse(PATCH_REQUEST);
    let start = patch_start(0, "call_p", "apply_patch");
    let fragment = patch_fragment(
        0,
        r#"{"input":"*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch\n"}"#,
    );

    let mut stream = OpenAIToOpenAIResponsesStream::new("test", &request, &Value::Null);
    stream.translate_line(start.as_bytes());
    stream.translate_line(fragment.as_bytes());
    stream.translate_line(PATCH_END[0].as_bytes());
    assert!(stream.can_finalize_response_stream());
    assert_eq!(stream.finalize_tool_input(), "");
    assert!(stream.tool_input_error().is_none());
    let out = events(&stream.translate_line(b"data: [DONE]"));
    assert_eq!(count(&out, "response.completed"), 1);
    assert_eq!(count(&out, "response.failed"), 0);

    let mut stream = OpenAIToOpenAIResponsesStream::new("test", &request, &Value::Null);
    stream.translate_line(start.as_bytes());
    stream.translate_line(fragment.as_bytes());
    assert!(!stream.can_finalize_response_stream());
    assert!(
        stream
            .finalize_tool_input()
            .starts_with("event: response.failed\n")
    );
    assert!(!stream.can_finalize_response_stream());
}
