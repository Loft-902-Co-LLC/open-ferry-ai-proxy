// Ported from CLIProxyAPI internal/runtime/executor/xai_websockets_executor_test.go,
// the WebSocket mode of xai_executor_test.go's
// TestXAIApplyPatchDispatcherEvidenceLifecycle and payload_barrier_test.go's
// TestPayloadBarrierXAIWebsocketRetry (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Responses WebSocket upstream against a mock xAI on 127.0.0.1 (Codex's
//! mock; see `crate::codex::websocket::mock`), ported from upstream's tests
//! where they test what is ported. The calls and errors are in this file,
//! tools and the `apply_patch` bridge in [`tools`], the session's IDs and
//! transcript in [`ids`], compaction in [`compaction`], and keeping the
//! credential's secret out of what the client is given in [`secrets`].
//!
//! Upstream's tests sign in with an OAuth access token; these use a dummy
//! API key, the only credential served. Upstream's WebSocket executor takes
//! any client; here [`XaiExecutor`] only takes the WebSocket for a client on
//! the Responses WebSocket, so every call here is one (with `stream` on;
//! upstream's tests that leave it off stream all the same).
//!
//! Dropped, with why:
//! - `TestXAIAutoExecutorRequiredUpstreamWebsocketRejectsHTTPFallback`,
//!   `TestXAIWebsocketsRequiredUpstreamRejectsCompactionHTTPFallback` and
//!   `TestXAIWebsocketMissingRequiredSessionDoesNotMarkUpstreamAttempt`:
//!   `RequiredUpstreamWebsocket` and upstream attempt markers aren't ported
//!   (as for Codex).
//! - `TestXAIWebsocketSuccessfulHandshakeDoesNotMarkRequestAttempt`: the
//!   attempt markers aren't ported; `the_request_is_told_sent_once_the_connection_is_up`
//!   checks the same moment through the call's taps.
//! - `TestXAIWebsockets_PingHandlerDoesNotBlockOnWriteMu`,
//!   `TestXAIWebsockets_KeepalivePingDuringUpload_WithSession` and
//!   `TestXAIWebsockets_KeepalivePingDuringUpload_Sessionless`: the WebSocket library
//!   answers pings as the reader reads, and a message is sent in one write;
//!   `answers_pings_during_a_call` checks a ping is answered, in a session
//!   and outside one.
//! - The cases of `TestXAIWebsocketApplyPatchTransport` whose client isn't
//!   on the Responses WebSocket, and the `ws_sse` mode of
//!   `TestXAIApplyPatchDispatcherEvidenceLifecycle`: such a client goes over
//!   HTTP here, where the executor's own tests cover the bridge. (The
//!   evidence test's `http` and `http_stream` modes belong to the HTTP
//!   executor's tests.)
//! - Upstream's assertions on `prompt_cache_key` and `x-grok-conv-id` being
//!   the execution session's ID: no session is made up (see
//!   [`crate::xai::request`]); `sends_response_create_with_previous_response_id`
//!   asserts both are absent unless the client sent a `prompt_cache_key`.
//!
//! Upstream's `TestSchedulerPick_XAIWebsocketPrefersWebsocketEnabledSubset`
//! is ported in `open-ferry-core`'s `manager/tests/scheduler.rs`; the xAI subtests of
//! `websocket_session_target_test.go` test the session store xAI shares with
//! Codex, tested in `crate::codex::websocket`, and
//! `ids::replays_transcript_when_auth_changes` covers xAI's own
//! use of a changed target.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::Method;
use open_ferry_core::exec::{ExecError, Format, Request, StreamResponse, TransportFault};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::observe::{AttemptRequest, Observation, RequestContext, Tap};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::*;
use crate::codex::client::USER_AGENT;
use crate::codex::websocket::errors::{Failure, should_retry};
use crate::codex::websocket::mock::{Answer, Server};
use crate::codex::websocket::session::{Conn, Target as ConnTarget};
use crate::json::{exists, str_at};
use crate::redact::Secrets;
use crate::xai::XaiExecutor;

mod compaction;
mod ids;
mod secrets;
mod tools;

/// How long a test waits for a call.
const WAIT: Duration = Duration::from_secs(10);

/// The dummy API key the tests send.
const API_KEY: &str = "xai-test-key";

const HELLO: &str =
    r#"{"model":"grok-4.3","input":[{"type":"message","role":"user","content":"hello"}]}"#;

const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp-xai-1","output":[],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}"#;

/// An executor that doesn't use the environment's proxy.
pub(crate) fn executor() -> XaiExecutor {
    XaiExecutor::new("direct")
}

/// An API key credential for `base_url`, with websockets on.
fn auth(base_url: &str) -> Arc<Auth> {
    auth_as("xai-auth", API_KEY, base_url)
}

/// [`auth`] with the ID `id` and the key `key`.
pub(crate) fn auth_as(id: &str, key: &str, base_url: &str) -> Arc<Auth> {
    let mut auth = Auth {
        id: id.into(),
        provider: "xai".into(),
        ..Auth::default()
    };
    for (name, value) in [
        ("base_url", base_url),
        ("api_key", key),
        ("websockets", "true"),
    ] {
        auth.attributes.insert(name.into(), value.into());
    }
    Arc::new(auth)
}

fn request(payload: &str) -> Request {
    let model = str_at(&json(payload), "model");
    Request {
        model: if model.is_empty() {
            "grok-4".into()
        } else {
            model
        },
        payload: Bytes::from(payload.to_owned()),
    }
}

/// Options of a client on the Responses WebSocket in the session `session`
/// (none when it is empty).
pub(crate) fn ws_options(session: &str) -> Options {
    let mut options = Options {
        stream: true,
        downstream_websocket: true,
        ..Options::new(Format::from("openai-response".to_owned()))
    };
    if !session.is_empty() {
        options.metadata.execution_session_id = Some(session.into());
    }
    options
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("{error}: {text}"))
}

/// Waits for `future`, failing the test after a while.
pub(crate) async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Reads a stream to its end: its chunks and its error.
pub(crate) async fn collect(response: StreamResponse) -> (Vec<String>, Option<ExecError>) {
    let mut chunks = response.chunks;
    let mut out = Vec::new();
    let mut error = None;
    within("the stream to end", async {
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(chunk) => out.push(String::from_utf8_lossy(&chunk).into_owned()),
                Err(failure) => {
                    assert!(error.is_none(), "a second error: {failure:?}");
                    error = Some(failure);
                }
            }
        }
    })
    .await;
    (out, error)
}

/// Makes a call of `executor` and reads it to its end.
async fn call(
    executor: &XaiExecutor,
    auth: &Arc<Auth>,
    payload: &str,
    options: Options,
) -> (Vec<String>, Option<ExecError>) {
    let response = within(
        "the call to start",
        executor.execute_stream(Arc::clone(auth), request(payload), options),
    )
    .await
    .unwrap_or_else(|error| panic!("the call didn't start: {error:?}"));
    collect(response).await
}

/// [`call`], for a call that must end without an error.
async fn streamed(
    executor: &XaiExecutor,
    auth: &Arc<Auth>,
    payload: &str,
    options: Options,
) -> Vec<String> {
    let (chunks, error) = call(executor, auth, payload, options).await;
    assert!(error.is_none(), "{error:?}\n{chunks:?}");
    chunks
}

/// The error of a call that didn't start.
async fn refused(
    executor: &XaiExecutor,
    auth: &Arc<Auth>,
    payload: &str,
    options: Options,
) -> ExecError {
    match within(
        "the call to fail",
        executor.execute_stream(Arc::clone(auth), request(payload), options),
    )
    .await
    {
        Ok(_) => panic!("the call went through"),
        Err(error) => error,
    }
}

/// Whether the session `id` names holds a connection (upstream's
/// `getOrCreateSession(id).conn != nil`).
pub(crate) fn has_conn(executor: &XaiExecutor, id: &str) -> bool {
    executor
        .websockets
        .store
        .get_or_create(id)
        .and_then(|session| session.conn())
        .is_some()
}

/// The JSON of each chunk.
fn events(chunks: &[String]) -> Vec<Value> {
    chunks.iter().map(|chunk| json(chunk)).collect()
}

/// The last event of `event_type`.
fn last_event(chunks: &[String], event_type: &str) -> Value {
    events(chunks)
        .into_iter()
        .rfind(|event| event["type"] == event_type)
        .unwrap_or_else(|| panic!("no {event_type} in {chunks:?}"))
}

/// The `n`th message xAI read, as JSON.
fn message(server: &Server, n: usize) -> Value {
    let record = server.record();
    json(
        record
            .messages
            .get(n)
            .unwrap_or_else(|| panic!("no message {n}: {record:?}")),
    )
}

/// A server whose every connection reads one message, sends `frames` and
/// holds the connection until the client ends it (upstream's servers that
/// wait for `releaseServer`).
async fn holding(frames: &[&str]) -> Server {
    let frames: Vec<String> = frames.iter().map(|frame| (*frame).to_owned()).collect();
    Server::start(move |_| {
        let frames = frames.clone();
        Answer::accept(move |mut peer| {
            let frames = frames.clone();
            async move {
                if peer.recv().await.is_some() {
                    peer.send_all(&frames).await;
                    peer.hold().await;
                }
            }
        })
    })
    .await
}

// TestXAIWebsocketsEnabledForConfigAPIKey, with the route it picks: the
// WebSocket only for a client on the Responses WebSocket with websockets
// on.
#[test]
fn websockets_enabled_for_config_api_key() {
    let mut auth = Auth {
        provider: "xai".into(),
        ..Auth::default()
    };
    auth.attributes.insert("api_key".into(), "xai-key".into());
    auth.attributes.insert("websockets".into(), "true".into());
    assert!(routes(&auth, &ws_options("")));
    let http_client = Options {
        stream: true,
        ..Options::new(Format::from("openai-response".to_owned()))
    };
    assert!(!routes(&auth, &http_client));
    auth.attributes.remove("websockets");
    assert!(!routes(&auth, &ws_options("")));
}

// Not upstream's (`buildXAIResponsesWebsocketURL` has no test of its own):
// the URL is the credential's base URL with a WebSocket scheme, and Grok's
// CLI chat proxy is never dialled.
#[test]
fn websocket_url_takes_a_websocket_scheme() {
    let mut auth = Auth::default();
    for (base, want) in [
        ("http://127.0.0.1:9/v1/", "ws://127.0.0.1:9/v1/responses"),
        (
            "https://api.example.test/v1",
            "wss://api.example.test/v1/responses",
        ),
        ("wss://api.example.test", "wss://api.example.test/responses"),
        (
            "https://cli-chat-proxy.grok.com/v1",
            "wss://api.x.ai/v1/responses",
        ),
    ] {
        auth.attributes.insert("base_url".into(), base.into());
        assert_eq!(websocket_url(&auth).unwrap(), want, "{base}");
    }
    auth.attributes.remove("base_url");
    assert_eq!(websocket_url(&auth).unwrap(), "wss://api.x.ai/v1/responses");
    // As upstream, a bare `http://` takes `responses` for its host.
    auth.attributes.insert("base_url".into(), "http://".into());
    assert_eq!(websocket_url(&auth).unwrap(), "ws://responses");
    for base in ["ftp://api.example.test", "http:///v1"] {
        auth.attributes.insert("base_url".into(), base.into());
        assert!(websocket_url(&auth).is_err(), "{base}");
    }
}

// TestMapXAIWebsocketWriteErrorStopsRetryForMessageTooBig. Upstream's
// `ErrCloseSent` is a send on a closed connection here.
#[test]
fn write_error_after_message_too_big_stops_the_retry() {
    let target = ConnTarget::new("auth", "ws://example.test/responses", "", "token");
    let broken_pipe = Failure::Other {
        message: "write: broken pipe".into(),
        transient: true,
    };
    for (name, code, failure, too_big) in [
        (
            "close sent after message too big",
            1009,
            Failure::closed(),
            true,
        ),
        (
            "network write error after message too big",
            1009,
            broken_pipe,
            true,
        ),
        ("other close", 1000, Failure::closed(), false),
    ] {
        let conn = Conn::detached(target.clone());
        conn.set_disconnect(code);
        let error = super::errors::write_error(conn.disconnect_code(), &failure);
        assert_eq!(should_retry(&error), !too_big, "{name}");
        if too_big {
            assert_eq!(error.status, 413, "{name}");
            assert!(error.request_scoped, "{name}");
        } else {
            assert_eq!(
                error.message, "xai websockets executor: use of closed network connection",
                "{name}"
            );
        }
    }
}

// TestXAIWebsocketsExecuteStreamMapsMessageTooBigClose
#[tokio::test]
async fn stream_maps_message_too_big_close() {
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.close(1009, "message too big").await;
                peer.hold().await;
            }
        })
    })
    .await;
    let (chunks, error) = call(&executor(), &auth(&server.url), HELLO, ws_options("")).await;
    assert!(chunks.is_empty(), "{chunks:?}");
    let error = error.expect("no error");
    assert_eq!(error.status, 413);
    assert_eq!(
        str_at(&json(&error.message), "error.code"),
        "message_too_big"
    );
    assert!(error.request_scoped);
}

// TestXAIWebsocketsExecuteStreamSendsResponseCreateWithPreviousResponseID.
// Upstream sends the execution session's ID as `x-grok-conv-id` and
// `prompt_cache_key`; here neither is made up, so both are absent unless
// the client sent a `prompt_cache_key`, which then is both.
#[tokio::test]
async fn sends_response_create_with_previous_response_id() {
    for cache_key in [None, Some("cache-1")] {
        let server = Server::once(&[COMPLETED]).await;
        let executor = executor();
        let mut payload = json(
            r#"{"model":"grok-4.3","stream":true,"previous_response_id":"resp-prev","instructions":"system prompt","input":[{"type":"message","role":"user","content":"hello"}]}"#,
        );
        if let Some(key) = cache_key {
            payload["prompt_cache_key"] = json!(key);
        }
        let chunks = streamed(
            &executor,
            &auth(&server.url),
            &payload.to_string(),
            ws_options("execution-session-1"),
        )
        .await;
        assert_eq!(str_at(&json(&chunks[0]), "type"), "response.completed");

        let record = server.record();
        let handshake = &record.handshakes[0];
        assert_eq!(handshake.method, "GET");
        assert_eq!(handshake.path, "/responses");
        assert_eq!(
            handshake.header("authorization"),
            Some(format!("Bearer {API_KEY}").as_str())
        );
        assert_eq!(handshake.header("user-agent"), Some(USER_AGENT));
        assert_eq!(
            handshake.header("x-grok-conv-id"),
            cache_key,
            "{cache_key:?}"
        );
        assert!(
            !handshake
                .headers
                .keys()
                .any(|name| name.as_str().starts_with("x-grok-client")
                    || name.as_str() == "x-client-id"),
            "{handshake:?}"
        );

        let sent = message(&server, 0);
        assert_eq!(str_at(&sent, "type"), "response.create", "{sent}");
        assert_eq!(str_at(&sent, "previous_response_id"), "resp-prev", "{sent}");
        assert!(!exists(&sent, "stream"), "{sent}");
        assert!(!exists(&sent, "instructions"), "{sent}");
        assert_eq!(sent["store"], json!(true), "{sent}");
        match cache_key {
            Some(key) => assert_eq!(str_at(&sent, "prompt_cache_key"), key, "{sent}"),
            None => assert!(!exists(&sent, "prompt_cache_key"), "{sent}"),
        }
        executor.close_execution_session("execution-session-1");
    }
}

// TestBuildXAIWebsocketRequestBodySetsStoreAndKeepsPromptCacheKey
#[test]
fn request_message_sets_store_and_keeps_prompt_cache_key() {
    let message = super::message::request_message(
        &json(
            r#"{"model":"grok-4.3","stream":true,"stream_options":{"include_usage":true},"background":true,"prompt_cache_key":"cache-1","previous_response_id":"resp-prev","instructions":"system prompt","input":[{"type":"message","role":"user","content":"hello"}]}"#,
        ),
        |_| {},
    );
    assert_eq!(str_at(&message, "type"), "response.create");
    for field in ["stream", "stream_options", "background", "instructions"] {
        assert!(!exists(&message, field), "{field}: {message}");
    }
    assert_eq!(str_at(&message, "prompt_cache_key"), "cache-1");
    assert_eq!(message["store"], json!(true));
}

// TestPayloadBarrierXAIWebsocketRetry: the config's payload rules apply to
// the message as it is framed, the `type` stays, and a retry sends the same
// message. Changed: the body is prepared as a call prepares it.
#[test]
fn payload_rules_apply_to_the_message_once() {
    let config = open_ferry_core::config::Config::parse(
        "payload:\n  override:\n    - models:\n        - name: \"*\"\n      params:\n        store: false\n        instructions: configured\n  filter:\n    - models:\n        - name: \"*\"\n      params:\n        - input.0\n        - previous_response_id\n",
    )
    .expect("config parses");
    let request = Request {
        model: "grok-4.3".into(),
        payload: Bytes::from_static(
            br#"{"previous_response_id":"previous","input":[{"content":"first"},{"content":"second"}]}"#,
        ),
    };
    let options = open_ferry_core::exec::Options::new(Format::OPENAI_RESPONSE);
    let context = crate::codex::request::Context {
        auth: None,
        config: Some(&config),
        models: None,
    };
    let prepared = crate::xai::request::prepare(context, &request, &options, true, Format::CODEX)
        .expect("prepares");
    let finalize = |message: &mut Value| {
        prepared
            .finalizer
            .apply(Some(&config), &request, &options, message);
    };
    let first = super::message::request_message(&prepared.body, finalize);
    let retry = super::message::request_message(&prepared.body, finalize);
    assert_eq!(first, retry);
    assert_eq!(first["store"], json!(false), "{first}");
    assert_eq!(str_at(&first, "instructions"), "configured", "{first}");
    assert!(!exists(&first, "previous_response_id"), "{first}");
    assert_eq!(first["input"].as_array().map(Vec::len), Some(1), "{first}");
    assert_eq!(str_at(&first, "type"), "response.create", "{first}");
}

// TestXAIWebsocketsExecuteStreamCompletesGenerateFalseWarmup
#[tokio::test]
async fn completes_generate_false_warmup() {
    let server = holding(&[
        r#"{"type":"response.created","response":{"id":"resp-warmup-1","object":"response","status":"in_progress","output":[]}}"#,
    ])
    .await;
    let chunks = streamed(
        &executor(),
        &auth(&server.url),
        r#"{"model":"grok-4.3","generate":false,"input":[{"type":"message","role":"user","content":"warm up"}]}"#,
        ws_options(""),
    )
    .await;
    let sent = message(&server, 0);
    assert_eq!(sent["generate"], json!(false), "{sent}");
    assert_eq!(str_at(&sent, "type"), "response.create", "{sent}");
    assert_eq!(sent["store"], json!(true), "{sent}");
    let types: Vec<String> = events(&chunks)
        .iter()
        .map(|event| str_at(event, "type"))
        .collect();
    assert_eq!(types, ["response.created", "response.completed"]);
    let completed = json(&chunks[1]);
    assert_eq!(str_at(&completed, "response.id"), "resp-warmup-1");
    assert_eq!(str_at(&completed, "response.status"), "completed");
    // The call's own connection goes once the warmup ends.
    server.wait_closed(1).await;
}

// TestXAIWebsocketsExecuteStreamHandshakeFreeUsageExhaustedSetsRetryAfter.
// Upstream's attempt marker isn't ported.
#[tokio::test]
async fn handshake_free_usage_exhausted_sets_retry_after() {
    let body = r#"{"code":"subscription:free-usage-exhausted","error":"You've used all the included free usage for now."}"#;
    let server = Server::start(move |_| Answer::Refuse {
        status: 429,
        headers: vec![("Content-Type", "application/json".into())],
        body: body.to_owned(),
    })
    .await;
    let error = refused(
        &executor(),
        &auth(&server.url),
        r#"{"model":"grok-4.3","input":"hello"}"#,
        ws_options(""),
    )
    .await;
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::from_secs(24 * 60 * 60)));
    assert_eq!(error.message, body);
}

/// The error `parse_error` reads from `payload`.
fn parse_error(payload: &str) -> ExecError {
    super::errors::parse_error(
        &json(payload),
        payload.as_bytes(),
        &Secrets::new(),
        SystemTime::now(),
    )
    .unwrap_or_else(|| panic!("not an error: {payload}"))
}

// TestParseXAIWebsocketErrorFreeUsageExhaustedSetsRetryAfter
#[test]
fn parse_error_free_usage_exhausted_sets_retry_after() {
    let error = parse_error(
        r#"{"type":"error","status":429,"error":{"code":"subscription:free-usage-exhausted","message":"You've used all the included free usage for now."}}"#,
    );
    assert_eq!(error.retry_after, Some(Duration::from_secs(24 * 60 * 60)));
    let body = json(&error.message);
    assert_eq!(body["status"], json!(429), "{body}");
    assert_eq!(
        str_at(&body, "error.code"),
        "subscription:free-usage-exhausted"
    );
}

// TestParseXAIWebsocketErrorBadCredentialsRemapsToUnauthorized
#[test]
fn parse_error_bad_credentials_remaps_to_unauthorized() {
    let error = parse_error(
        r#"{"type":"error","status":403,"headers":{"x-request-id":"req-bad-credentials"},"error":{"code":"unauthenticated:bad-credentials","message":"The OAuth2 access token could not be validated."}}"#,
    );
    assert_eq!(error.status, 401);
    assert_eq!(
        error
            .headers
            .get("x-request-id")
            .map(|value| value.to_str().unwrap()),
        Some("req-bad-credentials")
    );
    assert_eq!(
        str_at(&json(&error.message), "error.code"),
        "unauthenticated:bad-credentials"
    );
}

// TestParseXAIWebsocketBareErrorBadCredentialsRemapsToUnauthorized
#[test]
fn parse_bare_error_bad_credentials_remaps_to_unauthorized() {
    let error = parse_error(
        r#"{"status":403,"error":{"code":"unauthenticated:bad-credentials","message":"The OAuth2 access token could not be validated."}}"#,
    );
    assert_eq!(error.status, 401);
}

// TestParseXAIWebsocketBareErrorFreeUsageExhaustedSetsRetryAfter
#[test]
fn parse_bare_error_free_usage_exhausted_sets_retry_after() {
    let error = parse_error(
        r#"{"status":429,"error":{"code":"subscription:free-usage-exhausted","message":"You've used all the included free usage for now."}}"#,
    );
    assert_eq!(error.retry_after, Some(Duration::from_secs(24 * 60 * 60)));
    let body = json(&error.message);
    assert_eq!(str_at(&body, "type"), "error", "{body}");
    assert_eq!(
        str_at(&body, "error.code"),
        "subscription:free-usage-exhausted"
    );
}

// Not upstream's: a bare error's status is its own, else a positive code,
// else 400 for a request validation error, else 500; an event without an
// `error` isn't one.
#[test]
fn parse_bare_error_status() {
    for (payload, status) in [
        (r#"{"status_code":409,"error":{"message":"conflict"}}"#, 409),
        (r#"{"error":{"code":"503","message":"busy"}}"#, 503),
        (r#"{"error":{"status":"404","message":"gone"}}"#, 404),
        (r#"{"code":"429","error":"slow down"}"#, 429),
        (
            r#"{"error":{"message":"Request validation error: bad"}}"#,
            400,
        ),
        (r#"{"error":{"message":"boom"}}"#, 500),
    ] {
        assert_eq!(parse_error(payload).status, status, "{payload}");
    }
    let event = json(r#"{"type":"response.created","response":{"id":"r"}}"#);
    assert!(
        super::errors::parse_error(&event, b"{}", &Secrets::new(), SystemTime::now()).is_none()
    );
}

// TestXAIWebsocketsExecuteStreamStopsOnBareErrorPayload
#[tokio::test]
async fn stops_on_bare_error_payload() {
    let server = holding(&[
        r#"{"error":{"message":"Request validation error: {\"code\":\"400\",\"error\":\"Argument not supported: instructions and previous_response_id together\"}","type":"api_error"}}"#,
    ])
    .await;
    let (chunks, error) = call(
        &executor(),
        &auth(&server.url),
        r#"{"model":"grok-4.3","input":"hello"}"#,
        ws_options(""),
    )
    .await;
    assert!(chunks.is_empty(), "{chunks:?}");
    let error = error.expect("no error");
    assert_eq!(error.status, 400);
    assert_eq!(str_at(&json(&error.message), "error.type"), "api_error");
    server.wait_closed(1).await;
}

// Not upstream's: an error event with a status ends the stream with xAI's
// error and lets the session's connection go.
#[tokio::test]
async fn stops_on_error_event() {
    let server = holding(&[
        r#"{"type":"error","status":403,"error":{"code":"unauthenticated:bad-credentials","message":"The API key could not be validated."}}"#,
    ])
    .await;
    let executor = executor();
    let (chunks, error) = call(&executor, &auth(&server.url), HELLO, ws_options("erring")).await;
    assert!(chunks.is_empty(), "{chunks:?}");
    assert_eq!(error.expect("no error").status, 401);
    server.wait_closed(1).await;
    executor.close_execution_session("erring");
}

// Not upstream's, for `TestXAIWebsockets_PingHandlerDoesNotBlockOnWriteMu`,
// `TestXAIWebsockets_KeepalivePingDuringUpload_WithSession` and
// `TestXAIWebsockets_KeepalivePingDuringUpload_Sessionless`: a ping from
// xAI is answered during a call, in a session and outside one.
#[tokio::test]
async fn answers_pings_during_a_call() {
    for session in ["xai-session-ping-test", ""] {
        let server = Server::start(|_| {
            Answer::accept(|mut peer| async move {
                if peer.recv().await.is_some() {
                    let delta = if peer.ping_pong().await {
                        r#"{"type":"response.output_text.delta","delta":"pong"}"#
                    } else {
                        r#"{"type":"response.output_text.delta","delta":"no pong"}"#
                    };
                    peer.send(delta).await;
                    peer.send(
                        r#"{"type":"response.done","response":{"id":"resp-1","status":"completed","output":[]}}"#,
                    )
                    .await;
                }
            })
        })
        .await;
        let executor = executor();
        let chunks = streamed(&executor, &auth(&server.url), HELLO, ws_options(session)).await;
        assert_eq!(
            str_at(&json(&chunks[0]), "delta"),
            "pong",
            "the ping wasn't answered ({session:?})"
        );
        assert_eq!(str_at(&json(&chunks[1]), "type"), "response.done");
        executor.close_execution_session(session);
    }
}

// Not upstream's: the calls of a session share its connection, and closing
// the session closes it; closing all sessions closes every connection.
#[tokio::test]
async fn sessions_share_a_connection_until_closed() {
    let server = Server::turns(&[COMPLETED]).await;
    let executor = executor();
    let auth = auth(&server.url);
    for _ in 0..2 {
        streamed(&executor, &auth, HELLO, ws_options("shared")).await;
    }
    assert_eq!(server.record().handshakes.len(), 1);
    assert_eq!(server.record().messages.len(), 2);
    executor.close_execution_session("shared");
    server.wait_closed(1).await;

    for session in ["first", "second"] {
        streamed(&executor, &auth, HELLO, ws_options(session)).await;
    }
    assert_eq!(server.record().handshakes.len(), 3);
    executor.close_execution_session(CLOSE_ALL_EXECUTION_SESSIONS);
    server.wait_closed(3).await;
}

// Not upstream's: a connection idle past the timeout is closed, and the
// call gets a network error naming xAI's executor.
#[tokio::test]
async fn idle_connection_times_out() {
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.hold().await;
            }
        })
    })
    .await;
    let mut executor = executor();
    executor.websockets = Sessions::with_idle(Duration::from_millis(200));
    let (_, error) = call(&executor, &auth(&server.url), HELLO, ws_options("idle")).await;
    let error = error.expect("no error");
    assert!(
        error.message.starts_with("xai websockets executor: ")
            && error.message.contains("i/o timeout"),
        "{error:?}"
    );
    assert_eq!(error.transport, Some(TransportFault::Transient));
    server.wait_closed(1).await;
}

// Not upstream's: a send that fails on the session's connection is tried
// once more on a new one (Codex's
// `send_on_a_stale_connection_is_tried_once_more`; a connection without a
// socket stands in for a stale one).
#[tokio::test]
async fn send_on_a_stale_connection_is_tried_once_more() {
    let server = Server::once(&[COMPLETED]).await;
    let executor = executor();
    let auth = auth(&server.url);
    let session = executor.websockets.store.get_or_create("stale").unwrap();
    let stale = Conn::detached(ConnTarget::new(
        &auth.id,
        &websocket_url(&auth).unwrap(),
        &executor.proxy_for(&auth),
        API_KEY,
    ));
    session.set_conn(Arc::clone(&stale));
    let chunks = streamed(&executor, &auth, HELLO, ws_options("stale")).await;
    assert_eq!(str_at(&json(&chunks[0]), "type"), "response.completed");
    assert!(stale.is_closed());
    assert_eq!(server.record().handshakes.len(), 1);
    executor.close_execution_session("stale");
}

// Not upstream's: a refused handshake is the call's error, with xAI's
// status and body; a 403 for credentials xAI no longer takes is a 401.
#[tokio::test]
async fn refused_handshake_is_xais_error() {
    let body = r#"{"code":"unauthenticated:bad-credentials","error":"The API key could not be validated."}"#;
    let server = Server::refusing(403, body).await;
    let error = refused(&executor(), &auth(&server.url), HELLO, ws_options("")).await;
    assert_eq!(error.status, 401);
    assert_eq!(error.message, body);
}

// Not upstream's: `/responses/compact` can't stream.
#[tokio::test]
async fn compact_alt_is_refused() {
    let options = Options {
        alt: "responses/compact".into(),
        ..ws_options("")
    };
    let error = refused(&executor(), &auth("http://127.0.0.1:9"), HELLO, options).await;
    assert_eq!(error.status, 400);
    assert!(
        error
            .message
            .contains("streaming not supported for /responses/compact"),
        "{error:?}"
    );
}

/// A tap that writes down each step it is told, with how many handshakes
/// and messages the server had seen at that moment, and the messages it
/// was given.
struct Steps {
    server: Arc<Server>,
    steps: Mutex<Vec<String>>,
    chunks: Mutex<Vec<String>>,
}

impl Steps {
    fn new(server: &Arc<Server>) -> Arc<Self> {
        Arc::new(Self {
            server: Arc::clone(server),
            steps: Mutex::new(Vec::new()),
            chunks: Mutex::new(Vec::new()),
        })
    }

    fn note(&self, step: &str) {
        let seen = self.server.record();
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(format!(
                "{step} handshakes={} messages={}",
                seen.handshakes.len(),
                seen.messages.len()
            ));
    }

    fn steps(&self) -> Vec<String> {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn chunks(&self) -> Vec<String> {
        self.chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Options for a call on a WebSocket in `session` that `self` sees.
    fn options(self: &Arc<Self>, session: &str) -> Options {
        let mut options = ws_options(session);
        let context = Arc::new(RequestContext::new(
            Method::POST,
            "/v1/responses".to_owned(),
        ));
        options.observation = Some(Arc::new(Observation::new(
            context,
            vec![Arc::clone(self) as Arc<dyn Tap>],
        )));
        options
    }
}

impl Tap for Steps {
    fn attempt_request(&self, _request: &AttemptRequest<'_>) {
        self.note("request");
    }

    fn request_sent(&self) {
        self.note("sent");
    }

    fn chunk(&self, chunk: &Bytes) {
        self.note("chunk");
        self.chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(String::from_utf8_lossy(chunk).into_owned());
    }
}

// Not upstream's, for `TestXAIWebsocketSuccessfulHandshakeDoesNotMarkRequestAttempt`:
// the request is announced before the dial and told sent only once the
// handshake is answered, before anything is read.
#[tokio::test]
async fn the_request_is_told_sent_once_the_connection_is_up() {
    let (gate, held) = watch::channel(false);
    let server = Arc::new(
        Server::start(move |_| {
            Answer::Held(
                held.clone(),
                Box::new(Answer::accept(|mut peer| async move {
                    if peer.recv().await.is_some() {
                        peer.send(COMPLETED).await;
                        peer.hold().await;
                    }
                })),
            )
        })
        .await,
    );
    let steps = Steps::new(&server);
    let executor = executor();
    let auth = auth(&server.url);
    let call = tokio::spawn({
        let options = steps.options("");
        async move { call(&executor, &auth, HELLO, options).await }
    });
    server
        .wait_for("the handshake", |record| !record.handshakes.is_empty())
        .await;
    assert_eq!(steps.steps(), ["request handshakes=0 messages=0"]);
    gate.send_replace(true);
    let (_, error) = within("the call", call).await.unwrap();
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        steps.steps(),
        [
            "request handshakes=0 messages=0",
            "sent handshakes=1 messages=0",
            "chunk handshakes=1 messages=1",
        ]
    );
}

// Not upstream's: the call's taps are given each message as xAI sent it.
#[tokio::test]
async fn taps_see_each_message() {
    let delta = r#"{"type":"response.output_text.delta","delta":"hi"}"#;
    let server = Arc::new(Server::once(&[delta, COMPLETED]).await);
    let steps = Steps::new(&server);
    let chunks = streamed(&executor(), &auth(&server.url), HELLO, steps.options("")).await;
    assert_eq!(chunks.len(), 2);
    assert_eq!(steps.chunks(), [delta, COMPLETED]);
}
