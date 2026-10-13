// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_tool_search_test.go
// and claude_openai-responses_tool_search_disabled_test.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::{Value, json};

use super::test_support::sse_events;
use super::*;
use crate::models::ModelCatalog;

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/tool_search_disabled/cases.json")).unwrap()
}

fn session() -> Value {
    fixtures()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "codex_session")
        .unwrap()["request"]
        .clone()
}

fn translate(request: &Value, enabled: bool) -> Value {
    // The dev recording predates the larger registered output ceilings.
    // An empty catalog reproduces the recording's historical 32000 default.
    let (body, error) = convert_openai_responses_request_to_claude_with_tool_search(
        "claude-sonnet-4-5",
        request,
        true,
        &ModelCatalog::default(),
        false,
        enabled,
    );
    assert!(error.is_none(), "{error:?}");
    body
}

fn names(body: &Value) -> Vec<&str> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

fn blocks(body: &Value) -> Vec<&Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .collect()
}

fn stream(request: &Value, enabled: bool, chunks: &[Value]) -> Vec<(String, Value)> {
    let mut translator = ClaudeToOpenAIResponsesStream::with_tool_search(
        "claude-test",
        request,
        &Value::Null,
        enabled,
    );
    chunks
        .iter()
        .flat_map(|chunk| {
            sse_events(&translator.translate_line(chunk.as_str().unwrap().as_bytes()))
        })
        .collect()
}

fn normalize_time(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            if fields.contains_key("created_at") {
                fields.insert("created_at".into(), 0.into());
            }
            for value in fields.values_mut() {
                normalize_time(value);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_time),
        _ => {}
    }
}

#[test]
fn tool_search_disabled_matches_upstream_dev_golden() {
    let fixtures = fixtures();
    let golden: Value = serde_json::from_str(include_str!(
        "fixtures/tool_search_disabled/dev_golden.json"
    ))
    .unwrap();
    for case in fixtures["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let request = &case["request"];
        let want = &golden[name];
        let mut expected_request: Value =
            serde_json::from_str(want["request"].as_str().unwrap()).unwrap();
        // The recording synthesizes metadata.user_id. This port never invents
        // an upstream client identity; fixtures supply no caller user ID.
        expected_request["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("user_id");
        let translated = translate(request, false);
        assert_eq!(translated, expected_request, "request: {name}");
        assert!(
            want.get("request_error").is_none(),
            "unexpected fixture refusal"
        );
        let mut actual = stream(request, false, fixtures["stream"].as_array().unwrap());
        let mut expected: Vec<_> = want["stream"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|frame| sse_events(frame.as_str().unwrap()))
            .collect();
        actual
            .iter_mut()
            .for_each(|(_, value)| normalize_time(value));
        expected
            .iter_mut()
            .for_each(|(_, value)| normalize_time(value));
        assert_eq!(actual, expected, "stream: {name}");
        let raw = fixtures["stream"]
            .as_array()
            .unwrap()
            .iter()
            .map(|chunk| chunk.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let mut actual = convert_claude_response_to_openai_responses_non_stream(
            request,
            &translated,
            raw.as_bytes(),
        );
        let mut expected: Value =
            serde_json::from_str(want["non_stream"].as_str().unwrap()).unwrap();
        normalize_time(&mut actual);
        normalize_time(&mut expected);
        assert_eq!(actual, expected, "non-stream: {name}");
    }
}

#[test]
fn tool_search_declares_query_and_hides_deferred_tools_until_discovery() {
    let mut request = session();
    request["input"] = json!([{"role":"user","content":"hi"}]);
    let body = translate(&request, true);
    assert_eq!(names(&body), ["exec_command", "tool_search"]);
    let search = &body["tools"][1];
    assert_eq!(
        search["input_schema"]["properties"]["query"]["type"],
        "string"
    );
    assert_eq!(search["input_schema"]["required"][0], "query");
    assert_eq!(
        search["input_schema"]["properties"]["limit"]["type"],
        "number"
    );
    assert!(search.get("execution").is_none() && search.get("type").is_none());
    for parameters in [
        Value::Null,
        json!({"type":"object","properties":{"limit":{"type":"number"}}}),
        json!({"type":"object","properties":{"query":{"type":"number"}},"required":["limit"]}),
    ] {
        let request = json!({"input":"hi","tools":[{"type":"tool_search","execution":"client","parameters":parameters}]});
        let tool = &translate(&request, true)["tools"][0];
        assert_eq!(
            tool["input_schema"]["properties"]["query"]["type"],
            "string"
        );
        assert!(
            tool["input_schema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "query")
        );
    }
    assert_ne!(translate(&session(), true), translate(&session(), false));
}

#[test]
fn search_history_loads_native_references_and_preserves_discovery_order() {
    let request = session();
    let body = translate(&request, true);
    let names = names(&body);
    assert_eq!(
        &names[..4],
        [
            "exec_command",
            "tool_search",
            "mcp__codex_apps__calendar_create_event",
            "lookup_order"
        ]
    );
    let search = blocks(&body)
        .into_iter()
        .find(|b| b["id"] == "call_search_1")
        .unwrap();
    assert_eq!(search["input"]["query"], "calendar create");
    let result = blocks(&body)
        .into_iter()
        .find(|b| b["tool_use_id"] == "call_search_1")
        .unwrap();
    assert_eq!(
        result["content"],
        json!([
        {"type":"tool_reference","tool_name":"mcp__codex_apps__calendar_create_event"},
        {"type":"tool_reference","tool_name":"lookup_order"}])
    );
    for tool in body["tools"].as_array().unwrap().iter().skip(2) {
        assert_eq!(tool["defer_loading"], true);
        assert!(tool.get("cache_control").is_none());
    }
    let calendar = blocks(&body)
        .into_iter()
        .find(|b| b["id"] == "call_cal_1")
        .unwrap();
    assert_eq!(calendar["name"], "mcp__codex_apps__calendar_create_event");
}

#[test]
fn search_outputs_skip_eager_tools_deduplicate_references_and_keep_empty_text() {
    let base = json!({"input":[{"role":"user","content":"hi"},
        {"type":"tool_search_call","execution":"client","call_id":"search","arguments":"{\"query\":\"find\"}"}],
        "tools":[{"type":"tool_search","execution":"client"},{"type":"function","name":"eager","parameters":{}}]});
    for (tools, want) in [
        (json!([]), json!("No matching tools were found.")),
        (
            json!([{"type":"function","name":"eager"}]),
            json!("Tools now available to call: eager."),
        ),
        (
            json!([{"type":"function","name":"eager"},{"type":"custom","name":"found","cache_control":{"type":"ephemeral"}},
            {"type":"custom","name":"found"}]),
            json!([{"type":"tool_reference","tool_name":"found"}]),
        ),
    ] {
        let mut request = base.clone();
        request["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"tool_search_output","call_id":"search","tools":tools}));
        let body = translate(&request, true);
        let result = blocks(&body)
            .into_iter()
            .find(|b| b["tool_use_id"] == "search")
            .unwrap();
        assert_eq!(result["content"], want);
        for tool in body["tools"].as_array().unwrap() {
            assert!(tool.get("cache_control").is_none());
        }
    }
}

#[test]
fn search_name_winner_controls_both_deferred_filtering_and_response_type() {
    let search = json!({"type":"tool_search","execution":"client"});
    let function = json!({"type":"function","name":"tool_search","parameters":{"type":"object","properties":{"x":{"type":"string"}}}});
    let deferred = json!({"type":"function","name":"later","defer_loading":true});
    for (tools, want_type, count) in [
        (json!([function, search, deferred]), "function_call", 2),
        (json!([search, function, deferred]), "tool_search_call", 1),
    ] {
        let request = json!({"input":"hi","tools":tools});
        assert_eq!(names(&translate(&request, true)).len(), count);
        let raw = br#"{"id":"msg","type":"message","role":"assistant","model":"claude-test","content":[{"type":"tool_use","id":"search","name":"tool_search","input":{"x":"1"}}],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}"#;
        let body = convert_claude_response_to_openai_responses_non_stream_with_tool_search(
            &request,
            &Value::Null,
            raw,
            true,
        )
        .unwrap();
        assert_eq!(body["output"][0]["type"], want_type);
    }
    let request = json!({"input":"hi","tools":[{"type":"tool_search","execution":"server","name":"Hosted"},deferred]});
    assert_eq!(names(&translate(&request, true)), ["later"]);
    let request = json!({"input":"hi","tools":[{"type":"function","name":"tool.search"},search]});
    let body = translate(&request, true);
    let names = names(&body);
    assert_ne!(names[0], names[1]);
    assert_eq!(names[1], "tool_search");
}

#[test]
fn search_responses_use_object_arguments_without_argument_delta_events() {
    let fixtures = fixtures();
    let request = session();
    let events = stream(&request, true, fixtures["stream"].as_array().unwrap());
    let done: Vec<_> = events
        .iter()
        .filter(|(name, _)| name == "response.output_item.done")
        .map(|(_, e)| &e["item"])
        .collect();
    assert_eq!(done.len(), 3);
    assert_eq!(
        done[0],
        &json!({"id":"tsc_toolu_search","type":"tool_search_call","status":"completed",
        "call_id":"toolu_search","execution":"client","arguments":{"query":"calendar create","limit":8}})
    );
    assert_eq!(done[1]["namespace"], "mcp__codex_apps__");
    assert_eq!(done[1]["name"], "calendar_create_event");
    assert_eq!(done[2]["name"], "exec_command");
    assert!(!events.iter().any(
        |(name, e)| name.starts_with("response.function_call_arguments.") && e["output_index"] == 0
    ));
    let completed = events
        .iter()
        .find(|(name, _)| name == "response.completed")
        .unwrap();
    assert_eq!(&completed.1["response"]["output"][0], done[0]);
    let raw = fixtures["stream"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let body = convert_claude_response_to_openai_responses_non_stream_with_tool_search(
        &request,
        &Value::Null,
        raw.as_bytes(),
        true,
    )
    .unwrap();
    assert_eq!(&body["output"][0], done[0]);
}

#[test]
fn truncated_or_missing_search_arguments_are_incomplete() {
    let request = json!({"tools":[{"type":"tool_search","execution":"client"}]});
    for (arguments, stop, want_arguments) in [
        ("{\"query\":\"cal", "max_tokens", json!({})),
        (
            "{\"query\":\"calendar\"}",
            "max_tokens",
            json!({"query":"calendar"}),
        ),
        ("", "tool_use", json!({})),
    ] {
        let chunks = [json!({"type":"message_start","message":{"id":"msg","usage":{"input_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"search","name":"tool_search","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments}}),
            json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"})].map(|event| Value::String(format!("data: {event}")));
        let events = stream(&request, true, &chunks);
        for (_, event) in events.iter().filter(|(name, _)| {
            name == "response.output_item.done"
                || name == "response.completed"
                || name == "response.incomplete"
        }) {
            let item = event.get("item").unwrap_or(&event["response"]["output"][0]);
            assert_eq!(item["status"], "incomplete");
            assert_eq!(item["arguments"], want_arguments);
        }
        let raw = chunks
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let body = convert_claude_response_to_openai_responses_non_stream_with_tool_search(
            &request,
            &Value::Null,
            raw.as_bytes(),
            true,
        )
        .unwrap();
        assert_eq!(body["output"][0]["status"], "incomplete");
    }
}

#[test]
fn explicit_switch_pins_compatibility_request_and_both_response_paths() {
    let request = session();
    let fixtures = fixtures();
    for enabled in [true, false] {
        let (body, error) = convert_openai_responses_request_to_claude_with_tool_search(
            "claude-sonnet-4-5",
            &request,
            true,
            &ModelCatalog::default(),
            true,
            enabled,
        );
        assert!(error.is_none());
        assert_eq!(names(&body).contains(&"tool_search"), enabled);
        let expected_type = if enabled {
            "tool_search_call"
        } else {
            "function_call"
        };
        let events = stream(&request, enabled, fixtures["stream"].as_array().unwrap());
        let first = events
            .iter()
            .find(|(name, _)| name == "response.output_item.done")
            .unwrap();
        assert_eq!(first.1["item"]["type"], expected_type);
    }
}
