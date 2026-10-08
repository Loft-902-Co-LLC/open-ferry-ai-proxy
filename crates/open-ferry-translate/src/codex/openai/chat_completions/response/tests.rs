// Ported from CLIProxyAPI internal/translator/codex/openai/chat-completions/codex_openai_response_test.go
// and noop_optimization_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::Value;

use super::*;
use crate::codex::openai::chat_completions::convert_openai_chat_completions_request_to_codex;

fn parse(json: &str) -> Value {
    serde_json::from_str(json).expect("test JSON is valid")
}

/// A stream for a client request that Go's tests leave nil.
fn new_stream(model: &str) -> CodexToOpenAIChatCompletionsStream {
    CodexToOpenAIChatCompletionsStream::new(model, &Value::Null)
}

/// Feeds `lines` through one stream and returns the chunks it emits.
fn run_stream(model: &str, original_request: &Value, lines: &[String]) -> Vec<Value> {
    let mut stream = CodexToOpenAIChatCompletionsStream::new(model, original_request);
    lines
        .iter()
        .filter_map(|line| stream.translate_line(line.as_bytes()))
        .collect()
}

fn non_stream(original_request: &Value, event: &str) -> Value {
    convert_codex_response_to_openai_chat_completions_non_stream(original_request, &parse(event))
        .expect("terminal event converts")
}

/// Looks up a dotted path such as `choices.0.delta.tool_calls.0.index`, like a
/// plain gjson path.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Object(map) => map.get(key),
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => None,
    })
}

/// The value at `path` as gjson's `String()` would return it.
fn text_at(value: &Value, path: &str) -> String {
    str_of(at(value, path)).into_owned()
}

/// The value at `path` as gjson's `Int()` would return it.
fn int_at(value: &Value, path: &str) -> i64 {
    at(value, path).map_or(0, int_of)
}

/// The value at `path` as gjson's `Raw` would return it.
fn raw_at(value: &Value, path: &str) -> String {
    at(value, path).map_or_else(String::new, Value::to_string)
}

/// Checks the usage counts the cache write tests share, and that
/// `cached_creation_tokens` and `cache_write_tokens` are `want_cached_creation`,
/// or absent if it is `None`.
fn assert_usage_mapping(payload: &Value, want_cached_creation: Option<i64>) {
    for (path, want) in [
        ("usage.prompt_tokens", 100),
        ("usage.completion_tokens", 20),
        ("usage.total_tokens", 120),
        ("usage.prompt_tokens_details.cached_tokens", 30),
        ("usage.completion_tokens_details.reasoning_tokens", 5),
    ] {
        assert_eq!(int_at(payload, path), want, "{path}; payload={payload}");
    }

    let paths = [
        "usage.prompt_tokens_details.cached_creation_tokens",
        "usage.prompt_tokens_details.cache_write_tokens",
    ];
    for path in paths {
        let got = at(payload, path);
        match want_cached_creation {
            Some(want) => {
                assert!(got.is_some(), "expected {path} to exist, payload={payload}");
                assert_eq!(got.map_or(0, int_of), want, "{path}; payload={payload}");
            }
            None => assert!(
                got.is_none(),
                "expected {path} to be omitted, payload={payload}"
            ),
        }
    }
}

#[test]
fn convert_codex_response_to_openai_incomplete_terminal() {
    let terminal = r#"{"type":"response.incomplete","response":{"id":"resp_1","model":"gpt-5.5","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#;

    let mut stream = new_stream("gpt-5.5");
    let stream_out = stream
        .translate_line(format!("data: {terminal}").as_bytes())
        .expect("expected 1 streaming terminal chunk");
    assert_eq!(
        text_at(&stream_out, "choices.0.finish_reason"),
        "length",
        "stream finish_reason; payload={stream_out}"
    );
    assert_eq!(
        text_at(&stream_out, "choices.0.native_finish_reason"),
        "max_output_tokens",
        "stream native_finish_reason; payload={stream_out}"
    );

    let mut tool_stream = new_stream("gpt-5.5");
    tool_stream.translate_line(br#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"lookup"}}"#);
    let tool_stream_out = tool_stream
        .translate_line(format!("data: {terminal}").as_bytes())
        .expect("expected a tool stream terminal chunk");
    assert_eq!(
        text_at(&tool_stream_out, "choices.0.finish_reason"),
        "length",
        "tool stream finish_reason; payload={tool_stream_out}"
    );

    let non_stream_out = non_stream(&Value::Null, terminal);
    assert_eq!(
        text_at(&non_stream_out, "choices.0.finish_reason"),
        "length",
        "non-stream finish_reason; payload={non_stream_out}"
    );
}

#[test]
fn convert_codex_response_to_openai_stream_sets_model_from_response_created() {
    let model_name = "gpt-5.3-codex";
    let mut stream = new_stream(model_name);

    let out = stream.translate_line(br#"data: {"type":"response.created","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.3-codex"}}"#);
    assert!(
        out.is_none(),
        "expected no output for response.created, got {out:?}"
    );

    let out = stream
        .translate_line(br#"data: {"type":"response.output_text.delta","delta":"hello"}"#)
        .expect("expected 1 chunk");
    assert_eq!(text_at(&out, "model"), model_name, "chunk={out}");
}

#[test]
fn convert_codex_response_to_openai_first_chunk_uses_request_model_name() {
    let model_name = "gpt-5.3-codex";
    let mut stream = new_stream(model_name);

    let out = stream
        .translate_line(br#"data: {"type":"response.output_text.delta","delta":"hello"}"#)
        .expect("expected 1 chunk");
    assert_eq!(text_at(&out, "model"), model_name, "chunk={out}");
}

/// `TestConvertCodexResponseToOpenAI_PreservesURLCitations`, "non-stream".
#[test]
fn convert_codex_response_to_openai_preserves_url_citations_non_stream() {
    let raw = r#"{"type":"response.completed","response":{"id":"resp_citation","model":"gpt-5.5","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"前🙂"},{"type":"output_text","text":"引用","annotations":[{"type":"url_citation","url":"https://example.com","title":"Example","start_index":0,"end_index":2},{"type":"file_citation","file_id":"file_1","index":0}]}]}]}}"#;
    let out = non_stream(&Value::Null, raw);

    assert_eq!(
        text_at(&out, "choices.0.message.content"),
        "前🙂引用",
        "response={out}"
    );
    assert!(
        at(&out, "choices.0.message.annotations.0").is_some(),
        "expected message annotation, response={out}"
    );
    let count = at(&out, "choices.0.message.annotations").and_then(Value::as_array);
    assert_eq!(
        count.map(Vec::len),
        Some(1),
        "annotation count after filtering unsupported types; response={out}"
    );
    let annotation = "choices.0.message.annotations.0";
    assert_eq!(
        text_at(&out, &format!("{annotation}.type")),
        "url_citation",
        "response={out}"
    );
    assert_eq!(
        text_at(&out, &format!("{annotation}.url")),
        "https://example.com",
        "response={out}"
    );
    assert_eq!(
        text_at(&out, &format!("{annotation}.title")),
        "Example",
        "response={out}"
    );
    assert_eq!(
        int_at(&out, &format!("{annotation}.start_index")),
        2,
        "response={out}"
    );
    assert_eq!(
        int_at(&out, &format!("{annotation}.end_index")),
        4,
        "response={out}"
    );
}

/// `TestConvertCodexResponseToOpenAI_PreservesURLCitations`, "stream".
#[test]
fn convert_codex_response_to_openai_preserves_url_citations_stream() {
    let mut stream = new_stream("gpt-5.5");
    assert!(
        stream
            .translate_line(
                r#"data: {"type":"response.output_text.delta","delta":"前🙂"}"#.as_bytes()
            )
            .is_some(),
        "expected text delta chunk"
    );

    let out = stream
        .translate_line(br#"data: {"type":"response.output_text.annotation.added","annotation_index":0,"annotation":{"type":"url_citation","url":"https://example.com","title":"Example","start_index":0,"end_index":1}}"#)
        .expect("expected citation chunk");
    let annotation = "choices.0.delta.annotations.0";
    assert!(
        at(&out, annotation).is_some(),
        "expected delta annotation, chunk={out}"
    );
    assert_eq!(
        text_at(&out, &format!("{annotation}.type")),
        "url_citation",
        "chunk={out}"
    );
    assert_eq!(
        int_at(&out, &format!("{annotation}.start_index")),
        2,
        "chunk={out}"
    );
    assert_eq!(
        int_at(&out, &format!("{annotation}.end_index")),
        3,
        "chunk={out}"
    );

    assert!(
        stream
            .translate_line(
                r#"data: {"type":"response.output_text.delta","delta":"引用"}"#.as_bytes()
            )
            .is_some(),
        "expected second text delta chunk"
    );
    let out = stream.translate_line(br#"data: {"type":"response.output_text.annotation.added","annotation_index":0,"annotation":{"type":"url_citation","url":"https://example.com","title":"Example","start_index":0,"end_index":2}}"#);
    assert!(
        out.is_none(),
        "expected duplicate citation to be suppressed after more text, got {out:?}"
    );
}

/// `TestConvertCodexResponseToOpenAI_PreservesURLCitations`, "stream completion
/// annotation".
#[test]
fn convert_codex_response_to_openai_preserves_url_citations_stream_completion_annotation() {
    let mut stream = new_stream("gpt-5.5");
    for delta in ["前🙂", "引用"] {
        let line = format!(
            r#"data: {{"type":"response.output_text.delta","delta":{}}}"#,
            Value::from(delta)
        );
        assert!(
            stream.translate_line(line.as_bytes()).is_some(),
            "expected text delta chunk"
        );
    }

    let out = stream
        .translate_line(r#"data: {"type":"response.output_text.done","text":"前🙂引用","annotations":[{"type":"url_citation","url":"https://example.com","title":"Example","start_index":0,"end_index":2}]}"#.as_bytes())
        .expect("expected citation completion chunk");
    assert_eq!(
        int_at(&out, "choices.0.delta.annotations.0.start_index"),
        4,
        "chunk={out}"
    );
    assert_eq!(
        int_at(&out, "choices.0.delta.annotations.0.end_index"),
        6,
        "chunk={out}"
    );

    let out = stream
        .translate_line(r#"data: {"type":"response.content_part.done","part":{"type":"output_text","text":"前🙂引用","annotations":[{"type":"url_citation","url":"https://other.example","title":"Other","start_index":0,"end_index":1}]}}"#.as_bytes())
        .expect("expected content-part citation chunk");
    assert_eq!(
        text_at(&out, "choices.0.delta.annotations.0.url"),
        "https://other.example",
        "chunk={out}"
    );
    assert_eq!(
        int_at(&out, "choices.0.delta.annotations.0.start_index"),
        4,
        "chunk={out}"
    );

    let out = stream.translate_line(r#"data: {"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"前🙂引用","annotations":[{"type":"url_citation","url":"https://example.com","title":"Example","start_index":0,"end_index":2}]}]}}"#.as_bytes());
    assert!(
        out.is_none(),
        "expected duplicate completion citation to be suppressed, got {out:?}"
    );
}

/// Not upstream's: citations across messages count the text of the messages
/// before, a repeated ID or URL and start is left out, a citation without a
/// URL is known by its ID, a negative or backwards range is dropped, a lone
/// annotation counts as a list of one, and `annotations` goes after
/// `tool_calls` and before `images`, where sjson puts it. Checked against
/// upstream with a Go probe.
#[test]
fn non_stream_url_citations_across_messages() {
    let raw = [
        r#"{"type":"response.completed","response":{"id":"r","model":"m","created_at":1,"status":"completed","output":["#,
        r#"{"type":"message","content":[{"type":"output_text","text":"ab","annotations":{"type":"url_citation","url":"https://a.example","title":"A","start_index":0,"end_index":2,"id":"c1"}}]},"#,
        r#"{"type":"function_call","call_id":"call_1","name":"f","arguments":"{}"},"#,
        r#"{"type":"message","content":[{"type":"refusal","refusal":"no"},{"type":"output_text","text":"cd","annotations":["#,
        r#"{"type":"url_citation","url":"https://b.example","start_index":0,"end_index":1,"id":"c1"},"#,
        r#"{"type":"url_citation","id":"c2","title":5,"start_index":"1","end_index":2},"#,
        r#"{"type":"url_citation","url":"https://c.example","start_index":-3,"end_index":1},"#,
        r#"{"type":"url_citation","url":"https://d.example","start_index":1,"end_index":0},"#,
        r#"{"type":"url_citation","url":"https://a.example","start_index":0,"end_index":1}"#,
        r#"]}]},"#,
        r#"{"type":"image_generation_call","result":"iVBOR","output_format":"png"}"#,
        r#"]}}"#,
    ]
    .concat();
    let out = non_stream(&Value::Null, &raw);

    assert_eq!(text_at(&out, "choices.0.message.content"), "abcd");
    assert_eq!(
        raw_at(&out, "choices.0.message.annotations"),
        r#"[{"type":"url_citation","url":"https://a.example","title":"A","start_index":0,"end_index":2},{"type":"url_citation","url":"","title":"5","start_index":3,"end_index":4}]"#
    );
    let keys: Vec<&String> = at(&out, "choices.0.message")
        .and_then(Value::as_object)
        .map(|message| message.keys().collect())
        .unwrap_or_default();
    assert_eq!(
        keys,
        [
            "role",
            "content",
            "reasoning_content",
            "tool_calls",
            "annotations",
            "images"
        ]
    );
}

/// Not upstream's: a stream sends each citation once, from wherever the event
/// carries it, placed after the text sent so far, counting a non-string text
/// delta by its text; a message's `output_item.done` with nothing new sends
/// nothing. Checked against upstream with a Go probe.
#[test]
fn stream_url_citations_from_every_place() {
    let lines = [
        r#"data: {"type":"response.output_text.delta","delta":"héllo"}"#,
        r#"data: {"type":"response.output_text.annotation.added","annotation":{"type":"file_citation"}}"#,
        r#"data: {"type":"response.content_part.done","part":{"annotations":[{"type":"url_citation","url":"u","start_index":0,"end_index":5}]}}"#,
        r#"data: {"type":"response.output_item.done","item":{"type":"message","annotations":[{"type":"url_citation","url":"v","start_index":1,"end_index":2}],"content":[{"annotations":{"type":"url_citation","url":"u","start_index":0,"end_index":5}}]}}"#,
        r#"data: {"type":"response.output_item.done","item":{"type":"message","content":[]}}"#,
        r#"data: {"type":"response.output_text.delta","delta":1.50}"#,
        r#"data: {"type":"response.output_text.done","annotation":{"type":"url_citation","url":"w","start_index":0,"end_index":0}}"#,
    ];
    let mut stream = new_stream("m");
    let deltas: Vec<Option<String>> = lines
        .iter()
        .map(|line| {
            stream
                .translate_line(line.as_bytes())
                .map(|chunk| raw_at(&chunk, "choices.0.delta"))
        })
        .collect();
    let citation = |url: &str, start: i64, end: i64| {
        Some(format!(
            r#"{{"role":"assistant","annotations":[{{"type":"url_citation","url":"{url}","title":"","start_index":{start},"end_index":{end}}}]}}"#
        ))
    };
    assert_eq!(
        deltas,
        [
            Some(r#"{"role":"assistant","content":"héllo"}"#.to_owned()),
            None,
            citation("u", 5, 10),
            citation("v", 6, 7),
            None,
            Some(r#"{"role":"assistant","content":"1.5"}"#.to_owned()),
            citation("w", 8, 8),
        ]
    );
}

#[test]
fn convert_codex_response_to_openai_tool_call_chunk_omits_null_content_fields() {
    let mut stream = new_stream("gpt-5.4");

    let out = stream
        .translate_line(br#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_123","name":"websearch"}}"#)
        .expect("expected 1 chunk");
    assert!(
        at(&out, "choices.0.delta.content").is_none(),
        "expected content to be omitted, got {out}"
    );
    assert!(
        at(&out, "choices.0.delta.reasoning_content").is_none(),
        "expected reasoning_content to be omitted, got {out}"
    );
    assert!(
        at(&out, "choices.0.delta.tool_calls").is_some(),
        "expected tool_calls to exist, got {out}"
    );
}

#[test]
fn convert_codex_response_to_openai_tool_call_arguments_delta_omits_null_content_fields() {
    let mut stream = new_stream("gpt-5.4");

    stream
        .translate_line(br#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_123","name":"websearch"}}"#)
        .expect("expected tool call announcement chunk");

    let out = stream
        .translate_line(br#"data: {"type":"response.function_call_arguments.delta","delta":"{\"query\":\"OpenAI\"}"}"#)
        .expect("expected 1 chunk");
    assert!(
        at(&out, "choices.0.delta.content").is_none(),
        "expected content to be omitted, got {out}"
    );
    assert!(
        at(&out, "choices.0.delta.reasoning_content").is_none(),
        "expected reasoning_content to be omitted, got {out}"
    );
    assert!(
        at(&out, "choices.0.delta.tool_calls.0.function.arguments").is_some(),
        "expected tool call arguments delta to exist, got {out}"
    );
}

#[test]
fn convert_codex_response_to_openai_custom_tool_call_stream_deltas() {
    let mut stream = new_stream("gpt-5.5");
    let mut send = |event: &str| stream.translate_line(format!("data: {event}").as_bytes());

    let out = send(r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_apply","name":"ApplyPatch","input":"unexpected input"}}"#)
        .expect("expected 1 announcement chunk");
    let tool_call = at(&out, "choices.0.delta.tool_calls.0").unwrap_or(&NONE);
    assert_eq!(int_at(tool_call, "index"), 0, "tool index; chunk={out}");
    assert_eq!(
        text_at(tool_call, "id"),
        "call_apply",
        "call id; chunk={out}"
    );
    assert_eq!(
        text_at(tool_call, "function.name"),
        "ApplyPatch",
        "tool name; chunk={out}"
    );
    assert!(
        at(tool_call, "function.arguments").is_some()
            && text_at(tool_call, "function.arguments").is_empty(),
        "expected empty announced arguments, got {}; chunk={out}",
        raw_at(tool_call, "function.arguments")
    );

    for delta in ["*** Begin Patch\n", "*** End Patch"] {
        let out = send(
            &[
                r#"{"type":"response.custom_tool_call_input.delta","delta":"#,
                &go::json_string(delta),
                "}",
            ]
            .concat(),
        )
        .expect("expected 1 arguments delta chunk");
        assert_eq!(
            text_at(&out, "choices.0.delta.tool_calls.0.function.arguments"),
            delta,
            "arguments delta; chunk={out}"
        );
    }

    let full_input = "*** Begin Patch\n*** End Patch";
    let out = send(
        &[
            r#"{"type":"response.custom_tool_call_input.done","input":"#,
            &go::json_string(full_input),
            "}",
        ]
        .concat(),
    );
    assert!(
        out.is_none(),
        "expected custom input done to be suppressed after deltas, got {out:?}"
    );
    let out = send(&[r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_apply","name":"ApplyPatch","input":"#, &go::json_string(full_input), "}}"].concat());
    assert!(
        out.is_none(),
        "expected output item done to be suppressed after deltas, got {out:?}"
    );

    let out = send(r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#)
        .expect("expected 1 completion chunk");
    assert_eq!(
        text_at(&out, "choices.0.finish_reason"),
        "tool_calls",
        "finish reason; chunk={out}"
    );
}

#[test]
fn convert_codex_response_to_openai_empty_custom_tool_delta_uses_done_fallback() {
    let mut stream = new_stream("gpt-5.5");

    stream.translate_line(br#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_apply","name":"ApplyPatch","input":""}}"#);
    let out = stream.translate_line(br#"data: {"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","output_index":0,"delta":""}"#);
    assert!(
        out.is_none(),
        "expected empty delta to be suppressed, got {out:?}"
    );

    let out = stream
        .translate_line(br#"data: {"type":"response.custom_tool_call_input.done","item_id":"ctc_1","output_index":0,"input":"full patch"}"#)
        .expect("expected 1 done fallback chunk");
    assert_eq!(
        text_at(&out, "choices.0.delta.tool_calls.0.function.arguments"),
        "full patch",
        "chunk={out}"
    );
}

#[test]
fn convert_codex_response_to_openai_interleaved_tool_calls_keep_state_by_item() {
    let mut stream = new_stream("gpt-5.5");
    let mut send = |event: &str| stream.translate_line(format!("data: {event}").as_bytes());

    let out = send(r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_lookup","name":"lookup","arguments":""}}"#)
        .expect("expected function call announcement");
    assert_eq!(
        int_at(&out, "choices.0.delta.tool_calls.0.index"),
        0,
        "function call index; chunk={out}"
    );
    let out = send(r#"{"type":"response.output_item.added","output_index":1,"item":{"id":"ctc_2","type":"custom_tool_call","call_id":"call_apply","name":"ApplyPatch","input":""}}"#)
        .expect("expected custom call announcement");
    assert_eq!(
        int_at(&out, "choices.0.delta.tool_calls.0.index"),
        1,
        "custom call index; chunk={out}"
    );

    let out = send(r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"query\":"}"#)
        .expect("expected function arguments delta");
    assert_eq!(
        int_at(&out, "choices.0.delta.tool_calls.0.index"),
        0,
        "interleaved function delta index; chunk={out}"
    );
    let out =
        send(r#"{"type":"response.custom_tool_call_input.delta","output_index":1,"delta":""}"#);
    assert!(
        out.is_none(),
        "expected empty custom delta to be suppressed, got {out:?}"
    );
    let out =
        send(r#"{"type":"response.custom_tool_call_input.done","output_index":1,"input":"patch"}"#)
            .expect("expected custom done fallback");
    assert_eq!(
        int_at(&out, "choices.0.delta.tool_calls.0.index"),
        1,
        "output-index-routed custom fallback index; chunk={out}"
    );
    assert_eq!(
        text_at(&out, "choices.0.delta.tool_calls.0.function.arguments"),
        "patch",
        "custom fallback arguments; chunk={out}"
    );

    for event in [
        r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":0,"arguments":"{\"query\":\"test\"}"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_lookup","name":"lookup","arguments":"{\"query\":\"test\"}"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"id":"ctc_2","type":"custom_tool_call","call_id":"call_apply","name":"ApplyPatch","input":"patch"}}"#,
    ] {
        let out = send(event);
        assert!(
            out.is_none(),
            "expected terminal tool event to avoid duplicate output for {event}, got {out:?}"
        );
    }
}

#[test]
fn convert_codex_response_to_openai_custom_tool_call_input_done_fallback() {
    let mut stream = new_stream("gpt-5.5");

    stream.translate_line(br#"data: {"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_apply","name":"ApplyPatch","input":""}}"#);
    let out = stream
        .translate_line(
            br#"data: {"type":"response.custom_tool_call_input.done","input":"full patch"}"#,
        )
        .expect("expected 1 fallback arguments chunk");
    assert_eq!(
        text_at(&out, "choices.0.delta.tool_calls.0.function.arguments"),
        "full patch",
        "chunk={out}"
    );

    let out = stream.translate_line(br#"data: {"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_apply","name":"ApplyPatch","input":"full patch"}}"#);
    assert!(
        out.is_none(),
        "expected output item done to be suppressed after input done fallback, got {out:?}"
    );
}

#[test]
fn convert_codex_response_to_openai_tool_call_output_item_done_fallbacks() {
    // announced custom call emits arguments only
    let mut stream = new_stream("gpt-5.5");
    stream.translate_line(br#"data: {"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_first","name":"ApplyPatch","input":""}}"#);
    let out = stream
        .translate_line(br#"data: {"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_first","name":"ApplyPatch","input":"first patch"}}"#)
        .expect("expected 1 fallback arguments chunk");
    let tool_call = at(&out, "choices.0.delta.tool_calls.0").unwrap_or(&NONE);
    assert_eq!(int_at(tool_call, "index"), 0, "tool index; chunk={out}");
    assert!(
        at(tool_call, "id").is_none() && at(tool_call, "function.name").is_none(),
        "expected arguments-only fallback, got {tool_call}"
    );
    assert_eq!(
        text_at(tool_call, "function.arguments"),
        "first patch",
        "chunk={out}"
    );

    stream.translate_line(br#"data: {"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_second","name":"ApplyPatch","input":""}}"#);
    let out = stream
        .translate_line(br#"data: {"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_second","name":"ApplyPatch","input":"second patch"}}"#)
        .expect("expected 1 second fallback arguments chunk");
    assert_eq!(
        int_at(&out, "choices.0.delta.tool_calls.0.index"),
        1,
        "second tool index; chunk={out}"
    );

    // unannounced custom call emits complete call
    let mut stream = new_stream("gpt-5.5");
    let out = stream
        .translate_line(br#"data: {"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_apply","name":"ApplyPatch","input":"full patch"}}"#)
        .expect("expected 1 complete fallback chunk");
    let tool_call = at(&out, "choices.0.delta.tool_calls.0").unwrap_or(&NONE);
    assert_eq!(
        text_at(tool_call, "id"),
        "call_apply",
        "call id; chunk={out}"
    );
    assert_eq!(
        text_at(tool_call, "function.name"),
        "ApplyPatch",
        "tool name; chunk={out}"
    );
    assert_eq!(
        text_at(tool_call, "function.arguments"),
        "full patch",
        "chunk={out}"
    );

    // announced function call still falls back
    let mut stream = new_stream("gpt-5.5");
    stream.translate_line(br#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_lookup","name":"lookup","arguments":""}}"#);
    let out = stream
        .translate_line(br#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_lookup","name":"lookup","arguments":"{\"query\":\"test\"}"}}"#)
        .expect("expected 1 function arguments fallback chunk");
    assert_eq!(
        text_at(&out, "choices.0.delta.tool_calls.0.function.arguments"),
        r#"{"query":"test"}"#,
        "function arguments fallback; chunk={out}"
    );
}

#[test]
fn convert_codex_response_to_openai_tool_call_state_falls_back_from_unknown_item_id() {
    let mut stream = new_stream("gpt-5.6-terra");

    let added = stream
        .translate_line(br#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"TaskCreate","arguments":""}}"#)
        .expect("added chunks = 0, want 1");
    let done = stream
        .translate_line(br#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"TaskCreate","arguments":"{\"subject\":\"test\"}"}}"#)
        .expect("done chunks = 0, want 1");

    let added_name = text_at(&added, "choices.0.delta.tool_calls.0.function.name");
    let done_name = text_at(&done, "choices.0.delta.tool_calls.0.function.name");
    assert_eq!(added_name + &done_name, "TaskCreate", "assembled tool name");

    let tool_call = at(&done, "choices.0.delta.tool_calls.0").unwrap_or(&NONE);
    assert!(
        at(tool_call, "id").is_none() && at(tool_call, "function.name").is_none(),
        "done chunk repeated tool identity: {tool_call}"
    );
    assert_eq!(int_at(tool_call, "index"), 0, "done tool index");
    assert_eq!(
        text_at(tool_call, "function.arguments"),
        r#"{"subject":"test"}"#,
        "done arguments"
    );
}

#[test]
fn convert_codex_response_to_openai_non_stream_custom_tool_call() {
    let raw = r#"{"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.5","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"output":[{"type":"custom_tool_call","call_id":"call_apply","name":"ApplyPatch","input":"full patch"}]}}"#;

    let out = non_stream(&Value::Null, raw);
    let tool_call = at(&out, "choices.0.message.tool_calls.0").unwrap_or(&NONE);
    assert_eq!(
        text_at(tool_call, "id"),
        "call_apply",
        "call id; response={out}"
    );
    assert_eq!(
        text_at(tool_call, "function.name"),
        "ApplyPatch",
        "tool name; response={out}"
    );
    assert_eq!(
        text_at(tool_call, "function.arguments"),
        "full patch",
        "arguments; response={out}"
    );
    assert_eq!(
        text_at(&out, "choices.0.finish_reason"),
        "tool_calls",
        "finish reason; response={out}"
    );
}

#[test]
fn convert_codex_response_to_openai_stream_partial_image_emits_delta_images() {
    let mut stream = new_stream("gpt-5.4");

    let chunk = br#"data: {"type":"response.image_generation_call.partial_image","item_id":"ig_123","output_format":"png","partial_image_b64":"aGVsbG8=","partial_image_index":0}"#;

    let out = stream.translate_line(chunk).expect("expected 1 chunk");
    assert_eq!(
        text_at(&out, "choices.0.delta.images.0.image_url.url"),
        "data:image/png;base64,aGVsbG8=",
        "chunk={out}"
    );

    let out = stream.translate_line(chunk);
    assert!(
        out.is_none(),
        "expected duplicate image chunk to be suppressed, got {out:?}"
    );
}

#[test]
fn convert_codex_response_to_openai_stream_image_generation_call_done_emits_delta_images() {
    let mut stream = new_stream("gpt-5.4");

    stream
        .translate_line(br#"data: {"type":"response.image_generation_call.partial_image","item_id":"ig_123","output_format":"png","partial_image_b64":"aGVsbG8=","partial_image_index":0}"#)
        .expect("expected 1 chunk");

    let out = stream.translate_line(br#"data: {"type":"response.output_item.done","item":{"id":"ig_123","type":"image_generation_call","output_format":"png","result":"aGVsbG8="}}"#);
    assert!(
        out.is_none(),
        "expected output_item.done to be suppressed when identical to last partial image, got {out:?}"
    );

    let out = stream
        .translate_line(br#"data: {"type":"response.output_item.done","item":{"id":"ig_123","type":"image_generation_call","output_format":"jpeg","result":"Ymll"}}"#)
        .expect("expected 1 chunk");
    assert_eq!(
        text_at(&out, "choices.0.delta.images.0.image_url.url"),
        "data:image/jpeg;base64,Ymll",
        "chunk={out}"
    );
}

#[test]
fn convert_codex_response_to_openai_non_stream_image_generation_call_adds_message_images() {
    let raw = r#"{"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]},{"type":"image_generation_call","output_format":"png","result":"aGVsbG8="}]}}"#;
    let out = non_stream(&Value::Null, raw);

    assert_eq!(
        text_at(&out, "choices.0.message.images.0.image_url.url"),
        "data:image/png;base64,aGVsbG8=",
        "chunk={out}"
    );
}

#[test]
fn convert_codex_response_to_openai_stream_forwards_cache_write_tokens() {
    let mut stream = new_stream("gpt-5.4");

    // Seed response.created so response.completed can reuse response metadata.
    stream.translate_line(br#"data: {"type":"response.created","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4"}}"#);

    let chunk = br#"data: {"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":40},"output_tokens_details":{"reasoning_tokens":5}}}}"#;
    let out = stream.translate_line(chunk).expect("expected 1 chunk");

    assert_usage_mapping(&out, Some(40));
}

#[test]
fn convert_codex_response_to_openai_stream_omits_missing_cache_write_tokens() {
    let mut stream = new_stream("gpt-5.4");

    stream.translate_line(br#"data: {"type":"response.created","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4"}}"#);

    let chunk = br#"data: {"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":30},"output_tokens_details":{"reasoning_tokens":5}}}}"#;
    let out = stream.translate_line(chunk).expect("expected 1 chunk");

    assert_usage_mapping(&out, None);
}

#[test]
fn convert_codex_response_to_openai_stream_preserves_explicit_zero_cache_write_tokens() {
    let mut stream = new_stream("gpt-5.4");

    stream.translate_line(br#"data: {"type":"response.created","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4"}}"#);

    let chunk = br#"data: {"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":5}}}}"#;
    let out = stream.translate_line(chunk).expect("expected 1 chunk");

    assert_usage_mapping(&out, Some(0));
}

#[test]
fn convert_codex_response_to_openai_non_stream_forwards_cache_write_tokens() {
    let raw = r#"{"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4","status":"completed","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":40},"output_tokens_details":{"reasoning_tokens":5}},"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]}}"#;
    let out = non_stream(&Value::Null, raw);
    assert_usage_mapping(&out, Some(40));
}

#[test]
fn convert_codex_response_to_openai_non_stream_omits_missing_cache_write_tokens() {
    let raw = r#"{"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4","status":"completed","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":30},"output_tokens_details":{"reasoning_tokens":5}},"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]}}"#;
    let out = non_stream(&Value::Null, raw);
    assert_usage_mapping(&out, None);
}

#[test]
fn convert_codex_response_to_openai_non_stream_preserves_explicit_zero_cache_write_tokens() {
    let raw = r#"{"type":"response.completed","response":{"id":"resp_123","created_at":1700000000,"model":"gpt-5.4","status":"completed","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":5}},"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]}}"#;
    let out = non_stream(&Value::Null, raw);
    assert_usage_mapping(&out, Some(0));
}

#[test]
fn convert_codex_response_to_openai_non_stream_multi_message_empty_trailing_keeps_content() {
    let raw = [
        r#"{"type":"response.completed","response":{"id":"resp_1","created_at":1700000000,"model":"gpt-5.5","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15},"output":["#,
        r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"thinking"}]},"#,
        r#"{"type":"message","content":[{"type":"output_text","text":"the real answer"}]},"#,
        r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"thinking again"}]},"#,
        r#"{"type":"message","content":[{"type":"output_text","text":""}]}"#,
        r#"]}}"#,
    ]
    .concat();
    let out = non_stream(&Value::Null, &raw);

    assert!(
        !matches!(
            at(&out, "choices.0.message.content"),
            None | Some(Value::Null)
        ),
        "content was dropped to null by trailing empty message; resp={out}"
    );
    assert_eq!(
        text_at(&out, "choices.0.message.content"),
        "the real answer",
        "resp={out}"
    );
}

#[test]
fn convert_codex_response_to_openai_stream_reasoning_text_delta_and_done() {
    let mut stream = new_stream("MiniMax-M3");

    let stream_out = stream
        .translate_line(
            br#"data: {"type":"response.reasoning_text.delta","delta":"Thinking step 1"}"#,
        )
        .expect("expected 1 streaming chunk for reasoning_text.delta");
    assert_eq!(
        text_at(&stream_out, "choices.0.delta.reasoning_content"),
        "Thinking step 1",
        "payload={stream_out}"
    );
    assert_eq!(
        text_at(&stream_out, "choices.0.delta.role"),
        "assistant",
        "payload={stream_out}"
    );

    let done_out = stream
        .translate_line(
            br#"data: {"type":"response.reasoning_text.done","text":"Thinking step 1"}"#,
        )
        .expect("expected 1 streaming chunk for reasoning_text.done");
    assert_eq!(
        text_at(&done_out, "choices.0.delta.reasoning_content"),
        "\n\n",
        "payload={done_out}"
    );
}

#[test]
fn convert_codex_response_to_openai_non_stream_reasoning_text_content() {
    let raw = [
        r#"{"type":"response.completed","response":{"id":"resp_1","created_at":1700000000,"model":"MiniMax-M3","status":"completed","usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"output_tokens_details":{"reasoning_tokens":15}},"output":["#,
        r#"{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"Full reasoning from MiniMax"}]},"#,
        r#"{"type":"message","content":[{"type":"output_text","text":"Answer"}]}"#,
        r#"]}}"#,
    ]
    .concat();
    let out = non_stream(&Value::Null, &raw);

    assert!(
        !matches!(
            at(&out, "choices.0.message.reasoning_content"),
            None | Some(Value::Null)
        ),
        "expected reasoning_content to exist, got null/missing; payload={out}"
    );
    assert_eq!(
        text_at(&out, "choices.0.message.reasoning_content"),
        "Full reasoning from MiniMax",
        "payload={out}"
    );
}

#[test]
fn convert_codex_response_to_openai_non_stream_reasoning_summary_and_content() {
    let raw = [
        r#"{"type":"response.completed","response":{"id":"resp_1","created_at":1700000000,"model":"MiniMax-M3","status":"completed","usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"output_tokens_details":{"reasoning_tokens":15}},"output":["#,
        r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"Summary part"}],"content":[{"type":"reasoning_text","text":" and Content part"}]},"#,
        r#"{"type":"message","content":[{"type":"output_text","text":"Answer"}]}"#,
        r#"]}}"#,
    ]
    .concat();
    let out = non_stream(&Value::Null, &raw);

    assert!(
        !matches!(
            at(&out, "choices.0.message.reasoning_content"),
            None | Some(Value::Null)
        ),
        "expected reasoning_content to exist, got null/missing; payload={out}"
    );
    assert_eq!(
        text_at(&out, "choices.0.message.reasoning_content"),
        "Summary part and Content part",
        "payload={out}"
    );
}

#[test]
fn convert_codex_response_to_openai_issue5543_cache_write_tokens_and_service_tier() {
    let raw_terminal = r#"{"type":"response.completed","response":{"id":"resp_example","model":"example-model","service_tier":"default","output":[],"usage":{"input_tokens":7378,"output_tokens":6,"total_tokens":7384,"input_tokens_details":{"cached_tokens":7168,"cache_write_tokens":128}}}}"#;

    // non-stream response retains cache_write_tokens and service_tier
    let out = non_stream(&Value::Null, raw_terminal);
    assert_eq!(text_at(&out, "service_tier"), "default", "payload={out}");
    for (path, want) in [
        ("usage.prompt_tokens", 7378),
        ("usage.completion_tokens", 6),
        ("usage.total_tokens", 7384),
        ("usage.prompt_tokens_details.cache_write_tokens", 128),
        ("usage.prompt_tokens_details.cached_creation_tokens", 128),
        ("usage.prompt_tokens_details.cached_tokens", 7168),
    ] {
        assert_eq!(int_at(&out, path), want, "{path}; payload={out}");
    }

    // streaming terminal chunk retains cache_write_tokens and service_tier
    let mut stream = new_stream("example-model");
    let chunk = stream
        .translate_line(format!("data: {raw_terminal}").as_bytes())
        .expect("expected 1 chunk");
    assert_eq!(
        text_at(&chunk, "service_tier"),
        "default",
        "stream chunk service_tier; payload={chunk}"
    );
    for (path, want) in [
        ("usage.prompt_tokens", 7378),
        ("usage.completion_tokens", 6),
        ("usage.total_tokens", 7384),
        ("usage.prompt_tokens_details.cache_write_tokens", 128),
        ("usage.prompt_tokens_details.cached_creation_tokens", 128),
    ] {
        assert_eq!(
            int_at(&chunk, path),
            want,
            "stream chunk {path}; payload={chunk}"
        );
    }

    // streaming response.created carries over service_tier to deltas
    let mut stream = new_stream("example-model");
    let created = stream.translate_line(br#"data: {"type":"response.created","response":{"id":"resp_stream","created_at":1700000000,"model":"example-model","service_tier":"priority"}}"#);
    assert!(
        created.is_none(),
        "expected response.created to yield 0 chunks, got {created:?}"
    );

    let delta = stream
        .translate_line(br#"data: {"type":"response.output_text.delta","delta":"hello"}"#)
        .expect("expected 1 delta chunk");
    assert_eq!(
        text_at(&delta, "service_tier"),
        "priority",
        "delta chunk service_tier; payload={delta}"
    );

    // Terminal event omitting service_tier preserves prior actual tier
    let term = stream
        .translate_line(br#"data: {"type":"response.completed","response":{"id":"resp_stream","created_at":1700000000,"model":"example-model","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#)
        .expect("expected 1 terminal chunk");
    assert_eq!(
        text_at(&term, "service_tier"),
        "priority",
        "expected terminal chunk to retain prior service_tier; payload={term}"
    );

    // streaming in_progress updates tier and completed overrides tier
    let mut stream = new_stream("example-model");
    stream.translate_line(br#"data: {"type":"response.created","response":{"id":"resp_seq","created_at":1700000000,"model":"example-model","service_tier":"default"}}"#);

    // in_progress updates tier
    stream.translate_line(br#"data: {"type":"response.in_progress","response":{"id":"resp_seq","service_tier":"priority"}}"#);

    let delta = stream
        .translate_line(br#"data: {"type":"response.output_text.delta","delta":"hi"}"#)
        .expect("expected 1 delta chunk");
    assert_eq!(
        text_at(&delta, "service_tier"),
        "priority",
        "in_progress tier; chunk={delta}"
    );

    // completed overrides tier
    let completed = stream
        .translate_line(br#"data: {"type":"response.completed","response":{"id":"resp_seq","service_tier":"scale","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#)
        .expect("expected 1 completed chunk");
    assert_eq!(
        text_at(&completed, "service_tier"),
        "scale",
        "completed tier; chunk={completed}"
    );

    let cache_write_paths = [
        "usage.prompt_tokens_details.cache_write_tokens",
        "usage.prompt_tokens_details.cached_creation_tokens",
    ];

    // large integer cache_write_tokens beyond int64 preserved without precision loss
    let beyond_int64_val = "999999999999999999999999999999"; // > 2^63 - 1
    let raw = [
        r#"{"type":"response.completed","response":{"id":"resp_large","model":"example-model","service_tier":"default","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cache_write_tokens":"#,
        beyond_int64_val,
        "}}}}",
    ]
    .concat();
    let out = non_stream(&Value::Null, &raw);
    for path in cache_write_paths {
        assert_eq!(
            raw_at(&out, path),
            beyond_int64_val,
            "exact raw large {path}; payload={out}"
        );
    }

    // Stream path
    let mut stream = new_stream("example-model");
    let chunk = stream
        .translate_line(format!("data: {raw}").as_bytes())
        .expect("expected 1 chunk");
    for path in cache_write_paths {
        assert_eq!(
            raw_at(&chunk, path),
            beyond_int64_val,
            "stream exact raw large {path}; chunk={chunk}"
        );
    }

    // large integer cache_write_tokens preserved without precision loss
    let large_val = "9007199254740993"; // 2^53 + 1
    let raw = [
        r#"{"type":"response.completed","response":{"id":"resp_large","model":"example-model","service_tier":"default","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cache_write_tokens":"#,
        large_val,
        "}}}}",
    ]
    .concat();
    let out = non_stream(&Value::Null, &raw);
    for path in cache_write_paths {
        assert_eq!(
            raw_at(&out, path),
            large_val,
            "exact raw large {path}; payload={out}"
        );
    }

    // invalid cache_write_tokens formats are rejected
    let invalid_cases = [
        r#"{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cache_write_tokens":-1}}"#,
        r#"{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cache_write_tokens":1.5}}"#,
        r#"{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cache_write_tokens":"128"}}"#,
        r#"{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cache_write_tokens":true}}"#,
    ];
    for usage_json in invalid_cases {
        let raw = [
            r#"{"type":"response.completed","response":{"id":"resp_inv","model":"example-model","usage":"#,
            usage_json,
            "}}",
        ]
        .concat();
        let out = non_stream(&Value::Null, &raw);
        for path in cache_write_paths {
            assert!(
                at(&out, path).is_none(),
                "expected invalid {path} to be omitted; payload={out}"
            );
        }

        // Streaming path
        let mut stream = new_stream("example-model");
        if let Some(chunk) = stream.translate_line(format!("data: {raw}").as_bytes()) {
            for path in cache_write_paths {
                assert!(
                    at(&chunk, path).is_none(),
                    "expected stream invalid {path} to be omitted; chunk={chunk}"
                );
            }
        }
    }

    // invalid or whitespace service_tier is ignored
    let invalid_tiers = [r#""   ""#, r#""""#, "123", "true", "null"];
    for tier_val in invalid_tiers {
        let raw = [
            r#"{"type":"response.completed","response":{"id":"resp_tier","model":"example-model","service_tier":"#,
            tier_val,
            r#","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
        ]
        .concat();
        let out = non_stream(&Value::Null, &raw);
        assert!(
            at(&out, "service_tier").is_none(),
            "expected invalid service_tier {tier_val} to be omitted; payload={out}"
        );
    }
}

#[test]
fn convert_codex_response_to_openai_restores_normalized_tool_names() {
    let original_name = "mcp.server:search tool";
    let normalized_name = "mcp_server_search_tool";
    let original_request = parse(&format!(
        r#"{{
            "tools": [
                {{
                    "type": "function",
                    "function": {{
                        "name": "{original_name}"
                    }}
                }}
            ]
        }}"#
    ));

    // Test non-stream response
    let raw_non_stream = [
        r#"{"type":"response.completed","response":{"id":"resp_1","created_at":1700000000,"model":"gpt-5.6-sol","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2},"output":[{"type":"function_call","call_id":"call_1","name":""#,
        normalized_name,
        r#"","arguments":"{}"}]}}"#,
    ]
    .concat();
    let out_non_stream = non_stream(&original_request, &raw_non_stream);
    assert_eq!(
        text_at(
            &out_non_stream,
            "choices.0.message.tool_calls.0.function.name"
        ),
        original_name,
        "non-stream restored name"
    );

    // Test stream response
    let mut stream = CodexToOpenAIChatCompletionsStream::new("gpt-5.6-sol", &original_request);
    let raw_stream_added = [
        r#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":""#,
        normalized_name,
        r#""}}"#,
    ]
    .concat();
    let stream_chunk = stream
        .translate_line(raw_stream_added.as_bytes())
        .expect("expected 1 stream chunk");
    assert_eq!(
        text_at(&stream_chunk, "choices.0.delta.tool_calls.0.function.name"),
        original_name,
        "stream restored name"
    );
}

// Only the original winning custom patch declaration allows the JSON wrapper.
#[test]
fn apply_patch_custom_chat_completions_wrapper() {
    for tool_type in ["custom", "function"] {
        let request = parse(
            &[
                r#"{"tools":[{"type":""#,
                tool_type,
                r#"","name":"apply_patch"}]}"#,
            ]
            .concat(),
        );
        let patch = "*** Begin Patch\n+中文😀 \"\n*** End Patch\n";
        let mut events = vec![
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"a","call_id":"c","name":"apply_patch","input":""}}"#.to_owned(),
        ];
        for delta in ["*** Begin Patch\n", "+中文😀 \"\n", "*** End Patch\n"] {
            events.push(
                [
                    r#"{"type":"response.custom_tool_call_input.delta","item_id":"a","output_index":0,"delta":"#,
                    &go::json_string(delta),
                    "}",
                ]
                .concat(),
            );
        }
        events.push(
            [
                r#"{"type":"response.custom_tool_call_input.done","item_id":"a","output_index":0,"input":"#,
                &go::json_string(patch),
                "}",
            ]
            .concat(),
        );
        events.push(
            [
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"custom_tool_call","id":"a","call_id":"c","name":"apply_patch","input":"#,
                &go::json_string(patch),
                "}}",
            ]
            .concat(),
        );
        let lines: Vec<String> = events
            .iter()
            .map(|event| format!("data: {event}"))
            .collect();
        let arguments: String = run_stream("model", &request, &lines)
            .iter()
            .map(|out| text_at(out, "choices.0.delta.tool_calls.0.function.arguments"))
            .collect();

        let want = if tool_type == "custom" {
            [r#"{"input":"#, &go::json_string(patch), "}"].concat()
        } else {
            patch.to_owned()
        };
        assert_eq!(arguments, want, "{tool_type}: arguments");

        let out = non_stream(
            &request,
            &[
                r#"{"type":"response.completed","response":{"output":[{"type":"custom_tool_call","name":"apply_patch","call_id":"c","input":"#,
                &go::json_string(patch),
                "}]}}",
            ]
            .concat(),
        );
        assert_eq!(
            text_at(&out, "choices.0.message.tool_calls.0.function.arguments"),
            want,
            "{tool_type}: nonstream arguments"
        );
    }
}

#[test]
fn apply_patch_custom_chat_completions_done_fallback() {
    let request = parse(r#"{"tools":[{"type":"custom","name":"apply_patch"}]}"#);
    for added in [false, true] {
        let mut stream = CodexToOpenAIChatCompletionsStream::new("m", &request);
        if added {
            stream.translate_line(br#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"a","call_id":"c","name":"apply_patch","input":""}}"#);
        }
        let out = stream.translate_line(br#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"custom_tool_call","id":"a","call_id":"c","name":"apply_patch","input":"p"}}"#);
        assert!(
            out.as_ref().is_some_and(|out| {
                text_at(out, "choices.0.delta.tool_calls.0.function.arguments")
                    == r#"{"input":"p"}"#
            }),
            "fallback (added={added}): {out:?}"
        );
    }
}

// This round trip requires the request converter to unwrap only a winning custom
// patch's normalized function envelope, while explicit custom inputs remain raw.
#[test]
fn apply_patch_chat_completion_native_history_round_trip() {
    let original = parse(
        r#"{"messages":[{"role":"user","content":"patch"}],"tools":[{"type":"custom","name":"apply_patch"}]}"#,
    );
    let response = r#"{"type":"response.completed","response":{"output":[{"type":"custom_tool_call","call_id":"c","name":"apply_patch","input":"p"}]}}"#;
    let out = non_stream(&original, response);
    let message = raw_at(&out, "choices.0.message");
    let followup = parse(
        &[
            r#"{"messages":["#,
            &message,
            r#",{"role":"tool","tool_call_id":"c","content":"ok"}],"tools":[{"type":"custom","name":"apply_patch"}]}"#,
        ]
        .concat(),
    );
    let (request, _) = convert_openai_chat_completions_request_to_codex("m", &followup, true);
    assert_eq!(
        text_at(&request, "input.0.input"),
        "p",
        "normalized function history must restore raw patch before native Codex: request={request}"
    );
}

#[test]
fn apply_patch_chat_response_ordinary_function_preference() {
    let original = parse(
        r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","function":{"name":"apply_patch","parameters":{}}}]}"#,
    );
    let out = non_stream(
        &original,
        r#"{"type":"response.completed","response":{"output":[{"type":"custom_tool_call","call_id":"c","name":"apply_patch","input":"raw"}]}}"#,
    );
    assert_eq!(
        text_at(&out, "choices.0.message.tool_calls.0.function.arguments"),
        "raw",
        "ordinary preference stolen: {out}"
    );
}

#[test]
fn convert_codex_response_to_openai_non_stream_keeps_assistant_role() {
    let input = r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}]}}"#;

    let output = non_stream(&Value::Null, input);

    assert_eq!(
        text_at(&output, "choices.0.message.role"),
        "assistant",
        "role"
    );
}
