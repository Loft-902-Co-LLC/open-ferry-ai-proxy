//! The executor against a mock Meta server on 127.0.0.1, ported from
//! upstream's `meta_executor_test.go` (and `testApplyPatchResponsesExecutor`
//! in `xai_executor_test.go`) where they test what is ported. The mock
//! records each request's path, headers and body, and replies with
//! recorded-style SSE. The credentials are dummy keys.
//!
//! Ported: `TestMetaExecutor_Identifier`, `ExecuteSuccessAndRateLimit`,
//! `ExecuteStreamRateLimit`, `ExecuteShapesResponsesRequest`,
//! `ExecuteStreamUsesResponses`, `PreservesPreviousResponseID`,
//! `CompactNotSupported`, `ExecuteNonStreamMultiEventSSE` (what it sends and
//! returns; see below), `NotFoundCooldown_Shortened_Issue6117` (the error
//! carries the cooldown; the credential manager's use of it is tested with
//! the manager), `Execute_StripsSearchContentTypesFromWebSearch` and its
//! stream twin, `NormalizesToolFieldsForCodexUserAgent` and
//! `TestMetaApplyPatchResponsesExecutor` (non-stream and stream). The
//! credential resolution and the error parsing are tested beside the code
//! (`request.rs`, `error.rs`).
//!
//! Inverted: the three `ClientIdHeader_Issue6117` tests (`PrepareRequest`,
//! `applyMetaAPIHeaders`, `Execute`) assert that `X-Client-Id: tbh:tui` is
//! sent where upstream asserts it is. Here it must never be sent, nor a
//! `muse-*` user agent, nor anything Dynamic Client Assertion: the request
//! carries `Authorization`, `Content-Type`, `Accept`, `Cache-Control`, the
//! credential's custom headers, and the client's user agent or
//! `open-ferry/<version>`. A credential's `X-Client-Id`, and the other
//! vendors' identity headers the shared custom-header filter blocks, are
//! dropped and its `User-Agent` ignored, and a `dca:` token is never sent as
//! a bearer.
//!
//! Adapted: `ExecuteNonStreamMultiEventSSE_RecordsModelAndWarnsOnSubstitution`
//! checks the served model through upstream's usage plugin and its log
//! hook; usage, the served model and the log are the call's taps' work here
//! (see `observe_send`), so the executor's test checks that the taps are
//! told the attempt and its secrets, and the translated answer carries the
//! served model.
//!
//! Not ported, for what they test isn't (see the module's docs):
//! `Refresh_RequiresManagerAcceptance`, `PrepareConcurrentAccounts`,
//! `Execute_DCARecovery`, `ManagerRecoversUnauthorizedKey`,
//! `RefreshUsesMintedBaseURL`, `RequestAuthPreparer`,
//! `MintRejectsRemovedOrReloadedCredential`,
//! `Refresh_PreservesSubscriptionMetadata_Issue6117` and
//! `Refresh_ClearsStaleSubscriptionMetadata_Issue6117` all test Meta's
//! sign-in, its token mint and its refresh, which aren't ported.
//!
//! Not upstream's: the checks that the executor sends only what the policy
//! allows, the error events of a stream, a stream cut off, token counting,
//! the payload rules, and that errors hide every secret sent.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::Uri;
use futures_util::StreamExt as _;
use http::{HeaderMap, HeaderValue};
use open_ferry_core::observe::{AttemptRequest, Observation, RequestContext, Tap};
use serde_json::{Map, Value, json};

use super::*;
use crate::codex::client::USER_AGENT;
use crate::json::get;

/// One request the mock received.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: String,
}

impl Seen {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }
}

/// What the mock answers every request with.
#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
    /// Whether the connection fails after the body.
    cut_off: bool,
}

impl Reply {
    fn sse(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.to_owned(),
            cut_off: false,
        }
    }

    fn json(body: &str) -> Self {
        Self {
            content_type: "application/json",
            ..Self::sse(body)
        }
    }

    fn error(status: u16, body: &str) -> Self {
        Self {
            status,
            ..Self::json(body)
        }
    }

    fn cut_off(self) -> Self {
        Self {
            cut_off: true,
            ..self
        }
    }
}

/// A mock server bound to an ephemeral port on 127.0.0.1.
struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    async fn start(reply: Reply) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let reply = reply.clone();
            let recorder = recorder.clone();
            async move {
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Seen {
                        path: uri.path().to_owned(),
                        headers,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });
                let mut parts = vec![Ok::<_, io::Error>(Bytes::from(reply.body))];
                if reply.cut_off {
                    parts.push(Err(io::Error::other("connection cut off")));
                }
                let body = futures_util::stream::iter(parts).then(|part| async move {
                    if part.is_err() {
                        // Let the body out before the connection fails.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    part
                });
                axum::response::Response::builder()
                    .status(reply.status)
                    .header("content-type", reply.content_type)
                    .body(Body::from_stream(body))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, seen }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn last(&self) -> Seen {
        self.requests().pop().expect("no request reached the mock")
    }
}

/// An executor that doesn't use the environment's proxy.
fn executor() -> MetaExecutor {
    MetaExecutor::new("direct")
}

/// An API key credential for `base_url` (upstream's tests use the same).
fn api_key_auth(base_url: &str) -> Arc<Auth> {
    key_auth(base_url, "meta-token")
}

fn key_auth(base_url: &str, key: &str) -> Arc<Auth> {
    let mut auth = Auth {
        provider: "meta".into(),
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), key.into());
    Arc::new(auth)
}

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_owned()),
    }
}

fn options(format: &str) -> Options {
    Options::new(Format::from(format.to_owned()))
}

fn stream_options(format: &str) -> Options {
    Options {
        stream: true,
        ..options(format)
    }
}

fn with_header(mut options: Options, name: &'static str, value: &str) -> Options {
    options
        .headers
        .append(name, HeaderValue::from_str(value).unwrap());
    options
}

/// Reads a stream to its end: its chunks, a line each, and its error.
async fn collect(response: StreamResponse) -> (String, Option<ExecError>) {
    let mut chunks = response.chunks;
    let mut text = String::new();
    let mut error = None;
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => {
                text.push_str(&String::from_utf8_lossy(&chunk));
                text.push('\n');
            }
            Err(failure) => {
                assert!(error.is_none(), "a second error: {failure:?}");
                error = Some(failure);
            }
        }
    }
    (text, error)
}

/// The error of a stream that didn't start.
fn refused(result: Result<StreamResponse, ExecError>) -> ExecError {
    match result {
        Ok(_) => panic!("the stream started"),
        Err(error) => error,
    }
}

fn payload_json(response: &Response) -> Value {
    serde_json::from_slice(&response.payload).unwrap()
}

/// The element of an array whose `field` is `value` (gjson's
/// `#(field=="value")`).
fn find<'v>(array: Option<&'v Value>, field: &str, value: &str) -> &'v Value {
    array
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| get(item, field) == Some(&json!(value)))
        })
        .unwrap_or_else(|| panic!("no element with {field} == {value}"))
}

/// Unix seconds `wait` from now.
fn unix_after(wait: Duration) -> u64 {
    (SystemTime::now() + wait)
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A 429 body for the subscription quota, which resets in two hours
/// (`resetEpoch` in upstream's tests).
fn quota_body() -> String {
    json!({"error": {
        "code": "rate_limit_exceeded",
        "message": "Subscription quota exhausted. Your usage window resets soon.",
        "resets_at": unix_after(Duration::from_secs(2 * 3600)),
        "type": "rate_limit_error",
    }})
    .to_string()
}

/// What Meta answers an ordinary call with (`writeMetaResponsesOK`).
fn ok_stream(text: &str) -> String {
    let event = json!({
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "model": "muse-spark-1.3",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
            }],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
        },
    });
    format!("data: {event}\n\n")
}

/// A chat completions request, which Meta doesn't speak.
const CHAT: &str = r#"{"model":"muse-spark-1.3","messages":[{"role":"user","content":"hello"}]}"#;

/// What a call is allowed to carry besides the headers any HTTP client adds.
const ALLOWED_HEADERS: [&str; 8] = [
    "authorization",
    "content-type",
    "accept",
    "cache-control",
    "user-agent",
    "host",
    "content-length",
    "x-extra",
];

/// Asserts `seen` names no client of Meta's: the policy's headers only, the
/// user agent `agent`, no `X-Client-Id`, no `muse-*` and nothing DCA.
fn assert_no_client_identity(seen: &Seen, agent: &str) {
    assert_eq!(seen.header("user-agent"), Some(agent));
    assert!(seen.header("x-client-id").is_none(), "X-Client-Id was sent");
    for (name, value) in &seen.headers {
        let name = name.as_str();
        let value = value.to_str().unwrap().to_ascii_lowercase();
        assert!(
            ALLOWED_HEADERS.contains(&name) || name == "accept-encoding",
            "{name} was sent"
        );
        for identity in ["muse", "dca", "tbh:", "muse-build"] {
            assert!(!value.contains(identity), "{name}: {value}");
            assert!(!name.contains(identity), "{name}");
        }
    }
}

#[test]
fn identifies_itself() {
    assert_eq!(executor().id(), "meta");
    assert_eq!(executor().refresh_lead(), None);
}

// Meta's credentials aren't refreshed; the credential comes back as it is.
#[tokio::test]
async fn refresh_returns_the_credential() {
    let auth = api_key_auth("http://127.0.0.1:9");
    let refreshed = executor().refresh(Arc::clone(&auth)).await.unwrap();
    assert_eq!(refreshed.attributes, auth.attributes);
    assert_eq!(refreshed.provider, "meta");
}

// ExecuteShapesResponsesRequest, and the inverse of the
// ClientIdHeader_Issue6117 tests: upstream sends `X-Client-Id: tbh:tui` and
// a `muse-build/...` user agent, which must never be sent.
#[tokio::test]
async fn shapes_a_responses_request_and_names_no_client() {
    let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", CHAT),
            options("openai"),
        )
        .await
        .unwrap();

    let seen = mock.last();
    assert_eq!(seen.path, "/responses");
    assert_eq!(seen.header("authorization"), Some("Bearer meta-token"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("accept"), Some("text/event-stream"));
    assert_eq!(seen.header("cache-control"), Some("no-cache"));
    assert!(USER_AGENT.starts_with("open-ferry/"));
    assert_no_client_identity(&seen, USER_AGENT);

    let body = seen.json();
    assert!(!crate::json::exists(&body, "messages"), "{body}");
    assert!(crate::json::exists(&body, "input"), "{body}");
    assert_eq!(get(&body, "stream"), Some(&json!(true)));
    assert_eq!(get(&body, "model"), Some(&json!("muse-spark-1.3")));
    // The answer is a chat completion, as the client asked.
    assert_eq!(
        get(&payload_json(&response), "choices.0.message.content"),
        Some(&json!("ok"))
    );
}

#[tokio::test]
async fn stream_uses_responses_and_names_no_client() {
    let delta = json!({"type": "response.output_text.delta", "delta": "streamed"});
    let mock = Mock::start(Reply::sse(&format!(
        "data: {delta}

{}",
        ok_stream("streamed")
    )))
    .await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", CHAT),
            stream_options("openai"),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(text.contains("streamed"), "{text}");

    let seen = mock.last();
    assert_eq!(seen.path, "/responses");
    assert_eq!(seen.header("authorization"), Some("Bearer meta-token"));
    assert_no_client_identity(&seen, USER_AGENT);
    assert_eq!(get(&seen.json(), "stream"), Some(&json!(true)));
}

// What the client's user agent is: its own.
#[tokio::test]
async fn passes_the_clients_user_agent_on() {
    let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", CHAT),
            with_header(options("openai"), "user-agent", "curl/8.7.1"),
        )
        .await
        .unwrap();
    assert_no_client_identity(&mock.last(), "curl/8.7.1");
}

// A credential's custom headers go, except those that would name a client:
// Meta's `X-Client-Id` and the other vendors' identity headers (the shared
// filter's, see `custom_headers`) are dropped, as a literal or a `$Name` the
// client sent, and `User-Agent` isn't set by it.
#[tokio::test]
async fn custom_headers_cannot_name_a_client() {
    let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
    let mut auth = (*api_key_auth(&mock.url)).clone();
    let vendor_identity = [
        ("header:X-Goog-Api-Client", "gl-node/22 gdcl/9"),
        ("header:X-Msh-Platform", "kimi_cli"),
        ("header:X-Msh-Device-Id", "$X-Source"),
        ("header:X-Grok-Client-Version", "9.9.9"),
        ("header:X-Grok-Client-Identifier", "$X-Source"),
        ("header:X-Xai-Token-Auth", "$X-Source"),
        ("header:x-client-id", "$X-Source"),
    ];
    for (name, value) in [
        ("header:X-Client-Id", "tbh:tui"),
        ("header:User-Agent", "muse-build/9.9"),
        ("header:X-Extra", "kept"),
    ]
    .into_iter()
    .chain(vendor_identity)
    {
        auth.attributes.insert(name.into(), value.into());
    }
    for stream in [false, true] {
        if stream {
            let response = executor()
                .execute_stream(
                    Arc::new(auth.clone()),
                    request("muse-spark-1.3", CHAT),
                    with_header(stream_options("openai"), "x-source", "from-client"),
                )
                .await
                .unwrap();
            collect(response).await;
        } else {
            executor()
                .execute(
                    Arc::new(auth.clone()),
                    request("muse-spark-1.3", CHAT),
                    with_header(options("openai"), "x-source", "from-client"),
                )
                .await
                .unwrap();
        }
        let seen = mock.last();
        assert_eq!(seen.header("x-extra"), Some("kept"));
        for name in [
            "x-goog-api-client",
            "x-msh-platform",
            "x-msh-device-id",
            "x-grok-client-version",
            "x-grok-client-identifier",
            "x-xai-token-auth",
            "x-client-id",
        ] {
            assert!(seen.header(name).is_none(), "{name} was sent");
        }
        assert_no_client_identity(&seen, USER_AGENT);
    }
}

// A token that starts with `dca:` isn't an API key and is never sent as a
// bearer; the credential's other token is.
#[tokio::test]
async fn a_dca_token_is_never_sent() {
    let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
    let mut only_dca = Auth::default();
    only_dca
        .attributes
        .insert("base_url".into(), mock.url.clone());
    only_dca
        .attributes
        .insert("api_key".into(), "dca:minted-0123456789".into());
    let error = executor()
        .execute(
            Arc::new(only_dca.clone()),
            request("muse-spark-1.3", CHAT),
            options("openai"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    assert!(mock.requests().is_empty(), "a DCA token went out");

    only_dca
        .attributes
        .insert("access_token".into(), "real-token".into());
    executor()
        .execute(
            Arc::new(only_dca),
            request("muse-spark-1.3", CHAT),
            options("openai"),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.header("authorization"), Some("Bearer real-token"));
    assert_no_client_identity(&seen, USER_AGENT);
}

// A credential with no token fails before anything is sent, for each kind
// of call; a compaction is refused first.
#[tokio::test]
async fn a_credential_without_a_token_is_a_401() {
    let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
    let mut auth = Auth::default();
    auth.attributes.insert("base_url".into(), mock.url.clone());
    let auth = Arc::new(auth);
    let payload = r#"{"model":"muse-spark-1.3","input":"hi"}"#;

    let error = executor()
        .execute(
            Arc::clone(&auth),
            request("muse-spark-1.3", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    assert_eq!(
        error.message,
        "meta executor: missing API key or access token"
    );
    let error = refused(
        executor()
            .execute_stream(
                Arc::clone(&auth),
                request("muse-spark-1.3", payload),
                stream_options("openai-response"),
            )
            .await,
    );
    assert_eq!(error.status, 401);
    let error = executor()
        .count_tokens(
            Arc::clone(&auth),
            request("muse-spark-1.3", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    assert!(mock.requests().is_empty());

    // The compaction is refused before the credential is looked at.
    let compact = Options {
        alt: COMPACT_ALT.into(),
        ..options("openai-response")
    };
    let error = executor()
        .execute(auth, request("muse-spark-1.3", payload), compact)
        .await
        .unwrap_err();
    assert_eq!(error.status, 501);
}

// Meta's body: what Codex's has, less what Meta doesn't take. Upstream's
// `previous_response_id` is kept (PreservesPreviousResponseID).
#[tokio::test]
async fn preserves_previous_response_id_and_drops_what_meta_does_not_take() {
    let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
    let payload = r#"{"model":"muse-spark-1.3","input":"hello","previous_response_id":"resp_prev","generate":true,"prompt_cache_retention":"24h","safety_identifier":"someone","stream_options":{"include_usage":true},"client_metadata":{"a":"b"}}"#;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", payload),
            options("openai-response"),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(
        get(&body, "previous_response_id"),
        Some(&json!("resp_prev"))
    );
    for dropped in [
        "generate",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
        "client_metadata",
    ] {
        assert!(!crate::json::exists(&body, dropped), "{dropped}: {body}");
    }
}

// CompactNotSupported: before any network use, for a call and a stream.
#[tokio::test]
async fn compact_is_not_supported() {
    let compact = Options {
        alt: COMPACT_ALT.into(),
        ..options("openai-response")
    };
    let auth = api_key_auth("http://127.0.0.1:9");
    let payload = r#"{"model":"muse-spark-1.3"}"#;
    let error = executor()
        .execute(
            Arc::clone(&auth),
            request("muse-spark-1.3", payload),
            compact.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 501);
    assert_eq!(error.message, "/responses/compact not supported");
    let error = refused(
        executor()
            .execute_stream(auth, request("muse-spark-1.3", payload), compact)
            .await,
    );
    assert_eq!(error.status, 501);
}

// ExecuteSuccessAndRateLimit. A 429 for the subscription's quota is the
// credential's, and waits until the limit resets.
#[tokio::test]
async fn a_quota_429_is_the_credentials_and_waits() {
    let mock = Mock::start(Reply::error(429, &quota_body())).await;
    let error = executor()
        .execute(
            key_auth(&mock.url, "valid-token"),
            request("muse-spark-1.3", CHAT),
            options("openai"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 429);
    let wait = error.retry_after.expect("a wait");
    assert!(
        (Duration::from_secs(3600)..=Duration::from_secs(3 * 3600)).contains(&wait),
        "{wait:?}"
    );
    assert!(error.credential_scoped);
    assert!(!error.request_scoped);
    assert_eq!(
        mock.last().header("authorization"),
        Some("Bearer valid-token")
    );
}

// ExecuteStreamRateLimit.
#[tokio::test]
async fn a_stream_quota_429_is_the_credentials_and_waits() {
    let mock = Mock::start(Reply::error(429, &quota_body())).await;
    let error = refused(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request("muse-spark-1.3", CHAT),
                stream_options("openai"),
            )
            .await,
    );
    assert_eq!(error.status, 429);
    assert!(error.retry_after.is_some());
    assert!(error.credential_scoped);
}

// NotFoundCooldown_Shortened_Issue6117: the error carries the five minutes,
// which the credential manager cools the model down for.
#[tokio::test]
async fn an_unknown_model_cools_down_for_five_minutes() {
    let body = r#"{"error":{"type":"invalid_request_error","code":"model_not_found","message":"model not found"}}"#;
    let mock = Mock::start(Reply::error(404, body)).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", CHAT),
            options("openai"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 404);
    assert_eq!(error.message, body);
    assert_eq!(error.retry_after, Some(Duration::from_secs(5 * 60)));
    assert!(!error.credential_scoped);
}

#[tokio::test]
async fn other_failures_pass_through_with_their_body() {
    let mock = Mock::start(Reply::error(503, r#"{"error":"overloaded"}"#)).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", CHAT),
            options("openai"),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (503, r#"{"error":"overloaded"}"#)
    );
    assert_eq!(error.retry_after, None);
    assert!(!error.credential_scoped && !error.request_scoped);
}

// A call whose stream has no completed event is a 408 that doesn't excuse
// the credential or the request.
#[tokio::test]
async fn a_stream_without_a_completed_event_is_a_408() {
    let mock = Mock::start(Reply::sse(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
    ))
    .await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", CHAT),
            options("openai"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 408);
    assert!(!error.request_scoped && !error.credential_scoped);
}

// An `error` or `response.failed` event ends the call with the status in
// its `error.code`, 502 without one.
#[tokio::test]
async fn error_events_end_the_call() {
    for (event, status, scoped) in [
        (
            r#"{"type":"error","error":{"code":429,"message":"Subscription quota exhausted."}}"#,
            429,
            true,
        ),
        (
            r#"{"type":"response.failed","error":{"code":503}}"#,
            503,
            false,
        ),
        (r#"{"type":"error","error":{"code":"bad"}}"#, 502, false),
    ] {
        let sse = format!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"r\"}}}}\n\ndata: {event}\n\n{}",
            ok_stream("never read")
        );
        let mock = Mock::start(Reply::sse(&sse)).await;
        let error = executor()
            .execute(
                api_key_auth(&mock.url),
                request("muse-spark-1.3", CHAT),
                options("openai"),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, status, "{event}");
        assert_eq!(error.credential_scoped, scoped, "{event}");
        assert_eq!(error.message, event);

        // In a stream, what came before reaches the client, then the error.
        let response = executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request("muse-spark-1.3", CHAT),
                stream_options("openai-response"),
            )
            .await
            .unwrap();
        let (text, error) = collect(response).await;
        let error = error.expect("the stream's error");
        assert_eq!(error.status, status, "{event}");
        assert_eq!(error.message, event);
        assert!(text.contains("response.created"), "{text}");
        assert!(!text.contains("never read"), "{text}");
    }
}

// Reading stops with the error; a stream that ends with none is as silent
// as the one before it.
#[tokio::test]
async fn a_stream_that_ends_without_a_terminal_event_is_not_an_error() {
    let mock = Mock::start(Reply::sse(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
    ))
    .await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request(
                "muse-spark-1.3",
                r#"{"model":"muse-spark-1.3","input":"hi"}"#,
            ),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(text.contains("partial"), "{text}");
}

#[tokio::test]
async fn a_stream_cut_off_ends_with_an_error_after_what_came() {
    let mock = Mock::start(
        Reply::sse("data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n")
            .cut_off(),
    )
    .await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request(
                "muse-spark-1.3",
                r#"{"model":"muse-spark-1.3","input":"hi"}"#,
            ),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(text.contains("partial"), "{text}");
    let error = error.expect("the read failed");
    assert_eq!(error.kind, ErrorKind::Upstream);
}

// ExecuteNonStreamMultiEventSSE: the items the stream gives fill in the
// completed event's empty output, and the served model reaches the client.
#[tokio::test]
async fn a_multi_event_stream_answers_one_response() {
    let sse = concat!(
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"item_0\",\"type\":\"message\",\"role\":\"assistant\"}}\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"item_0\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"model\":\"substituted-meta-model\",\"output\":[],\"usage\":{\"total_tokens\":10}}}\n\n",
    );
    let mock = Mock::start(Reply::sse(sse)).await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", CHAT),
            options("openai"),
        )
        .await
        .unwrap();
    let body = payload_json(&response);
    assert_eq!(
        get(&body, "choices.0.message.content"),
        Some(&json!("hello")),
        "{body}"
    );
    assert_eq!(
        get(&body, "model"),
        Some(&json!("substituted-meta-model")),
        "{body}"
    );
}

// An answer that isn't a stream, a plain Responses object, is read as the
// completed event.
#[tokio::test]
async fn a_plain_json_answer_is_read_as_the_completed_event() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"resp_1","object":"response","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"plain"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
    ))
    .await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", CHAT),
            options("openai"),
        )
        .await
        .unwrap();
    assert_eq!(
        get(&payload_json(&response), "choices.0.message.content"),
        Some(&json!("plain"))
    );
}

// Execute_StripsSearchContentTypesFromWebSearch and its stream twin.
#[tokio::test]
async fn strips_search_content_types_from_web_search() {
    let payload = r#"{
        "model": "muse-spark-1.3-contributor",
        "input": [{"role": "user", "content": "search something"}],
        "tools": [
            {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
            {"type": "web_search", "external_web_access": true, "search_content_types": ["text", "image"]}
        ]
    }"#;
    for stream in [false, true] {
        let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
        let model = "muse-spark-1.3-contributor";
        if stream {
            let response = executor()
                .execute_stream(
                    api_key_auth(&mock.url),
                    request(model, payload),
                    stream_options("codex"),
                )
                .await
                .unwrap();
            collect(response).await;
        } else {
            executor()
                .execute(
                    api_key_auth(&mock.url),
                    request(model, payload),
                    options("codex"),
                )
                .await
                .unwrap();
        }
        let body = mock.last().json();
        let search = find(get(&body, "tools"), "type", "web_search");
        assert!(
            !crate::json::exists(search, "search_content_types"),
            "stream={stream}: {body}"
        );
        assert_eq!(get(search, "external_web_access"), Some(&json!(true)));
        find(get(&body, "tools"), "type", "function");
    }
}

/// A function tool whose parameters are numbers named `fields`.
fn number_tool(name: &str, fields: &[&str]) -> Value {
    let properties: Map<String, Value> = fields
        .iter()
        .map(|field| ((*field).to_owned(), json!({"type": "number"})))
        .collect();
    json!({
        "type": "function",
        "name": name,
        "parameters": {"type": "object", "properties": properties},
    })
}

/// The tools of a Codex client and the integer parameters it expects.
const CODEX_TOOLS: [(&str, &[&str]); 7] = [
    (
        "exec_command",
        &["yield_time_ms", "max_output_tokens", "timeout_ms"],
    ),
    (
        "write_stdin",
        &["session_id", "yield_time_ms", "max_output_tokens"],
    ),
    ("sleep", &["duration_ms"]),
    ("wait_agent", &["timeout_ms"]),
    ("wait", &["yield_time_ms", "max_tokens"]),
    ("tool_search", &["limit"]),
    (
        "test_sync_tool",
        &[
            "sleep_before_ms",
            "sleep_after_ms",
            "participants",
            "timeout_ms",
        ],
    ),
];

// NormalizesToolFieldsForCodexUserAgent, for a call and a stream.
#[tokio::test]
async fn normalizes_tool_fields_for_a_codex_user_agent() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"resp_1","object":"response","status":"completed","output":[],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}"#,
    ))
    .await;
    let tools: Vec<Value> = CODEX_TOOLS
        .iter()
        .map(|(name, fields)| number_tool(name, fields))
        .collect();
    let payload = json!({
        "model": "muse-spark-1.3",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "additional_tools", "tools": [number_tool("functions__exec_command", &["yield_time_ms"])]},
        ],
        "tools": tools,
    })
    .to_string();
    let executor = executor();
    let call = |agent: Option<&str>| {
        let options = agent.map_or_else(
            || options("openai-response"),
            |agent| with_header(options("openai-response"), "user-agent", agent),
        );
        executor.execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", &payload),
            options,
        )
    };

    // Another client's, or no, user agent keeps the numbers.
    for agent in [Some("curl/8.7.1"), None] {
        call(agent).await.unwrap();
        let body = mock.last().json();
        assert_eq!(
            get(&body, "tools.0.parameters.properties.yield_time_ms.type"),
            Some(&json!("number")),
            "{agent:?}"
        );
    }

    // A Codex client's gets the integers it expects.
    call(Some("codex-desktop/0.159.0")).await.unwrap();
    let body = mock.last().json();
    for (name, fields) in CODEX_TOOLS {
        let tool = find(get(&body, "tools"), "name", name);
        for field in fields {
            assert_eq!(
                get(tool, &format!("parameters.properties.{field}.type")),
                Some(&json!("integer")),
                "{name} {field}"
            );
        }
    }
    let extra = find(get(&body, "input"), "type", "additional_tools");
    assert_eq!(
        get(extra, "tools.0.parameters.properties.yield_time_ms.type"),
        Some(&json!("integer"))
    );

    let response = executor
        .execute_stream(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", &payload),
            with_header(
                stream_options("openai-response"),
                "user-agent",
                "codex-desktop/0.159.0",
            ),
        )
        .await
        .unwrap();
    collect(response).await;
    let body = mock.last().json();
    let tool = find(get(&body, "tools"), "name", "exec_command");
    assert_eq!(
        get(tool, "parameters.properties.yield_time_ms.type"),
        Some(&json!("integer"))
    );
    let extra = find(get(&body, "input"), "type", "additional_tools");
    assert_eq!(
        get(extra, "tools.0.parameters.properties.yield_time_ms.type"),
        Some(&json!("integer"))
    );
}

// TestMetaApplyPatchResponsesExecutor (`testApplyPatchResponsesExecutor`):
// Meta gets the custom `apply_patch` tool, its history and its choice as
// functions, and the client gets the call back as a custom tool call.
#[tokio::test]
async fn apply_patch_goes_to_meta_as_a_function_and_comes_back_custom() {
    let patch = "*** Begin Patch\n+中😀\n*** End Patch\n";
    let item = json!({
        "type": "function_call",
        "id": "fc1",
        "call_id": "c1",
        "name": "apply_patch",
        "arguments": json!({"input": patch}).to_string(),
    });
    let completed = json!({
        "type": "response.completed",
        "response": {"id": "r1", "status": "completed", "output": [item.clone()]},
    });
    let done = json!({"type": "response.output_item.done", "output_index": 0, "item": item});
    let sse = format!("data: {done}\n\ndata: {completed}\n\n");
    let payload = r#"{"input":[{"type":"custom_tool_call","call_id":"old","name":"apply_patch","input":"old\n"},{"type":"custom_tool_call_output","call_id":"old","output":"ok"}],"tools":[{"type":"custom","name":"apply_patch"}],"tool_choice":{"type":"custom","name":"apply_patch"}}"#;

    for stream in [false, true] {
        let mock = Mock::start(Reply::sse(&sse)).await;
        if stream {
            let response = executor()
                .execute_stream(
                    api_key_auth(&mock.url),
                    request("muse-spark-1.3", payload),
                    stream_options("openai-response"),
                )
                .await
                .unwrap();
            let (text, error) = collect(response).await;
            assert!(error.is_none(), "{error:?}");
            assert!(
                text.contains("\"custom_tool_call\"")
                    && text.contains("\"response.custom_tool_call_input.done\""),
                "bridge bypass: {text}"
            );
        } else {
            let response = executor()
                .execute(
                    api_key_auth(&mock.url),
                    request("muse-spark-1.3", payload),
                    options("openai-response"),
                )
                .await
                .unwrap();
            let body = payload_json(&response);
            let root = get(&body, "response").unwrap_or(&body);
            assert_eq!(
                get(root, "output.0.type"),
                Some(&json!("custom_tool_call")),
                "{body}"
            );
            assert_eq!(get(root, "output.0.input"), Some(&json!(patch)), "{body}");
        }

        let body = mock.last().json();
        assert_eq!(
            get(&body, "tools.0.type"),
            Some(&json!("function")),
            "{body}"
        );
        assert!(
            crate::json::exists(&body, "tools.0.parameters.properties.input"),
            "{body}"
        );
        assert_eq!(
            get(&body, "tool_choice.type"),
            Some(&json!("function")),
            "{body}"
        );
        assert_eq!(
            get(&body, "input.0.arguments"),
            Some(&json!(r#"{"input":"old\n"}"#)),
            "{body}"
        );
        assert_eq!(
            get(&body, "input.1.type"),
            Some(&json!("function_call_output")),
            "{body}"
        );
    }
}

// Not upstream's: the base model goes upstream, and the payload rules of
// the executor, which upstream matches by its identifier, apply.
#[tokio::test]
async fn payload_rules_apply_and_the_thinking_suffix_is_read() {
    let config = Config::parse(
        r#"
payload:
  override:
    - models:
        - name: "muse-*"
          protocol: meta
      params:
        "metadata.rule": "meta"
    - models:
        - name: "muse-*"
          protocol: codex
      params:
        "metadata.other": "codex"
"#,
    )
    .unwrap();
    let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
    executor()
        .with_config(Arc::new(config))
        .execute(
            api_key_auth(&mock.url),
            request(
                "muse-spark-1.3(high)",
                r#"{"model":"muse-spark-1.3(high)","input":"hi"}"#,
            ),
            options("openai-response"),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(
        get(&body, "model"),
        Some(&json!("muse-spark-1.3")),
        "{body}"
    );
    assert_eq!(get(&body, "metadata.rule"), Some(&json!("meta")), "{body}");
    assert!(!crate::json::exists(&body, "metadata.other"), "{body}");
    assert_eq!(
        get(&body, "reasoning.effort"),
        Some(&json!("high")),
        "{body}"
    );
}

// Counting is local: nothing is sent, and the answer is in the client's
// format.
#[tokio::test]
async fn count_tokens_answers_in_the_clients_format() {
    let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
    let payload = r#"{"model":"muse-spark-1.3","instructions":"be brief","input":"hello there"}"#;
    let response = executor()
        .count_tokens(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", payload),
            options("openai-response"),
        )
        .await
        .unwrap();
    let body = payload_json(&response);
    let count = get(&body, "response.usage.input_tokens")
        .and_then(Value::as_i64)
        .unwrap();
    assert!(count > 0, "{body}");
    assert_eq!(get(&body, "response.usage.output_tokens"), Some(&json!(0)));
    assert_eq!(
        get(&body, "response.usage.total_tokens"),
        Some(&json!(count))
    );

    let claude =
        r#"{"model":"muse-spark-1.3","messages":[{"role":"user","content":"hello there"}]}"#;
    let response = executor()
        .count_tokens(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", claude),
            options("claude"),
        )
        .await
        .unwrap();
    assert!(
        get(&payload_json(&response), "input_tokens")
            .and_then(Value::as_i64)
            .is_some_and(|count| count > 0)
    );
    assert!(mock.requests().is_empty(), "counting called Meta");
}

/// A tap that writes down what it is told.
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl Tap for Recorder {
    fn attempt_request(&self, request: &AttemptRequest<'_>) {
        let names: Vec<&str> = request
            .headers
            .keys()
            .map(http::HeaderName::as_str)
            .collect();
        self.push(format!(
            "request {} {} {} {} {}",
            request.method,
            request.provider,
            request.model,
            request.format.as_str(),
            names.join(",")
        ));
        self.push(format!(
            "secrets {}",
            request.secrets.iter().collect::<Vec<_>>().join(",")
        ));
        self.push(format!("url {}", request.url));
    }

    fn response_head(&self, status: u16, _headers: &HeaderMap) {
        self.push(format!("head {status}"));
    }
}

impl Recorder {
    fn push(&self, event: String) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }

    fn events(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

// Not upstream's: the taps are told the attempt, with the token among its
// secrets to scrub, and what the attempt sends names no client of Meta's.
#[tokio::test]
async fn the_taps_are_told_the_attempt() {
    let mock = Mock::start(Reply::sse(&ok_stream("ok"))).await;
    let recorder = Arc::new(Recorder::default());
    let context = Arc::new(RequestContext::new(
        Method::POST,
        "/v1/chat/completions".into(),
    ));
    let observation = Arc::new(Observation::new(context, vec![recorder.clone()]));
    let options = Options {
        observation: Some(observation),
        ..options("openai")
    };
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("muse-spark-1.3", CHAT),
            options,
        )
        .await
        .unwrap();

    let events = recorder.events();
    assert_eq!(events.len(), 4, "{events:?}");
    let request = &events[0];
    assert!(
        request.starts_with("request POST meta muse-spark-1.3 codex "),
        "{request}"
    );
    assert!(request.contains("authorization"), "{request}");
    assert!(!request.contains("x-client-id"), "{request}");
    assert_eq!(events[1], "secrets meta-token");
    assert_eq!(events[2], format!("url {}/responses", mock.url));
    assert_eq!(events[3], "head 200");
}

// Not upstream's: an upstream or proxy that echoes what it was sent in its
// error gets none of it back to the client, not the credential headers
// after the custom ones, nor a cookie, nor the URL's credentials, nor the
// proxy's password; for a call and for a stream.
#[tokio::test]
async fn errors_hide_every_secret_sent() {
    let payload = r#"{"model":"muse-spark-1.3","input":"hello"}"#;
    for case in crate::secret_echo::cases(|base_url| (*api_key_auth(base_url)).clone()).await {
        for options in [
            options("openai-response"),
            stream_options("openai-response"),
        ] {
            let options = Options {
                headers: case.headers.clone(),
                ..options
            };
            let auth = Arc::clone(&case.auth);
            let error = if options.stream {
                executor()
                    .execute_stream(auth, request("muse-spark-1.3", payload), options)
                    .await
                    .err()
            } else {
                executor()
                    .execute(auth, request("muse-spark-1.3", payload), options)
                    .await
                    .err()
            };
            case.check(&error.expect("the call went through"));
        }
    }
}
