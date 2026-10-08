// Ported from CLIProxyAPI internal/translator/openai/claude/openai_claude_response_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

// All 42 tests are ported. Table-driven tests run their cases in a loop
// rather than as subtests. TestExtractOpenAIUsage checks the four fields of
// Usage, which holds the four counts extractOpenAIUsage returns; its
// "absent usage" case passes null, which has no fields either.

use serde_json::{Value, json};

use super::*;

/// One Claude SSE event: its name and its data.
#[derive(Debug)]
struct Event {
    kind: String,
    payload: Value,
}

/// `streamReq`: a Claude request asking for a stream.
const STREAM_REQUEST: &str = r#"{"stream":true}"#;

/// `runStream`: feeds each chunk as a `data:` line, then `[DONE]`, and
/// returns the events emitted.
fn run_stream(original_request: &str, chunks: &[&str]) -> Vec<Event> {
    let original_request: Value =
        serde_json::from_str(original_request).expect("test request is valid JSON");
    let mut stream = OpenAIToClaudeStream::new(&original_request);
    let mut emitted = Vec::new();
    for chunk in chunks {
        emitted.extend(stream.translate_line(format!("data: {chunk}").as_bytes()));
    }
    emitted.extend(stream.translate_line(b"data: [DONE]"));
    emitted.iter().filter_map(|raw| parse_event(raw)).collect()
}

fn parse_event(raw: &str) -> Option<Event> {
    let rest = raw.strip_prefix("event: ")?;
    let (kind, rest) = rest.split_once('\n')?;
    let payload = rest.strip_prefix("data: ")?.trim_end_matches('\n');
    Some(Event {
        kind: kind.to_owned(),
        payload: serde_json::from_str(payload).expect("event data is JSON"),
    })
}

/// Looks up a dotted path such as `content_block.name`, like a plain gjson
/// path.
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

fn count_by_type(events: &[Event], kind: &str) -> usize {
    events.iter().filter(|event| event.kind == kind).count()
}

fn tool_use_starts(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|event| {
            event.kind == "content_block_start"
                && text_at(&event.payload, "content_block.type") == "tool_use"
        })
        .collect()
}

fn block_indices(events: &[Event]) -> Vec<i64> {
    events
        .iter()
        .filter(|event| event.kind == "content_block_start")
        .map(|event| int_at(&event.payload, "index"))
        .collect()
}

fn last_stop_reason(events: &[Event]) -> String {
    events
        .iter()
        .rev()
        .find(|event| event.kind == "message_delta")
        .map(|event| text_at(&event.payload, "delta.stop_reason"))
        .unwrap_or_default()
}

fn kinds(events: &[Event]) -> Vec<&str> {
    events.iter().map(|event| event.kind.as_str()).collect()
}

fn first_message_delta(events: &[Event]) -> &Value {
    &events
        .iter()
        .find(|event| event.kind == "message_delta")
        .expect("a message_delta event")
        .payload
}

fn input_json_deltas(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|event| {
            event.kind == "content_block_delta"
                && text_at(&event.payload, "delta.type") == "input_json_delta"
        })
        .map(|event| text_at(&event.payload, "delta.partial_json"))
        .collect()
}

#[test]
fn late_usage_only_does_not_emit_after_message_stop() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            r#"{"id":"c1","model":"m","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        ],
    );

    assert_eq!(count_by_type(&events, "message_delta"), 1, "{events:?}");
    assert_eq!(count_by_type(&events, "message_stop"), 1, "{events:?}");
    assert_eq!(kinds(&events).last(), Some(&"message_stop"), "{events:?}");
}

#[test]
fn stream_ignores_null_tool_name_delta() {
    let mut stream = OpenAIToClaudeStream::new(&json!({"stream": true}));

    let first = stream
        .translate_line(br#"data: {"id":"chatcmpl_1","model":"test-model","created":1,"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}]},"finish_reason":null}]}"#)
        .concat();
    assert!(first.contains(r#""name":"read_file""#), "{first}");

    let second = stream
        .translate_line(br#"data: {"id":"chatcmpl_1","model":"test-model","created":1,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":null,"arguments":"{\"path\":\"/tmp/a\"}"}}]},"finish_reason":null}]}"#)
        .concat();
    assert!(!second.contains("content_block_start"), "{second}");
    assert!(!second.contains(r#""name":"""#), "{second}");
}

#[test]
fn tool_empty_name_throughout() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_a","function":{"name":"","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"","arguments":"{\"x\":1}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 1, "one start with a made-up name: {events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "tool_0");
    assert_eq!(text_at(&starts[0].payload, "content_block.id"), "call_a");
    assert_eq!(count_by_type(&events, "content_block_delta"), 1);
    assert_eq!(count_by_type(&events, "content_block_stop"), 1);
    assert_eq!(last_stop_reason(&events), "tool_use");
}

#[test]
fn tool_null_name() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_a","function":{"name":null,"arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 1, "{events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "tool_0");
    assert_eq!(text_at(&starts[0].payload, "content_block.id"), "call_a");
    assert_eq!(count_by_type(&events, "content_block_stop"), 1);
    assert_eq!(last_stop_reason(&events), "tool_use");
}

#[test]
fn tool_non_string_name() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_a","function":{"name":123,"arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 1, "{events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "tool_0");
}

#[test]
fn tool_repeated_name() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_a","function":{"name":"do_it","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"do_it","arguments":"{\"x\""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"do_it","arguments":":1}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 1, "{events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "do_it");
    assert_eq!(count_by_type(&events, "content_block_stop"), 1);
}

#[test]
fn tool_mixed_empty_name_and_valid() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"index":0,"id":"call_empty","function":{"name":"","arguments":""}},
                {"index":1,"id":"call_real","function":{"name":"do_it","arguments":""}}
            ]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
                {"index":1,"function":{"arguments":"{}"}}
            ]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    // The named call starts mid-stream; the unnamed one starts at the end.
    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 2, "{events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "do_it");
    assert_eq!(text_at(&starts[1].payload, "content_block.name"), "tool_0");
    assert_eq!(count_by_type(&events, "content_block_stop"), 2);
    assert_eq!(block_indices(&events)[..2], [0, 1]);
}

#[test]
fn tool_empty_name_without_signal_is_suppressed() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    assert!(tool_use_starts(&events).is_empty(), "{events:?}");
    assert_ne!(last_stop_reason(&events), "tool_use");
}

#[test]
fn tool_empty_id_defer_start() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"","function":{"name":"do_it","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_real","function":{"arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 1, "{events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.id"), "call_real");
}

#[test]
fn tool_id_in_delta_without_function() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"do_it"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_real"}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 1, "{events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.id"), "call_real");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "do_it");
    assert_eq!(count_by_type(&events, "content_block_stop"), 1);
}

#[test]
fn tool_stop_reason_with_emitted_tool() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_a","function":{"name":"do_it","arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        ],
    );

    assert_eq!(last_stop_reason(&events), "tool_use");
}

#[test]
fn tool_stop_reason_when_id_never_arrives() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"do_it","arguments":""}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 1, "{events:?}");
    let id = text_at(&starts[0].payload, "content_block.id");
    assert!(id.starts_with("toolu_"), "made-up ID: {id}");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "do_it");
    assert_eq!(last_stop_reason(&events), "tool_use");
}

#[test]
fn tool_belated_starts_use_openai_tool_index_order() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"index":2,"function":{"name":"third_tool","arguments":"{}"}},
                {"index":0,"function":{"name":"first_tool","arguments":"{}"}},
                {"index":1,"function":{"name":"second_tool","arguments":"{}"}}
            ]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 3, "{events:?}");
    for (i, name) in ["first_tool", "second_tool", "third_tool"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(text_at(&starts[i].payload, "content_block.name"), name);
        assert_eq!(int_at(&starts[i].payload, "index"), i as i64);
    }
}

#[test]
fn tool_late_id_after_finalization() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"do_it"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_late"}]}}]}"#,
        ],
    );

    assert_eq!(tool_use_starts(&events).len(), 1, "{events:?}");
    let stop = events
        .iter()
        .position(|event| event.kind == "message_stop")
        .expect("message_stop");
    for event in &events[stop..] {
        assert!(
            !event.kind.starts_with("content_block_"),
            "{} after message_stop: {events:?}",
            event.kind
        );
    }
}

#[test]
fn tool_stop_reason_mixed_empty_name_and_valid() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"index":0,"id":"call_empty","function":{"name":"","arguments":""}},
                {"index":1,"id":"call_real","function":{"name":"do_it","arguments":"{}"}}
            ]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    assert_eq!(last_stop_reason(&events), "tool_use");
    assert_eq!(tool_use_starts(&events).len(), 2, "{events:?}");
}

#[test]
fn tool_empty_name_args_only_no_id() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"name":"","arguments":"{\"q\":\"x\"}"}}]}}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 1, "{events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "tool_0");
    let id = text_at(&starts[0].payload, "content_block.id");
    assert!(id.starts_with("toolu_"), "made-up ID: {id}");
    assert_eq!(last_stop_reason(&events), "tool_use");
}

#[test]
fn tool_omitted_finish_reason_emits_message_delta_on_done() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":"{\"loc\":\"Paris\"}"}}]},"finish_reason":null}]}"#,
        ],
    );

    assert_eq!(count_by_type(&events, "message_delta"), 1, "{events:?}");
    assert_eq!(last_stop_reason(&events), "tool_use");
    assert_eq!(count_by_type(&events, "message_stop"), 1, "{events:?}");
    assert_eq!(
        kinds(&events)[events.len() - 2..],
        ["message_delta", "message_stop"]
    );
}

#[test]
fn text_omitted_finish_reason_emits_end_turn_on_done() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"hello world"},"finish_reason":null}]}"#,
        ],
    );

    assert_eq!(count_by_type(&events, "message_delta"), 1, "{events:?}");
    assert_eq!(last_stop_reason(&events), "end_turn");
    assert_eq!(count_by_type(&events, "message_stop"), 1, "{events:?}");
}

#[test]
fn tool_usage_without_finish_reason_emits_message_delta() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":"{\"loc\":\"Paris\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        ],
    );

    assert_eq!(count_by_type(&events, "message_delta"), 1, "{events:?}");
    assert_eq!(last_stop_reason(&events), "tool_use");
    let delta = first_message_delta(&events);
    assert_eq!(int_at(delta, "usage.input_tokens"), 10);
    assert_eq!(int_at(delta, "usage.output_tokens"), 5);
    assert_eq!(count_by_type(&events, "message_stop"), 1, "{events:?}");
}

/// The checks shared by the two per-chunk usage tests.
fn check_per_chunk_usage(events: &[Event]) {
    let starts = tool_use_starts(events);
    assert_eq!(starts.len(), 1, "{events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "Skill");
    assert_eq!(text_at(&starts[0].payload, "content_block.id"), "call_1");

    let deltas = input_json_deltas(events);
    assert!(!deltas.is_empty(), "{events:?}");
    assert_eq!(deltas.concat(), r#"{"skill": "stop-slop"}"#);

    assert_eq!(count_by_type(events, "message_delta"), 1, "{events:?}");
    assert_eq!(last_stop_reason(events), "tool_use");
    assert_eq!(count_by_type(events, "message_stop"), 1, "{events:?}");
    let delta = first_message_delta(events);
    assert_eq!(int_at(delta, "usage.input_tokens"), 191);
    assert_eq!(int_at(delta, "usage.output_tokens"), 15);
}

#[test]
fn tool_per_chunk_usage_preserves_tool_arguments() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Skill","arguments":""}}]},"finish_reason":null}],"usage":{"prompt_tokens":191,"completion_tokens":5}}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":"{\"skill\": \"stop-s"}}]},"finish_reason":null}],"usage":{"prompt_tokens":191,"completion_tokens":10}}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":"lop\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":191,"completion_tokens":15}}"#,
        ],
    );
    check_per_chunk_usage(&events);
}

#[test]
fn tool_per_chunk_usage_omitted_finish_reason_preserves_tool_arguments() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Skill","arguments":""}}]},"finish_reason":null}],"usage":{"prompt_tokens":191,"completion_tokens":5}}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":"{\"skill\": \"stop-s"}}]},"finish_reason":null}],"usage":{"prompt_tokens":191,"completion_tokens":10}}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":"lop\"}"}}]},"finish_reason":null}],"usage":{"prompt_tokens":191,"completion_tokens":15}}"#,
        ],
    );
    check_per_chunk_usage(&events);
}

#[test]
fn tool_omitted_tool_call_index_preserves_parallel_calls() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"id":"call_weather","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}},
                {"id":"call_time","type":"function","function":{"name":"get_time","arguments":"{\"tz\":\"UTC\"}"}}
            ]},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 2, "{events:?}");
    assert_eq!(
        text_at(&starts[0].payload, "content_block.id"),
        "call_weather"
    );
    assert_eq!(
        text_at(&starts[0].payload, "content_block.name"),
        "get_weather"
    );
    assert_eq!(text_at(&starts[1].payload, "content_block.id"), "call_time");
    assert_eq!(
        text_at(&starts[1].payload, "content_block.name"),
        "get_time"
    );

    let deltas = input_json_deltas(&events);
    assert_eq!(deltas.len(), 2, "{deltas:?}");
    let first: Value = serde_json::from_str(&deltas[0]).expect("first arguments are JSON");
    let second: Value = serde_json::from_str(&deltas[1]).expect("second arguments are JSON");
    assert_eq!(text_at(&first, "city"), "Paris");
    assert_eq!(text_at(&second, "tz"), "UTC");

    assert_eq!(count_by_type(&events, "content_block_stop"), 2);
    assert_eq!(last_stop_reason(&events), "tool_use");
}

/// One case of the cache-write usage tests: the usage object, then the
/// input, output, cache-read and cache-write counts Claude should see.
type CacheCase = (&'static str, i64, i64, i64, i64);

const CACHE_WRITE_CASES: [CacheCase; 6] = [
    // cache_write_tokens field
    (
        r#"{"prompt_tokens":1000,"completion_tokens":200,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":150}}"#,
        50,
        200,
        800,
        150,
    ),
    // cache_creation_tokens alias
    (
        r#"{"prompt_tokens":1000,"completion_tokens":200,"prompt_tokens_details":{"cached_tokens":800,"cache_creation_tokens":150}}"#,
        50,
        200,
        800,
        150,
    ),
    // cached_tokens greater than prompt_tokens clamps input_tokens to zero
    (
        r#"{"prompt_tokens":500,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":50}}"#,
        0,
        100,
        800,
        50,
    ),
    // zero cache_write_tokens does not emit cache_creation_input_tokens
    (
        r#"{"prompt_tokens":1000,"completion_tokens":200,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":0}}"#,
        200,
        200,
        800,
        0,
    ),
    // cache_write_tokens only deducts from input_tokens
    (
        r#"{"prompt_tokens":4022,"completion_tokens":462,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":4019}}"#,
        3,
        462,
        0,
        4019,
    ),
    // combined cached and cache_write greater than prompt_tokens clamps to zero
    (
        r#"{"prompt_tokens":500,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":300,"cache_write_tokens":300}}"#,
        0,
        100,
        300,
        300,
    ),
];

fn check_claude_usage(usage: &Value, case: &CacheCase, context: &str) {
    let (_, input, output, cache_read, cache_write) = *case;
    assert_eq!(int_at(usage, "input_tokens"), input, "{context}");
    assert_eq!(int_at(usage, "output_tokens"), output, "{context}");
    assert_eq!(
        int_at(usage, "cache_read_input_tokens"),
        cache_read,
        "{context}"
    );
    if cache_write == 0 {
        assert!(
            at(usage, "cache_creation_input_tokens").is_none(),
            "cache_creation_input_tokens should not be emitted when zero: {context}"
        );
    } else {
        assert_eq!(
            int_at(usage, "cache_creation_input_tokens"),
            cache_write,
            "{context}"
        );
    }
}

#[test]
fn streaming_usage_preserves_cache_write_tokens() {
    for case in &CACHE_WRITE_CASES {
        let usage_chunk = format!(
            r#"{{"id":"c1","model":"m","choices":[],"usage":{}}}"#,
            case.0
        );
        let events = run_stream(
            STREAM_REQUEST,
            &[
                r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"}}]}"#,
                r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                &usage_chunk,
            ],
        );
        let usage = at(first_message_delta(&events), "usage").expect("usage");
        check_claude_usage(usage, case, case.0);
    }
}

#[test]
fn non_streaming_usage_preserves_cache_write_tokens() {
    let request = json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [{"role": "user", "content": "Hello"}]
    });
    for case in &CACHE_WRITE_CASES {
        let response: Value = serde_json::from_str(&format!(
            r#"{{
                "id":"chatcmpl-123",
                "object":"chat.completion",
                "created":1677652288,
                "model":"gpt-5.4",
                "choices":[{{"index":0,"message":{{"role":"assistant","content":"Hello world"}},"finish_reason":"stop"}}],
                "usage":{}
            }}"#,
            case.0
        ))
        .expect("test response is valid JSON");
        let out = convert_openai_response_to_claude_non_stream(&request, &response);
        let usage = at(&out, "usage").expect("usage");
        check_claude_usage(usage, case, case.0);
    }
}

/// `assertSequentialContentBlocks`: every block starts, gets its deltas and
/// stops before the next one starts.
fn assert_sequential_content_blocks(events: &[Event]) {
    let mut active = -1;
    for event in events {
        let index = int_at(&event.payload, "index");
        match event.kind.as_str() {
            "content_block_start" => {
                assert_eq!(
                    active, -1,
                    "start for {index} while {active} is open: {events:?}"
                );
                active = index;
            }
            "content_block_delta" => {
                assert_ne!(
                    active, -1,
                    "delta for {index} with no open block: {events:?}"
                );
                assert_eq!(
                    index, active,
                    "delta for {index} while {active} is open: {events:?}"
                );
            }
            "content_block_stop" => {
                assert_ne!(
                    active, -1,
                    "stop for {index} with no open block: {events:?}"
                );
                assert_eq!(
                    index, active,
                    "stop for {index} while {active} is open: {events:?}"
                );
                active = -1;
            }
            _ => {}
        }
    }
    assert_eq!(
        active, -1,
        "stream ended with block {active} open: {events:?}"
    );
}

#[test]
fn streaming_interleaved_content_and_tool_use_strict_sequential_blocks() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"\n"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    assert_sequential_content_blocks(&events);

    let deltas = input_json_deltas(&events);
    assert_eq!(deltas.len(), 1, "{deltas:?}");
    let arguments: Value = serde_json::from_str(&deltas[0]).expect("arguments are JSON");
    assert_eq!(text_at(&arguments, "command"), "ls");
}

#[test]
fn streaming_parallel_tool_calls_strict_sequential_blocks() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
            {"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}},
            {"index":1,"id":"call_2","type":"function","function":{"name":"Read","arguments":"{\"path\":\"/tmp\"}"}}
        ]},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    assert_sequential_content_blocks(&events);

    let starts = tool_use_starts(&events);
    assert_eq!(starts.len(), 2, "{events:?}");
    assert_eq!(text_at(&starts[0].payload, "content_block.id"), "call_1");
    assert_eq!(text_at(&starts[0].payload, "content_block.name"), "Bash");
    assert_eq!(text_at(&starts[1].payload, "content_block.id"), "call_2");
    assert_eq!(text_at(&starts[1].payload, "content_block.name"), "Read");

    let deltas = input_json_deltas(&events);
    assert_eq!(deltas.len(), 2, "{deltas:?}");
    let first: Value = serde_json::from_str(&deltas[0]).expect("first arguments are JSON");
    let second: Value = serde_json::from_str(&deltas[1]).expect("second arguments are JSON");
    assert_eq!(text_at(&first, "command"), "ls");
    assert_eq!(text_at(&second, "path"), "/tmp");
}

#[test]
fn streaming_interleaved_text_and_thinking_preserves_order() {
    let events = run_stream(
        STREAM_REQUEST,
        &[
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"Note A: "},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"running check"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_content":"Thinking about safety"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"Note B: done"},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"pwd\"}"}}]},"finish_reason":null}]}"#,
            r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ],
    );

    assert_sequential_content_blocks(&events);

    // The tool call comes first; the text and thinking held while it was
    // open follow in the order they arrived.
    let block_types: Vec<String> = events
        .iter()
        .filter(|event| event.kind == "content_block_start")
        .map(|event| text_at(&event.payload, "content_block.type"))
        .collect();
    assert_eq!(block_types, ["tool_use", "text", "thinking", "text"]);

    let deltas_of = |kind: &str, field: &str| -> Vec<String> {
        events
            .iter()
            .filter(|event| {
                event.kind == "content_block_delta" && text_at(&event.payload, "delta.type") == kind
            })
            .map(|event| text_at(&event.payload, field))
            .collect()
    };
    let tool = deltas_of("input_json_delta", "delta.partial_json");
    let arguments: Value =
        serde_json::from_str(tool.last().expect("a tool delta")).expect("arguments are JSON");
    assert_eq!(text_at(&arguments, "command"), "pwd");
    assert_eq!(
        deltas_of("text_delta", "delta.text"),
        ["Note A: running check", "Note B: done"]
    );
    assert_eq!(
        deltas_of("thinking_delta", "delta.thinking"),
        ["Thinking about safety"]
    );
}

fn stop_reason_of(chunks: &[&str]) -> String {
    last_stop_reason(&run_stream(STREAM_REQUEST, chunks))
}

#[test]
fn streaming_tool_finish_reason_length_emits_max_tokens_stop_reason() {
    let stop_reason = stop_reason_of(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\",\"content\":\"hello"}}]},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":400}}"#,
    ]);
    assert_eq!(stop_reason, "max_tokens");
}

#[test]
fn streaming_tool_truncated_arguments_without_finish_reason_emits_max_tokens() {
    let stop_reason = stop_reason_of(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\",\"content\":\"hello"}}]},"finish_reason":null}]}"#,
    ]);
    assert_eq!(stop_reason, "max_tokens");
}

#[test]
fn streaming_tool_valid_arguments_with_stop_reason_emits_tool_use() {
    let stop_reason = stop_reason_of(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\"}"}}]},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":20}}"#,
    ]);
    assert_eq!(stop_reason, "tool_use");
}

#[test]
fn streaming_tool_truncated_arguments_with_stop_reason_emits_max_tokens() {
    let stop_reason = stop_reason_of(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\""}}]},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":20}}"#,
    ]);
    assert_eq!(stop_reason, "max_tokens");
}

#[test]
fn streaming_tool_empty_arguments_with_tool_calls_emits_tool_use() {
    let stop_reason = stop_reason_of(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_time","arguments":""}}]},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":10}}"#,
    ]);
    assert_eq!(stop_reason, "tool_use");
}

#[test]
fn streaming_tool_whitespace_only_arguments_without_finish_reason_emits_max_tokens() {
    let stop_reason = stop_reason_of(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":" \n\t"}}]},"finish_reason":null}]}"#,
    ]);
    assert_eq!(stop_reason, "max_tokens");
}

#[test]
fn streaming_tool_content_filter_with_tool_call_emits_end_turn() {
    let stop_reason = stop_reason_of(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/test.txt\"}"}}]},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}],"usage":{"prompt_tokens":10,"completion_tokens":20}}"#,
    ]);
    assert_eq!(stop_reason, "end_turn");
}

#[test]
fn streaming_tool_parallel_calls_one_truncated_emits_max_tokens() {
    let stop_reason = stop_reason_of(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/a\"}"}},{"index":1,"id":"call_2","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/b"}}]},"finish_reason":null}]}"#,
    ]);
    assert_eq!(stop_reason, "max_tokens");
}

#[test]
fn streaming_tool_multi_chunk_truncated_with_trailing_usage_emits_max_tokens() {
    let stop_reason = stop_reason_of(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"/tmp/a\","}}]},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":"\"content\":\"incompl"}}]},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":400}}"#,
    ]);
    assert_eq!(stop_reason, "max_tokens");
}

fn thinking_deltas(chunks: &[&str]) -> Vec<String> {
    run_stream(STREAM_REQUEST, chunks)
        .iter()
        .filter(|event| {
            event.kind == "content_block_delta"
                && text_at(&event.payload, "delta.type") == "thinking_delta"
        })
        .map(|event| text_at(&event.payload, "delta.thinking"))
        .collect()
}

#[test]
fn streaming_reasoning_field_emits_thinking_delta() {
    let thinking = thinking_deltas(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning":"I am thinking","reasoning_details":[{"type":"reasoning.text","text":"I am thinking"}]},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    assert_eq!(thinking, ["I am thinking"]);
}

#[test]
fn streaming_reasoning_content_still_preferred() {
    let thinking = thinking_deltas(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_content":"primary reasoning","reasoning":"fallback reasoning"},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    assert_eq!(thinking, ["primary reasoning"]);
}

#[test]
fn streaming_reasoning_details_only_emits_thinking_delta() {
    let thinking = thinking_deltas(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","text":"Only details thinking"}]},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"content":"Answer"},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    assert_eq!(thinking, ["Only details thinking"]);
}

#[test]
fn streaming_empty_reasoning_content_falls_back_to_reasoning() {
    let thinking = thinking_deltas(&[
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_content":"","reasoning":"fallback from empty"},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"reasoning_content":null,"reasoning":"fallback from null"},"finish_reason":null}]}"#,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    assert_eq!(thinking, ["fallback from empty", "fallback from null"]);
}

fn thinking_texts(message: &Value) -> Vec<String> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| text_at(block, "type") == "thinking")
        .map(|block| text_at(block, "thinking"))
        .collect()
}

#[test]
fn non_stream_reasoning_field_emits_thinking_block() {
    let raw = r#"{"id":"chatcmpl-1","object":"chat.completion","model":"deepseek","choices":[{"index":0,"message":{"role":"assistant","content":"Done","reasoning":"Thought process"},"finish_reason":"stop"}]}"#;
    let response: Value = serde_json::from_str(raw).expect("test response is valid JSON");

    let out = convert_openai_response_to_claude_non_stream(&Value::Null, &response);
    assert_eq!(thinking_texts(&out), ["Thought process"], "{out}");

    // A stream translator for a client that didn't ask for a stream turns
    // each chunk into a whole message.
    let mut stream = OpenAIToClaudeStream::new(&json!({"stream": false}));
    let emitted = stream.translate_line(format!("data: {raw}").as_bytes());
    assert!(!emitted.is_empty(), "a message for a whole response");
    let message: Value = serde_json::from_str(&emitted[0]).expect("message is JSON");
    assert_eq!(thinking_texts(&message), ["Thought process"]);
}

#[test]
fn extract_openai_usage() {
    // The usage, then the input, output, cached and cache-write counts.
    let cases: [(&str, &str, i64, i64, i64, i64); 12] = [
        ("nil / absent usage", "", 0, 0, 0, 0),
        ("null usage", "null", 0, 0, 0, 0),
        (
            "only prompt and completion tokens without cache details",
            r#"{"prompt_tokens":100,"completion_tokens":50}"#,
            100,
            50,
            0,
            0,
        ),
        (
            "deducts cache_read_tokens only",
            r#"{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":30}}"#,
            70,
            50,
            30,
            0,
        ),
        (
            "deducts cache_write_tokens only (issue 5956)",
            r#"{"prompt_tokens":4022,"completion_tokens":462,"prompt_tokens_details":{"cache_write_tokens":4019}}"#,
            3,
            462,
            0,
            4019,
        ),
        (
            "deducts cache_creation_tokens alias only",
            r#"{"prompt_tokens":4022,"completion_tokens":462,"prompt_tokens_details":{"cache_creation_tokens":4019}}"#,
            3,
            462,
            0,
            4019,
        ),
        (
            "deducts both cached_tokens and cache_write_tokens",
            r#"{"prompt_tokens":1000,"completion_tokens":200,"prompt_tokens_details":{"cached_tokens":800,"cache_write_tokens":150}}"#,
            50,
            200,
            800,
            150,
        ),
        (
            "clamps input_tokens to zero when cache exceeds prompt",
            r#"{"prompt_tokens":500,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":300,"cache_write_tokens":300}}"#,
            0,
            100,
            300,
            300,
        ),
        (
            "handles negative cache numbers safely without corrupting input",
            r#"{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":-10,"cache_write_tokens":-5}}"#,
            100,
            50,
            -10,
            0,
        ),
        (
            "clamps raw negative prompt_tokens to zero",
            r#"{"prompt_tokens":-10,"completion_tokens":50}"#,
            0,
            50,
            0,
            0,
        ),
        (
            "negative cache_write_tokens falls back to cache_creation_tokens alias",
            r#"{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cache_write_tokens":-1,"cache_creation_tokens":40}}"#,
            60,
            50,
            0,
            40,
        ),
        (
            "prevents int64 overflow when cached_tokens and cache_write_tokens are huge",
            r#"{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":9223372036854775800,"cache_write_tokens":100}}"#,
            0,
            50,
            9_223_372_036_854_775_800,
            100,
        ),
    ];
    for (name, raw, input, output, cached, cache_write) in cases {
        // An empty string stands for a missing usage, which reads as null.
        let usage: Value = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(raw).expect("test usage is valid JSON")
        };
        let got = extract_usage(&usage);
        assert_eq!(got.input, input, "{name}: input");
        assert_eq!(got.output, output, "{name}: output");
        assert_eq!(got.cached, cached, "{name}: cached");
        assert_eq!(got.cache_write, cache_write, "{name}: cache write");
    }
}
