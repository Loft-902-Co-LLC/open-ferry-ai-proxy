// Ported from CLIProxyAPI internal/translator/claude/openai/responses/
// claude_openai-responses_stop_reasons_test.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use crate::claude::openai::responses::request::{
    convert_openai_responses_request_to_claude,
    convert_openai_responses_request_to_claude_with_compat,
};
use crate::models::ModelCatalog;

fn translate(response: &Value) -> (Vec<Value>, Value) {
    let bytes = response.to_string();
    // Claude streams server-tool inputs as deltas; native Messages retain
    // their full input. Keep both fixtures faithful to their wire shapes.
    let mut streaming_response = response.clone();
    for block in streaming_response["content"].as_array_mut().unwrap() {
        if block["type"] == "server_tool_use" {
            block["input"] = json!({});
        }
    }
    let streaming_bytes = streaming_response.to_string();
    let Native::Events { sse, .. } = messages_json_to_sse(streaming_bytes.as_bytes()) else {
        panic!("native test response");
    };
    let mut stream = ClaudeToOpenAIResponsesStream::new("claude-test", &Value::Null, &Value::Null);
    let mut out = String::new();
    for line in sse.lines() {
        out.push_str(&stream.translate_line(line.as_bytes()));
        if let Some(data) = line.strip_prefix("data: ") {
            let event: Value = serde_json::from_str(data).unwrap();
            if event["type"] == "content_block_start"
                && event["content_block"]["type"] == "server_tool_use"
            {
                let index = event["index"].as_u64().unwrap() as usize;
                let delta = json!({"type":"content_block_delta","index":index,"delta":{
                    "type":"input_json_delta","partial_json":response["content"][index]["input"].to_string()}});
                out.push_str(&stream.translate_line(format!("data: {delta}").as_bytes()));
            }
        }
    }
    let events = out
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    let complete = convert_claude_response_to_openai_responses_non_stream(
        &Value::Null,
        &Value::Null,
        bytes.as_bytes(),
    );
    (events, complete)
}

fn native(content: Value, reason: &str) -> Value {
    json!({"id":"msg_test","type":"message","role":"assistant","model":"claude-test",
        "content":content,"stop_reason":reason,
        "usage":{"input_tokens":7,"cache_read_input_tokens":3,"output_tokens":4}})
}

fn terminal(events: &[Value]) -> &Value {
    events
        .iter()
        .find(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.completed" | "response.incomplete" | "response.failed")
            )
        })
        .unwrap()
}

#[test]
fn stop_reasons_preserve_output_usage_and_terminal_status() {
    for (reason, event, status, detail) in [
        (
            "max_tokens",
            "response.incomplete",
            "incomplete",
            json!({"reason":"max_output_tokens"}),
        ),
        (
            "refusal",
            "response.incomplete",
            "incomplete",
            json!({"reason":"content_filter"}),
        ),
        (
            "model_context_window_exceeded",
            "response.incomplete",
            "incomplete",
            json!({"reason":"max_output_tokens"}),
        ),
        ("pause_turn", "response.completed", "completed", Value::Null),
        (
            " PAUSE_TURN ",
            "response.completed",
            "completed",
            Value::Null,
        ),
        ("end_turn", "response.completed", "completed", Value::Null),
        ("tool_use", "response.completed", "completed", Value::Null),
        (
            "stop_sequence",
            "response.completed",
            "completed",
            Value::Null,
        ),
    ] {
        let (events, complete) = translate(&native(
            json!([{"type":"text","text":"Partial answer"}]),
            reason,
        ));
        assert_eq!(terminal(&events)["type"], event);
        assert_eq!(events.iter().filter(|e| e["type"] == event).count(), 1);
        for response in [&terminal(&events)["response"], &complete] {
            assert_eq!(response["status"], status, "{response}");
            assert_eq!(response["incomplete_details"], detail);
            assert_eq!(
                response["output"][0]["content"][0]["text"],
                "Partial answer"
            );
            assert_eq!(response["usage"]["input_tokens"], 10);
            assert_eq!(
                response["usage"]["input_tokens_details"]["cached_tokens"],
                3
            );
            assert_eq!(response["usage"]["output_tokens"], 4);
            assert!(
                response.get("error").is_none_or(Value::is_null),
                "{response}"
            );
            assert_eq!(response["output"][0]["status"], status);
            if crate::go::equal_fold(reason.trim(), "pause_turn") {
                assert_eq!(response["end_turn"], false);
            } else {
                assert!(response.get("end_turn").is_none());
            }
        }
        let done = events
            .iter()
            .position(|e| e["type"] == "response.output_item.done")
            .unwrap();
        let end = events.iter().position(|e| e["type"] == event).unwrap();
        assert!(done < end);
    }
}

fn paused(id: &str, query: &str) -> Value {
    let (_, response) = translate(&native(
        json!([
            {"type":"text","text":"Searching"},
            {"type":"server_tool_use","id":id,"name":"web_search","input":{"query":query}}
        ]),
        "pause_turn",
    ));
    response
}

fn continuation(id: &str, reason: &str, next: Option<&str>) -> Value {
    let mut content = vec![json!({"type":"web_search_tool_result","tool_use_id":id,
        "content":[{"type":"web_search_result","url":"https://example.com","title":"Example","encrypted_content":"dummy-result"}]})];
    if let Some(next) = next {
        content.push(json!({"type":"server_tool_use","id":next,"name":"web_search","input":{"query":"query b"}}));
    }
    let (events, response) = translate(&native(content.into(), reason));
    assert_eq!(
        terminal(&events)["response"]["output"][0]["id"],
        format!("ws_{id}")
    );
    assert_eq!(
        response["output"][0]["results"][0]["encrypted_content"],
        "dummy-result"
    );
    response
}

fn history(responses: &[Value]) -> Value {
    let mut input = vec![json!({"role":"user","content":"Research"})];
    for response in responses {
        input.extend(response["output"].as_array().unwrap().iter().cloned());
    }
    json!({"tools":[{"type":"web_search"}],"input":input})
}

fn request(model: &str, request: &Value, compat: bool) -> Value {
    let (out, error) = if compat {
        convert_openai_responses_request_to_claude_with_compat(
            model,
            request,
            true,
            &ModelCatalog::default(),
        )
    } else {
        convert_openai_responses_request_to_claude(model, request, true, &ModelCatalog::default())
    };
    assert!(error.is_none());
    out
}

fn assistant_blocks(out: &Value) -> Vec<Value> {
    out["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| message["content"].as_array().unwrap().iter().cloned())
        .collect()
}

#[test]
fn paused_search_round_trip_continuation_and_second_pause() {
    let first = paused("srvtoolu_a", "query a");
    assert_eq!(first["output"][1]["status"], "in_progress");
    assert!(first["output"][1].get("results").is_none());
    for compat in [false, true] {
        for model in ["claude-test", "claude-opus-5"] {
            let out = request(model, &history(std::slice::from_ref(&first)), compat);
            let blocks = assistant_blocks(&out);
            assert_eq!(blocks.last().unwrap()["type"], "server_tool_use");
            assert_eq!(blocks.last().unwrap()["input"]["query"], "query a");
            assert_eq!(
                blocks
                    .iter()
                    .filter(|b| b["type"] == "web_search_tool_result")
                    .count(),
                0
            );
        }
    }
    let second = continuation("srvtoolu_a", "pause_turn", Some("srvtoolu_b"));
    let out = request("claude-opus-5", &history(&[first.clone(), second]), false);
    let blocks = assistant_blocks(&out);
    let uses: Vec<_> = blocks
        .iter()
        .filter(|b| b["type"] == "server_tool_use")
        .collect();
    assert_eq!(uses.len(), 2);
    assert_eq!(uses[0]["input"]["query"], "query a");
    assert_eq!(uses[1]["id"], "srvtoolu_b");
    assert_eq!(blocks.last().unwrap()["type"], "server_tool_use");
    assert_eq!(
        blocks
            .iter()
            .filter(|b| b["type"] == "web_search_tool_result")
            .count(),
        1
    );
    let completed = continuation("srvtoolu_a", "end_turn", None);
    let out = request("claude-test", &history(&[first, completed]), false);
    let blocks = assistant_blocks(&out);
    assert_eq!(
        blocks
            .iter()
            .filter(|b| b["type"] == "server_tool_use")
            .count(),
        1
    );
    assert_eq!(
        blocks
            .iter()
            .filter(|b| b["type"] == "web_search_tool_result")
            .count(),
        1
    );
}

#[test]
fn search_replay_never_coalesces_across_user_turns() {
    for boundary in [
        json!({"role":"user","content":"Again"}),
        json!({"type":"message","role":"user","content":"Again"}),
    ] {
        let first = paused("srvtoolu_a", "query a");
        let completed = continuation("srvtoolu_a", "end_turn", None);
        let mut input = history(&[first])["input"].as_array().unwrap().clone();
        input.push(boundary);
        input.extend(completed["output"].as_array().unwrap().iter().cloned());
        let out = request(
            "claude-test",
            &json!({"input":input,"tools":[{"type":"web_search"}]}),
            false,
        );
        let blocks = assistant_blocks(&out);
        assert_eq!(
            blocks
                .iter()
                .filter(|b| b["type"] == "server_tool_use")
                .count(),
            2,
            "{out}"
        );
    }
}

#[test]
fn parallel_false_only_affects_effective_tools_and_choices() {
    let tools =
        json!([{"type":"function","name":"shell","parameters":{"type":"object","properties":{}}}]);
    for (choice, expected) in [
        (None, Some("auto")),
        (Some(json!("auto")), Some("auto")),
        (Some(json!("required")), Some("any")),
        (
            Some(json!({"type":"function","name":"shell"})),
            Some("tool"),
        ),
        (Some(json!("none")), None),
        (Some(json!({"type":"none"})), None),
        (Some(json!({"type":"function","name":"missing"})), None),
        (Some(json!({"type":"web_search"})), None),
    ] {
        for compat in [false, true] {
            let mut req = json!({"input":"hi","tools":tools,"parallel_tool_calls":false});
            if let Some(choice) = &choice {
                req["tool_choice"] = choice.clone();
            }
            let out = request("claude-test", &req, compat);
            if let Some(expected) = expected {
                assert_eq!(out["tool_choice"]["type"], expected, "{out}");
                assert_eq!(out["tool_choice"]["disable_parallel_tool_use"], true);
            } else {
                assert!(out.get("tool_choice").is_none(), "{out}");
            }
        }
    }
    for parallel in [None, Some(json!(true)), Some(json!("false"))] {
        let mut req = json!({"input":"hi","tools":tools});
        if let Some(parallel) = parallel {
            req["parallel_tool_calls"] = parallel;
        }
        assert!(
            request("claude-test", &req, false)
                .get("tool_choice")
                .is_none()
        );
    }
    let out = request(
        "claude-test",
        &json!({"input":"hi","parallel_tool_calls":false}),
        false,
    );
    assert!(out.get("tool_choice").is_none());
    let out = request(
        "claude-test",
        &json!({"input":"hi","parallel_tool_calls":false,
        "tools":[{"type":"web_search"}],"tool_choice":{"type":"web_search"}}),
        false,
    );
    assert_eq!(out["tool_choice"]["disable_parallel_tool_use"], true);
}

#[test]
fn paused_status_uses_go_unicode_case_folding() {
    let out = request(
        "claude-test",
        &json!({"input":[{"role":"user","content":"Research"},
        {"type":"web_search_call","id":"ws_srvtoolu_p","status":" IN_PROGREſS ","action":{"query":"q"}}]}),
        false,
    );
    let blocks = assistant_blocks(&out);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "server_tool_use");
}

#[test]
fn parallel_guard_resolves_forced_names_and_preserves_exact_string_choices() {
    // Go-probed: literal type-named tools cannot hide a missing forced name.
    let tools = json!([{"type":"function","name":"function","parameters":{"type":"object"}},
        {"type":"custom","name":"custom"}]);
    for compat in [false, true] {
        for choice in [
            json!({"type":"function","function":{"name":"filtered_out"}}),
            json!({"type":"custom","custom":{"name":"filtered_out"}}),
        ] {
            let out = request(
                "claude-test",
                &json!({"input":"hi","tools":tools,
                "parallel_tool_calls":false,"tool_choice":choice}),
                compat,
            );
            assert!(out.get("tool_choice").is_none(), "{out}");
        }
        for (choice, expected) in [
            (json!(" none "), true),
            (json!("none"), false),
            (json!({"type":" none "}), false),
        ] {
            let out = request(
                "claude-test",
                &json!({"input":"hi","tools":tools,
                "parallel_tool_calls":false,"tool_choice":choice}),
                compat,
            );
            if expected {
                assert_eq!(
                    out["tool_choice"],
                    json!({"type":"auto","disable_parallel_tool_use":true})
                );
            } else {
                assert!(out.get("tool_choice").is_none(), "{out}");
            }
        }
        for choice in [
            json!({"type":"function","function":{"name":"lookup","namespace":"ops"}}),
            json!({"type":"custom","custom":{"name":"lookup","namespace":"ops"}}),
            json!({"type":"function","name":"lookup","namespace":"ops"}),
            json!({"type":"function","name":"lookup"}),
        ] {
            let out = request(
                "claude-test",
                &json!({"input":"hi",
                "tools":[{"type":"namespace","name":"ops","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}],
                "parallel_tool_calls":false,"tool_choice":choice}),
                compat,
            );
            assert_eq!(out["tool_choice"]["name"], out["tools"][0]["name"], "{out}");
            assert_eq!(
                out["tool_choice"]["disable_parallel_tool_use"], true,
                "{out}"
            );
        }
        let out = request(
            "claude-test",
            &json!({"input":"hi","parallel_tool_calls":false,
            "tools":[{"type":"namespace","name":"ops","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}],
            "tool_choice":{"type":"function","name":" lookup "}}),
            compat,
        );
        assert!(out.get("tool_choice").is_none(), "{out}");
    }
}

#[test]
fn context_window_preserves_tools_and_marks_only_unfinished_output_incomplete() {
    for reason in [
        "model_context_window_exceeded",
        " MODEL_CONTEXT_WINDOW_EXCEEDED ",
    ] {
        let (events, complete) = translate(&native(
            json!([
                {"type":"tool_use","id":"toolu_1","name":"exec","input":{"cmd":"pwd"}},
                {"type":"text","text":"Partial answer"}
            ]),
            reason,
        ));
        let terminal = terminal(&events);
        assert_eq!(terminal["type"], "response.incomplete");
        for response in [&terminal["response"], &complete] {
            assert_eq!(response["status"], "incomplete");
            assert_eq!(
                response["incomplete_details"]["reason"],
                "max_output_tokens"
            );
            assert!(response.get("error").is_none_or(Value::is_null));
            let output = response["output"].as_array().unwrap();
            assert_eq!(output.len(), 2);
            assert_eq!(output[0]["type"], "function_call");
            assert_eq!(output[0]["call_id"], "toolu_1");
            assert_eq!(output[0]["status"], "completed");
            assert_eq!(output[1]["status"], "incomplete");
            assert_eq!(output[1]["content"][0]["text"], "Partial answer");
            assert_eq!(response["usage"]["input_tokens"], 10);
            assert_eq!(response["usage"]["output_tokens"], 4);
        }
    }
}

#[test]
fn deferred_search_replays_and_coalesces_across_tool_outputs() {
    for reason in ["tool_use", " TOOL_USE "] {
        let (events, complete) = translate(&native(
            json!([
                {"type":"server_tool_use","id":"srvtoolu_deferred","name":"web_search","input":{"query":"deferred query"}},
                {"type":"tool_use","id":"toolu_exec","name":"exec","input":{"cmd":"uname"}}
            ]),
            reason,
        ));
        let continued = continuation("srvtoolu_deferred", "end_turn", None);
        for response in [&terminal(&events)["response"], &complete] {
            assert_eq!(response["output"][0]["status"], "in_progress");
            assert!(response["output"][0].get("results").is_none());
            assert_eq!(response["output"][1]["call_id"], "toolu_exec");
            for output in [
                json!({"type":"function_call_output","call_id":"toolu_exec","output":"Linux"}),
                json!({"type":"custom_tool_call_output","call_id":"toolu_exec","output":"Linux"}),
                json!({"type":"tool_search_output","call_id":"toolu_exec","tools":[]}),
            ] {
                for compat in [false, true] {
                    let mut req = history(&[response.clone(), json!({"output":[output]})]);
                    let out = request("claude-test", &req, compat);
                    let blocks = assistant_blocks(&out);
                    let types: Vec<_> = blocks
                        .iter()
                        .map(|block| block["type"].as_str().unwrap())
                        .collect();
                    assert_eq!(types, ["server_tool_use", "tool_use"], "{out}");
                    req["input"]
                        .as_array_mut()
                        .unwrap()
                        .extend(continued["output"].as_array().unwrap().iter().cloned());
                    req["input"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"role":"user","content":"thanks"}));
                    let out = request("claude-test", &req, compat);
                    let blocks = assistant_blocks(&out);
                    let uses: Vec<_> = blocks
                        .iter()
                        .filter(|b| b["type"] == "server_tool_use")
                        .collect();
                    assert_eq!(uses.len(), 1, "{out}");
                    assert_eq!(uses[0]["input"]["query"], "deferred query");
                    let results: Vec<_> = blocks
                        .iter()
                        .filter(|b| b["type"] == "web_search_tool_result")
                        .collect();
                    assert_eq!(results.len(), 1, "{out}");
                    assert_eq!(
                        results[0]["content"][0]["encrypted_content"],
                        "dummy-result"
                    );
                    assert_eq!(blocks.iter().filter(|b| b["type"] == "tool_use").count(), 1);
                }
            }
        }
    }
}

#[test]
fn context_window_keeps_completed_search_results_and_usage() {
    let (events, complete) = translate(&native(
        json!([
            {"type":"server_tool_use","id":"srvtoolu_context","name":"web_search","input":{"query":"lindorm vector"}},
            {"type":"web_search_tool_result","tool_use_id":"srvtoolu_context","content":[{"type":"web_search_result","url":"https://example.com","title":"Example","encrypted_content":"dummy-result"}]}
        ]),
        "model_context_window_exceeded",
    ));
    assert_eq!(terminal(&events)["type"], "response.incomplete");
    for response in [&terminal(&events)["response"], &complete] {
        assert_eq!(response["status"], "incomplete");
        assert_eq!(
            response["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert!(response.get("error").is_none_or(Value::is_null));
        assert_eq!(response["output"][0]["action"]["query"], "lindorm vector");
        assert_eq!(
            response["output"][0]["results"][0]["encrypted_content"],
            "dummy-result"
        );
        assert_eq!(response["usage"]["input_tokens"], 10);
        assert_eq!(response["usage"]["output_tokens"], 4);
    }
}
