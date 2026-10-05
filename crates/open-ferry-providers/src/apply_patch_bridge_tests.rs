// Ported from CLIProxyAPI internal/runtime/executor/apply_patch_bridge_test.go
// (applyPatchTestInput, applyPatchTestPartial, applyPatchTestRemainder,
// applyPatchTestRequest, applyPatchTestPayloads, applyPatchTestFrames,
// applyPatchTestDeclaration, applyPatchTestLifecycle),
// apply_patch_identity_test.go (assertApplyPatchIdentityLifecycle),
// apply_patch_integration_test.go (task6PatchRequest, task6ProviderFixture,
// assertTask6PatchError, assertTask6FailedStream) and
// apply_patch_repair_test.go (task6RepairSSE) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! upstream's executor-level `apply_patch` tests, part 1: the bridge
//! ([`bridge`]), the identity of a call whose name or IDs come late
//! ([`identity`]), an Interactions call whose step stops before its input
//! is whole ([`source_stop`]) and upstream's `helps` stream helpers
//! ([`helps`]). Each runs the executors this port serves against a mock on
//! an ephemeral port of 127.0.0.1, with a dummy key: Codex (on the
//! Responses WebSocket), Claude (an API key, and an OAuth token as the key),
//! Gemini, Vertex AI, Gemini Interactions, an OpenAI-compatible provider
//! (upstream's `custom-compat`, here `openai-compatible-custom`), Meta and
//! xAI (over HTTP, and on the Responses WebSocket). Upstream's
//! `apply_patch_capability_test.go` is ported in the server's model list
//! tests (`handlers/models/codex/tests.rs`), where what a provider takes is
//! decided; so is the model list half of
//! `TestApplyPatchBridgeLiveHTTPPreviewMatrix`, as upstream's gateway is the
//! server's.
//!
//! This file holds what the tests share: the patch, the request, the
//! frames each protocol streams it in, the checks of the declaration sent
//! and of the lifecycle given back, a mock HTTP upstream that can hold its
//! answer back, and the executors and credentials.
//!
//! Upstream's executors publish their own usage, and its tests check that a
//! failed call publishes exactly one failure (`task6CaptureFailureUsage`).
//! Here the manager's usage tap records the call from the error the stream
//! ends with, so those checks become: the stream ends with exactly one
//! error, the 502.
//!
//! Deviations from upstream:
//! - Kimi, Devin and Antigravity aren't ported (policy), so their cases are
//!   dropped: their rows of `TestApplyPatchBridgeLiveHTTPPreviewMatrix` and
//!   of the identity tests, and `TestApplyPatchBridgeKimiReusedClientControls`.
//! - Upstream's `ws-sse` transport, the WebSocket executor serving a client
//!   that isn't on the Responses WebSocket, is the HTTP one here: such a
//!   client goes over HTTP (the `xai` transport). The `ws-raw` transport is
//!   the `ws` one.
//! - gin's test mode, the gateway and the global model registry are the
//!   server's: these tests call the executors with the options the Responses
//!   handler gives them.
//! - `RequiredUpstreamWebsocket` and upstream's attempt markers, and Home,
//!   aren't ported.
//! - No session or device ID is made up (policy): the Claude OAuth case
//!   sends the token as its key without upstream's `account_uuid`.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ExecError, StreamResponse};
use open_ferry_core::executor::ProviderExecutor;
use serde_json::Value;
use tokio::sync::watch;

use crate::claude::ClaudeExecutor;
use crate::codex::CodexExecutor;
use crate::gemini::{GeminiExecutor, InteractionsExecutor, VertexExecutor};
use crate::json::{get, str_at};
use crate::meta::MetaExecutor;
use crate::openai_compat::OpenAiCompatExecutor;
use crate::xai::XaiExecutor;

mod bridge;
mod helps;
mod identity;
mod source_stop;

/// How long a test waits for a call.
pub(crate) const WAIT: Duration = Duration::from_secs(10);

/// The patch (upstream's `applyPatchTestInput`).
pub(crate) const PATCH_INPUT: &str =
    "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch\n";

/// The arguments' first fragment, up to a whole line of the patch
/// (`applyPatchTestPartial`).
pub(crate) const PATCH_PARTIAL: &str =
    r#"{"input":"*** Begin Patch\n*** Add File: a.txt\n+hello\n"#;

/// The arguments' last fragment (`applyPatchTestRemainder`).
pub(crate) const PATCH_REMAINDER: &str = r#"*** End Patch\n"}"#;

/// A Responses request with the grammar `apply_patch` tool
/// (`applyPatchTestRequest`).
pub(crate) const PATCH_REQUEST: &str = r#"{"model":"test","input":[{"role":"user","content":"edit a.txt"}],"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}]}"#;

/// The smallest Responses request with the `apply_patch` tool
/// (`task6PatchRequest`).
pub(crate) const TASK6_PATCH_REQUEST: &str =
    r#"{"input":"patch","tools":[{"type":"custom","name":"apply_patch"}]}"#;

/// The client's error for an `apply_patch` call that can't be carried over.
pub(crate) const PATCH_ERROR: &str = "Invalid apply_patch tool arguments received from upstream.";

/// `text` as a JSON string (Go's `%q` for the text these tests quote).
pub(crate) fn q(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// `text` as JSON.
pub(crate) fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
}

/// The JSON of each `data:` line of `chunk`, or of `chunk` itself when it
/// has none and is JSON, as a client on the Responses WebSocket is given
/// (`applyPatchTestPayloads`).
pub(crate) fn payloads(chunk: &str) -> Vec<Value> {
    let mut out: Vec<Value> = chunk
        .split('\n')
        .filter_map(|line| line.trim().strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect();
    if out.is_empty()
        && let Ok(value) = serde_json::from_str(chunk)
    {
        out.push(value);
    }
    out
}

/// The JSON events of all of `chunks`.
pub(crate) fn events(chunks: &[String]) -> Vec<Value> {
    chunks.iter().flat_map(|chunk| payloads(chunk)).collect()
}

/// `events` as an event stream (`task6RepairSSE`).
pub(crate) fn sse(events: &[String]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

/// What a `protocol` provider streams before and after the patch's first
/// whole line, when it calls the tool `name` (`applyPatchTestFrames`).
/// Gemini's (`gemini`) gives the call whole at the end, after some text.
pub(crate) fn frames(protocol: &str, name: &str) -> (String, String) {
    let args = format!("{PATCH_PARTIAL}{PATCH_REMAINDER}");
    let (partial, remainder) = (q(PATCH_PARTIAL), q(PATCH_REMAINDER));
    let name = q(name);
    match protocol {
        "claude" => (
            format!(
                "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"r1\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-6\"}}}}\n\n\
                 data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":{name},\"input\":{{}}}}}}\n\n\
                 data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{partial}}}}}\n\n"
            ),
            format!(
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{remainder}}}}}\n\n\
                 data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
                 data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}}}}\n\n\
                 data: {{\"type\":\"message_stop\"}}\n\n"
            ),
        ),
        "interactions" => (
            format!(
                "data: {{\"event_type\":\"interaction.created\",\"interaction\":{{\"id\":\"r1\"}}}}\n\n\
                 data: {{\"event_type\":\"step.start\",\"index\":0,\"step\":{{\"type\":\"function_call\",\"id\":\"c1\",\"call_id\":\"c1\",\"name\":{name}}}}}\n\n\
                 data: {{\"event_type\":\"step.delta\",\"index\":0,\"delta\":{{\"type\":\"arguments_delta\",\"arguments\":{partial}}}}}\n\n"
            ),
            format!(
                "data: {{\"event_type\":\"step.delta\",\"index\":0,\"delta\":{{\"type\":\"arguments_delta\",\"arguments\":{remainder}}}}}\n\n\
                 data: {{\"event_type\":\"step.stop\",\"index\":0}}\n\n\
                 data: {{\"event_type\":\"interaction.completed\",\"interaction\":{{\"id\":\"r1\"}}}}\n\n\
                 data: [DONE]\n\n"
            ),
        ),
        "responses" => {
            let item = format!(
                r#"{{"type":"function_call","id":"a1","call_id":"c1","name":{name},"arguments":{}}}"#,
                q(&args)
            );
            (
                format!(
                    "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"r1\"}}}}\n\n\
                     data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"id\":\"a1\",\"call_id\":\"c1\",\"name\":{name},\"arguments\":\"\"}}}}\n\n\
                     data: {{\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"a1\",\"delta\":{partial}}}\n\n"
                ),
                format!(
                    "data: {{\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"a1\",\"delta\":{remainder}}}\n\n\
                     data: {{\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"item_id\":\"a1\",\"arguments\":{}}}\n\n\
                     data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{item}}}\n\n\
                     data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r1\",\"status\":\"completed\",\"output\":[{item}]}}}}\n\n",
                    q(&args)
                ),
            )
        }
        "gemini" => (
            "data: {\"responseId\":\"r1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"preparing\"}]}}]}\n\n".to_owned(),
            format!(
                "data: {{\"responseId\":\"r1\",\"candidates\":[{{\"content\":{{\"parts\":[{{\"functionCall\":{{\"name\":{name},\"args\":{args}}}}}]}},\"finishReason\":\"STOP\"}}]}}\n\n"
            ),
        ),
        _ => (
            format!(
                "data: {{\"id\":\"r1\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{{\"name\":{name},\"arguments\":{partial}}}}}]}}}}]}}\n\n"
            ),
            format!(
                "data: {{\"id\":\"r1\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":{remainder}}}}}]}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
                 data: [DONE]\n\n"
            ),
        ),
    }
}

/// An OpenAI-compatible provider's answer, as upstream's
/// `task6ProviderFixture("custom-compat", mode, ...)` gives it: a Chat
/// Completions body (`nonstream`) or stream (`stream`) that calls
/// `apply_patch` with `args`.
pub(crate) fn compat_fixture(mode: &str, args: &str) -> String {
    let args = q(args);
    if mode == "nonstream" {
        return format!(
            r#"{{"id":"r","object":"chat.completion","choices":[{{"message":{{"tool_calls":[{{"id":"c","type":"function","function":{{"name":"apply_patch","arguments":{args}}}}}]}},"finish_reason":"tool_calls"}}],"usage":{{"prompt_tokens":5,"completion_tokens":3}}}}"#
        );
    }
    format!(
        "data: {{\"id\":\"r\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c\",\"type\":\"function\",\"function\":{{\"name\":\"apply_patch\",\"arguments\":{args}}}}}]}}}}]}}\n\n\
         data: {{\"id\":\"r\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":3}}}}\n\n\
         data: [DONE]\n\n"
    )
}

/// Checks the standard function `body` declares `apply_patch` as, for a
/// `protocol` provider: the patch format in its description and a strict
/// `input` string (`applyPatchTestDeclaration`).
#[track_caller]
pub(crate) fn assert_declaration(protocol: &str, body: &Value) {
    let (tool, parameters) = match protocol {
        "chat" => {
            assert_eq!(
                str_at(body, "tools.0.type"),
                "function",
                "not a standard function: {body}"
            );
            (get(body, "tools.0.function"), "parameters")
        }
        "gemini" => (
            get(body, "tools.0.functionDeclarations.0"),
            "parametersJsonSchema",
        ),
        "claude" => (get(body, "tools.0"), "input_schema"),
        _ => (get(body, "tools.0"), "parameters"),
    };
    let tool = tool.unwrap_or_else(|| panic!("no tool declared: {body}"));
    let description = str_at(tool, "description");
    for marker in [
        "*** Begin Patch",
        "*** End Patch",
        "*** Add File:",
        "*** Update File:",
        "*** Delete File:",
        "@@",
        "start: patch",
    ] {
        assert!(
            description.contains(marker),
            "missing patch format marker {marker:?}: {tool}"
        );
    }
    assert!(
        str_at(tool, &format!("{parameters}.properties.input.type")) == "string"
            && str_at(tool, &format!("{parameters}.required.0")) == "input",
        "wrong standard input schema: {tool}"
    );
    let extra = get(tool, &format!("{parameters}.additionalProperties"));
    assert!(
        extra.is_some_and(|extra| extra == &Value::Bool(false)),
        "schema permits extra properties: {tool}"
    );
}

/// What a client was given of one `apply_patch` call
/// (`applyPatchTestLifecycle`).
#[derive(Debug, Default)]
pub(crate) struct Lifecycle {
    pub(crate) deltas: String,
    pub(crate) input_done: Option<String>,
    pub(crate) item_done: Option<String>,
    pub(crate) response_done: Option<String>,
    pub(crate) completed: usize,
    pub(crate) inputs: usize,
    pub(crate) items: usize,
    pub(crate) item_id: String,
    pub(crate) call_id: String,
}

impl Lifecycle {
    /// Takes one event in, failing on one a bridged call mustn't give.
    #[track_caller]
    pub(crate) fn consume(&mut self, event: &Value) {
        match str_at(event, "type").as_str() {
            kind @ ("response.failed" | "error" | "response.function_call_arguments.delta") => {
                panic!("unexpected bridge event {kind}: {event}")
            }
            kind @ ("response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done") => {
                let (id, call) = (str_at(event, "item_id"), str_at(event, "call_id"));
                assert!(
                    !id.is_empty()
                        && !call.is_empty()
                        && (self.item_id.is_empty()
                            || (self.item_id == id && self.call_id == call)),
                    "unstable call identity: {event}"
                );
                (self.item_id, self.call_id) = (id, call);
                if kind == "response.custom_tool_call_input.delta" {
                    self.deltas.push_str(&str_at(event, "delta"));
                } else {
                    self.inputs += 1;
                    self.input_done = Some(str_at(event, "input"));
                }
            }
            "response.output_item.done" => {
                let item = &event["item"];
                if item["type"] == "custom_tool_call" {
                    assert!(
                        str_at(item, "id") == self.item_id
                            && str_at(item, "call_id") == self.call_id
                            && str_at(item, "name") == "apply_patch",
                        "item lost identity: {item}"
                    );
                    self.items += 1;
                    self.item_done = Some(str_at(item, "input"));
                }
            }
            "response.completed" => {
                self.completed += 1;
                let item = event["response"]["output"]
                    .as_array()
                    .and_then(|output| {
                        output
                            .iter()
                            .find(|item| item["type"] == "custom_tool_call")
                    })
                    .unwrap_or(&Value::Null);
                assert!(
                    str_at(item, "id") == self.item_id && str_at(item, "call_id") == self.call_id,
                    "response lost identity: {event}"
                );
                self.response_done = Some(str_at(item, "input"));
            }
            _ => {}
        }
    }

    /// Whether the call's input, the item or the response is done.
    pub(crate) fn done(&self) -> bool {
        self.completed != 0 || self.input_done.is_some() || self.item_done.is_some()
    }

    /// Checks the call was given whole, once, as the patch.
    #[track_caller]
    pub(crate) fn assert_whole(&self, case: &str) {
        let whole = Some(PATCH_INPUT.to_owned());
        assert!(
            self.deltas == PATCH_INPUT
                && self.input_done == whole
                && self.item_done == whole
                && self.response_done == whole
                && (self.inputs, self.items, self.completed) == (1, 1, 1),
            "inconsistent lifecycle: {self:?}\n{case}"
        );
    }
}

/// Checks every identity of an `apply_patch` call's lifecycle, not merely
/// the final snapshot or the counts (upstream's
/// `assertApplyPatchIdentityLifecycle`): the call `call` of item `id` at
/// `index`, whose input is `input`, streamed as `fragments`.
#[track_caller]
pub(crate) fn assert_identity_lifecycle(
    events: &[Value],
    id: &str,
    call: &str,
    input: &str,
    index: i64,
    fragments: &[&str],
) {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
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

/// Checks `error` is the clean 502 of an `apply_patch` call that couldn't
/// be carried over (`assertTask6PatchError`).
#[track_caller]
pub(crate) fn assert_patch_error(error: &ExecError) {
    assert!(
        error.status == 502 && error.message == PATCH_ERROR,
        "expected the clean 502, got {error:?}"
    );
}

/// Checks a stream failed as a bridged call must: one `response.failed`,
/// then the 502, and nothing of the provider's call or of a success
/// (`assertTask6FailedStream`).
#[track_caller]
pub(crate) fn assert_failed_stream(chunks: &[String], errors: &[ExecError]) {
    let output = chunks.concat();
    let failed = output
        .split('\n')
        .filter_map(|line| line.strip_prefix("data:"))
        .filter(|data| {
            serde_json::from_str::<Value>(data.trim())
                .is_ok_and(|event| event["type"] == "response.failed")
        })
        .count();
    for error in errors {
        assert_patch_error(error);
    }
    assert!(
        failed == 1
            && errors.len() == 1
            && !output.contains(r#""type":"response.completed""#)
            && !output.contains("[DONE]")
            && !output.contains("RAW_SECRET")
            && !output.contains(r#""input":7"#),
        "failure contract: failed={failed} errors={} output={output}",
        errors.len()
    );
}

/// Reads a stream to its end: its chunks and its errors, failing the test
/// if it takes too long.
pub(crate) async fn collect(response: StreamResponse) -> (Vec<String>, Vec<ExecError>) {
    let mut stream = response.chunks;
    let mut chunks = Vec::new();
    let mut errors = Vec::new();
    tokio::time::timeout(WAIT, async {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => chunks.push(String::from_utf8_lossy(&chunk).into_owned()),
                Err(error) => errors.push(error),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the stream didn't end: {chunks:?}"));
    (chunks, errors)
}

/// A mock HTTP upstream on 127.0.0.1 that keeps each request's body. Its
/// answer can come in two parts, the second held back until
/// [`Upstream::release`]: an event stream unless it starts with `{`.
pub(crate) struct Upstream {
    pub(crate) url: String,
    bodies: Arc<Mutex<Vec<String>>>,
    release: watch::Sender<bool>,
}

/// What [`Upstream`] answers: the first part, and the part it holds back
/// until it is released, if any.
pub(crate) type Parts = (String, Option<String>);

impl Upstream {
    /// An upstream that answers every request with `body`.
    pub(crate) async fn answering(body: &str) -> Self {
        let body = body.to_owned();
        Self::start(move |_| (body.clone(), None)).await
    }

    /// An upstream that answers each request with the parts `reply` makes
    /// of its body.
    pub(crate) async fn start(reply: impl Fn(&Value) -> Parts + Send + Sync + 'static) -> Self {
        let reply = Arc::new(reply);
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (release, gate) = watch::channel(false);
        let recorder = Arc::clone(&bodies);
        let app = Router::new().fallback(move |body: Bytes| {
            let reply = Arc::clone(&reply);
            let recorder = Arc::clone(&recorder);
            let mut gate = gate.clone();
            async move {
                let text = String::from_utf8_lossy(&body).into_owned();
                let (first, last) = reply(&serde_json::from_str(&text).unwrap_or(Value::Null));
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(text);
                let content_type = if first.starts_with('{') {
                    "application/json"
                } else {
                    "text/event-stream"
                };
                let head =
                    futures_util::stream::once(
                        async move { Ok::<_, io::Error>(Bytes::from(first)) },
                    );
                let tail = futures_util::stream::once(async move {
                    match last {
                        Some(last) if gate.wait_for(|open| *open).await.is_ok() => {
                            Some(Ok(Bytes::from(last)))
                        }
                        _ => None,
                    }
                })
                .filter_map(std::future::ready);
                axum::response::Response::builder()
                    .header("content-type", content_type)
                    .body(Body::from_stream(head.chain(tail)))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            url,
            bodies,
            release,
        }
    }

    /// Lets every held answer finish.
    pub(crate) fn release(&self) {
        self.release.send_replace(true);
    }

    /// The bodies received, as JSON.
    pub(crate) fn bodies(&self) -> Vec<Value> {
        self.bodies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|body| json(body))
            .collect()
    }

    /// The only body received, as JSON.
    #[track_caller]
    pub(crate) fn body(&self) -> Value {
        let mut bodies = self.bodies();
        assert_eq!(bodies.len(), 1, "requests: {bodies:?}");
        bodies.remove(0)
    }
}

/// The OpenAI-compatible provider standing in for upstream's
/// `custom-compat`.
pub(crate) const COMPAT: &str = "openai-compatible-custom";

/// The executor of upstream's provider `provider`: `custom-compat`,
/// `claude`, `claude-oauth`, `gemini`, `vertex`, `gemini-interactions`,
/// `meta`, `xai` or `codex`.
pub(crate) fn executor(provider: &str) -> Arc<dyn ProviderExecutor> {
    match provider {
        "custom-compat" => {
            let mut config = Config::default();
            config.proxy_url = "direct".into();
            Arc::new(OpenAiCompatExecutor::new(COMPAT, Arc::new(config)))
        }
        "claude" | "claude-oauth" => Arc::new(ClaudeExecutor::new("direct")),
        "gemini" => Arc::new(GeminiExecutor::new("direct")),
        "vertex" => Arc::new(VertexExecutor::new("direct")),
        "gemini-interactions" => Arc::new(InteractionsExecutor::new("direct")),
        "meta" => Arc::new(MetaExecutor::new("direct")),
        "xai" => Arc::new(XaiExecutor::new("direct")),
        "codex" => Arc::new(CodexExecutor::new("direct")),
        other => panic!("no executor for {other}"),
    }
}

/// A credential of `executor` for `base_url` with a dummy key: an OAuth
/// token for `claude-oauth`, with websockets on for `websockets`.
pub(crate) fn auth(
    provider: &str,
    executor: &dyn ProviderExecutor,
    base_url: &str,
    websockets: bool,
) -> Arc<Auth> {
    let mut auth = Auth {
        id: format!("apply-patch-{provider}"),
        provider: executor.id().to_owned(),
        ..Auth::default()
    };
    let key = if provider == "claude-oauth" {
        "sk-ant-oat-test"
    } else {
        "test"
    };
    auth.attributes.insert("api_key".into(), key.into());
    auth.attributes.insert("base_url".into(), base_url.into());
    if websockets {
        auth.attributes.insert("websockets".into(), "true".into());
    }
    Arc::new(auth)
}
