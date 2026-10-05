// Ported from CLIProxyAPI internal/runtime/executor/apply_patch_bridge_test.go
// (TestApplyPatchBridgeLiveHTTPPreviewMatrix,
// TestApplyPatchBridgeInvalidArgumentsReturnsBadGateway,
// TestApplyPatchBridgeHistoryRoundTrip,
// TestApplyPatchBridgeTruncatedStreamDoesNotComplete,
// TestApplyPatchBridgeOrdinaryFunctionControl,
// TestApplyPatchBridgeLiveWebsocketPreview,
// TestApplyPatchBridgeOrdinaryFunctionStreamControl,
// TestApplyPatchBridgeInvalidStreamArguments, applyPatchTestPreviewChunks,
// newApplyPatchCompatTestExecutor, applyPatchTestChatReply) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The bridge end to end: a client's custom `apply_patch` tool declared to
//! each provider as a standard function, the provider's call given back as
//! the custom call, line by line as it streams, and arguments that can't be
//! a patch failing the call with a 502.
//!
//! Deviations from upstream:
//! - `TestApplyPatchBridgeLiveHTTPPreviewMatrix` calls the executor with
//!   the options the Responses handler gives it, and reads its stream,
//!   rather than a gin gateway over a real socket: the server's flush is the
//!   server's. Its model list half is in the server's model list tests.
//! - Its Devin, Kimi and Antigravity rows, and
//!   `TestApplyPatchBridgeKimiReusedClientControls`, are dropped: those
//!   providers aren't ported (policy).
//! - Upstream's Codex WebSocket executor turns off cloaking for
//!   `TestApplyPatchBridgeLiveWebsocketPreview`; nothing is cloaked here.

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::HeaderValue;
use open_ferry_core::exec::{Format, Options, Request, StreamResponse};
use serde_json::{Value, json as json_value};
use tokio::sync::watch;

use super::{
    Lifecycle, PATCH_INPUT, PATCH_PARTIAL, PATCH_REMAINDER, PATCH_REQUEST, Upstream, WAIT,
    assert_declaration, assert_failed_stream, assert_patch_error, auth, collect, compat_fixture,
    executor, frames, json, payloads, q,
};
use crate::codex::request::RESPONSES_LITE_HEADER;
use crate::codex::websocket::mock::{Answer, Server};
use crate::json::{exists, get, set, str_at};

/// The arguments upstream's invalid argument tests send, each of which
/// can't be a patch.
const INVALID_ARGS: [&str; 7] = [
    r#"{"input":7}"#,
    "{}",
    r#"{"input":null}"#,
    r#"{"input":"x","input":"y"}"#,
    r#"{"input":"x","extra":"RAW_SECRET"}"#,
    r#"{"input":"RAW_SECRET""#,
    r#"{"input":"x"} trailing"#,
];

/// [`INVALID_ARGS`] as upstream's stream test has them.
const INVALID_STREAM_ARGS: [&str; 7] = [
    r#"{"input":7}"#,
    "{}",
    r#"{"input":null}"#,
    r#"{"input":"x","input":"y"}"#,
    r#"{"input":"x","extra":"RAW_SECRET"}"#,
    r#"{"input":"unfinished""#,
    r#"{"input":"x"} trailing"#,
];

/// The arguments of the ordinary function tests.
const ORDINARY_ARGS: &str = r#"{"input":"RAW_SECRET","extra":7}"#;

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_owned()),
    }
}

/// The options the handler of `format` gives an executor for `payload`.
fn options(format: Format, payload: &str, stream: bool) -> Options {
    Options {
        stream,
        original_request: Bytes::from(payload.to_owned()),
        ..Options::new(format)
    }
}

/// A Chat Completions answer calling `apply_patch` with `arguments`
/// (`applyPatchTestChatReply`).
fn chat_reply(arguments: &str) -> String {
    format!(
        r#"{{"id":"r1","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","tool_calls":[{{"id":"c1","type":"function","function":{{"name":"apply_patch","arguments":{}}}}}]}},"finish_reason":"tool_calls"}}]}}"#,
        q(arguments)
    )
}

/// An OpenAI-compatible provider answering every request with `reply`
/// (`newApplyPatchCompatTestExecutor`).
async fn compat(reply: &str) -> Upstream {
    Upstream::answering(reply).await
}

/// Calls the OpenAI-compatible provider of `upstream` once, unstreamed.
async fn compat_execute(
    upstream: &Upstream,
    format: Format,
    payload: &str,
) -> Result<Value, open_ferry_core::exec::ExecError> {
    let executor = executor("custom-compat");
    let auth = auth(
        "custom-compat",
        executor.as_ref(),
        &format!("{}/v1", upstream.url),
        false,
    );
    let response = tokio::time::timeout(
        WAIT,
        executor.execute(
            auth,
            request("test", payload),
            options(format, payload, false),
        ),
    )
    .await
    .expect("the call didn't end")?;
    Ok(json(&String::from_utf8_lossy(&response.payload)))
}

/// Calls the OpenAI-compatible provider of `upstream` once, streamed.
async fn compat_stream(upstream: &Upstream, format: Format, payload: &str) -> StreamResponse {
    let executor = executor("custom-compat");
    let auth = auth(
        "custom-compat",
        executor.as_ref(),
        &format!("{}/v1", upstream.url),
        false,
    );
    tokio::time::timeout(
        WAIT,
        executor.execute_stream(
            auth,
            request("test", payload),
            options(format, payload, true),
        ),
    )
    .await
    .expect("the call didn't start")
    .unwrap_or_else(|error| panic!("the call failed: {error:?}"))
}

/// Reads a bridged call's stream, calling `release` once the client has
/// seen the patch's first whole line (`applyPatchTestPreviewChunks`), or,
/// for a provider that only gives the call whole (`snapshot`), once it has
/// seen unrelated text with no patch progress made up before it. Fails if
/// that never comes while the provider holds the rest back.
async fn preview(
    response: StreamResponse,
    snapshot: bool,
    release: impl Fn(),
) -> (Lifecycle, String) {
    let mut stream = response.chunks;
    let mut lifecycle = Lifecycle::default();
    let mut output = String::new();
    let mut acknowledged = false;
    let read = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap_or_else(|error| panic!("{error:?}: {output}"));
            let text = String::from_utf8_lossy(&chunk);
            output.push_str(&text);
            for event in payloads(&text) {
                lifecycle.consume(&event);
                if acknowledged {
                    continue;
                }
                if snapshot {
                    assert!(
                        lifecycle.deltas.is_empty(),
                        "made up patch progress before the provider's snapshot: {output}"
                    );
                    acknowledged = event["type"] == "response.output_text.delta";
                } else {
                    acknowledged = lifecycle.deltas.contains("+hello\n");
                }
                if acknowledged {
                    assert!(
                        !lifecycle.done(),
                        "the preview came only after completion: {output}"
                    );
                    release();
                }
            }
        }
    };
    let read = tokio::time::timeout(WAIT, read).await;
    assert!(
        read.is_ok() && acknowledged,
        "no live preview before the provider's rest: {output}"
    );
    (lifecycle, output)
}

// TestApplyPatchBridgeLiveHTTPPreviewMatrix, at the executor: the provider
// holds back the rest of its answer until the client has seen the patch's
// first whole line (or, for Gemini's whole call, unrelated text before it),
// so a preview held until completion hangs. Upstream reads it through a gin
// gateway over a real socket, and checks that gateway's model list; the
// server's flush and its model list are the server's (the latter is in its
// model list tests). `custom-compat` is `openai-compatible-custom` here.
#[tokio::test]
async fn live_http_preview_matrix() {
    for (provider, protocol, snapshot) in [
        ("custom-compat", "chat", false),
        ("claude", "claude", false),
        ("claude-oauth", "claude", false),
        ("gemini-interactions", "interactions", false),
        ("xai", "responses", false),
        ("meta", "responses", false),
        ("gemini", "gemini", true),
        ("vertex", "gemini", true),
    ] {
        let upstream = Upstream::start(move |body| {
            let name = if protocol == "claude" {
                str_at(body, "tools.0.name")
            } else {
                "apply_patch".to_owned()
            };
            let (first, last) = frames(protocol, &name);
            (first, Some(last))
        })
        .await;
        let executor = executor(provider);
        let auth = auth(provider, executor.as_ref(), &upstream.url, false);
        let mut payload = json(PATCH_REQUEST);
        set(&mut payload, "stream", Value::Bool(true));
        let payload = payload.to_string();
        let response = tokio::time::timeout(
            WAIT,
            executor.execute_stream(
                auth,
                request("test", &payload),
                options(Format::OPENAI_RESPONSE, &payload, true),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{provider}: the call didn't start"))
        .unwrap_or_else(|error| panic!("{provider}: {error:?}"));
        let (lifecycle, output) = preview(response, snapshot, || upstream.release()).await;
        lifecycle.assert_whole(&format!("{provider}: {output}"));
        assert_declaration(protocol, &upstream.body());
    }
}

// TestApplyPatchBridgeInvalidArgumentsReturnsBadGateway
#[tokio::test]
async fn invalid_arguments_return_bad_gateway() {
    for args in INVALID_ARGS {
        let upstream = compat(&chat_reply(args)).await;
        match compat_execute(&upstream, Format::OPENAI_RESPONSE, PATCH_REQUEST).await {
            Ok(response) => panic!("{args}: expected the 502, got {response}"),
            Err(error) => assert_patch_error(&error),
        }
    }
}

// TestApplyPatchBridgeHistoryRoundTrip
#[tokio::test]
async fn history_round_trip() {
    let upstream = compat(&chat_reply(&format!("{PATCH_PARTIAL}{PATCH_REMAINDER}"))).await;
    let response = compat_execute(&upstream, Format::OPENAI_RESPONSE, PATCH_REQUEST)
        .await
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert_declaration("chat", &upstream.body());
    let call = response["output"]
        .as_array()
        .and_then(|output| {
            output
                .iter()
                .find(|item| item["type"] == "custom_tool_call")
        })
        .cloned()
        .unwrap_or_else(|| panic!("no custom call: {response}"));
    assert!(
        str_at(&call, "call_id") == "c1" && str_at(&call, "input") == PATCH_INPUT,
        "wrong client call: {response}"
    );

    let mut next = json(PATCH_REQUEST);
    set(
        &mut next,
        "input",
        json_value!([call, {"type": "custom_tool_call_output", "call_id": "c1", "output": "Success"}]),
    );
    compat_execute(&upstream, Format::OPENAI_RESPONSE, &next.to_string())
        .await
        .unwrap_or_else(|error| panic!("{error:?}"));
    let body = upstream.bodies().pop().expect("no second request");
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let by_role = |role: &str| {
        messages
            .iter()
            .find(|message| message["role"] == role)
            .cloned()
            .unwrap_or_else(|| panic!("no {role} message: {body}"))
    };
    let assistant = by_role("assistant");
    let arguments = str_at(&assistant, "tool_calls.0.function.arguments");
    let arguments: Value = serde_json::from_str(&arguments)
        .unwrap_or_else(|error| panic!("{error}: invalid replay arguments: {body}"));
    assert!(
        str_at(&arguments, "input") == str_at(&call, "input")
            && str_at(&assistant, "tool_calls.0.id") == "c1",
        "wrong replay arguments or identity: {body}"
    );
    let tool = by_role("tool");
    assert!(
        str_at(&tool, "tool_call_id") == "c1" && str_at(&tool, "content") == "Success",
        "wrong replay result identity: {body}"
    );
}

// TestApplyPatchBridgeTruncatedStreamDoesNotComplete
#[tokio::test]
async fn truncated_stream_does_not_complete() {
    let (first, _) = frames("chat", "apply_patch");
    let upstream = compat(&first).await;
    let response = compat_stream(&upstream, Format::OPENAI_RESPONSE, PATCH_REQUEST).await;
    let (chunks, errors) = collect(response).await;
    assert_failed_stream(&chunks, &errors);
}

/// The ordinary `apply_patch` function of a `format` client's request: a
/// JSON function, not the custom tool.
fn ordinary_request(format: &Format, model: bool) -> String {
    let parameters =
        r#"{"type":"object","properties":{"input":{"type":"string"},"extra":{"type":"integer"}}}"#;
    let model = if model { r#""model":"test","# } else { "" };
    if *format == Format::OPENAI_RESPONSE {
        format!(
            r#"{{{model}"input":"edit","tools":[{{"type":"function","name":"apply_patch","parameters":{parameters}}}]}}"#
        )
    } else {
        format!(
            r#"{{{model}"messages":[{{"role":"user","content":"edit"}}],"tools":[{{"type":"function","function":{{"name":"apply_patch","parameters":{parameters}}}}}]}}"#
        )
    }
}

/// Checks the provider was sent the ordinary function as it is.
#[track_caller]
fn assert_ordinary_declaration(body: &Value) {
    assert!(
        exists(body, "tools.0.function.parameters.properties.extra")
            && !str_at(body, "tools.0.function.description").contains("*** Begin Patch"),
        "ordinary function schema changed: {body}"
    );
}

// TestApplyPatchBridgeOrdinaryFunctionControl
#[tokio::test]
async fn ordinary_function_control() {
    for format in [Format::OPENAI, Format::OPENAI_RESPONSE] {
        let upstream = compat(&chat_reply(ORDINARY_ARGS)).await;
        let payload = ordinary_request(&format, true);
        let response = compat_execute(&upstream, format.clone(), &payload)
            .await
            .unwrap_or_else(|error| panic!("{format:?}: {error:?}"));
        assert_ordinary_declaration(&upstream.body());
        let arguments = if format == Format::OPENAI_RESPONSE {
            response["output"]
                .as_array()
                .and_then(|output| output.iter().find(|item| item["type"] == "function_call"))
                .map(|item| str_at(item, "arguments"))
                .unwrap_or_default()
        } else {
            str_at(
                &response,
                "choices.0.message.tool_calls.0.function.arguments",
            )
        };
        assert!(
            arguments == ORDINARY_ARGS
                && serde_json::from_str::<Value>(&arguments).is_ok()
                && !response.to_string().contains(r#""custom_tool_call""#),
            "{format:?}: ordinary JSON function promoted to freeform: {response}"
        );
    }
}

/// The JSON text of each `data:` line of `stream`, as the provider wrote it.
fn raw_payloads(stream: &str) -> Vec<String> {
    stream
        .split('\n')
        .filter_map(|line| line.trim().strip_prefix("data:"))
        .map(|data| data.trim().to_owned())
        .filter(|data| serde_json::from_str::<Value>(data).is_ok())
        .collect()
}

// TestApplyPatchBridgeLiveWebsocketPreview: a client on the Responses
// WebSocket (the context upstream marks with WithDownstreamWebsocket is
// `downstream_websocket` here) previews the patch line by line, through
// Codex's native custom tool and through xAI's bridged function.
#[tokio::test]
async fn live_websocket_preview() {
    for native in [false, true] {
        let (mut first, mut last) = frames("responses", "apply_patch");
        if native {
            first = "data: { \"type\":\"response.output_item.added\", \"output_index\":0,\"item\":{\"type\":\"custom_tool_call\",\"id\":\"a1\",\"call_id\":\"c1\",\"name\":\"apply_patch\",\"input\":\"\"}}\n\n\
                     data: { \"type\":\"response.custom_tool_call_input.delta\", \"output_index\":0,\"item_id\":\"a1\",\"call_id\":\"c1\",\"delta\":\"*** Begin Patch\\n*** Add File: a.txt\\n+hello\\n\"}\n\n".to_owned();
            let item = format!(
                r#"{{ "type":"custom_tool_call", "id":"a1","call_id":"c1","name":"apply_patch","input":{}}}"#,
                q(PATCH_INPUT)
            );
            last = format!(
                "data: {{ \"type\":\"response.custom_tool_call_input.delta\", \"output_index\":0,\"item_id\":\"a1\",\"call_id\":\"c1\",\"delta\":\"*** End Patch\\n\"}}\n\n\
                 data: {{ \"type\":\"response.custom_tool_call_input.done\", \"item_id\":\"a1\",\"call_id\":\"c1\",\"input\":{}}}\n\n\
                 data: {{ \"type\":\"response.output_item.done\", \"output_index\":0,\"item\":{item}}}\n\n\
                 data: {{ \"type\":\"response.completed\", \"response\":{{\"id\":\"r1\",\"status\":\"completed\",\"output\":[{item}]}}}}\n\n",
                q(PATCH_INPUT)
            );
        }
        let (release, gate) = watch::channel(false);
        let (head, tail) = (raw_payloads(&first), raw_payloads(&last));
        let server = Server::start(move |_| {
            let (head, tail, gate) = (head.clone(), tail.clone(), gate.clone());
            Answer::accept(move |mut peer| {
                let (head, tail, mut gate) = (head.clone(), tail.clone(), gate.clone());
                async move {
                    if peer.recv().await.is_some() {
                        peer.send_all(&head).await;
                        if gate.wait_for(|open| *open).await.is_ok() {
                            peer.send_all(&tail).await;
                        }
                    }
                }
            })
        })
        .await;
        let (provider, model) = if native {
            ("codex", "gpt-5.6-sol")
        } else {
            ("xai", "grok-4")
        };
        let executor = executor(provider);
        let auth = auth(provider, executor.as_ref(), &server.url, true);
        let mut options = options(Format::OPENAI_RESPONSE, PATCH_REQUEST, true);
        options.downstream_websocket = true;
        options
            .headers
            .insert(RESPONSES_LITE_HEADER, HeaderValue::from_static("true"));
        let response = tokio::time::timeout(
            WAIT,
            executor.execute_stream(auth, request(model, PATCH_REQUEST), options),
        )
        .await
        .unwrap_or_else(|_| panic!("{provider}: the call didn't start"))
        .unwrap_or_else(|error| panic!("{provider}: {error:?}"));
        let (lifecycle, output) = preview(response, false, || {
            release.send_replace(true);
        })
        .await;
        lifecycle.assert_whole(&format!("{provider}: {output}"));
        let record = server.record();
        let body = json(record.messages.first().expect("no message sent"));
        if native {
            assert_eq!(
                get(&body, "tools.0"),
                get(&json(PATCH_REQUEST), "tools.0"),
                "native grammar declaration altered: {body}"
            );
            for raw in raw_payloads(&format!("{first}{last}")) {
                assert!(
                    output.contains(&raw),
                    "native event bytes altered: {raw}\n{output}"
                );
            }
        } else {
            assert_declaration("responses", &body);
        }
    }
}

// TestApplyPatchBridgeOrdinaryFunctionStreamControl
#[tokio::test]
async fn ordinary_function_stream_control() {
    for format in [Format::OPENAI, Format::OPENAI_RESPONSE] {
        let upstream = compat(&compat_fixture("stream", ORDINARY_ARGS)).await;
        let payload = ordinary_request(&format, false);
        let response = compat_stream(&upstream, format.clone(), &payload).await;
        let (chunks, errors) = collect(response).await;
        assert!(errors.is_empty(), "{format:?}: {errors:?}");
        let mut arguments = String::new();
        for chunk in &chunks {
            assert!(
                !chunk.contains("custom_tool_call") && !chunk.contains("response.failed"),
                "{format:?}: ordinary stream was promoted or restricted: {chunk}"
            );
            for event in payloads(chunk) {
                if format == Format::OPENAI {
                    arguments.push_str(&str_at(
                        &event,
                        "choices.0.delta.tool_calls.0.function.arguments",
                    ));
                } else if event["type"] == "response.function_call_arguments.delta" {
                    arguments.push_str(&str_at(&event, "delta"));
                }
            }
        }
        assert!(
            arguments == ORDINARY_ARGS && serde_json::from_str::<Value>(&arguments).is_ok(),
            "{format:?}: ordinary stream lost its JSON arguments: {arguments:?}"
        );
        assert!(
            exists(
                &upstream.body(),
                "tools.0.function.parameters.properties.extra"
            ),
            "{format:?}: ordinary stream lost its schema"
        );
    }
}

// TestApplyPatchBridgeInvalidStreamArguments: valid decoded prefixes can
// precede the failure; the provider's own fields mustn't leak.
#[tokio::test]
async fn invalid_stream_arguments() {
    for args in INVALID_STREAM_ARGS {
        let upstream = compat(&compat_fixture("stream", args)).await;
        let response = compat_stream(&upstream, Format::OPENAI_RESPONSE, PATCH_REQUEST).await;
        let (chunks, errors) = collect(response).await;
        assert_failed_stream(&chunks, &errors);
    }
}
