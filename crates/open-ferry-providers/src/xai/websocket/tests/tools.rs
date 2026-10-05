//! Tools over the WebSocket: namespace tools and a client's `web_search`
//! given their names back, X search's own calls dropped, reasoning text
//! made a summary, and the `apply_patch` bridge, ported from upstream's
//! `xai_websockets_executor_test.go` and the `ws_raw` mode of
//! `xai_executor_test.go`'s `TestXAIApplyPatchDispatcherEvidenceLifecycle`.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{auth, auth_as, call, events, executor, last_event, message, streamed, ws_options};
use crate::codex::terminal::APPLY_PATCH_ERROR_MESSAGE;
use crate::codex::websocket::mock::Server;
use crate::json::{exists, str_at};

/// How many times `needle` is in `haystack`.
fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

// TestXAIWebsocketsExecuteStreamRestoresNamespaceToolCalls
#[tokio::test]
async fn restores_namespace_tool_calls() {
    let server = Server::once(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"mcp__exa__web_search_exa","call_id":"call_1","arguments":"{}"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ])
    .await;
    let chunks = streamed(
        &executor(),
        &auth(&server.url),
        r#"{"model":"grok-4.3","input":[{"type":"additional_tools","role":"developer","tools":[{"type":"namespace","name":"mcp__exa","tools":[{"type":"function","name":"web_search_exa","parameters":{"type":"object"}}]}]},{"role":"user","content":"use Exa"}]}"#,
        ws_options(""),
    )
    .await;

    let sent = message(&server, 0);
    for item in sent["input"].as_array().unwrap() {
        assert_ne!(item["type"], "additional_tools", "{sent}");
    }
    assert_eq!(str_at(&sent, "input.0.role"), "user", "{sent}");
    assert_eq!(
        str_at(&sent, "tools.0.name"),
        "mcp__exa__web_search_exa",
        "{sent}"
    );
    assert!(!exists(&sent, "tools.0.tools"), "{sent}");

    for (label, item) in [
        (
            "output_item.done",
            last_event(&chunks, "response.output_item.done")["item"].clone(),
        ),
        (
            "completed",
            last_event(&chunks, "response.completed")["response"]["output"][0].clone(),
        ),
    ] {
        assert_eq!(item["name"], "web_search_exa", "{label}: {item}");
        assert_eq!(item["namespace"], "mcp__exa", "{label}: {item}");
    }
}

// TestXAIWebsocketsExecuteStreamRestoresAliasedWebSearch
#[tokio::test]
async fn restores_aliased_web_search() {
    let server = Server::once(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"clientfn_web_search","call_id":"call_1","arguments":"{}"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","output":[{"type":"function_call","name":"clientfn_web_search","call_id":"call_1"}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ])
    .await;
    let chunks = streamed(
        &executor(),
        &auth(&server.url),
        r#"{"model":"grok-4.6","input":[{"role":"user","content":"search query"}],"tools":[{"type":"function","name":"web_search","parameters":{"type":"object"}}]}"#,
        ws_options(""),
    )
    .await;
    let sent = message(&server, 0);
    assert_eq!(
        str_at(&sent, "tools.0.name"),
        "clientfn_web_search",
        "{sent}"
    );
    for (label, item) in [
        (
            "output_item.done",
            last_event(&chunks, "response.output_item.done")["item"].clone(),
        ),
        (
            "completed",
            last_event(&chunks, "response.completed")["response"]["output"][0].clone(),
        ),
    ] {
        assert_eq!(item["name"], "web_search", "{label}: {item}");
    }
}

// TestXAIWebsocketsExecuteStreamDoesNotRestoreNamespacedClientfnWebSearch
#[tokio::test]
async fn does_not_restore_namespaced_clientfn_web_search() {
    let server = Server::once(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"acme__clientfn_web_search","call_id":"call_1","arguments":"{}"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","name":"clientfn_web_search","call_id":"call_2","arguments":"{}"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","output":[{"type":"function_call","name":"acme__clientfn_web_search","call_id":"call_1"},{"type":"function_call","name":"clientfn_web_search","call_id":"call_2"}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ])
    .await;
    let chunks = streamed(
        &executor(),
        &auth(&server.url),
        r#"{"model":"grok-4.6","input":[{"role":"user","content":"search query"}],"tools":[{"type":"function","name":"web_search","parameters":{"type":"object"}},{"type":"namespace","name":"acme","tools":[{"type":"function","name":"clientfn_web_search","parameters":{"type":"object"}}]}]}"#,
        ws_options(""),
    )
    .await;
    let done: Vec<Value> = events(&chunks)
        .into_iter()
        .filter(|event| event["type"] == "response.output_item.done")
        .collect();
    assert_eq!(done.len(), 2, "{chunks:?}");
    // The namespaced tool keeps its name.
    assert_eq!(done[0]["item"]["name"], "clientfn_web_search");
    assert_eq!(done[0]["item"]["namespace"], "acme");
    // The other is the client's web_search.
    assert_eq!(done[1]["item"]["name"], "web_search");

    let completed = last_event(&chunks, "response.completed");
    let output = &completed["response"]["output"];
    assert_eq!(output[0]["name"], "clientfn_web_search", "{completed}");
    assert_eq!(output[0]["namespace"], "acme", "{completed}");
    assert_eq!(output[1]["name"], "web_search", "{completed}");
}

/// Checks no chunk shows X search's own call, and returns the function
/// calls of the `response.output_item.done` events.
fn client_calls(chunks: &[String]) -> Vec<Value> {
    let mut calls = Vec::new();
    for chunk in chunks {
        assert!(
            !chunk.contains("xs_call"),
            "X search's call ID leaked: {chunk}"
        );
        assert!(
            !chunk.contains("custom_tool_call"),
            "X search's custom_tool_call leaked: {chunk}"
        );
        let event: Value = serde_json::from_str(chunk).unwrap();
        if event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call"
        {
            calls.push(event["item"].clone());
        }
    }
    calls
}

/// Whether `items` has a function call to `name` in `namespace` (none when
/// empty), with `call_id` when given.
fn has_call(items: &[Value], name: &str, namespace: &str, call_id: Option<&str>) -> bool {
    items.iter().any(|item| {
        item["type"] == "function_call"
            && item["name"] == name
            && str_at(item, "namespace") == namespace
            && call_id.is_none_or(|id| item["call_id"] == id)
    })
}

// TestXAIWebsocketsExecuteStreamPreservesClientSameNameToolsWithXSearch
#[tokio::test]
async fn preserves_client_same_name_tools_with_x_search() {
    // X search's own call and the client's tools share x_keyword_search.
    let server = Server::once(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}","status":"completed"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_ns","type":"function_call","call_id":"call_ns","name":"acme__x_keyword_search","arguments":"{}","status":"completed"}}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"id":"fc_plain","type":"function_call","call_id":"call_plain","name":"x_keyword_search","arguments":"{}","status":"completed"}}"#,
        r#"{"type":"response.output_item.done","output_index":3,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}],"status":"completed"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","status":"completed","output":[{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}"},{"id":"fc_ns","type":"function_call","call_id":"call_ns","name":"acme__x_keyword_search","arguments":"{}"},{"id":"fc_plain","type":"function_call","call_id":"call_plain","name":"x_keyword_search","arguments":"{}"},{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ])
    .await;
    let chunks = streamed(
        &executor(),
        &auth(&server.url),
        r#"{"model":"grok-4.5","input":"search X","tools":[{"type":"x_search"},{"type":"function","name":"x_keyword_search","parameters":{"type":"object"}},{"type":"namespace","name":"acme","tools":[{"type":"function","name":"x_keyword_search","parameters":{"type":"object"}}]}]}"#,
        ws_options(""),
    )
    .await;
    let calls = client_calls(&chunks);
    assert!(
        has_call(&calls, "x_keyword_search", "", Some("call_plain")),
        "the client's plain x_keyword_search is missing: {chunks:?}"
    );
    assert!(
        has_call(&calls, "x_keyword_search", "acme", None),
        "the client's acme x_keyword_search is missing: {chunks:?}"
    );
    let completed = last_event(&chunks, "response.completed");
    let output = completed["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 3, "{completed}");
    assert!(
        has_call(output, "x_keyword_search", "", Some("call_plain"))
            && has_call(output, "x_keyword_search", "acme", None),
        "{completed}"
    );
}

// TestXAIWebsocketsExecuteStreamPreservesNormalizedCustomSameNameToolWithXSearch:
// the client's custom tool goes to xAI as a function, and its call comes
// back.
#[tokio::test]
async fn preserves_normalized_custom_same_name_tool_with_x_search() {
    let server = Server::once(&[
        r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}","status":"completed"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_custom","type":"function_call","call_id":"call_custom","name":"x_keyword_search","arguments":"{}","status":"completed"}}"#,
        r#"{"type":"response.output_item.done","output_index":2,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}],"status":"completed"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","status":"completed","output":[{"id":"ctc_1","type":"custom_tool_call","call_id":"xs_call-1","name":"x_keyword_search","input":"{}"},{"id":"fc_custom","type":"function_call","call_id":"call_custom","name":"x_keyword_search","arguments":"{}"},{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ])
    .await;
    let chunks = streamed(
        &executor(),
        &auth(&server.url),
        r#"{"model":"grok-4.5","input":"search X","tools":[{"type":"x_search"},{"type":"custom","name":"x_keyword_search"}]}"#,
        ws_options(""),
    )
    .await;
    let calls = client_calls(&chunks);
    assert!(
        has_call(&calls, "x_keyword_search", "", Some("call_custom")),
        "the client's custom tool call is missing: {chunks:?}"
    );
    let completed = last_event(&chunks, "response.completed");
    let output = completed["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 2, "{completed}");
    assert!(
        has_call(output, "x_keyword_search", "", Some("call_custom")),
        "{completed}"
    );

    // response.create keeps the tools at the top of the message.
    let sent = message(&server, 0);
    let tools = sent["tools"].as_array().unwrap();
    assert!(
        tools
            .iter()
            .any(|tool| tool["type"] == "function" && tool["name"] == "x_keyword_search"),
        "{sent}"
    );
    assert!(
        !tools
            .iter()
            .any(|tool| tool["type"] == "custom" && tool["name"] == "x_keyword_search"),
        "{sent}"
    );
}

// TestXAIWebsocketsExecuteStreamNormalizesReasoningTextEvents. Upstream's
// client isn't on the Responses WebSocket; here it is.
#[tokio::test]
async fn normalizes_reasoning_text_events() {
    let server = Server::once(&[
        r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"rs_1","type":"reasoning","status":"in_progress","summary":[]}}"#,
        r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"rs_1","output_index":0,"content_index":0,"part":{"type":"reasoning_text","text":""}}"#,
        r#"{"type":"response.reasoning_text.delta","sequence_number":3,"item_id":"rs_1","output_index":0,"content_index":0,"delta":"thinking"}"#,
        r#"{"type":"response.reasoning_text.done","sequence_number":4,"item_id":"rs_1","output_index":0,"content_index":0,"text":"thinking"}"#,
        r#"{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"rs_1","type":"reasoning","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"thinking"}]}}"#,
        r#"{"type":"response.completed","sequence_number":6,"response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"grok-4.3","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ])
    .await;
    let output = streamed(
        &executor(),
        &auth(&server.url),
        r#"{"model":"grok-4.3","input":"hello"}"#,
        ws_options(""),
    )
    .await
    .concat();
    assert!(!output.contains("reasoning_text"), "{output}");
    for want in [
        r#""type":"response.reasoning_summary_part.added""#,
        r#""type":"response.reasoning_summary_text.delta""#,
        r#""type":"response.reasoning_summary_text.done""#,
        r#""type":"response.reasoning_summary_part.done""#,
        r#""part":{"type":"summary_text","text":"thinking"}"#,
        r#""summary_index":0"#,
        r#""summary":[{"type":"summary_text","text":"thinking"}]"#,
    ] {
        assert!(output.contains(want), "{want} is missing: {output}");
    }
    let text_done = output
        .find(r#""type":"response.reasoning_summary_text.done""#)
        .unwrap();
    let part_done = output
        .find(r#""type":"response.reasoning_summary_part.done""#)
        .unwrap();
    assert!(text_done < part_done, "out of order: {output}");
}

/// Checks every identity of an `apply_patch` call's lifecycle, not merely
/// the final snapshot or the counts (upstream's
/// `assertApplyPatchIdentityLifecycle`): the call `call` of item `id` at
/// `index`, whose input is `input`, streamed as `fragments`.
fn assert_patch_lifecycle(
    events: &[Value],
    id: &str,
    call: &str,
    input: &str,
    index: i64,
    fragments: &[&str],
) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut actual = Vec::new();
    for event in events {
        let kind = str_at(event, "type");
        let output_index = event["output_index"].as_i64().unwrap_or_default();
        let (identity, id_key) = match kind.as_str() {
            "response.output_item.added" | "response.output_item.done" => {
                if event["item"]["type"] != "custom_tool_call" {
                    continue;
                }
                assert_eq!(output_index, index, "wrong output index: {event}");
                let item = &event["item"];
                if kind == "response.output_item.added" {
                    assert_eq!(str_at(item, "input"), "", "nonempty added input: {event}");
                } else {
                    assert_eq!(str_at(item, "input"), input, "item input: {event}");
                }
                (item, "id")
            }
            "response.custom_tool_call_input.delta" | "response.custom_tool_call_input.done" => {
                assert_eq!(output_index, index, "wrong input index: {event}");
                if kind == "response.custom_tool_call_input.delta" {
                    actual.push(str_at(event, "delta"));
                } else {
                    assert_eq!(str_at(event, "input"), input, "done input: {event}");
                }
                (event, "item_id")
            }
            "response.completed" => {
                let item = &event["response"]["output"][0];
                assert_eq!(str_at(item, "input"), input, "final input: {event}");
                (item, "id")
            }
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.failed" => panic!("unsafe patch event: {event}"),
            _ => continue,
        };
        *counts.entry(kind).or_default() += 1;
        assert!(
            str_at(identity, id_key) == id && str_at(identity, "call_id") == call,
            "unstable identity: {event}"
        );
    }
    for kind in [
        "response.output_item.added",
        "response.custom_tool_call_input.done",
        "response.output_item.done",
        "response.completed",
    ] {
        assert_eq!(
            counts.get(kind),
            Some(&1),
            "missing or repeated {kind}: {counts:?}"
        );
    }
    assert_eq!(actual, fragments, "source fragment replay");
}

/// The tools of a patch test: `apply_patch` alone, or folded into the
/// namespace `n` with 205 other tools.
fn patch_tools(fold: bool) -> String {
    if !fold {
        return r#"[{"type":"custom","name":"apply_patch"}]"#.to_owned();
    }
    let mut declarations = vec![r#"{"type":"custom","name":"apply_patch"}"#.to_owned()];
    declarations.extend((0..205).map(|i| {
        format!(r#"{{"type":"function","name":"lookup{i}","parameters":{{"type":"object"}}}}"#)
    }));
    format!(
        r#"[{{"type":"namespace","name":"n","tools":[{}]}}]"#,
        declarations.join(",")
    )
}

/// The `response.completed` event's response, if one came.
fn final_response(chunks: &[String]) -> Option<Value> {
    events(chunks)
        .into_iter()
        .rfind(|event| event["type"] == "response.completed")
        .map(|event| event["response"].clone())
}

// TestXAIWebsocketApplyPatchTransport, for a client on the Responses
// WebSocket (one that isn't goes over HTTP here).
#[tokio::test]
async fn apply_patch_transport() {
    const INPUT: &str = "p\n中😀";
    for fold in [false, true] {
        for (snapshot_only, sparse_terminal, late_name) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, false, true),
            (true, true, true),
        ] {
            let case = format!(
                "fold={fold}/snapshotOnly={snapshot_only}/sparseTerminal={sparse_terminal}/lateName={late_name}"
            );
            let (name, args) = if fold {
                (
                    "n",
                    json!({"name": "apply_patch", "arguments": {"input": INPUT}}).to_string(),
                )
            } else {
                ("apply_patch", json!({"input": INPUT}).to_string())
            };
            let mut events = Vec::new();
            if snapshot_only {
                // The call's identity may first come with the item's done,
                // which has no arguments.
                events.push(json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "id": "a", "name": name, "arguments": ""}}));
            } else {
                events.push(json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "id": "a", "call_id": "c", "name": name, "arguments": ""}}));
                events.push(json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "a", "delta": args}));
            }
            events.push(json!({"type": "response.function_call_arguments.done", "item_id": "a", "arguments": args}));
            events.push(if snapshot_only {
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "id": "a", "call_id": "c", "name": name}})
            } else {
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "id": "a", "call_id": "c", "name": name, "arguments": args}})
            });
            events
                .push(json!({"type": "response.completed", "response": {"id": "r", "output": []}}));
            if late_name {
                events[0] = json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "id": "a"}});
            }
            if sparse_terminal && let Some(last) = events.last_mut() {
                *last = json!({"type": "response.completed", "response": {"id": "r", "output": [{"type": "function_call", "id": "a", "call_id": "c", "name": name}]}});
            }
            let frames: Vec<String> = events.iter().map(Value::to_string).collect();
            let frames: Vec<&str> = frames.iter().map(String::as_str).collect();
            let server = Server::once(&frames).await;

            let (chunks, error) = call(
                &executor(),
                &auth_as("patch-ws", "test", &server.url),
                &format!(r#"{{"input":[],"tools":{}}}"#, patch_tools(fold)),
                ws_options(""),
            )
            .await;
            assert!(error.is_none(), "{case}: {error:?}");
            let output = chunks.concat();
            assert!(
                output.contains(r#""custom_tool_call""#)
                    && output.contains(r#""response.custom_tool_call_input.done""#)
                    && output.contains(r#""input":"p\n中😀""#),
                "{case}: the bridge was bypassed: {output}"
            );
            if snapshot_only {
                assert!(
                    !output.contains(r#""response.custom_tool_call_input.delta""#),
                    "{case}: a snapshot-only completion made up progress: {output}"
                );
            }
            let fragments: &[&str] = if !snapshot_only && !fold {
                &[INPUT]
            } else {
                &[]
            };
            assert_patch_lifecycle(&super::events(&chunks), "a", "c", INPUT, 0, fragments);
            let last = final_response(&chunks).expect("no response.completed");
            assert_eq!(
                last["output"][0]["type"], "custom_tool_call",
                "{case}: {last}"
            );
            assert_eq!(last["output"][0]["input"], INPUT, "{case}: {last}");
            assert_eq!(last["output"][0]["call_id"], "c", "{case}: {last}");
            if fold {
                assert_eq!(last["output"][0]["namespace"], "n", "{case}: {last}");
            }
            assert_eq!(
                count(&output, r#""type":"response.custom_tool_call_input.done""#),
                1,
                "{case}: {output}"
            );
            assert_eq!(
                count(&output, r#""type":"response.completed""#),
                1,
                "{case}: {output}"
            );

            let body = message(&server, 0);
            if fold {
                assert_eq!(body["tools"][0]["name"], "n", "{case}: {body}");
                assert!(
                    str_at(&body, "tools.0.description").contains("apply_patch"),
                    "{case}: the dispatcher lost the patch tool: {body}"
                );
            } else {
                assert!(
                    exists(&body, "tools.0.parameters.properties.input"),
                    "{case}: no patch schema: {body}"
                );
            }
        }
    }
}

// TestXAIWebsocketApplyPatchIncompleteEOF
#[tokio::test]
async fn apply_patch_incomplete_eof() {
    let server = Server::once(&[
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":""}}"#,
    ])
    .await;
    let (chunks, error) = call(
        &executor(),
        &auth_as("patch-ws", "test", &server.url),
        r#"{"input":[],"tools":[{"type":"custom","name":"apply_patch"}]}"#,
        ws_options(""),
    )
    .await;
    let error = error.expect("no error");
    assert_eq!(error.status, 502, "{error:?}");
    assert_eq!(error.message, APPLY_PATCH_ERROR_MESSAGE);
    let output = chunks.concat();
    assert_eq!(count(&output, r#""type":"response.failed""#), 1, "{output}");
    assert!(
        !output.contains(r#""type":"response.completed""#),
        "{output}"
    );
}

/// One case of [`apply_patch_dispatcher_evidence_lifecycle`].
struct EvidenceCase {
    name: String,
    events: Vec<String>,
    /// The client tool the call is to, and its arguments; none when the
    /// call must fail.
    want: Option<(&'static str, String)>,
}

impl EvidenceCase {
    fn passes(name: &str, events: &[&str], want_name: &'static str, want_args: &str) -> Self {
        Self {
            name: name.to_owned(),
            events: events.iter().map(|event| (*event).to_owned()).collect(),
            want: Some((want_name, want_args.to_owned())),
        }
    }

    fn fails(name: &str, events: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            events: events.iter().map(|event| (*event).to_owned()).collect(),
            want: None,
        }
    }
}

/// The cases of upstream's `TestXAIApplyPatchDispatcherEvidenceLifecycle`.
#[allow(clippy::too_many_lines)]
fn evidence_cases() -> Vec<EvidenceCase> {
    let patch = r#"{"name":"apply_patch","arguments":{"input":"p"}}"#;
    let ordinary =
        r#"{"name":"lookup0","arguments":{"name":"apply_patch","arguments":{"input":"ordinary"}}}"#;
    let quote = |text: &str| Value::from(text).to_string();
    let added = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a"}}"#;
    let args_done = format!(
        r#"{{"type":"response.function_call_arguments.done","item_id":"a","arguments":{}}}"#,
        quote(patch)
    );
    let args_done = args_done.as_str();
    let done = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}"#;
    let sparse = r#"{"type":"function_call","id":"a","call_id":"c","name":"n"}"#;
    let terminal = r#"{"type":"response.completed","response":{"output":[]}}"#;
    let sparse_terminal =
        format!(r#"{{"type":"response.completed","response":{{"output":[{sparse}]}}}}"#);
    let sparse_terminal = sparse_terminal.as_str();
    let unnamed_added = format!(
        r#"{{"type":"response.output_item.added","output_index":0,"item":{{"type":"function_call","id":"a","arguments":{}}}}}"#,
        quote(patch)
    );
    let ordinary_done = format!(
        r#"{{"type":"response.function_call_arguments.done","item_id":"a","arguments":{}}}"#,
        quote(ordinary)
    );
    let plain_done = format!(
        r#"{{"type":"response.output_item.done","output_index":0,"item":{{"type":"function_call","id":"a","call_id":"c","name":"plain","arguments":{}}}}}"#,
        quote(patch)
    );
    let patch_delta = format!(
        r#"{{"type":"response.function_call_arguments.delta","item_id":"a","delta":{}}}"#,
        quote(patch)
    );
    let named_added = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n"}}"#;
    let child_delta = r#"{"type":"response.function_call_arguments.delta","item_id":"a","name":"lookup0","namespace":"n","delta":""}"#;
    let mut cases = vec![
        EvidenceCase::passes(
            "unnamed_added_full_wrapper",
            &[&unnamed_added, done, terminal],
            "apply_patch",
            "p",
        ),
        EvidenceCase::passes(
            "index_acquired_after_snapshot",
            &[
                args_done,
                r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"a"}}"#,
                r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}"#,
                sparse_terminal,
            ],
            "apply_patch",
            "p",
        ),
        EvidenceCase::fails(
            "completed_namespace_conflict",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","name":"n","namespace":"other"}]}}"#,
            ],
        ),
        EvidenceCase::fails(
            "candidate_dual_call_conflict",
            &[
                r#"{"type":"response.output_item.added","output_index":0,"call_id":"x","item":{"type":"function_call","id":"a","call_id":"c"}}"#,
                args_done,
                done,
                terminal,
            ],
        ),
        EvidenceCase::fails(
            "candidate_dual_item_conflict",
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item_id":"x","item":{"type":"function_call","id":"a"}}"#,
                args_done,
                done,
                terminal,
            ],
        ),
        EvidenceCase::passes(
            "terminal_first_dispatcher_name",
            &[added, args_done, sparse_terminal],
            "apply_patch",
            "p",
        ),
        EvidenceCase::passes(
            "late_ordinary_dispatcher_child",
            &[added, &ordinary_done, done, sparse_terminal],
            "lookup0",
            r#"{"name":"apply_patch","arguments":{"input":"ordinary"}}"#,
        ),
        EvidenceCase::passes(
            "ordinary_wrapper_is_literal",
            &[added, args_done, &plain_done, terminal],
            "plain",
            patch,
        ),
        EvidenceCase::fails(
            "dispatcher_delta_child_conflict",
            &[named_added, child_delta, args_done, done, terminal],
        ),
        EvidenceCase::fails(
            "completed_delta_child_conflict",
            &[added, args_done, done, child_delta, terminal],
        ),
        EvidenceCase::fails(
            "completed_added_child_conflict",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"lookup0","namespace":"n","arguments":""}}"#,
                terminal,
            ],
        ),
        EvidenceCase::passes(
            "completed_added_repeated",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}"#,
                terminal,
            ],
            "apply_patch",
            "p",
        ),
        EvidenceCase::passes(
            "folded_child_snapshot_is_valid",
            &[
                named_added,
                &patch_delta,
                r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"input\":\"p\"}"}"#,
                done,
                terminal,
            ],
            "apply_patch",
            "p",
        ),
        EvidenceCase::passes(
            "completed_child_snapshot_consistent",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"input\":\"p\"}"}"#,
                terminal,
            ],
            "apply_patch",
            "p",
        ),
        EvidenceCase::fails(
            "completed_child_snapshot_conflicting",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"input\":\"q\"}"}"#,
                terminal,
            ],
        ),
        EvidenceCase::fails(
            "completed_arguments_done_conflict",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"q\"}}"}"#,
                terminal,
            ],
        ),
        EvidenceCase::passes(
            "completed_arguments_done_repeated",
            &[added, args_done, done, args_done, terminal],
            "apply_patch",
            "p",
        ),
        EvidenceCase::fails(
            "completed_child_conflict",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","name":"n","arguments":"{\"name\":\"lookup0\",\"arguments\":{\"input\":\"p\"}}"}]}}"#,
            ],
        ),
        EvidenceCase::fails(
            "completed_snapshot_conflict",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","name":"n","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"q\"}}"}]}}"#,
            ],
        ),
        EvidenceCase::fails(
            "completed_call_conflict",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"different","name":"n"}]}}"#,
            ],
        ),
        EvidenceCase::fails(
            "completed_type_conflict",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.completed","response":{"output":[{"type":"message","id":"a","call_id":"c","name":"n"}]}}"#,
            ],
        ),
        EvidenceCase::fails(
            "completed_item_conflict",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"different","call_id":"c","name":"n"}}"#,
                terminal,
            ],
        ),
        EvidenceCase::fails(
            "completed_index_conflict",
            &[
                added,
                args_done,
                done,
                r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}"#,
                terminal,
            ],
        ),
        EvidenceCase::fails(
            "candidate_item_conflict",
            &[
                added,
                r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"different","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#,
                done,
                terminal,
            ],
        ),
        EvidenceCase::fails(
            "candidate_type_conflict",
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"a"}}"#,
                args_done,
                done,
                terminal,
            ],
        ),
        EvidenceCase::fails(
            "candidate_name_conflict",
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"plain"}}"#,
                args_done,
                done,
                terminal,
            ],
        ),
        EvidenceCase::fails(
            "candidate_snapshot_type_conflict",
            &[
                added,
                r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":{"name":"apply_patch","arguments":{"input":"p"}}}"#,
                done,
                terminal,
            ],
        ),
    ];
    for discover in 0..3 {
        let mut events: Vec<String> = (0..3)
            .map(|i| {
                format!(
                    r#"{{"type":"response.output_item.added","output_index":{i},"item":{{"type":"function_call","id":"i{i}","call_id":"c{i}"}}}}"#
                )
            })
            .collect();
        events.push(format!(
            r#"{{"type":"response.function_call_arguments.done","output_index":0,"item_id":"i1","call_id":"c2","arguments":{}}}"#,
            quote(patch)
        ));
        events.push(format!(
            r#"{{"type":"response.output_item.done","output_index":{discover},"item":{{"type":"function_call","id":"i{discover}","call_id":"c{discover}","name":"n"}}}}"#
        ));
        events.push(terminal.to_owned());
        cases.push(EvidenceCase {
            name: format!("candidate_all_keys_discover_{discover}"),
            events,
            want: None,
        });
    }
    cases
}

// TestXAIApplyPatchDispatcherEvidenceLifecycle, its `ws_raw` mode: the
// dispatcher's call is restored to the client's `apply_patch` only on
// evidence that holds together, and fails the call when it doesn't. Its
// `ws_sse` mode is a client that isn't on the Responses WebSocket, which
// goes over HTTP here; `http` and `http_stream` are the HTTP executor's.
#[tokio::test]
async fn apply_patch_dispatcher_evidence_lifecycle() {
    let mut declarations = vec![r#"{"type":"custom","name":"apply_patch"}"#.to_owned()];
    declarations.extend((0..205).map(|i| {
        format!(r#"{{"type":"function","name":"lookup{i}","parameters":{{"type":"object"}}}}"#)
    }));
    let payload = format!(
        r#"{{"input":[],"tools":[{{"type":"function","name":"plain","parameters":{{"type":"object"}}}},{{"type":"namespace","name":"n","tools":[{}]}}]}}"#,
        declarations.join(",")
    );
    for case in evidence_cases() {
        let name = &case.name;
        let frames: Vec<&str> = case.events.iter().map(String::as_str).collect();
        let server = Server::once(&frames).await;
        let (chunks, error) = call(
            &executor(),
            &auth_as("patch-ws", "test", &server.url),
            &payload,
            ws_options(""),
        )
        .await;
        let output = chunks.concat();
        let last = final_response(&chunks);
        let Some((want_name, want_args)) = &case.want else {
            let error =
                error.unwrap_or_else(|| panic!("{name}: the conflict got through: {output}"));
            assert!(
                error.message.contains("apply_patch") && last.is_none(),
                "{name}: the conflict got through: {error:?} {output}"
            );
            assert_eq!(
                count(&output, r#""type":"response.failed""#),
                1,
                "{name}: {output}"
            );
            continue;
        };
        assert!(error.is_none(), "{name}: {error:?} {output}");
        assert_eq!(
            count(&output, r#""type":"response.completed""#),
            1,
            "{name}: no or repeated completion: {output}"
        );
        let last = last.unwrap();
        let item = &last["output"][0];
        assert_eq!(
            item["name"], *want_name,
            "{name}: the child's identity was lost: {last}"
        );
        assert_eq!(
            item["call_id"], "c",
            "{name}: the child's identity was lost: {last}"
        );
        if *want_name == "apply_patch" {
            assert_eq!(item["type"], "custom_tool_call", "{name}: {last}");
            assert_eq!(item["namespace"], "n", "{name}: {last}");
            assert_eq!(str_at(item, "input"), *want_args, "{name}: {last}");
            assert!(
                !output.contains(r#""response.custom_tool_call_input.delta""#),
                "{name}: progress made up: {output}"
            );
            assert_eq!(
                count(&output, r#""type":"response.custom_tool_call_input.done""#),
                1,
                "{name}: {output}"
            );
        } else {
            assert_eq!(item["type"], "function_call", "{name}: {last}");
            assert_eq!(str_at(item, "arguments"), *want_args, "{name}: {last}");
            if *want_name == "lookup0" {
                assert_eq!(item["namespace"], "n", "{name}: {last}");
            }
            assert!(
                !output.contains("custom_tool_call"),
                "{name}: an ordinary call was made a patch: {output}"
            );
            assert_eq!(
                count(&output, r#""type":"response.function_call_arguments.done""#),
                1,
                "{name}: {output}"
            );
        }
    }
}
