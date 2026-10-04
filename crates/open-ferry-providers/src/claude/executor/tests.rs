//! The executor against a mock Anthropic server on 127.0.0.1, covering what
//! upstream's `claude_executor_*_test.go` test of the ported paths. The mock
//! records each request's path, query, headers and body, and replies with
//! recorded-style SSE or JSON.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Body as AxumBody;
use axum::http::Uri;
use futures_util::StreamExt as _;
use http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::*;
use crate::claude::client::USER_AGENT;
use crate::claude::request::OAUTH_BETA;

/// One request the mock received.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    query: String,
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

    /// Fails if any of `names` was sent.
    fn assert_absent(&self, names: &[&str]) {
        for name in names {
            assert_eq!(self.header(name), None, "{name} was sent");
        }
    }
}

/// What the mock answers every request with.
#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: &'static str,
    headers: Vec<(&'static str, String)>,
    body: String,
    /// Whether the connection fails after the body.
    cut_off: bool,
}

impl Reply {
    fn sse(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![("request-id", "req_test".to_owned())],
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

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
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
                        query: uri.query().unwrap_or_default().to_owned(),
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
                let mut response = axum::response::Response::builder()
                    .status(reply.status)
                    .header("content-type", reply.content_type);
                for (name, value) in reply.headers {
                    response = response.header(name, value);
                }
                response.body(AxumBody::from_stream(body)).unwrap()
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

    /// An executor that sends credentials without a base URL here, as if it
    /// were Anthropic's API.
    fn executor(&self) -> ClaudeExecutor {
        ClaudeExecutor::new("direct").with_base_url(self.url.clone())
    }
}

const API_KEY: &str = "sk-ant-api03-test";
const OAUTH_TOKEN: &str = "sk-ant-oat01-test";

/// An API key credential.
fn api_key_auth() -> Arc<Auth> {
    let mut auth = Auth {
        provider: "claude".into(),
        ..Auth::default()
    };
    auth.attributes.insert("api_key".into(), API_KEY.into());
    Arc::new(auth)
}

/// An API key credential for a gateway at `base_url`.
fn gateway_auth(base_url: &str) -> Arc<Auth> {
    let mut auth = (*api_key_auth()).clone();
    auth.attributes.insert("base_url".into(), base_url.into());
    Arc::new(auth)
}

/// A Claude sign-in.
fn oauth_auth() -> Arc<Auth> {
    let mut auth = Auth {
        provider: "claude".into(),
        ..Auth::default()
    };
    let metadata = json!({
        "type": "claude",
        "access_token": OAUTH_TOKEN,
        "refresh_token": "rt-old",
        "email": "user@example.com",
        "claude_device_ids": {"opaque": ["kept-as-is"]}
    });
    auth.metadata = metadata.as_object().cloned().unwrap();
    Arc::new(auth)
}

fn request(payload: Value) -> Request {
    Request {
        model: "claude-sonnet-4-5".into(),
        payload: Bytes::from(payload.to_string()),
    }
}

fn options(format: Format) -> Options {
    Options::new(format)
}

fn stream_options(format: Format) -> Options {
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

fn claude_payload() -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 256,
        "temperature": 0.5,
        "betas": ["context-1m-2025-08-07"],
        "messages": [{"role": "user", "content": "hi"}]
    })
}

/// A recorded-style Messages stream.
const SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5-20250929\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":1}}}\n",
    "\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
    "\n",
    "event: ping\n",
    "data: {\"type\": \"ping\"}\n",
    "\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n",
    "\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n",
    "\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
    "\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":5}}\n",
    "\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n",
    "\n",
);

/// The same reply as one Messages JSON body.
const MESSAGE: &str = r#"{"id":"msg_01","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[{"type":"text","text":"Hello there"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":5}}"#;

/// Headers that would make a request pose as Claude Code, which are never
/// synthesized.
const DISGUISE_HEADERS: &[&str] = &[
    "x-app",
    "x-stainless-lang",
    "x-stainless-package-version",
    "x-stainless-os",
    "x-stainless-arch",
    "x-stainless-runtime",
    "x-stainless-runtime-version",
    "x-stainless-retry-count",
    "x-stainless-timeout",
    "x-claude-code-session-id",
    "anthropic-dangerous-direct-browser-access",
    "x-client-request-id",
    "accept-encoding",
];

async fn collect(response: StreamResponse) -> Vec<Result<Bytes, ExecError>> {
    response.chunks.collect().await
}

fn text(chunk: &Result<Bytes, ExecError>) -> String {
    String::from_utf8(chunk.as_ref().unwrap().to_vec()).unwrap()
}

#[tokio::test]
async fn native_call_with_an_api_key() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let response = mock
        .executor()
        .execute(
            api_key_auth(),
            request(claude_payload()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap();
    assert_eq!(response.payload, MESSAGE.as_bytes());
    assert_eq!(
        response.headers.get("request-id").unwrap(),
        HeaderValue::from_static("req_test")
    );

    let seen = mock.last();
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.query, "beta=true");
    assert_eq!(seen.header("x-api-key"), Some(API_KEY));
    assert_eq!(seen.header("authorization"), None);
    assert_eq!(seen.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(seen.header("anthropic-beta"), Some("context-1m-2025-08-07"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("accept"), Some("application/json"));
    assert_eq!(seen.header("user-agent"), Some(USER_AGENT));
    assert!(USER_AGENT.starts_with("open-ferry/"));
    seen.assert_absent(DISGUISE_HEADERS);

    let body = seen.json();
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 256,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
            ]}],
            "stream": false
        })
    );
}

// A sign-in sends its token as a Bearer with the OAuth beta, the client's own
// headers and IDs pass through, and nothing is added to the body.
#[tokio::test]
async fn oauth_call_passes_the_client_through() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let payload = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "metadata": {"user_id": "client-chosen-id"},
        "system": [{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}],
        "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "hi"}]
    });
    let options = with_header(
        with_header(
            with_header(
                options(Format::CLAUDE),
                "anthropic-beta",
                "interleaved-thinking-2025-05-14",
            ),
            "user-agent",
            "my-app/2.0",
        ),
        "x-stainless-lang",
        "js",
    );
    mock.executor()
        .execute(oauth_auth(), request(payload.clone()), options)
        .await
        .unwrap();

    let seen = mock.last();
    assert_eq!(
        seen.header("authorization"),
        Some("Bearer sk-ant-oat01-test")
    );
    assert_eq!(seen.header("x-api-key"), None);
    assert_eq!(
        seen.header("anthropic-beta"),
        Some("oauth-2025-04-20,interleaved-thinking-2025-05-14")
    );
    assert_eq!(seen.header("user-agent"), Some("my-app/2.0"));
    assert_eq!(seen.header("x-stainless-lang"), Some("js"));
    seen.assert_absent(&["x-app", "x-claude-code-session-id", "accept-encoding"]);

    let body = seen.json();
    assert_eq!(body["metadata"], json!({"user_id": "client-chosen-id"}));
    assert_eq!(body["system"], payload["system"]);
    assert_eq!(body["tools"], payload["tools"]);
    assert_eq!(body["model"], "claude-sonnet-4-5");
}

#[tokio::test]
async fn oauth_alone_gets_only_the_oauth_beta() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    mock.executor()
        .execute(
            oauth_auth(),
            request(json!({"messages": [{"role": "user", "content": "hi"}]})),
            options(Format::CLAUDE),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.header("anthropic-beta"), Some(OAUTH_BETA));
    assert!(seen.json().get("metadata").is_none());
    assert!(seen.json().get("system").is_none());
}

// Not upstream's: a `betas` that a payload rule writes reaches the
// `anthropic-beta` header, and leaves the body, exactly as the same `betas`
// in the client's body do. Upstream applies the rules first and then
// `extractAndRemoveBetas` (claude_executor_execute.go), and nothing filters
// either, so the rule's betas are the operator's choice.
#[tokio::test]
async fn betas_a_rule_writes_are_handled_as_the_clients_are() {
    let rules = Arc::new(
        Config::parse(
            r"
payload:
  override:
    - models:
        - name: claude-sonnet-4-5
          protocol: claude
      params:
        betas: [payload-rule-probe]
",
        )
        .unwrap(),
    );
    let without_betas = || {
        let mut payload = claude_payload();
        payload.as_object_mut().unwrap().remove("betas");
        payload
    };
    let client_header =
        |options| with_header(options, "anthropic-beta", "interleaved-thinking-2025-05-14");
    for (auth, expected) in [
        (
            api_key_auth(),
            "interleaved-thinking-2025-05-14,payload-rule-probe",
        ),
        (
            oauth_auth(),
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,payload-rule-probe",
        ),
    ] {
        // The rule writes the betas of a body that has none.
        let by_rule = Mock::start(Reply::json(MESSAGE)).await;
        by_rule
            .executor()
            .with_config(Arc::clone(&rules))
            .execute(
                Arc::clone(&auth),
                request(without_betas()),
                client_header(options(Format::CLAUDE)),
            )
            .await
            .unwrap();
        // The client sends the same betas, and no rule is configured.
        let by_client = Mock::start(Reply::json(MESSAGE)).await;
        let mut payload = without_betas();
        payload["betas"] = json!(["payload-rule-probe"]);
        by_client
            .executor()
            .execute(
                Arc::clone(&auth),
                request(payload),
                client_header(options(Format::CLAUDE)),
            )
            .await
            .unwrap();

        let (by_rule, by_client) = (by_rule.last(), by_client.last());
        assert_eq!(by_rule.header("anthropic-beta"), Some(expected));
        assert_eq!(by_client.header("anthropic-beta"), Some(expected));
        assert!(by_rule.json().get("betas").is_none(), "{}", by_rule.body);
        assert_eq!(by_rule.json(), by_client.json());
    }
}

#[tokio::test]
async fn native_stream_forwards_whole_events() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let mut payload = claude_payload();
    payload["stream"] = json!(true);
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(payload),
            stream_options(Format::CLAUDE),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers.get("content-type").unwrap(),
        HeaderValue::from_static("text/event-stream")
    );
    let chunks = collect(response).await;
    assert_eq!(chunks.len(), 8);
    for chunk in &chunks {
        let event = text(chunk);
        assert!(event.starts_with("event: "), "{event:?}");
        assert!(event.ends_with("\n\n"), "{event:?}");
    }
    let joined: String = chunks.iter().map(text).collect();
    assert_eq!(joined, SSE);

    let seen = mock.last();
    assert_eq!(seen.json()["stream"], true);
    assert_eq!(seen.header("accept"), Some("application/json"));
}

#[tokio::test]
async fn native_stream_stops_at_message_stop() {
    let body = format!("{SSE}event: ping\ndata: {{\"type\":\"ping\"}}\n\n");
    let mock = Mock::start(Reply::sse(&body)).await;
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(claude_payload()),
            stream_options(Format::CLAUDE),
        )
        .await
        .unwrap();
    let joined: String = collect(response).await.iter().map(text).collect();
    assert_eq!(joined, SSE);
}

#[tokio::test]
async fn cut_off_stream_ends_with_an_error() {
    let partial = SSE.split("event: message_delta").next().unwrap();
    let mock = Mock::start(Reply::sse(partial).cut_off()).await;
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(claude_payload()),
            stream_options(Format::CLAUDE),
        )
        .await
        .unwrap();
    let chunks = collect(response).await;
    let (last, events) = chunks.split_last().unwrap();
    let joined: String = events.iter().map(text).collect();
    assert_eq!(joined, partial);
    let error = last.as_ref().unwrap_err();
    assert_eq!(error.status, 0);
    assert!(!error.message.is_empty());

    // A clean end without message_stop just ends.
    let mock = Mock::start(Reply::sse(partial)).await;
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(claude_payload()),
            stream_options(Format::CLAUDE),
        )
        .await
        .unwrap();
    let chunks = collect(response).await;
    assert!(chunks.iter().all(Result::is_ok));
}

fn openai_payload(stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 128,
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

#[tokio::test]
async fn openai_stream_is_translated() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(openai_payload(true)),
            stream_options(Format::OPENAI),
        )
        .await
        .unwrap();
    let chunks = collect(response).await;
    let mut content = String::new();
    let mut finish = None;
    for chunk in &chunks {
        let chunk = text(chunk);
        assert!(!chunk.starts_with("data:"), "{chunk:?}");
        let event: Value = serde_json::from_str(&chunk).unwrap();
        assert_eq!(event["object"], "chat.completion.chunk");
        if let Some(delta) = event["choices"][0]["delta"]["content"].as_str() {
            content.push_str(delta);
        }
        if let Some(reason) = event["choices"][0]["finish_reason"].as_str() {
            finish = Some(reason.to_owned());
        }
    }
    assert_eq!(content, "Hello there");
    assert_eq!(finish.as_deref(), Some("stop"));

    let seen = mock.last();
    let body = seen.json();
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(body["stream"], true);
    assert!(body["messages"].is_array());
}

#[tokio::test]
async fn openai_call_reads_claudes_stream() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let response = mock
        .executor()
        .execute(
            api_key_auth(),
            request(openai_payload(false)),
            options(Format::OPENAI),
        )
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(reply["object"], "chat.completion");
    assert_eq!(reply["choices"][0]["message"]["content"], "Hello there");
    assert_eq!(reply["choices"][0]["finish_reason"], "stop");

    // Claude is asked for a stream, which it is sent as.
    let seen = mock.last();
    assert_eq!(seen.json()["stream"], true);
}

#[tokio::test]
async fn openai_responses_call_gets_usage_details() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let payload = json!({"model": "claude-sonnet-4-5", "input": "hi"});
    let response = mock
        .executor()
        .execute(
            api_key_auth(),
            request(payload),
            options(Format::OPENAI_RESPONSE),
        )
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(reply["object"], "response");
    assert_eq!(
        reply["usage"]["output_tokens_details"]["reasoning_tokens"],
        0
    );
    assert_eq!(reply["usage"]["input_tokens_details"]["cached_tokens"], 0);
}

// An error event in a stream that came with a 200 is a 502 for a
// non-streaming call (validateClaudeStreamingResponse).
#[tokio::test]
async fn error_event_in_a_success_is_a_bad_gateway() {
    let body = concat!(
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );
    let mock = Mock::start(Reply::sse(body)).await;
    let error = mock
        .executor()
        .execute(
            api_key_auth(),
            request(openai_payload(false)),
            options(Format::OPENAI),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 502);
    assert_eq!(
        error.message,
        "claude executor: upstream returned error event: Overloaded"
    );
}

// Not upstream's: such an error event that quotes the key has it
// redacted.
#[tokio::test]
async fn an_error_event_in_a_success_hides_the_key() {
    let body = format!(
        "event: error
data: {{\"type\":\"error\",\"error\":{{\"type\":\"authentication_error\",\"message\":\"bad key {API_KEY}\"}}}}

"
    );
    let mock = Mock::start(Reply::sse(&body)).await;
    let error = mock
        .executor()
        .execute(
            api_key_auth(),
            request(openai_payload(false)),
            options(Format::OPENAI),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "claude executor: upstream returned error event: bad key [redacted]"
    );
}

#[tokio::test]
async fn account_wide_rate_limit_is_the_credentials() {
    let reset = chrono::Utc::now().timestamp() + 3600;
    let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your account's rate limit."}}"#;
    let mock = Mock::start(
        Reply::error(429, body)
            .header("anthropic-ratelimit-unified-status", "rejected")
            .header("anthropic-ratelimit-unified-5h-status", "rejected")
            .header("anthropic-ratelimit-unified-5h-reset", reset.to_string())
            .header("anthropic-ratelimit-unified-reset", reset.to_string()),
    )
    .await;
    let error = mock
        .executor()
        .execute(
            oauth_auth(),
            request(claude_payload()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 429);
    assert_eq!(error.message, body);
    assert!(error.credential_scoped);
    assert!(!error.request_scoped);
    let wait = error.retry_after.unwrap();
    assert!(
        (Duration::from_secs(3500)..=Duration::from_secs(3640)).contains(&wait),
        "{wait:?}"
    );
    assert_eq!(
        error
            .headers
            .get("anthropic-ratelimit-unified-status")
            .unwrap(),
        HeaderValue::from_static("rejected")
    );

    // With model-level cooling it stays with the model.
    let error = mock
        .executor()
        .with_model_level_cooling(true)
        .execute(
            oauth_auth(),
            request(claude_payload()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap_err();
    assert!(!error.credential_scoped);
}

#[tokio::test]
async fn fast_mode_rate_limit_is_the_requests() {
    let mock = Mock::start(Reply::error(429, r#"{"error":{"message":"slow down"}}"#)).await;
    let mut payload = claude_payload();
    payload["speed"] = json!("fast");
    let error = mock
        .executor()
        .execute(api_key_auth(), request(payload), options(Format::CLAUDE))
        .await
        .unwrap_err();
    assert_eq!(error.status, 429);
    assert!(error.request_scoped);
    assert!(!error.credential_scoped);
    let seen = mock.last();
    assert_eq!(
        seen.header("anthropic-beta"),
        Some("fast-mode-2026-02-01,context-1m-2025-08-07")
    );
}

#[tokio::test]
async fn overloaded_and_unauthorized_pass_through() {
    for (status, body) in [
        (
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ),
        (
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"OAuth authentication is currently not supported."}}"#,
        ),
    ] {
        let mock = Mock::start(Reply::error(status, body).header("retry-after", "30")).await;
        let error = mock
            .executor()
            .execute_stream(
                oauth_auth(),
                request(claude_payload()),
                stream_options(Format::CLAUDE),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, status);
        assert_eq!(error.message, body);
        assert!(!error.credential_scoped);
        assert!(!error.terminal_auth);
        let wait = error.retry_after.unwrap();
        assert!((Duration::from_secs(30)..=Duration::from_secs(61)).contains(&wait));
    }
}

#[tokio::test]
async fn errors_hide_the_key() {
    for (auth, secret) in [(api_key_auth(), API_KEY), (oauth_auth(), OAUTH_TOKEN)] {
        let body = format!(
            r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key {secret}"}}}}"#
        );
        let mock = Mock::start(Reply::error(401, &body)).await;
        let error = mock
            .executor()
            .execute(auth, request(claude_payload()), options(Format::CLAUDE))
            .await
            .unwrap_err();
        assert_eq!(error.status, 401);
        assert_eq!(
            error.message,
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key [redacted]"}}"#
        );
    }
}

#[tokio::test]
async fn compressed_success_fails() {
    let mock = Mock::start(Reply::json(MESSAGE).header("content-encoding", "gzip")).await;
    let error = mock
        .executor()
        .execute(
            api_key_auth(),
            request(claude_payload()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 0);
    assert!(
        error
            .message
            .contains("unsupported response content encoding")
    );
}

// A gateway gets the key as a Bearer, and an SSE Accept when streaming.
#[tokio::test]
async fn gateway_gets_a_bearer() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let response = ClaudeExecutor::new("direct")
        .execute_stream(
            gateway_auth(&format!("{}/proxy", mock.url)),
            request(claude_payload()),
            stream_options(Format::CLAUDE),
        )
        .await
        .unwrap();
    assert!(collect(response).await.iter().all(Result::is_ok));
    let seen = mock.last();
    assert_eq!(seen.path, "/proxy/v1/messages");
    assert_eq!(
        seen.header("authorization"),
        Some("Bearer sk-ant-api03-test")
    );
    assert_eq!(seen.header("x-api-key"), None);
    assert_eq!(seen.header("accept"), Some("text/event-stream"));
}

/// `header:` attributes that would make a request pass for another client,
/// in several cases, and the headers they name.
const IDENTITY_ATTRIBUTES: [(&str, &str); 9] = [
    ("header:User-Agent", "claude-cli/2.1.280 (external, cli)"),
    ("header:X-App", "cli"),
    ("header:x-stainless-runtime", "node"),
    ("header:X-STAINLESS-LANG", "js"),
    ("header:X-Stainless-Package-Version", "0.70.0"),
    ("header:Originator", "codex-tui"),
    ("header:Session_id", "synthetic-session"),
    ("header:SESSION-ID", "synthetic-session"),
    ("header:x-claude-code-session-id", "synthetic-session"),
];
const IDENTITY_HEADERS: [&str; 8] = [
    "x-app",
    "x-stainless-runtime",
    "x-stainless-lang",
    "x-stainless-package-version",
    "originator",
    "session_id",
    "session-id",
    "x-claude-code-session-id",
];

fn with_identity_attributes(auth: Arc<Auth>) -> Arc<Auth> {
    let mut auth = (*auth).clone();
    for (key, value) in IDENTITY_ATTRIBUTES {
        auth.attributes.insert(key.into(), value.into());
    }
    auth.attributes
        .insert("header:X-Team".into(), "blue".into());
    Arc::new(auth)
}

// No custom header makes a call pass for another client, on any path: Claude
// gets the client's own user agent or this project's, and none of the made-up
// identity headers. Other custom headers still go through.
#[tokio::test]
async fn custom_headers_cannot_set_the_clients_identity() {
    let check = |seen: Seen, user_agent: &str| {
        assert_eq!(seen.header("user-agent"), Some(user_agent), "{}", seen.path);
        seen.assert_absent(&IDENTITY_HEADERS);
        assert_eq!(seen.header("x-team"), Some("blue"), "{}", seen.path);
    };
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let executor = mock.executor();
    for auth in [api_key_auth(), oauth_auth(), gateway_auth(&mock.url)] {
        let auth = with_identity_attributes(auth);
        executor
            .execute(
                auth.clone(),
                request(claude_payload()),
                options(Format::CLAUDE),
            )
            .await
            .unwrap();
        check(mock.last(), USER_AGENT);
        let client = with_header(options(Format::CLAUDE), "user-agent", "actual-client/1");
        executor
            .execute(auth, request(claude_payload()), client)
            .await
            .unwrap();
        check(mock.last(), "actual-client/1");
    }

    let mock = Mock::start(Reply::sse(SSE)).await;
    let response = mock
        .executor()
        .execute_stream(
            with_identity_attributes(oauth_auth()),
            request(claude_payload()),
            stream_options(Format::CLAUDE),
        )
        .await
        .unwrap();
    assert!(collect(response).await.iter().all(Result::is_ok));
    check(mock.last(), USER_AGENT);

    let mock = Mock::start(Reply::json(r#"{"input_tokens":1}"#)).await;
    mock.executor()
        .count_tokens(
            with_identity_attributes(api_key_auth()),
            request(claude_payload()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap();
    check(mock.last(), USER_AGENT);
}

#[tokio::test]
async fn compact_is_not_supported() {
    let executor = ClaudeExecutor::new("direct").with_base_url("http://127.0.0.1:9");
    let options = Options {
        alt: COMPACT_ALT.into(),
        ..options(Format::OPENAI_RESPONSE)
    };
    let error = executor
        .execute(api_key_auth(), request(json!({})), options)
        .await
        .unwrap_err();
    assert_eq!(error.status, 501);
    assert_eq!(error.message, "/responses/compact not supported");
}

#[tokio::test]
async fn counts_tokens_with_anthropic() {
    let mock = Mock::start(Reply::json(r#"{"input_tokens":42}"#)).await;
    let payload = json!({
        "model": "claude-sonnet-4-5",
        "metadata": {"user_id": "u"},
        "context_management": {"edits": []},
        "betas": ["context-1m-2025-08-07"],
        "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
        "messages": [{"role": "user", "content": "hi"}]
    });
    let response = mock
        .executor()
        .count_tokens(oauth_auth(), request(payload), options(Format::CLAUDE))
        .await
        .unwrap();
    assert_eq!(response.payload, r#"{"input_tokens":42}"#.as_bytes());

    let seen = mock.last();
    assert_eq!(seen.path, "/v1/messages/count_tokens");
    assert_eq!(seen.query, "beta=true");
    assert_eq!(
        seen.header("anthropic-beta"),
        Some("oauth-2025-04-20,context-1m-2025-08-07,token-counting-2024-11-01")
    );
    assert_eq!(seen.header("accept"), Some("application/json"));
    let body = seen.json();
    assert!(body.get("metadata").is_none());
    assert!(body.get("context_management").is_none());
    assert!(body.get("betas").is_none());
    assert_eq!(body["model"], "claude-sonnet-4-5");
}

#[tokio::test]
async fn token_count_errors_are_classified() {
    let mock = Mock::start(Reply::error(400, r#"{"error":{"message":"bad"}}"#)).await;
    let error = mock
        .executor()
        .count_tokens(
            api_key_auth(),
            request(claude_payload()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(error.message, r#"{"error":{"message":"bad"}}"#);
}

// Gateways have no count_tokens contract; upstream estimates locally.
#[tokio::test]
async fn gateway_token_counts_are_deferred() {
    let executor = ClaudeExecutor::new("direct");
    let error = executor
        .count_tokens(
            gateway_auth("http://127.0.0.1:9"),
            request(claude_payload()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 501);
}

#[tokio::test]
async fn refresh_updates_the_tokens() {
    let mock = Mock::start(Reply::json(
        r#"{"access_token":"sk-ant-oat01-new","refresh_token":"rt-new","token_type":"Bearer","expires_in":28800,"account":{"uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","email_address":""},"organization":{"uuid":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","name":"Example Org"}}"#,
    ))
    .await;
    let executor =
        ClaudeExecutor::new("direct").with_oauth_endpoints(Endpoints::with_base(&mock.url));
    assert_eq!(
        executor.refresh_lead(),
        Some(Duration::from_secs(4 * 60 * 60))
    );
    let auth = oauth_auth();
    let refreshed = executor.refresh(auth.clone()).await.unwrap();

    let metadata = &refreshed.metadata;
    assert_eq!(metadata["access_token"], "sk-ant-oat01-new");
    assert_eq!(metadata["refresh_token"], "rt-new");
    // An empty email doesn't erase the known one.
    assert_eq!(metadata["email"], "user@example.com");
    assert_eq!(
        metadata["account_uuid"],
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    );
    assert_eq!(metadata["organization_name"], "Example Org");
    assert_eq!(metadata["type"], "claude");
    assert!(!metadata["expired"].as_str().unwrap().is_empty());
    assert!(!metadata["last_refresh"].as_str().unwrap().is_empty());
    assert_eq!(
        metadata["claude_device_ids"],
        auth.metadata["claude_device_ids"]
    );

    let seen = mock.last();
    assert_eq!(seen.path, "/v1/oauth/token");
    let body = seen.json();
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], "rt-old");

    // An API key has nothing to refresh.
    let unchanged = executor.refresh(api_key_auth()).await.unwrap();
    assert_eq!(unchanged.attributes, api_key_auth().attributes);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn refresh_failure_is_an_error() {
    let mock = Mock::start(Reply::error(400, r#"{"error":"invalid_grant"}"#)).await;
    let executor =
        ClaudeExecutor::new("direct").with_oauth_endpoints(Endpoints::with_base(&mock.url));
    let mut auth = (*oauth_auth()).clone();
    auth.metadata
        .insert("refresh_token".into(), "rt-failing".into());
    let error = executor.refresh(Arc::new(auth)).await.unwrap_err();
    assert_eq!(error.status, 0);
    assert!(
        error.message.contains("token refresh failed"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("rt-failing"));
}

// The model's thinking suffix sets the budget and leaves the model name.
#[tokio::test]
async fn thinking_suffix_is_applied() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let request = Request {
        model: "claude-sonnet-4-5-20250929(16384)".into(),
        payload: Bytes::from(
            json!({
                "model": "claude-sonnet-4-5-20250929(16384)",
                "max_tokens": 32000,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ),
    };
    mock.executor()
        .execute(api_key_auth(), request, options(Format::CLAUDE))
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(body["model"], "claude-sonnet-4-5-20250929");
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 16384})
    );
}

// In a stream, an error event that came with a 200 reaches a Claude client as
// Claude sent it.
#[tokio::test]
async fn error_event_in_a_stream_is_forwarded() {
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"model\":\"claude-sonnet-4-5\"}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );
    let mock = Mock::start(Reply::sse(body)).await;
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(claude_payload()),
            stream_options(Format::CLAUDE),
        )
        .await
        .unwrap();
    let chunks = collect(response).await;
    assert_eq!(chunks.len(), 2);
    assert!(text(&chunks[1]).contains("overloaded_error"));
}

// Not upstream's: an error event in a stream that quotes the key has it
// redacted, for a Claude client and for one that gets Claude's stream
// translated; the events around it go out as Claude sent them.
#[tokio::test]
async fn an_error_event_in_a_stream_hides_the_key() {
    let start = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"model\":\"claude-sonnet-4-5\"}}\n\n",
    );
    let error = |key: &str| {
        format!(
            "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"authentication_error\",\"message\":\"bad key {key}\"}}}}\n\n"
        )
    };
    let mock = Mock::start(Reply::sse(&format!("{start}{}", error(API_KEY)))).await;
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(claude_payload()),
            stream_options(Format::CLAUDE),
        )
        .await
        .unwrap();
    let joined: String = collect(response).await.iter().map(text).collect();
    assert_eq!(joined, format!("{start}{}", error("[redacted]")));

    let mock = Mock::start(Reply::sse(&format!("{start}{}", error(API_KEY)))).await;
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(openai_payload(true)),
            stream_options(Format::OPENAI),
        )
        .await
        .unwrap();
    let joined: String = collect(response).await.iter().map(text).collect();
    assert!(joined.contains("bad key [redacted]"), "{joined}");
    assert!(!joined.contains(API_KEY), "{joined}");
}

#[tokio::test]
async fn openai_responses_stream_gets_usage_details() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let payload = json!({"model": "claude-sonnet-4-5", "input": "hi", "stream": true});
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(payload),
            stream_options(Format::OPENAI_RESPONSE),
        )
        .await
        .unwrap();
    let chunks = collect(response).await;
    let completed = chunks
        .iter()
        .map(text)
        .find(|chunk| chunk.contains("response.completed"))
        .expect("a response.completed event");
    let data = completed
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    let event: Value = serde_json::from_str(data).unwrap();
    assert_eq!(
        event["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
        0
    );
    assert_eq!(
        event["response"]["usage"]["input_tokens_details"]["cached_tokens"],
        0
    );
}

// Not upstream's: an error quotes none of the secrets the request sent (the
// credential headers after the custom ones, each cookie, the URL's
// credentials), nor the password of a proxy that answers 407.
#[tokio::test]
async fn errors_hide_every_secret_sent() {
    for case in crate::secret_echo::cases(|base_url| (*gateway_auth(base_url)).clone()).await {
        for stream in [false, true] {
            let options = Options {
                headers: case.headers.clone(),
                stream,
                ..options(Format::CLAUDE)
            };
            let executor = ClaudeExecutor::new("direct");
            let auth = Arc::clone(&case.auth);
            let error = if stream {
                executor
                    .execute_stream(auth, request(claude_payload()), options)
                    .await
                    .err()
            } else {
                executor
                    .execute(auth, request(claude_payload()), options)
                    .await
                    .err()
            };
            case.check(&error.expect("the call went through"));
        }
    }
}
