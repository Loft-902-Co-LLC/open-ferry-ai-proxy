//! The executor against a mock Codex server on 127.0.0.1, ported from
//! upstream's `codex_executor_*_test.go` where they test what is ported.
//! The mock records each request's path, headers and body, and replies with
//! recorded-style SSE.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Body;
use axum::http::Uri;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::StreamExt as _;
use http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::*;
use crate::codex::client::USER_AGENT;
use crate::codex::reasoning::tests::valid_signature;
use crate::json::{exists, get};

mod replay;

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
fn executor() -> CodexExecutor {
    CodexExecutor::new("direct")
}

/// An API key credential for `base_url` (upstream's tests use the same).
fn api_key_auth(base_url: &str) -> Arc<Auth> {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), "test".into());
    Arc::new(auth)
}

/// A ChatGPT sign-in, whose requests go to the executor's base URL.
fn oauth_auth() -> Arc<Auth> {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.metadata
        .insert("access_token".into(), "oauth-token".into());
    auth.metadata.insert("account_id".into(), "acct-1".into());
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

fn compact_options(format: &str) -> Options {
    Options {
        alt: COMPACT_ALT.into(),
        ..options(format)
    }
}

fn with_header(mut options: Options, name: &'static str, value: &str) -> Options {
    options
        .headers
        .append(name, HeaderValue::from_str(value).unwrap());
    options
}

/// Reads a stream to its end: its chunks, a line each, and its error. A
/// chunk is a line; framing them is the handler's job, as upstream.
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

/// The error's credential and request scope flags (upstream's
/// `IsCredentialScoped` and `IsRequestScoped`).
fn scope(error: &ExecError) -> (bool, bool) {
    (error.credential_scoped, error.request_scoped)
}

/// The JSON of each `data:` line in SSE text.
fn sse_events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect()
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

/// Headers upstream makes up to pass for Codex's own client. None may reach
/// Codex unless the client sent it.
const MADE_UP_IDENTITY: [&str; 14] = [
    "originator",
    "session_id",
    "session-id",
    "conversation_id",
    "x-codex-routing-hint",
    "x-client-request-id",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-codex-turn-state",
    "thread-id",
    "version",
    "x-app",
    "x-stainless-lang",
    "connection",
];

fn assert_own_identity(seen: &Seen) {
    assert_eq!(seen.header("user-agent"), Some(USER_AGENT));
    assert!(USER_AGENT.starts_with("open-ferry/"));
    for name in MADE_UP_IDENTITY {
        assert!(seen.header(name).is_none(), "{name} was sent");
    }
}

const COMPLETED_EMPTY: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":0,\"status\":\"completed\",\"background\":false,\"error\":null}}\n\n";

const COMPLETED_WITH_USAGE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n";

const COMPACTION: &str = r#"{"id":"resp_1","object":"response.compaction","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;

#[test]
fn identifies_and_refreshes_ahead() {
    let executor = executor();
    assert_eq!(executor.id(), "codex");
    assert_eq!(executor.refresh_lead(), Some(Duration::from_secs(86_400)));
}

// The request itself: path, headers present and absent, and body.
#[tokio::test]
async fn sends_own_identity_and_adjusted_body() {
    let mock = Mock::start(Reply::sse(COMPLETED_WITH_USAGE)).await;
    let payload = r#"{"model":"gpt-5.4(high)","input":"hello","previous_response_id":"resp_0","generate":true,"prompt_cache_retention":"24h","safety_identifier":"someone","stream_options":{"include_usage":true}}"#;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4(high)", payload),
            options("openai-response"),
        )
        .await
        .unwrap();

    let seen = mock.last();
    assert_eq!(seen.path, "/responses");
    assert_eq!(seen.header("authorization"), Some("Bearer test"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("accept"), Some("text/event-stream"));
    assert!(
        seen.header("chatgpt-account-id").is_none(),
        "an API key has no account"
    );
    assert_own_identity(&seen);

    let body = seen.json();
    assert_eq!(get(&body, "model"), Some(&json!("gpt-5.4")));
    assert_eq!(get(&body, "stream"), Some(&json!(true)));
    assert_eq!(get(&body, "instructions"), Some(&json!("")));
    for field in [
        "previous_response_id",
        "generate",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
        "prompt_cache_key",
        "tools",
    ] {
        assert!(!exists(&body, field), "{field} in {body}");
    }
}

#[tokio::test]
async fn passes_client_headers_through() {
    let mock = Mock::start(Reply::sse(COMPLETED_WITH_USAGE)).await;
    let mut options = options("openai-response");
    for (name, value) in [
        ("user-agent", "my-client/1.2"),
        ("originator", "my-client"),
        ("session-id", "client-session"),
        ("x-codex-turn-state", "turn-state"),
        ("x-codex-beta-features", "a,b"),
        ("version", "1.2.3"),
        ("x-not-passed", "nope"),
    ] {
        options = with_header(options, name, value);
    }
    let payload = r#"{"model":"gpt-5.4","input":"hello","prompt_cache_key":"client-key"}"#;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            options,
        )
        .await
        .unwrap();

    let seen = mock.last();
    assert_eq!(seen.header("user-agent"), Some("my-client/1.2"));
    assert_eq!(seen.header("originator"), Some("my-client"));
    assert_eq!(seen.header("session-id"), Some("client-session"));
    assert_eq!(seen.header("x-codex-turn-state"), Some("turn-state"));
    assert_eq!(seen.header("x-codex-beta-features"), Some("a,b"));
    assert_eq!(seen.header("version"), Some("1.2.3"));
    assert!(seen.header("x-not-passed").is_none());
    assert!(seen.header("session_id").is_none());
    assert!(seen.header("conversation_id").is_none());
    assert_eq!(
        get(&seen.json(), "prompt_cache_key"),
        Some(&json!("client-key"))
    );
}

// TestApplyCodexHeadersUsesAccountHeaderForOAuth, end to end.
#[tokio::test]
async fn sends_account_header_for_chatgpt_sign_ins() {
    let mock = Mock::start(Reply::sse(COMPLETED_WITH_USAGE)).await;
    executor()
        .with_base_url(&mock.url)
        .execute(
            oauth_auth(),
            request("gpt-5.4", r#"{"input":"hi"}"#),
            options("openai-response"),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.path, "/responses");
    assert_eq!(seen.header("authorization"), Some("Bearer oauth-token"));
    assert_eq!(seen.header("chatgpt-account-id"), Some("acct-1"));
    assert_own_identity(&seen);

    // An API key never sends an account, even if one is on record.
    let mut auth = (*api_key_auth(&mock.url)).clone();
    auth.metadata.insert("account_id".into(), "acct-1".into());
    executor()
        .execute(
            Arc::new(auth),
            request("gpt-5.4", r#"{"input":"hi"}"#),
            options("openai-response"),
        )
        .await
        .unwrap();
    assert!(mock.last().header("chatgpt-account-id").is_none());
}

// TestCodexExecutorExecute_NonEmptyCompletionOutputHydratesMissingItemID.
#[tokio::test]
async fn execute_hydrates_missing_item_ids() {
    let mock = Mock::start(Reply::sse(concat!(
        r#"data: {"type":"response.output_item.done","item":{"id":"fc_123","type":"function_call","call_id":"call_123","name":"weather","arguments":"{}"},"output_index":0}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","item":{"id":"fc_done_existing","type":"function_call","call_id":"call_existing","name":"other","arguments":"{}"},"output_index":1}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"id":"resp_1","object":"response","status":"completed","output":[{"id":null,"type":"function_call","call_id":"call_123","name":"weather-terminal","arguments":"{}"},{"id":"fc_existing","type":"function_call","call_id":"call_existing","name":"preserved","arguments":"{}"}]}}"#,
        "\n\n",
    )))
    .await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "gpt-5.4",
                r#"{"model":"gpt-5.4","input":"What is the weather?"}"#,
            ),
            options("openai-response"),
        )
        .await
        .unwrap();
    let payload = payload_json(&response);
    assert_eq!(
        get(&payload, "output.0.id"),
        Some(&json!("fc_123")),
        "{payload}"
    );
    assert_eq!(
        get(&payload, "output.0.name"),
        Some(&json!("weather-terminal")),
        "{payload}"
    );
    assert_eq!(
        get(&payload, "output.1.id"),
        Some(&json!("fc_existing")),
        "{payload}"
    );
    assert_eq!(
        response
            .headers
            .get("content-type")
            .map(HeaderValue::as_bytes),
        Some(b"text/event-stream".as_slice())
    );
}

const OUTPUT_ITEM_THEN_EMPTY_COMPLETION: &str = concat!(
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]},\"output_index\":0}\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":1775555723,\"status\":\"completed\",\"model\":\"gpt-5.4-mini-2026-03-17\",\"output\":[],\"usage\":{\"input_tokens\":8,\"output_tokens\":28,\"total_tokens\":36}}}\n\n",
);

// TestCodexExecutorExecute_EmptyStreamCompletionOutputUsesOutputItemDone.
#[tokio::test]
async fn execute_fills_empty_output_from_items() {
    let mock = Mock::start(Reply::sse(OUTPUT_ITEM_THEN_EMPTY_COMPLETION)).await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "gpt-5.4-mini",
                r#"{"model":"gpt-5.4-mini","messages":[{"role":"user","content":"Say ok"}]}"#,
            ),
            options("openai"),
        )
        .await
        .unwrap();
    let payload = payload_json(&response);
    assert_eq!(
        get(&payload, "choices.0.message.content"),
        Some(&json!("ok")),
        "{payload}"
    );
}

// TestCodexExecutorExecuteStream_EmptyStreamCompletionOutputUsesOutputItemDone.
#[tokio::test]
async fn stream_fills_empty_output_from_items() {
    let mock = Mock::start(Reply::sse(OUTPUT_ITEM_THEN_EMPTY_COMPLETION)).await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request(
                "gpt-5.4-mini",
                r#"{"model":"gpt-5.4-mini","input":"Say ok"}"#,
            ),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    let completed = sse_events(&text)
        .into_iter()
        .find(|event| get(event, "type") == Some(&json!("response.completed")))
        .unwrap_or_else(|| panic!("no response.completed chunk: {text}"));
    assert_eq!(
        get(&completed, "response.output.0.content.0.text"),
        Some(&json!("ok")),
        "{completed}"
    );
}

const CONTEXT_WINDOW_ERROR: &str = concat!(
    "event: response.created\n",
    r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}"#,
    "\n\n",
    "event: error\n",
    r#"data: {"type":"error","error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again.","param":"input"},"sequence_number":2}"#,
    "\n\n",
);

fn assert_context_too_large(error: &ExecError) {
    assert_eq!(error.status, 400, "{error:?}");
    let body: Value = serde_json::from_str(&error.message).unwrap();
    assert_eq!(
        get(&body, "error.type"),
        Some(&json!("invalid_request_error"))
    );
    assert_eq!(get(&body, "error.code"), Some(&json!("context_too_large")));
    assert!(
        error
            .message
            .contains("Your input exceeds the context window"),
        "{}",
        error.message
    );
}

// TestCodexExecutorExecuteSurfacesTerminalStreamError.
#[tokio::test]
async fn execute_surfaces_terminal_stream_error() {
    let body = format!(
        "{CONTEXT_WINDOW_ERROR}event: response.failed\n{}\n\n",
        r#"data: {"type":"response.failed","response":{"id":"resp_1","status":"failed","error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again."}}}"#
    );
    let mock = Mock::start(Reply::sse(&body)).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", r#"{"model":"gpt-5.5","input":"hello"}"#),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_context_too_large(&error);
}

// TestCodexExecutorExecuteStreamSurfacesTerminalStreamError.
#[tokio::test]
async fn stream_surfaces_terminal_stream_error() {
    let mock = Mock::start(Reply::sse(CONTEXT_WINDOW_ERROR)).await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.5", r#"{"model":"gpt-5.5","input":"hello"}"#),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    assert_context_too_large(&error.expect("a terminal error"));
}

// TestCodexExecutorExecuteIncompleteResponseIsSuccessful.
#[tokio::test]
async fn execute_incomplete_response_is_successful() {
    let mock = Mock::start(Reply::sse(concat!(
        r#"data: {"type":"response.incomplete","response":{"id":"resp_1","model":"gpt-5.5","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
        "\n\n",
    )))
    .await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "gpt-5.5",
                r#"{"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}"#,
            ),
            options("claude"),
        )
        .await
        .unwrap();
    let payload = payload_json(&response);
    assert_eq!(
        get(&payload, "stop_reason"),
        Some(&json!("max_tokens")),
        "{payload}"
    );
}

const INVALID_INPUT: &str = concat!(
    r#"data: {"type":"error","error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."}}"#,
    "\n\n",
);

// TestCodexExecutorExecuteExplicitTerminalFailureIsNotRequestScoped.
#[tokio::test]
async fn execute_explicit_terminal_failure() {
    let mock = Mock::start(Reply::sse(INVALID_INPUT)).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", r#"{"model":"gpt-5.5","input":"hello"}"#),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 400, "{error:?}");
    assert_eq!(scope(&error), (false, false));
}

// TestCodexExecutorExecuteStreamExplicitTerminalFailureIsNotSuccessful.
#[tokio::test]
async fn stream_explicit_terminal_failure() {
    let body = format!(
        "{}\n\n{INVALID_INPUT}",
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}"#
    );
    let mock = Mock::start(Reply::sse(&body)).await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.5", r#"{"model":"gpt-5.5","input":"hello"}"#),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    let error = error.expect("a terminal error");
    assert_eq!(error.status, 400);
    assert_eq!(scope(&error), (false, false));
}

#[tokio::test]
async fn errors_hide_the_token() {
    let key = "sk-codex-secret";
    let with_key = |url: &str| {
        let mut auth = (*api_key_auth(url)).clone();
        auth.attributes.insert("api_key".into(), key.into());
        Arc::new(auth)
    };
    let payload = r#"{"model":"gpt-5.5","input":"hello"}"#;

    let mock = Mock::start(Reply::error(
        401,
        r#"{"error":{"message":"bad key sk-codex-secret"}}"#,
    ))
    .await;
    let error = executor()
        .execute(
            with_key(&mock.url),
            request("gpt-5.5", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    // A 401 comes back classified, with the provider's message in it.
    assert!(!error.message.contains(key), "{error:?}");
    assert!(error.message.contains("bad key [redacted]"), "{error:?}");

    let failure = concat!(
        r#"data: {"type":"response.failed","response":{"id":"resp_1","status":"failed","error":{"code":"server_error","message":"bad key sk-codex-secret"}}}"#,
        "\n\n",
    );
    let mock = Mock::start(Reply::sse(failure)).await;
    let error = executor()
        .execute(
            with_key(&mock.url),
            request("gpt-5.5", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert!(!error.message.contains(key), "{error:?}");
    assert!(error.message.contains("bad key [redacted]"), "{error:?}");

    let response = executor()
        .execute_stream(
            with_key(&mock.url),
            request("gpt-5.5", payload),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    let error = error.expect("a terminal error");
    assert!(!error.message.contains(key), "{error:?}");
    assert!(error.message.contains("bad key [redacted]"), "{error:?}");
}

const CREATED_ONLY: &str = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.5\"}}\n\n";

// TestCodexExecutorExecuteMissingCompletionIsRequestScoped and
// TestCodexExecutorExecuteStreamMissingCompletionIsRequestScoped.
#[tokio::test]
async fn missing_completion_is_a_timeout() {
    let mock = Mock::start(Reply::sse(CREATED_ONLY)).await;
    let payload = r#"{"model":"gpt-5.5","input":"hello"}"#;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 408, "{error:?}");
    assert_eq!(
        error.message,
        crate::codex::terminal::INCOMPLETE_STREAM_MESSAGE
    );
    assert_eq!(scope(&error), (false, true));

    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.5", payload),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(text.contains("response.created"), "{text}");
    let error = error.expect("a stream error");
    assert_eq!(error.status, 408);
    assert_eq!(scope(&error), (false, true));
}

// TestCodexExecutorTransportFailureBeforeTerminalIsRequestScoped.
#[tokio::test]
async fn transport_failure_before_terminal_is_a_timeout() {
    let mock = Mock::start(Reply::sse(CREATED_ONLY).cut_off()).await;
    let payload = r#"{"model":"gpt-5.5","input":"hello"}"#;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 408, "{error:?}");
    assert_eq!(scope(&error), (false, true));

    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.5", payload),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    let error = error.expect("a stream error");
    assert_eq!(error.status, 408);
    assert_eq!(scope(&error), (false, true));
}

const COMPLETED_RESP_1: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";

// TestCodexExecutorExecuteIgnoresTransportErrorAfterCompletion and
// TestCodexExecutorExecuteStreamIgnoresTransportErrorAfterCompletion.
#[tokio::test]
async fn transport_error_after_completion_is_ignored() {
    let mock = Mock::start(Reply::sse(COMPLETED_RESP_1).cut_off()).await;
    let payload = r#"{"model":"gpt-5.5","input":"hello"}"#;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", payload),
            options("openai-response"),
        )
        .await
        .unwrap();
    assert_eq!(get(&payload_json(&response), "id"), Some(&json!("resp_1")));

    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.5", payload),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(text.contains("response.completed"), "{text}");
}

#[tokio::test]
async fn stream_closed_before_any_payload_is_empty() {
    let mock = Mock::start(Reply::sse("")).await;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.5", r#"{"input":"hello"}"#),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(text.is_empty() && error.is_none(), "{text} {error:?}");
}

// TestCodexAutoExecutorHTTPFallbackForwardsSequentialCutoffReasoningSummaryDelivery:
// a Responses WebSocket client's call goes over HTTP when the credential
// has websockets off.
#[tokio::test]
async fn stream_forwards_reasoning_summary_delivery() {
    let mock = Mock::start(Reply::sse(concat!(
        r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_1","summary_index":0,"text":"Checking"}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"id":"resp_1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
        "\n\n",
    )))
    .await;
    let payload = r#"{"model":"gpt-5.6-sol","input":"hello","reasoning":{"summary":"detailed"},"stream_options":{"reasoning_summary_delivery":"sequential_cutoff","include_usage":true}}"#;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.6-sol", payload),
            Options {
                downstream_websocket: true,
                ..stream_options("openai-response")
            },
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert!(
        text.contains(r#""type":"response.reasoning_summary_text.done""#),
        "{text}"
    );

    let body = mock.last().json();
    assert!(!exists(&body, "stream_options.include_usage"), "{body}");
    assert_eq!(
        get(&body, "stream_options.reasoning_summary_delivery"),
        Some(&json!("sequential_cutoff"))
    );
}

const ZERO_TOKEN_INCOMPLETE: &str = concat!(
    r#"data: {"type":"response.incomplete","response":{"id":"resp_1","model":"gpt-5.5","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":10,"output_tokens":0,"total_tokens":10}}}"#,
    "\n\n",
);

async fn stream_chat(mock: &Mock) -> (String, Option<ExecError>) {
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request(
                "gpt-5.5",
                r#"{"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}"#,
            ),
            stream_options("openai"),
        )
        .await
        .unwrap();
    collect(response).await
}

// TestCodexExecutorExecuteStream_ZeroTokenIncompleteResponseIsFailure,
// ..._PartialDeltasIncompleteResponseIsSuccessful and
// ..._EmptyDeltaDoesNotBypassZeroTokenFailure.
#[tokio::test]
async fn zero_token_incomplete_streams() {
    let mock = Mock::start(Reply::sse(ZERO_TOKEN_INCOMPLETE)).await;
    let (_, error) = stream_chat(&mock).await;
    let error = error.expect("a zero-token incomplete fails");
    assert_eq!(error.status, 502);
    assert_eq!(
        error.message,
        crate::codex::terminal::EMPTY_INCOMPLETE_STREAM_MESSAGE
    );
    assert_eq!(scope(&error), (false, true));

    let partial = format!(
        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"Hello world\"}}\n\n{ZERO_TOKEN_INCOMPLETE}"
    );
    let mock = Mock::start(Reply::sse(&partial)).await;
    let (text, error) = stream_chat(&mock).await;
    assert!(error.is_none(), "{error:?}");
    assert!(text.contains("Hello world"), "{text}");

    let empty = format!(
        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"\"}}\n\n{ZERO_TOKEN_INCOMPLETE}"
    );
    let mock = Mock::start(Reply::sse(&empty)).await;
    let (_, error) = stream_chat(&mock).await;
    assert_eq!(error.expect("an empty delta isn't output").status, 502);

    // Execute fails the same way.
    let mock = Mock::start(Reply::sse(ZERO_TOKEN_INCOMPLETE)).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", r#"{"input":"hello"}"#),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 502);
    assert_eq!(scope(&error), (false, true));
}

// TestCodexExecutorSimplifiesComplexOneOfToolSchema.
#[tokio::test]
async fn simplifies_complex_one_of_tool_schema() {
    let mock = Mock::start(Reply::sse(COMPLETED_WITH_USAGE)).await;
    let consts = [
        "p.list",
        "m.list",
        "s.list",
        "s.create",
        "s.send",
        "s.fork",
        "s.status",
        "s.messages",
        "sch.list",
        "sch.create",
        "sch.run",
        "sch.delete",
        "sch.toggle",
    ];
    let one_of: Vec<Value> = consts
        .iter()
        .map(|value| json!({"const": value, "description": format!("Action {value}")}))
        .collect();
    let payload = json!({
        "model": "gpt-5.5",
        "messages": [{"role": "user", "content": "Reply with exactly: OK"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "t1",
                "description": "test tool",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": consts, "oneOf": one_of, "description": "Action to perform"},
                        "target_id": {"type": "string", "description": "Optional target"}
                    },
                    "required": ["action"]
                }
            }
        }]
    });
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", &payload.to_string()),
            options("openai"),
        )
        .await
        .unwrap();

    let body = mock.last().json();
    let tool = find(get(&body, "tools"), "name", "t1");
    assert!(
        !exists(tool, "parameters.properties.action.oneOf"),
        "{tool}"
    );
    assert_eq!(
        get(tool, "parameters.properties.action.type"),
        Some(&json!("string"))
    );
    assert_eq!(
        get(tool, "parameters.properties.action.enum")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(13)
    );
    assert_eq!(
        get(tool, "parameters.properties.target_id.type"),
        Some(&json!("string"))
    );
    assert_eq!(get(tool, "parameters.required.0"), Some(&json!("action")));
}

fn number_types_payload() -> String {
    let function = |name: &str, fields: &[&str]| {
        let properties: serde_json::Map<String, Value> = fields
            .iter()
            .map(|field| ((*field).to_owned(), json!({"type": "number"})))
            .collect();
        json!({"type": "function", "name": name, "parameters": {"type": "object", "properties": properties}})
    };
    json!({
        "model": "gpt-5.5",
        "tools": [
            function("exec_command", &["yield_time_ms", "max_output_tokens", "timeout_ms"]),
            function("write_stdin", &["session_id", "yield_time_ms", "max_output_tokens"]),
            function("sleep", &["duration_ms"]),
            function("wait_agent", &["timeout_ms"]),
            function("wait", &["yield_time_ms", "max_tokens"]),
            function("tool_search", &["limit"]),
            function("test_sync_tool", &["sleep_before_ms", "sleep_after_ms", "participants", "timeout_ms"]),
            function("unrelated_tool", &["unrelated_num"]),
            {"type": "namespace", "name": "collaboration", "tools": [function("wait_agent", &["timeout_ms"])]}
        ],
        "input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "additional_tools", "tools": [
                function("functions__exec_command", &["yield_time_ms"]),
                {"type": "namespace", "name": "collaboration", "tools": [function("wait_agent", &["timeout_ms"])]}
            ]}
        ]
    })
    .to_string()
}

fn assert_number_types(body: &Value) {
    let tools = get(body, "tools");
    for (name, fields) in [
        (
            "exec_command",
            &["yield_time_ms", "max_output_tokens", "timeout_ms"][..],
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
        ("unrelated_tool", &["unrelated_num"]),
    ] {
        let tool = find(tools, "name", name);
        for field in fields {
            let path = format!("parameters.properties.{field}.type");
            assert_eq!(get(tool, &path), Some(&json!("number")), "{name}.{field}");
        }
    }
    let collaboration = find(tools, "name", "collaboration");
    assert_eq!(
        get(
            collaboration,
            "tools.0.parameters.properties.timeout_ms.type"
        ),
        Some(&json!("number"))
    );
    let additional = find(get(body, "input"), "type", "additional_tools");
    assert_eq!(
        get(
            additional,
            "tools.0.parameters.properties.yield_time_ms.type"
        ),
        Some(&json!("number"))
    );
    let nested = find(get(additional, "tools"), "name", "collaboration");
    assert_eq!(
        get(nested, "tools.0.parameters.properties.timeout_ms.type"),
        Some(&json!("number"))
    );
}

// TestCodexExecutor_DoesNotNormalizeToolIntegerTypesForCodexUserAgent.
// Upstream sends a Codex CLI user agent to check that it changes nothing;
// nothing here depends on the user agent, so any client's does.
#[tokio::test]
async fn keeps_tool_number_types() {
    let payload = number_types_payload();
    let client = |options: Options| with_header(options, "user-agent", "some-codex-client/0.1.0");

    let mock = Mock::start(Reply::sse(COMPLETED_WITH_USAGE)).await;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", &payload),
            client(options("openai-response")),
        )
        .await
        .unwrap();
    assert_number_types(&mock.last().json());
    assert_eq!(
        mock.last().header("user-agent"),
        Some("some-codex-client/0.1.0")
    );

    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.5", &payload),
            client(stream_options("openai-response")),
        )
        .await
        .unwrap();
    collect(response).await;
    assert_number_types(&mock.last().json());

    let mock = Mock::start(Reply::json(COMPACTION)).await;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", &payload),
            client(compact_options("openai-response")),
        )
        .await
        .unwrap();
    assert_number_types(&mock.last().json());
}

// TestCodexExecutorCountTokensPreservesToolNumberSchemas. Upstream reads the
// counted body through a plugin hook, which isn't ported; the body comes
// from the same preparation here.
#[tokio::test]
async fn count_tokens_keeps_tool_number_types() {
    for (format, payload) in [
        (
            "openai-response",
            r#"{"input":"hi","tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"yield_time_ms":{"type":"number"}}}}]}"#,
        ),
        (
            "claude",
            r#"{"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"exec_command","input_schema":{"type":"object","properties":{"yield_time_ms":{"type":"number"}}}}]}"#,
        ),
    ] {
        let prepared = prepare_body(
            Kind::CountTokens,
            Context::default(),
            &request("gpt-5.4", payload),
            &options(format),
        )
        .unwrap();
        assert_eq!(
            get(
                &prepared.body,
                "tools.0.parameters.properties.yield_time_ms.type"
            ),
            Some(&json!("number")),
            "{format}: {}",
            prepared.body
        );
        let response = executor()
            .count_tokens(
                Arc::new(Auth::default()),
                request("gpt-5.4", payload),
                options(format),
            )
            .await
            .unwrap();
        assert!(!response.payload.is_empty(), "{format}");
    }
}

#[tokio::test]
async fn count_tokens_answers_in_the_clients_format() {
    let payload = r#"{"model":"gpt-5.4","instructions":"be brief","input":"hello there"}"#;
    let response = executor()
        .count_tokens(
            Arc::new(Auth::default()),
            request("gpt-5.4", payload),
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

    let claude = r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hello there"}]}"#;
    let response = executor()
        .count_tokens(
            Arc::new(Auth::default()),
            request("gpt-5.4", claude),
            options("claude"),
        )
        .await
        .unwrap();
    let body = payload_json(&response);
    assert!(
        get(&body, "input_tokens")
            .and_then(Value::as_i64)
            .is_some_and(|count| count > 0),
        "{body}"
    );
}

// TestCodexExecutorExecuteNormalizesNullInstructions,
// TestCodexExecutorExecuteStreamNormalizesNullInstructions and
// TestCodexExecutorCountTokensTreatsNullInstructionsAsEmpty.
#[tokio::test]
async fn null_instructions_become_empty() {
    let mock = Mock::start(Reply::sse(COMPLETED_EMPTY)).await;
    let payload = r#"{"model":"gpt-5.4","instructions":null,"input":"hello"}"#;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            options("openai-response"),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.path, "/responses");
    assert_eq!(get(&seen.json(), "instructions"), Some(&json!("")));

    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    collect(response).await;
    assert_eq!(get(&mock.last().json(), "instructions"), Some(&json!("")));

    let executor = executor();
    let count = |payload: &'static str| {
        executor.count_tokens(
            Arc::new(Auth::default()),
            request("gpt-5.4", payload),
            options("openai-response"),
        )
    };
    let null = count(payload).await.unwrap();
    let empty = count(r#"{"model":"gpt-5.4","instructions":"","input":"hello"}"#)
        .await
        .unwrap();
    assert_eq!(null.payload, empty.payload);
}

// TestCodexExecutorCompactAddsDefaultInstructionsWithoutInjectingImageTool.
#[tokio::test]
async fn compact_adds_default_instructions() {
    for payload in [
        r#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":"history"},{"type":"compaction_trigger"}]}"#,
        r#"{"model":"gpt-5.4","instructions":null,"input":[{"type":"message","role":"user","content":"history"},{"type":"compaction_trigger"}]}"#,
    ] {
        let mock = Mock::start(Reply::json(COMPACTION)).await;
        let response = executor()
            .execute(
                api_key_auth(&mock.url),
                request("gpt-5.4", payload),
                compact_options("openai-response"),
            )
            .await
            .unwrap();
        let seen = mock.last();
        assert_eq!(seen.path, "/responses/compact");
        assert_eq!(seen.header("accept"), Some("application/json"));
        assert_own_identity(&seen);
        let body = seen.json();
        assert_eq!(get(&body, "instructions"), Some(&json!("")), "{body}");
        assert!(!exists(&body, "tools"), "{body}");
        assert!(!exists(&body, "stream"), "{body}");
        assert_eq!(
            get(&body, "input.1.type"),
            Some(&json!("compaction_trigger")),
            "{body}"
        );
        assert_eq!(
            get(&body, "input").and_then(Value::as_array).map(Vec::len),
            Some(2)
        );
        assert_eq!(response.payload, COMPACTION.as_bytes());
    }
}

#[tokio::test]
async fn compact_cannot_stream() {
    let error = executor()
        .execute_stream(
            api_key_auth("http://127.0.0.1:9"),
            request("gpt-5.4", r#"{"input":"x"}"#),
            Options {
                stream: true,
                ..compact_options("openai-response")
            },
        )
        .await;
    let error = refused(error);
    assert_eq!(error.status, 400);
    assert_eq!(
        error.message,
        "streaming not supported for /responses/compact"
    );
}

// TestCodexExecutorExecuteStreamSanitizesOverlongInputItemIDs.
#[tokio::test]
async fn stream_sanitizes_overlong_input_item_ids() {
    let mock = Mock::start(Reply::sse(
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n",
    ))
    .await;
    let long_reasoning_id = format!("rs_{}", "a".repeat(64));
    let long_call_id = "grok-call-item-".repeat(6);
    let long_output_id = "grok-output-item-".repeat(6);
    let payload = json!({
        "model": "gpt-5.4",
        "stream": true,
        "input": [
            {"type": "reasoning", "id": long_reasoning_id, "encrypted_content": valid_signature(), "summary": []},
            {"type": "function_call", "id": long_call_id, "call_id": "call-1", "name": "lookup", "arguments": "{}"},
            {"type": "function_call_output", "id": long_output_id, "call_id": "call-1", "output": "ok"},
            {"type": "message", "id": "item_74ec40c883248ebb4885ec84", "role": "user", "content": "continue"}
        ]
    });
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.4", &payload.to_string()),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    collect(response).await;

    let body = mock.last().json();
    assert_eq!(
        get(&body, "input").and_then(Value::as_array).map(Vec::len),
        Some(3),
        "{body}"
    );
    assert_eq!(
        get(&body, "input.0.type"),
        Some(&json!("function_call")),
        "{body}"
    );
    for (path, original) in [
        ("input.0.id", &long_call_id),
        ("input.1.id", &long_output_id),
    ] {
        let id = get(&body, path).and_then(Value::as_str).unwrap();
        assert!(id.chars().count() <= 64 && id != original, "{path} = {id}");
    }
    assert_eq!(get(&body, "input.0.call_id"), Some(&json!("call-1")));
    assert_eq!(get(&body, "input.1.call_id"), Some(&json!("call-1")));
    assert_eq!(
        get(&body, "input.2.id"),
        Some(&json!("msg_item_74ec40c883248ebb4885ec84"))
    );
}

// TestCodexExecutorDropsInvalidReasoningEncryptedContentFromFinalRequest,
// and its ExecuteStream and compact forms.
#[tokio::test]
async fn drops_invalid_reasoning_encrypted_content() {
    let valid = valid_signature();
    let mock = Mock::start(Reply::sse(COMPLETED_EMPTY)).await;
    let payload = json!({
        "model": "gpt-5.4",
        "input": [
            {"id": "rs_bad", "type": "reasoning", "encrypted_content": "gAAAAABqFTIa…abc", "summary": []},
            {"id": "rs_non_string", "type": "reasoning", "encrypted_content": 123, "summary": []},
            {"id": "rs_good", "type": "reasoning", "encrypted_content": valid, "summary": []},
            {"role": "user", "content": "hello", "encrypted_content": "leave-message-alone"}
        ]
    });
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", &payload.to_string()),
            options("openai-response"),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    for path in [
        "input.0.encrypted_content",
        "input.0.id",
        "input.1.encrypted_content",
        "input.1.id",
    ] {
        assert!(!exists(&body, path), "{path} in {body}");
    }
    assert_eq!(get(&body, "input.2.encrypted_content"), Some(&json!(valid)));
    assert_eq!(
        get(&body, "input.3.encrypted_content"),
        Some(&json!("leave-message-alone"))
    );

    let bad = r#"{"model":"gpt-5.4","stream":true,"input":[{"id":"rs_bad","type":"reasoning","encrypted_content":"bad","summary":[]}]}"#;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.4", bad),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    collect(response).await;
    assert!(!exists(&mock.last().json(), "input.0.encrypted_content"));

    let mock = Mock::start(Reply::json(COMPACTION)).await;
    let bad = r#"{"model":"gpt-5.4","input":[{"id":"rs_bad","type":"reasoning","encrypted_content":"bad","summary":[]}]}"#;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", bad),
            compact_options("openai-response"),
        )
        .await
        .unwrap();
    assert!(!exists(&mock.last().json(), "input.0.encrypted_content"));
}

const KEEPALIVE_STREAM: &str = concat!(
    "event: response.created\n",
    r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6-luna"}}"#,
    "\n\n",
    "event: keepalive\n",
    r#"data: {"type":"keepalive","sequence_number":3}"#,
    "\n\n",
    "event: response.completed\n",
    r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}}"#,
    "\n\n",
);

async fn stream_with_agent(mock: &Mock, agent: &str) -> String {
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.6-luna", r#"{"model":"gpt-5.6-luna","input":"test"}"#),
            with_header(stream_options("openai-response"), "user-agent", agent),
        )
        .await
        .unwrap();
    let (text, error) = tokio::time::timeout(Duration::from_secs(3), collect(response))
        .await
        .expect("the stream ends");
    assert!(error.is_none(), "{error:?}");
    text
}

// TestCodexExecutorExecuteStream_GrokBuildConvertsKeepaliveToSSEComment.
#[tokio::test]
async fn grok_clients_get_keepalive_comments() {
    let mock = Mock::start(Reply::sse(KEEPALIVE_STREAM)).await;
    for agent in [
        "grok-pager/1.0.5 grok-shell/1.0.5 (linux; x86_64)",
        "grok-shell/0.2.119 (macos; aarch64)",
        "grok-pager/1.0.5 (linux; x86_64)",
    ] {
        let text = stream_with_agent(&mock, agent).await;
        assert!(
            !text.contains(r#"{"type":"keepalive""#) && !text.contains("event: keepalive"),
            "{agent}: {text}"
        );
        assert!(text.contains(": keepalive"), "{agent}: {text}");
        assert!(
            text.contains("response.created") && text.contains("response.completed"),
            "{agent}: {text}"
        );
    }
}

// TestCodexExecutorExecuteStream_NonGrokClientKeepsVerbatim.
#[tokio::test]
async fn other_clients_get_keepalive_events() {
    let mock = Mock::start(Reply::sse(KEEPALIVE_STREAM)).await;
    let text = stream_with_agent(&mock, "curl/8.7.1").await;
    assert!(
        text.contains(r#"{"type":"keepalive""#) || text.contains("event: keepalive"),
        "{text}"
    );
}

// A native Codex client (Responses Lite) gets Codex's terminal event as it
// is, and its request keeps no instructions and no parallel tool calls.
#[tokio::test]
async fn native_requests_keep_codex_fidelity() {
    let mock = Mock::start(Reply::sse(OUTPUT_ITEM_THEN_EMPTY_COMPLETION)).await;
    let payload = r#"{"model":"gpt-5.4","input":"hi","parallel_tool_calls":true,"tools":[{"type":"function","name":"lookup"}]}"#;
    let options = with_header(
        stream_options("openai-response"),
        "x-openai-internal-codex-responses-lite",
        "true",
    );
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            options,
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    let completed = sse_events(&text)
        .into_iter()
        .find(|event| get(event, "type") == Some(&json!("response.completed")))
        .unwrap_or_else(|| panic!("no response.completed chunk: {text}"));
    assert_eq!(
        get(&completed, "response.output"),
        Some(&json!([])),
        "{completed}"
    );

    let seen = mock.last();
    assert_eq!(
        seen.header("x-openai-internal-codex-responses-lite"),
        Some("true")
    );
    let body = seen.json();
    assert!(!exists(&body, "instructions"), "{body}");
    assert_eq!(get(&body, "parallel_tool_calls"), Some(&json!(false)));
}

// A Claude stream's message_start gets the request's estimated input
// tokens when Codex gives none (TranslateStreamWithClaudeInputTokens).
#[tokio::test]
async fn claude_streams_get_estimated_input_tokens() {
    let mock = Mock::start(Reply::sse(concat!(
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","delta":"Hi"}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.5","status":"completed","output":[],"usage":{"input_tokens":0,"output_tokens":1,"total_tokens":1}}}"#,
        "\n\n",
    )))
    .await;
    let payload = r#"{"model":"gpt-5.5","max_tokens":100,"messages":[{"role":"user","content":"Tell me about the weather in Paris today."}]}"#;
    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.5", payload),
            stream_options("claude"),
        )
        .await
        .unwrap();
    let (text, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    let start = sse_events(&text)
        .into_iter()
        .find(|event| get(event, "type") == Some(&json!("message_start")))
        .expect("a message_start event");
    let tokens = get(&start, "message.usage.input_tokens")
        .and_then(Value::as_i64)
        .unwrap();
    assert!(tokens > 0, "{start}");
    assert_eq!(
        text.matches("message_start").count(),
        2,
        "event and data: {text}"
    );
}

// Codex's error statuses map as upstream's newCodexStatusErr does.
#[tokio::test]
async fn maps_error_statuses() {
    let usage_limit =
        r#"{"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":30}}"#;
    let mock = Mock::start(Reply::error(403, usage_limit)).await;
    let payload = r#"{"input":"hi"}"#;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 429, "{error:?}");
    assert_eq!(error.retry_after, Some(Duration::from_secs(30)));
    assert_eq!(error.message, usage_limit);
    assert_eq!(scope(&error), (true, false));

    let error = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            stream_options("openai-response"),
        )
        .await;
    let error = refused(error);
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::from_secs(30)));
    assert_eq!(scope(&error), (true, false));

    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            compact_options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::from_secs(30)));
    assert_eq!(scope(&error), (true, false));

    let mock = Mock::start(Reply::error(401, "")).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            compact_options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    let body: Value = serde_json::from_str(&error.message).unwrap();
    assert_eq!(get(&body, "error.code"), Some(&json!("auth_unavailable")));
    assert_eq!(scope(&error), (false, false));

    let mock = Mock::start(Reply::error(500, "")).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!((error.status, error.message.as_str()), (500, "status 500"));
    assert_eq!(scope(&error), (false, false));
}

// A reset time too far ahead for Go's clock: upstream gives the 429 with no
// retry delay, on every path.
#[tokio::test]
async fn oversized_reset_time_has_no_retry_delay() {
    let body = r#"{"error":{"type":"usage_limit_reached","resets_at":9223372036854775807}}"#;
    let mock = Mock::start(Reply::error(429, body)).await;
    let payload = r#"{"input":"hi"}"#;
    let mut errors = Vec::new();
    for options in [
        options("openai-response"),
        compact_options("openai-response"),
    ] {
        let result = executor()
            .execute(
                api_key_auth(&mock.url),
                request("gpt-5.4", payload),
                options,
            )
            .await;
        errors.push(result.unwrap_err());
    }
    let result = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.4", payload),
            stream_options("openai-response"),
        )
        .await;
    errors.push(refused(result));
    for error in errors {
        assert_eq!(error.status, 429, "{error:?}");
        assert_eq!(error.retry_after, None);
        assert_eq!(scope(&error), (true, false));
    }
}

// A usage limit that arrives as a stream's terminal event is the credential's
// too (newCodexStatusErr through codexTerminalStreamErr).
#[tokio::test]
async fn usage_limit_events_are_scoped_to_the_credential() {
    let event = concat!(
        r#"data: {"type":"error","error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":300}}"#,
        "\n\n",
    );
    let mock = Mock::start(Reply::sse(event)).await;
    let payload = r#"{"model":"gpt-5.5","input":"hello"}"#;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("gpt-5.5", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 429, "{error:?}");
    assert_eq!(error.retry_after, Some(Duration::from_secs(300)));
    assert_eq!(scope(&error), (true, false));

    let response = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request("gpt-5.5", payload),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    let error = error.expect("a terminal error");
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::from_secs(300)));
    assert_eq!(scope(&error), (true, false));
}

/// `header:` attributes that would make a request pass for another client,
/// in several cases.
const IDENTITY_ATTRIBUTES: [(&str, &str); 9] = [
    ("header:User-Agent", "codex_cli_rs/0.200.0"),
    ("header:ORIGINATOR", "codex-tui"),
    ("header:Session_id", "synthetic-session"),
    ("header:session-ID", "synthetic-session"),
    ("header:X-App", "cli"),
    ("header:x-stainless-lang", "js"),
    ("header:X-Stainless-Runtime", "node"),
    ("header:X-Claude-Code-Session-Id", "synthetic-session"),
    ("header:user-agent ", "claude-cli/2.1.280"),
];

// No custom header makes a call pass for another client, on any path: Codex
// gets the client's own user agent or this project's, and none of the
// made-up identity headers. Other custom headers still go through.
#[tokio::test]
async fn custom_headers_cannot_set_the_clients_identity() {
    let mock = Mock::start(Reply::sse(COMPLETED_WITH_USAGE)).await;
    let compact = Mock::start(Reply::json(COMPACTION)).await;
    let with_attributes = |url: &str| {
        let mut auth = (*api_key_auth(url)).clone();
        for (key, value) in IDENTITY_ATTRIBUTES {
            auth.attributes.insert(key.into(), value.into());
        }
        auth.attributes
            .insert("header:X-Team".into(), "blue".into());
        Arc::new(auth)
    };
    let payload = r#"{"model":"gpt-5.4","input":"hello"}"#;
    let check = |seen: Seen, user_agent: Option<&str>| {
        match user_agent {
            Some(user_agent) => {
                assert_eq!(seen.header("user-agent"), Some(user_agent));
                assert_eq!(seen.header("originator"), Some("actual_originator"));
            }
            None => assert_own_identity(&seen),
        }
        for name in [
            "session_id",
            "session-id",
            "x-app",
            "x-stainless-lang",
            "x-stainless-runtime",
            "x-claude-code-session-id",
        ] {
            assert!(seen.header(name).is_none(), "{name} was sent");
        }
        assert_eq!(seen.header("x-team"), Some("blue"));
    };
    for client in [false, true] {
        let dress = |options: Options| {
            if client {
                with_header(
                    with_header(options, "user-agent", "actual-client/1"),
                    "originator",
                    "actual_originator",
                )
            } else {
                options
            }
        };
        let user_agent = client.then_some("actual-client/1");
        executor()
            .execute(
                with_attributes(&mock.url),
                request("gpt-5.4", payload),
                dress(options("openai-response")),
            )
            .await
            .unwrap();
        check(mock.last(), user_agent);

        let response = executor()
            .execute_stream(
                with_attributes(&mock.url),
                request("gpt-5.4", payload),
                dress(stream_options("openai-response")),
            )
            .await
            .unwrap();
        let (_, error) = collect(response).await;
        assert!(error.is_none(), "{error:?}");
        check(mock.last(), user_agent);

        executor()
            .execute(
                with_attributes(&compact.url),
                request("gpt-5.4", payload),
                dress(compact_options("openai-response")),
            )
            .await
            .unwrap();
        check(compact.last(), user_agent);
    }
}

#[tokio::test]
async fn connection_failures_are_upstream_errors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = executor()
        .execute(
            api_key_auth(&url),
            request("gpt-5.4", r#"{"input":"hi"}"#),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Upstream);
    assert_eq!(error.status, 0);
    assert!(!error.message.contains("test"), "{}", error.message);
}

#[test]
fn rejects_tokens_that_are_not_header_values() {
    let mut auth = (*api_key_auth("http://127.0.0.1:9")).clone();
    auth.attributes
        .insert("api_key".into(), "bad\nsecret-token".into());
    let error = build_headers(&auth, &HeaderMap::new(), true).unwrap_err();
    assert!(!error.message.contains("secret-token"), "{}", error.message);
}

/// An unsigned ID token with `plan_type`, if any (upstream's
/// `makeTestCodexRefreshJWT`).
fn id_token(plan_type: &str, account_id: &str) -> String {
    let mut auth_info = json!({"chatgpt_account_id": account_id});
    if !plan_type.is_empty() {
        auth_info["chatgpt_plan_type"] = json!(plan_type);
    }
    let claims = json!({"email": "user@example.com", "https://api.openai.com/auth": auth_info});
    format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

/// A mock OpenAI token endpoint that hands out `id_token`.
async fn token_server(id_token: &str) -> Mock {
    let body = json!({
        "access_token": "new-mock-access-token",
        "refresh_token": "new-mock-refresh-token",
        "id_token": id_token,
        "token_type": "Bearer",
        "expires_in": 3600
    });
    Mock::start(Reply::json(&body.to_string())).await
}

fn refreshable(attributes: &[(&str, &str)]) -> Arc<Auth> {
    let mut auth = Auth {
        id: "test-codex-refresh".into(),
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.metadata
        .insert("refresh_token".into(), "valid-refresh-token".into());
    for (key, value) in attributes {
        auth.attributes.insert((*key).into(), (*value).into());
    }
    Arc::new(auth)
}

fn refresh_executor(server: &Mock) -> CodexExecutor {
    executor().with_oauth_endpoints(Endpoints::with_base(&server.url))
}

// TestCodexExecutorRefresh_MissingPlanTypeDefaultsToFree. Upstream reaches
// a TLS server through a CONNECT proxy because its endpoint is fixed; here
// the endpoint is injected. Saving the file is the store's job.
#[tokio::test]
async fn refresh_defaults_missing_plan_type_to_free() {
    let server = token_server(&id_token("", "acc-codex-test")).await;
    let auth = refreshable(&[("plan_type", "unknown")]);
    let refreshed = refresh_executor(&server)
        .refresh(auth.clone())
        .await
        .unwrap();

    assert_eq!(refreshed.attribute("plan_type"), Some("free"));
    assert_eq!(refreshed.metadata_str("plan_type"), Some("free"));
    assert_eq!(
        refreshed.metadata_str("access_token"),
        Some("new-mock-access-token")
    );
    assert_eq!(
        refreshed.metadata_str("refresh_token"),
        Some("new-mock-refresh-token")
    );
    assert_eq!(refreshed.metadata_str("account_id"), Some("acc-codex-test"));
    assert_eq!(refreshed.metadata_str("email"), Some("user@example.com"));
    assert_eq!(refreshed.metadata_str("type"), Some("codex"));
    assert!(
        refreshed
            .metadata_str("expired")
            .is_some_and(|expired| !expired.is_empty())
    );
    assert!(
        refreshed
            .metadata_str("last_refresh")
            .is_some_and(|at| !at.is_empty())
    );
    // The original record is untouched.
    assert_eq!(auth.attribute("plan_type"), Some("unknown"));
    assert_eq!(auth.metadata_str("access_token"), None);

    let seen = server.last();
    assert_eq!(seen.path, "/oauth/token");
    assert!(
        seen.body.contains("grant_type=refresh_token"),
        "{}",
        seen.body
    );
    assert!(
        seen.body.contains("refresh_token=valid-refresh-token"),
        "{}",
        seen.body
    );
    assert_own_identity(&seen);
}

// TestCodexExecutorRefresh_ExtractsPlanTypeWhenPresent.
#[tokio::test]
async fn refresh_takes_plan_type_from_the_id_token() {
    let server = token_server(&id_token("team", "acc-codex-test-team")).await;
    let auth = refreshable(&[("plan_type", "free")]);
    let refreshed = refresh_executor(&server)
        .refresh(auth.clone())
        .await
        .unwrap();
    assert_eq!(refreshed.attribute("plan_type"), Some("team"));
    assert_eq!(refreshed.metadata_str("plan_type"), Some("team"));
    assert_eq!(auth.attribute("plan_type"), Some("free"));
}

// TestCodexExecutorRefresh_EmptyAttributesSnapshotIsolation.
#[tokio::test]
async fn refresh_leaves_the_original_attributes_alone() {
    let server = token_server(&id_token("", "acc-codex-empty-attrs")).await;
    let auth = refreshable(&[]);
    let refreshed = refresh_executor(&server)
        .refresh(auth.clone())
        .await
        .unwrap();
    assert_eq!(refreshed.attribute("plan_type"), Some("free"));
    assert!(auth.attributes.is_empty());
}

#[tokio::test]
async fn refresh_without_a_refresh_token_changes_nothing() {
    let server = token_server(&id_token("", "acct")).await;
    let executor = refresh_executor(&server);
    for auth in [api_key_auth("http://127.0.0.1:9"), oauth_auth()] {
        let refreshed = executor.refresh(auth.clone()).await.unwrap();
        assert_eq!(refreshed.metadata, auth.metadata);
        assert_eq!(refreshed.attributes, auth.attributes);
    }
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn refresh_failure_is_reported_without_the_token() {
    let server = Mock::start(Reply::error(
        400,
        r#"{"error":"invalid_grant","error_description":"refresh_token_reused"}"#,
    ))
    .await;
    let error = refresh_executor(&server)
        .refresh(refreshable(&[]))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Upstream);
    assert!(
        error.message.contains("refresh_token_reused"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("valid-refresh-token"),
        "{}",
        error.message
    );
    // A reused token isn't retried.
    assert_eq!(server.requests().len(), 1);
}
