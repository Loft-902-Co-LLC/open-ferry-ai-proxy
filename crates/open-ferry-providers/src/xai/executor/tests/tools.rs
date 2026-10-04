//! Tools through the executor: namespaces folded for xAI's 200-tool limit
//! and given back, client tools that share X search's names, and the
//! `apply_patch` bridge, ported from upstream's `xai_executor_test.go`.

use super::*;
use crate::codex::terminal::APPLY_PATCH_ERROR_MESSAGE;

/// 47 namespaces of 10 tools each: 470 tools, past xAI's 200.
fn many_namespaces(descriptions: bool) -> String {
    let namespaces: Vec<String> = (0..47)
        .map(|namespace| {
            let tools: Vec<String> = (0..10)
                .map(|tool| {
                    let description = if descriptions {
                        format!(r#","description":"child tool {tool}""#)
                    } else {
                        String::new()
                    };
                    format!(
                        r#"{{"type":"function","name":"tool_{tool}"{description},"parameters":{{"type":"object","properties":{{"q":{{"type":"string"}}}}}}}}"#
                    )
                })
                .collect();
            let description = if descriptions {
                format!(r#","description":"App {namespace} tools""#)
            } else {
                String::new()
            };
            format!(
                r#"{{"type":"namespace","name":"mcp__app_{namespace}"{description},"tools":[{}]}}"#,
                tools.join(",")
            )
        })
        .collect();
    namespaces.join(",")
}

/// xAI calling `tool_2` through the folded `mcp__app_0` with `q`.
fn folded_call(q: &str) -> String {
    let call = format!(
        r#"{{"type":"function_call","name":"mcp__app_0","call_id":"call_1","arguments":"{{\"name\":\"tool_2\",\"arguments\":{{\"q\":\"{q}\"}}}}"}}"#
    );
    format!(
        "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{call}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"grok-4.6\",\"output\":[{call}],\"usage\":{{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}}}\n\n"
    )
}

// TestXAIExecutorExecuteFoldsNamespacesWhenToolsExceed200.
#[tokio::test]
async fn execute_folds_namespaces_when_tools_exceed_200() {
    let mock = Mock::start(Reply::sse(&folded_call("test"))).await;
    let namespaces = many_namespaces(true);
    let turn1 = format!(
        r#"{{"model":"grok-4.6","tools":[{namespaces}],"input":[{{"role":"user","content":"call tool_2"}}]}}"#
    );
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.6", &turn1),
            options("openai-response"),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 47);
    assert_eq!(tools[0]["name"], "mcp__app_0");
    assert_eq!(tools[0]["type"], "function");
    let output = &payload_json(&response)["output"][0];
    assert_eq!(output["name"], "tool_2");
    assert_eq!(output["namespace"], "mcp__app_0");
    assert_eq!(output["arguments"], r#"{"q":"test"}"#);

    // The client sends the restored call back.
    let turn2 = format!(
        r#"{{"model":"grok-4.6","tools":[{namespaces}],"input":[{{"role":"user","content":"call tool_2"}},{{"type":"function_call","name":"tool_2","namespace":"mcp__app_0","call_id":"call_1","arguments":"{{\"q\":\"test\"}}"}},{{"type":"function_call_output","call_id":"call_1","output":"ok"}}]}}"#
    );
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.6", &turn2),
            options("openai-response"),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    let input = body["input"].as_array().expect("input");
    assert_eq!(input.len(), 3);
    assert_eq!(input[1]["name"], "mcp__app_0");
    assert!(input[1].get("namespace").is_none(), "{}", input[1]);
    let arguments = input[1]["arguments"].as_str().unwrap_or_default();
    assert!(
        arguments == r#"{"arguments":{"q":"test"},"name":"tool_2"}"#
            || arguments == r#"{"name":"tool_2","arguments":{"q":"test"}}"#,
        "{arguments}"
    );
}

// TestXAIExecutorExecuteStreamFoldsNamespacesWhenToolsExceed200.
#[tokio::test]
async fn stream_folds_namespaces_when_tools_exceed_200() {
    let mock = Mock::start(Reply::sse(&folded_call("stream_test"))).await;
    let payload = format!(
        r#"{{"model":"grok-4.6","tools":[{}],"input":[{{"role":"user","content":"call tool_2"}}]}}"#,
        many_namespaces(false)
    );
    let text = streamed(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request("grok-4.6", &payload),
                stream_options("openai-response"),
            )
            .await,
    )
    .await;
    assert_eq!(
        mock.last().json()["tools"].as_array().map(Vec::len),
        Some(47)
    );
    assert!(text.contains(r#""namespace":"mcp__app_0""#), "{text}");
    assert!(text.contains(r#""name":"tool_2""#), "{text}");
}

// TestXAIExecutorExecuteRestoresAdditionalToolsNamespaceCalls.
#[tokio::test]
async fn execute_restores_additional_tools_namespace_calls() {
    let mock = Mock::start(Reply::sse(concat!(
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"name\":\"mcp__exa__web_search_exa\",\"call_id\":\"call_1\",\"arguments\":\"{}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"grok-4.3\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
    )))
    .await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "grok-4.3",
                r#"{"model":"grok-4.3","input":[{"type":"additional_tools","role":"developer","tools":[{"type":"namespace","name":"mcp__exa","tools":[{"type":"function","name":"web_search_exa","parameters":{"type":"object"}}]}]},{"role":"user","content":"use Exa"}]}"#,
            ),
            options("openai-response"),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    for item in body["input"].as_array().expect("input") {
        assert_ne!(item["type"], "additional_tools", "{body}");
    }
    assert_eq!(body["input"][0]["role"], "user");
    let tool = &body["tools"][0];
    assert_eq!(tool["name"], "mcp__exa__web_search_exa");
    assert_eq!(tool["type"], "function");
    assert!(tool.get("tools").is_none(), "{tool}");
    let output = &payload_json(&response)["output"][0];
    assert_eq!(output["name"], "web_search_exa");
    assert_eq!(output["namespace"], "mcp__exa");
}

/// xAI's answer to a request whose client tools share X search's names:
/// X search's own call, the namespaced and the plain client calls, and a
/// message, with `event:` lines when `event_lines`.
fn same_name_calls(event_lines: bool, namespaced: bool) -> String {
    let mut items = vec![
        r#"{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}"}"#,
    ];
    if namespaced {
        items.push(r#"{"id":"fc_ns","type":"function_call","call_id":"call_ns","name":"acme__x_keyword_search","arguments":"{}"}"#);
        items.push(r#"{"id":"fc_plain","type":"function_call","call_id":"call_plain","name":"x_keyword_search","arguments":"{}"}"#);
    } else {
        items.push(r#"{"id":"fc_custom","type":"function_call","call_id":"call_custom","name":"x_keyword_search","arguments":"{}"}"#);
    }
    items.push(r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}"#);
    let mut body = String::new();
    let mut frame = |event: &str, data: String| {
        if event_lines {
            body.push_str(&format!("event: {event}\n"));
        }
        body.push_str(&format!("data: {data}\n\n"));
    };
    for (index, item) in items.iter().enumerate() {
        let mut done: Value = serde_json::from_str(item).unwrap();
        done["status"] = json!("completed");
        frame(
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": index, "item": done})
                .to_string(),
        );
    }
    frame(
        "response.completed",
        format!(
            r#"{{"type":"response.completed","response":{{"id":"resp_1","object":"response","status":"completed","output":[{}]}}}}"#,
            items.join(",")
        ),
    );
    body
}

const SAME_NAME_TOOLS: &str = r#"{"model":"grok-4.5","input":"search X","tools":[{"type":"x_search"},{"type":"function","name":"x_keyword_search","parameters":{"type":"object"}},{"type":"namespace","name":"acme","tools":[{"type":"function","name":"x_keyword_search","parameters":{"type":"object"}}]}]}"#;

const SAME_NAME_CUSTOM_TOOL: &str = r#"{"model":"grok-4.5","input":"search X","tools":[{"type":"x_search"},{"type":"custom","name":"x_keyword_search"}]}"#;

/// Whether `items` hold the namespaced and the plain client calls.
fn client_same_name_calls<'a>(items: impl IntoIterator<Item = &'a Value>) -> (bool, bool) {
    let mut plain = false;
    let mut namespaced = false;
    for item in items {
        assert_ne!(item["type"], "custom_tool_call", "{item}");
        if item["type"] != "function_call" || item["name"] != "x_keyword_search" {
            continue;
        }
        namespaced |= item["namespace"] == "acme";
        plain |= item.get("namespace").is_none() && item["call_id"] == "call_plain";
    }
    (plain, namespaced)
}

// TestXAIExecutorExecutePreservesClientSameNameToolsWithXSearch.
#[tokio::test]
async fn execute_preserves_client_same_name_tools_with_x_search() {
    let mock = Mock::start(Reply::sse(&same_name_calls(false, true))).await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.5", SAME_NAME_TOOLS),
            options("openai-response"),
        )
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&response.payload);
    assert!(
        !text.contains("xs_call") && !text.contains("custom_tool_call"),
        "{text}"
    );
    let payload = payload_json(&response);
    let output = payload["output"].as_array().expect("output");
    assert_eq!(output.len(), 3, "{text}");
    assert_eq!(client_same_name_calls(output), (true, true), "{text}");
}

// TestXAIExecutorExecuteStreamPreservesClientSameNameToolsWithXSearch.
#[tokio::test]
async fn stream_preserves_client_same_name_tools_with_x_search() {
    let mock = Mock::start(Reply::sse(&same_name_calls(true, true))).await;
    let text = streamed(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request("grok-4.5", SAME_NAME_TOOLS),
                stream_options("openai-response"),
            )
            .await,
    )
    .await;
    assert!(
        !text.contains("xs_call") && !text.contains("custom_tool_call"),
        "{text}"
    );
    let events = sse_events(&text);
    let items: Vec<&Value> = events
        .iter()
        .filter_map(|event| event.get("item"))
        .collect();
    assert_eq!(client_same_name_calls(items), (true, true), "{text}");
    let completed = last_event(&text, "response.completed");
    let output = completed["response"]["output"].as_array().expect("output");
    assert_eq!(output.len(), 3, "{completed}");
    assert_eq!(client_same_name_calls(output), (true, true), "{completed}");
}

/// Asserts the client's custom tool went to xAI as a function.
fn assert_custom_tool_normalized(body: &Value) {
    let tools = body["tools"].as_array().expect("tools");
    assert!(
        tools
            .iter()
            .any(|tool| tool["type"] == "function" && tool["name"] == "x_keyword_search"),
        "{body}"
    );
    assert!(
        !tools
            .iter()
            .any(|tool| tool["type"] == "custom" && tool["name"] == "x_keyword_search"),
        "{body}"
    );
}

/// Whether `items` hold the client's call of its normalized custom tool.
fn has_client_function_call<'a>(items: impl IntoIterator<Item = &'a Value>) -> bool {
    items.into_iter().any(|item| {
        assert_ne!(item["type"], "custom_tool_call", "{item}");
        item["type"] == "function_call"
            && item["name"] == "x_keyword_search"
            && item["call_id"] == "call_custom"
    })
}

// TestXAIExecutorExecutePreservesNormalizedCustomSameNameToolWithXSearch.
#[tokio::test]
async fn execute_preserves_normalized_custom_same_name_tool_with_x_search() {
    let mock = Mock::start(Reply::sse(&same_name_calls(false, false))).await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.5", SAME_NAME_CUSTOM_TOOL),
            options("openai-response"),
        )
        .await
        .unwrap();
    assert_custom_tool_normalized(&mock.last().json());
    let text = String::from_utf8_lossy(&response.payload);
    assert!(
        !text.contains("xs_call") && !text.contains("custom_tool_call"),
        "{text}"
    );
    let payload = payload_json(&response);
    let output = payload["output"].as_array().expect("output");
    assert_eq!(output.len(), 2, "{text}");
    assert!(has_client_function_call(output), "{text}");
}

// TestXAIExecutorExecuteStreamPreservesNormalizedCustomSameNameToolWithXSearch.
#[tokio::test]
async fn stream_preserves_normalized_custom_same_name_tool_with_x_search() {
    let mock = Mock::start(Reply::sse(&same_name_calls(true, false))).await;
    let text = streamed(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request("grok-4.5", SAME_NAME_CUSTOM_TOOL),
                stream_options("openai-response"),
            )
            .await,
    )
    .await;
    assert_custom_tool_normalized(&mock.last().json());
    assert!(
        !text.contains("xs_call") && !text.contains("custom_tool_call"),
        "{text}"
    );
    let events = sse_events(&text);
    assert!(
        has_client_function_call(events.iter().filter_map(|event| event.get("item"))),
        "{text}"
    );
    let completed = last_event(&text, "response.completed");
    let output = completed["response"]["output"].as_array().expect("output");
    assert_eq!(output.len(), 2, "{completed}");
    assert!(has_client_function_call(output), "{completed}");
}

/// The patch xAI's `apply_patch` call carries.
const PATCH: &str = "*** Begin Patch\n+中😀\n*** End Patch\n";

// TestXAIApplyPatchResponsesExecutor (testApplyPatchResponsesExecutor).
#[tokio::test]
async fn apply_patch_responses_executor() {
    for stream in [false, true] {
        let item = json!({"type": "function_call", "id": "fc1", "call_id": "c1", "name": "apply_patch", "arguments": json!({"input": PATCH}).to_string()});
        let completed = json!({"type": "response.completed", "response": {"id": "r1", "status": "completed", "output": [item]}});
        let done = json!({"type": "response.output_item.done", "output_index": 0, "item": item});
        let mock = Mock::start(Reply::sse(&format!(
            "data: {done}\n\ndata: {completed}\n\n"
        )))
        .await;
        let payload = json!({"input": [{"type": "custom_tool_call", "call_id": "old", "name": "apply_patch", "input": "old\n"}, {"type": "custom_tool_call_output", "call_id": "old", "output": "ok"}], "tools": [{"type": "custom", "name": "apply_patch"}], "tool_choice": {"type": "custom", "name": "apply_patch"}}).to_string();
        let auth = api_key_auth(&mock.url);
        if stream {
            let text = streamed(
                executor()
                    .execute_stream(
                        auth,
                        request("grok-4", &payload),
                        stream_options("openai-response"),
                    )
                    .await,
            )
            .await;
            assert!(
                text.contains(r#""custom_tool_call""#)
                    && text.contains(r#""response.custom_tool_call_input.done""#),
                "bridge bypass: {text}"
            );
        } else {
            let response = executor()
                .execute(
                    auth,
                    request("grok-4", &payload),
                    options("openai-response"),
                )
                .await
                .unwrap();
            let payload = payload_json(&response);
            let root = payload.get("response").unwrap_or(&payload);
            assert_eq!(root["output"][0]["type"], "custom_tool_call", "{payload}");
            assert_eq!(root["output"][0]["input"], PATCH, "{payload}");
        }
        let body = mock.last().json();
        assert_eq!(body["tools"][0]["type"], "function", "{body}");
        assert!(
            exists(&body, "tools.0.parameters.properties.input"),
            "{body}"
        );
        assert_eq!(body["tool_choice"]["type"], "function", "{body}");
        assert_eq!(
            body["input"][0]["arguments"],
            json!({"input": "old\n"}).to_string(),
            "{body}"
        );
        assert_eq!(body["input"][1]["type"], "function_call_output", "{body}");
    }
}

/// The events of one case of [`apply_patch_folded_namespace_http`].
fn folded_namespace_events(snapshot_only: bool, sparse_terminal: bool, late_name: bool) -> String {
    let patch = r#"{"type":"function_call","id":"a","call_id":"c","name":"n","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#;
    let ordinary = r#"{"type":"function_call","id":"b","call_id":"d","name":"n","arguments":"{\"name\":\"lookup0\",\"arguments\":{\"x\":1}}"}"#;
    let mut events = if snapshot_only {
        vec![
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}"#.to_owned(),
            r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#.to_owned(),
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}"#.to_owned(),
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"b","name":"n","arguments":""}}"#.to_owned(),
            r#"{"type":"response.function_call_arguments.done","item_id":"b","arguments":"{\"name\":\"lookup0\",\"arguments\":{\"x\":1}}"}"#.to_owned(),
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"b","call_id":"d","name":"n"}}"#.to_owned(),
            r#"{"type":"response.completed","response":{"output":[]}}"#.to_owned(),
        ]
    } else {
        vec![
            format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{patch}}}"#),
            format!(r#"{{"type":"response.output_item.done","output_index":1,"item":{ordinary}}}"#),
            format!(r#"{{"type":"response.completed","response":{{"output":[{ordinary}]}}}}"#),
        ]
    };
    if late_name {
        events[0] = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a"}}"#.to_owned();
        events[3] = r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"b"}}"#.to_owned();
    }
    if sparse_terminal && let Some(last) = events.last_mut() {
        *last = r#"{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","name":"n"},{"type":"function_call","id":"b","call_id":"d","name":"n"}]}}"#.to_owned();
    }
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

// TestXAIApplyPatchFoldedNamespaceHTTP.
#[tokio::test]
async fn apply_patch_folded_namespace_http() {
    let mut declarations = vec![r#"{"type":"custom","name":"apply_patch"}"#.to_owned()];
    declarations.extend((0..205).map(|i| {
        format!(r#"{{"type":"function","name":"lookup{i}","parameters":{{"type":"object"}}}}"#)
    }));
    let payload = format!(
        r#"{{"tools":[{{"type":"namespace","name":"n","tools":[{}]}}],"input":[{{"type":"custom_tool_call","call_id":"old","name":"apply_patch","namespace":"n","input":"old"}},{{"type":"custom_tool_call_output","call_id":"old","output":"ok"}}]}}"#,
        declarations.join(",")
    );
    for (stream, snapshot_only, sparse_terminal, late_name) in [
        (false, false, false, false),
        (true, false, false, false),
        (false, true, false, false),
        (true, true, false, false),
        (false, true, true, false),
        (true, true, true, false),
        (false, true, false, true),
        (true, true, false, true),
        (false, true, true, true),
        (true, true, true, true),
    ] {
        let case = format!(
            "stream={stream}/snapshotOnly={snapshot_only}/sparseTerminal={sparse_terminal}/lateName={late_name}"
        );
        let mock = Mock::start(Reply::sse(&folded_namespace_events(
            snapshot_only,
            sparse_terminal,
            late_name,
        )))
        .await;
        let auth = api_key_auth(&mock.url);
        let (output, last) = if stream {
            let text = streamed(
                executor()
                    .execute_stream(
                        auth,
                        request("grok-4", &payload),
                        stream_options("openai-response"),
                    )
                    .await,
            )
            .await;
            let last = last_event(&text, "response.completed")["response"].clone();
            (text, last)
        } else {
            let response = executor()
                .execute(
                    auth,
                    request("grok-4", &payload),
                    options("openai-response"),
                )
                .await
                .unwrap();
            (String::new(), payload_json(&response))
        };
        let items = &last["output"];
        assert_eq!(items[0]["type"], "custom_tool_call", "{case}: {last}");
        assert_eq!(items[0]["namespace"], "n", "{case}: {last}");
        assert_eq!(items[0]["input"], "p", "{case}: {last}");
        assert_eq!(items[1]["name"], "lookup0", "{case}: {last}");
        assert_eq!(items[1]["arguments"], r#"{"x":1}"#, "{case}: {last}");
        if stream && snapshot_only {
            assert!(
                !output.contains(r#""response.custom_tool_call_input.delta""#)
                    && output
                        .matches(r#""type":"response.custom_tool_call_input.done""#)
                        .count()
                        == 1,
                "{case}: snapshot-only completion invented progress or lost input.done: {output}"
            );
        }
        let body = mock.last().json();
        assert_eq!(body["tools"][0]["name"], "n", "{case}: {body}");
        assert_eq!(body["input"][0]["name"], "n", "{case}: {body}");
        assert_eq!(
            body["input"][0]["arguments"], r#"{"arguments":{"input":"old"},"name":"apply_patch"}"#,
            "{case}: {body}"
        );
        assert_eq!(
            body["input"][1]["type"], "function_call_output",
            "{case}: {body}"
        );
    }
}

// TestApplyPatchResponsesExecutorLocalFailures, for xAI (Meta and Kimi
// have executors of their own): an apply_patch call the bridge can't
// restore fails the call with a 502 instead of passing it on.
#[tokio::test]
async fn apply_patch_local_failures() {
    let payload = r#"{"input":[],"tools":[{"type":"custom","name":"apply_patch"}]}"#;
    for stream in [false, true] {
        let body = if stream {
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"a\",\"name\":\"apply_patch\",\"arguments\":\"\"}}\n\n"
        } else {
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"name\":\"apply_patch\",\"arguments\":\"{}\"}]}}\n\n"
        };
        let mock = Mock::start(Reply::sse(body)).await;
        let auth = api_key_auth(&mock.url);
        let error = if stream {
            let (output, error) = collect(
                executor()
                    .execute_stream(
                        auth,
                        request("grok-4", payload),
                        stream_options("openai-response"),
                    )
                    .await
                    .unwrap(),
            )
            .await;
            assert!(
                output.contains(r#""type":"response.failed""#)
                    && !output.contains(r#""type":"response.completed""#),
                "EOF falsely succeeded: {output}"
            );
            error.expect("the stream fails")
        } else {
            executor()
                .execute(auth, request("grok-4", payload), options("openai-response"))
                .await
                .expect_err("malformed nonstream succeeded")
        };
        assert_eq!(error.http_status(), 502, "{error:?}");
        assert_eq!(error.message, APPLY_PATCH_ERROR_MESSAGE);
    }
}
