// Ported from CLIProxyAPI internal/runtime/executor/apply_patch_identity_test.go
// (applyPatchIdentitySource, applyPatchIdentityExecutor,
// TestApplyPatchNamedLateIdentityActualTransports,
// TestApplyPatchNamedLateIdentityActualFailures,
// TestApplyPatchNamedLateIdentityActualInterleaved) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A bridged `apply_patch` call whose item ID or call ID comes late, or
//! only with its end, through each transport that bridges a Responses or
//! Interactions provider: xAI and Meta over HTTP (`xai`, `meta`), Gemini
//! Interactions (`interactions`) and xAI on the Responses WebSocket
//! (`ws`). The call must keep one identity from its first event to the
//! response's end, and a call whose identity or input changes, or never
//! ends, must fail the response.
//!
//! Deviations from upstream:
//! - The `kimi` transport is dropped: Kimi isn't ported (policy).
//! - Upstream's `ws-sse` transport, xAI's WebSocket executor serving a
//!   client that isn't on the Responses WebSocket, is `xai` here, where
//!   such a client goes over HTTP; its `ws-raw` transport is `ws`.
//! - Upstream's failures also check that the call published one failed
//!   usage record (`task6CaptureFailureUsage`); here the manager's usage
//!   tap records the failure from the stream's error, so they check the
//!   stream ends with exactly one error, the 502.

use std::sync::Arc;

use bytes::Bytes;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ExecError, Format, Options, Request};
use open_ferry_core::executor::ProviderExecutor;
use serde_json::{Value, json};

use super::{
    TASK6_PATCH_REQUEST, Upstream, WAIT, assert_identity_lifecycle, assert_patch_error, auth,
    collect, executor, payloads, sse,
};
use crate::codex::websocket::mock::Server;
use crate::json::{exists, get, get_mut, set, str_at};

/// The transports a bridged call is tested through.
const TRANSPORTS: [&str; 4] = ["xai", "meta", "interactions", "ws"];

/// A provider's stream of one `apply_patch` call with arguments
/// `{"input":"pq"}` (`applyPatchIdentitySource`), in Gemini Interactions'
/// events (`interactions`) or Responses events. `first` says which ID the
/// call starts with: `item`, `call`, or `neither`, which a later
/// argument-less start or delta may bring first (`neither-item-first`,
/// `neither-call-first`). `boundary` is where both IDs come: with an empty
/// delta and the call's end (`delta`), with its end (`item`), or only with
/// the terminal event (`terminal`). With `snapshot` the arguments come
/// only whole, with the end.
fn identity_source(interactions: bool, first: &str, boundary: &str, snapshot: bool) -> Vec<Value> {
    let id = if first == "item" { "a" } else { "" };
    let call = if first == "call" { "c" } else { "" };
    let fragments = if snapshot {
        &[][..]
    } else {
        &[r#"{"input":"p"#, r#"q"}"#][..]
    };
    let late = first.strip_prefix("neither-").map(|first| {
        if first == "call-first" {
            ("call_id", "c")
        } else {
            ("id", "a")
        }
    });
    let mut events = Vec::new();
    if interactions {
        events.push(json!({"event_type": "step.start", "index": 0, "step": {"type": "function_call", "id": id, "call_id": call, "name": "apply_patch"}}));
        for fragment in fragments {
            events.push(json!({"event_type": "step.delta", "index": 0, "delta": {"type": "arguments_delta", "arguments": fragment}}));
        }
        if let Some((key, value)) = late {
            let mut step = json!({"type": "function_call", "name": "apply_patch"});
            set(&mut step, key, json!(value));
            events.push(json!({"event_type": "step.start", "index": 0, "step": step}));
        }
        if boundary == "delta" {
            events.push(json!({"event_type": "step.delta", "index": 0, "step": {"id": "a", "call_id": "c"}, "delta": {"type": "arguments_delta", "arguments": ""}}));
        }
        let step = json!({"index": 0, "type": "function_call", "id": "a", "call_id": "c", "name": "apply_patch", "arguments": {"input": "pq"}});
        if boundary == "item" || boundary == "delta" {
            events.push(json!({"event_type": "step.stop", "index": 0, "step": step}));
        } else {
            events.push(json!({"event_type": "step.stop", "index": 0}));
        }
        events
            .push(json!({"event_type": "interaction.completed", "interaction": {"steps": [step]}}));
    } else {
        let args = r#"{"input":"pq"}"#;
        events.push(json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "id": id, "call_id": call, "name": "apply_patch", "arguments": ""}}));
        for fragment in fragments {
            events.push(json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": fragment}));
        }
        if let Some((key, value)) = late {
            let key = if key == "id" { "item_id" } else { key };
            let mut delta =
                json!({"type": "response.function_call_arguments.delta", "output_index": 0});
            set(&mut delta, key, json!(value));
            set(&mut delta, "delta", json!(""));
            events.push(delta);
        }
        if boundary == "delta" {
            events.push(json!({"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "a", "call_id": "c", "delta": ""}));
        }
        events.push(json!({"type": "response.function_call_arguments.done", "output_index": 0, "arguments": args}));
        let item = json!({"type": "function_call", "id": "a", "call_id": "c", "name": "apply_patch", "arguments": args});
        if boundary == "item" || boundary == "delta" {
            events.push(
                json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            );
        }
        events.push(json!({"type": "response.completed", "response": {"output": [item]}}));
    }
    events
}

/// A mock serving `source` through `transport`, and the executor and the
/// credential to call it with (`applyPatchIdentityExecutor`). Over HTTP the
/// events come as an event stream; on the WebSocket, one message each,
/// after the request, and then the connection ends. The mock must outlive
/// the call.
async fn identity_executor(
    transport: &str,
    source: &[Value],
) -> (Box<dyn std::any::Any>, Arc<dyn ProviderExecutor>, Arc<Auth>) {
    let events: Vec<String> = source.iter().map(Value::to_string).collect();
    let provider = match transport {
        "interactions" => "gemini-interactions",
        "ws" => "xai",
        other => other,
    };
    let executor = executor(provider);
    if transport == "ws" {
        let frames: Vec<&str> = events.iter().map(String::as_str).collect();
        let server = Server::once(&frames).await;
        let auth = auth(provider, executor.as_ref(), &server.url, true);
        return (Box::new(server), executor, auth);
    }
    let upstream = Upstream::answering(&sse(&events)).await;
    let auth = auth(provider, executor.as_ref(), &upstream.url, false);
    (Box::new(upstream), executor, auth)
}

/// Streams [`TASK6_PATCH_REQUEST`] from a Responses client through
/// `transport`, whose provider streams `source`: the client's events, and
/// the stream's errors. A client on the Responses WebSocket for `ws`.
async fn stream(transport: &str, source: &[Value]) -> (Vec<String>, Vec<ExecError>) {
    let (_mock, executor, auth) = identity_executor(transport, source).await;
    let options = Options {
        stream: true,
        downstream_websocket: transport == "ws",
        ..Options::new(Format::OPENAI_RESPONSE)
    };
    let request = Request {
        model: "grok-4".into(),
        payload: Bytes::from_static(TASK6_PATCH_REQUEST.as_bytes()),
    };
    let response = tokio::time::timeout(WAIT, executor.execute_stream(auth, request, options))
        .await
        .unwrap_or_else(|_| panic!("{transport}: the call didn't start"))
        .unwrap_or_else(|error| panic!("{transport}: {error:?}"));
    collect(response).await
}

// TestApplyPatchNamedLateIdentityActualTransports
#[tokio::test]
async fn named_late_identity_actual_transports() {
    for transport in TRANSPORTS {
        let interactions = transport == "interactions";
        for first in [
            "item",
            "call",
            "neither",
            "neither-item-first",
            "neither-call-first",
        ] {
            for boundary in ["delta", "item", "terminal"] {
                for snapshot in [false, true] {
                    let case = format!("{transport}/{first}/{boundary}/snapshot={snapshot}");
                    let source = identity_source(interactions, first, boundary, snapshot);
                    let (chunks, errors) = stream(transport, &source).await;
                    assert!(errors.is_empty(), "{case}: {errors:?}\n{chunks:?}");
                    let events: Vec<Value> =
                        chunks.iter().flat_map(|chunk| payloads(chunk)).collect();
                    // Interactions keeps its one snapshot delta.
                    let fragments: &[&str] = match (snapshot, interactions) {
                        (false, _) => &["p", "q"],
                        (true, true) => &["pq"],
                        (true, false) => &[],
                    };
                    let checked = std::panic::catch_unwind(|| {
                        assert_identity_lifecycle(&events, "a", "c", "pq", 0, fragments);
                    });
                    assert!(checked.is_ok(), "{case}: {chunks:?}");
                }
            }
        }
    }
}

// TestApplyPatchNamedLateIdentityActualFailures
#[tokio::test]
async fn named_late_identity_actual_failures() {
    for transport in TRANSPORTS {
        let interactions = transport == "interactions";
        let (item_path, final_path) = if interactions {
            ("step", "interaction.steps.0")
        } else {
            ("item", "response.output.0")
        };
        for mode in [
            "item-conflict",
            "call-conflict",
            "partial-snapshot",
            "invalid-snapshot",
            "eof",
        ] {
            let case = format!("{transport}/{mode}");
            let first = if mode == "call-conflict" {
                "call"
            } else {
                "item"
            };
            let mut source = identity_source(interactions, first, "item", false);
            let last = source.len() - 1;
            match mode {
                "item-conflict" | "call-conflict" => {
                    let key = if mode == "call-conflict" {
                        "call_id"
                    } else {
                        "id"
                    };
                    set(
                        &mut source[last - 1],
                        &format!("{item_path}.{key}"),
                        json!("changed"),
                    );
                    set(
                        &mut source[last],
                        &format!("{final_path}.{key}"),
                        json!("changed"),
                    );
                }
                "partial-snapshot" | "invalid-snapshot" => {
                    let args = if mode == "invalid-snapshot" {
                        r#"{"input":"p","extra":"RAW_SECRET"}"#
                    } else {
                        r#"{"input":"p"#
                    };
                    set(
                        &mut source[0],
                        &format!("{item_path}.arguments"),
                        json!(args),
                    );
                }
                _ => source.truncate(last - 1),
            }
            let (chunks, errors) = stream(transport, &source).await;
            for error in &errors {
                assert_patch_error(error);
            }
            let mut failed = 0;
            for chunk in &chunks {
                assert!(
                    !chunk.contains("RAW_SECRET") && !chunk.contains("[DONE]"),
                    "{case}: the failure leaked or completed: {chunk}"
                );
                for event in payloads(chunk) {
                    assert_eq!(
                        event["type"], "response.failed",
                        "{case}: an unresolved call was published: {event}"
                    );
                    failed += 1;
                }
            }
            assert!(
                failed == 1 && errors.len() == 1,
                "{case}: failed={failed} errors={errors:?}\n{chunks:?}"
            );
        }
    }
}

// TestApplyPatchNamedLateIdentityActualInterleaved: two calls whose events
// interleave keep their own identities, indexes and inputs.
#[tokio::test]
async fn named_late_identity_actual_interleaved() {
    for transport in TRANSPORTS {
        let interactions = transport == "interactions";
        let first = identity_source(interactions, "item", "item", false);
        let mut second = identity_source(interactions, "call", "item", false);
        let (index_key, item_path, final_path) = if interactions {
            ("index", "step", "interaction.steps")
        } else {
            ("output_index", "item", "response.output")
        };
        let last = second.len() - 1;
        for (i, event) in second.iter_mut().enumerate().take(last) {
            if exists(event, index_key) {
                set(event, index_key, json!(1));
            }
            if str_at(event, &format!("{item_path}.id")) == "a" {
                set(event, &format!("{item_path}.id"), json!("b"));
            }
            if str_at(event, &format!("{item_path}.call_id")) == "c" {
                set(event, &format!("{item_path}.call_id"), json!("d"));
            }
            if interactions && exists(event, "step.index") {
                set(event, "step.index", json!(1));
            }
            *event = super::json(&event.to_string().replace("pq", "uv"));
            if i == 1 || i == 2 {
                let path = if interactions {
                    "delta.arguments"
                } else {
                    "delta"
                };
                let value = if i == 2 { r#"v"}"# } else { r#"{"input":"u"# };
                set(event, path, json!(value));
            }
        }
        let mut terminal = first[first.len() - 1].clone();
        let item = get(&second[last - 1], item_path)
            .cloned()
            .expect("the second call's end has its item");
        get_mut(&mut terminal, final_path)
            .and_then(Value::as_array_mut)
            .expect("the terminal event has its output")
            .push(item);
        let mut source = vec![
            first[0].clone(),
            second[0].clone(),
            first[1].clone(),
            second[1].clone(),
            second[2].clone(),
            first[2].clone(),
        ];
        source.extend_from_slice(&second[3..last]);
        source.extend_from_slice(&first[3..first.len() - 1]);
        source.push(terminal);

        let (chunks, errors) = stream(transport, &source).await;
        assert!(errors.is_empty(), "{transport}: {errors:?}\n{chunks:?}");
        let mut per_call: [Vec<Value>; 2] = [Vec::new(), Vec::new()];
        for event in chunks.iter().flat_map(|chunk| payloads(chunk)) {
            if event["type"] == "response.completed" {
                let output = event["response"]["output"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                assert_eq!(output.len(), 2, "{transport}: lost a final call: {event}");
                for (index, item) in output.into_iter().enumerate() {
                    let mut last = event.clone();
                    set(&mut last, "response.output", json!([item]));
                    per_call[index].push(last);
                }
            } else {
                let index = event["output_index"].as_i64().unwrap_or_default();
                let calls = usize::try_from(index)
                    .ok()
                    .and_then(|index| per_call.get_mut(index))
                    .unwrap_or_else(|| panic!("{transport}: unknown call index: {event}"));
                calls.push(event);
            }
        }
        for (events, (id, call, input, index, fragments)) in per_call.iter().zip([
            ("a", "c", "pq", 0, ["p", "q"]),
            ("b", "d", "uv", 1, ["u", "v"]),
        ]) {
            let checked = std::panic::catch_unwind(|| {
                assert_identity_lifecycle(events, id, call, input, index, &fragments);
            });
            assert!(checked.is_ok(), "{transport}: {chunks:?}");
        }
    }
}
