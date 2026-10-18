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

#[path = "tool_search_tests.rs"]
mod tool_search_tests;

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

/// Not upstream's: upstream sends the client's Messages body as its bytes,
/// edited in place with sjson, so a tool's input and schema keep each number
/// as the client wrote it.
#[tokio::test]
async fn sends_the_clients_numbers_as_written() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let payload = concat!(
        r#"{"model":"claude-sonnet-4-5","max_tokens":64,"#,
        r#""tools":[{"name":"f","input_schema":{"type":"object","properties":{"x":{"type":"number","minimum":-0,"maximum":1E20}}}}],"#,
        r#""messages":[{"role":"user","content":"hi"},"#,
        r#"{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"f","input":{"x":-0,"y":1E20,"z":[1e5,-0.0]}}]},"#,
        r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}]}"#
    );
    let request = Request {
        model: "claude-sonnet-4-5".into(),
        payload: Bytes::from_static(payload.as_bytes()),
    };
    mock.executor()
        .execute(api_key_auth(), request, options(Format::CLAUDE))
        .await
        .unwrap();
    let body = mock.last().body;
    for kept in [
        r#""input":{"x":-0,"y":1E20,"z":[1e5,-0.0]}"#,
        r#""x":{"type":"number","minimum":-0,"maximum":1E20}"#,
    ] {
        assert!(body.contains(kept), "{kept} in {body}");
    }
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
// in the client's body do. Upstream applies the rules at its final barrier,
// before `extractAndRemoveBetas` (claude_executor_execute.go), and nothing
// filters either, so the rule's betas are the operator's choice.
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

// TestClaudeExecutor_ExecutePayloadStreamOverrideDoesNotChangeResponseParser:
// a rule's `stream` is sent, and the answer is still asked for (`Accept`)
// and read as the call needs it. Changed: no `Accept-Encoding` is sent (see
// `DISGUISE_HEADERS`), so neither upstream's attribute for it nor its value
// is checked.
#[tokio::test]
async fn a_stream_rule_leaves_the_answers_parser() {
    const MODEL: &str = "claude-3-5-sonnet-20241022";
    let events = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\",\"model\":\"claude-3-5-sonnet-20241022\"}}\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n",
    );
    let message = r#"{"id":"msg_123","type":"message","role":"assistant","model":"claude-3-5-sonnet-20241022","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":2,"output_tokens":1}}"#;
    for (source, answer, want_stream) in [
        (Format::OPENAI, Format::OPENAI, true),
        (Format::OPENAI, Format::CLAUDE, false),
        (Format::CLAUDE, Format::OPENAI, true),
        (Format::CLAUDE, Format::CLAUDE, false),
    ] {
        let case = format!("{source} to {answer}");
        let reply = if want_stream {
            Reply::sse(events)
        } else {
            Reply::json(message)
        };
        let mock = Mock::start(reply).await;
        let rules = Config::parse(format!(
            "payload:\n  override:\n    - models:\n        - name: {MODEL}\n          protocol: claude\n      params:\n        stream: {}\n",
            !want_stream
        ))
        .unwrap();
        let mut auth = (*gateway_auth(&mock.url)).clone();
        if want_stream {
            auth.attributes
                .insert("header:Accept".into(), "application/json".into());
        }
        let options = Options {
            response_format: answer,
            ..with_header(options(source), "anthropic-beta", "client-beta")
        };
        ClaudeExecutor::new("direct")
            .with_config(Arc::new(rules))
            .execute(
                Arc::new(auth),
                Request {
                    model: MODEL.into(),
                    payload: Bytes::from_static(
                        br#"{"model":"claude-3-5-sonnet-20241022","stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
                    ),
                },
                options,
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: {error:?}"));
        let seen = mock.last();
        assert_eq!(seen.json()["stream"], !want_stream, "{case}: {}", seen.body);
        let accept = if want_stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        assert_eq!(seen.header("accept"), Some(accept), "{case}");
        let betas = seen.header("anthropic-beta").unwrap_or_default();
        assert!(betas.contains("client-beta"), "{case}: {betas}");
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

// TestClaudeExecutor_ChatCompletionsDefaultMaxTokensAndPayloadDefaultRule:
// a client that sets no output limit gets the model's registered one, and a
// `payload.default` rule for `max_tokens` sets it instead; a limit the client
// sent is kept, for every protocol, on a call and a stream. Changed: the
// model is the one the mock executor is addressed with (claude-opus-5-5 in
// the payloads, whose registered limit is 128000), and the rule's protocol
// is `claude` as upstream's.
#[tokio::test]
async fn default_max_tokens_is_the_registered_limit_and_a_default_rule_replaces_it() {
    let rules = Arc::new(
        Config::parse(
            r"
payload:
  default:
    - models:
        - name: claude-opus-5-5
          protocol: claude
      params:
        max_tokens: 16384
",
        )
        .unwrap(),
    );
    let opus = |payload: Value| Request {
        model: "claude-opus-5-5".into(),
        payload: Bytes::from(payload.to_string()),
    };

    // No rule: an omitted limit is the registered 128000.
    let mock = Mock::start(Reply::sse(SSE)).await;
    mock.executor()
        .execute(
            api_key_auth(),
            opus(json!({"model":"claude-opus-5-5","reasoning_effort":"high","messages":[{"role":"user","content":"hi"}]})),
            options(Format::OPENAI),
        )
        .await
        .unwrap();
    assert_eq!(mock.last().json()["max_tokens"], 128000);

    let hi = json!([{"role":"user","content":"hi"}]);
    let parts = json!([{"role":"user","parts":[{"text":"hi"}]}]);
    for (name, format, payload, want) in [
        (
            "chat omitted",
            Format::OPENAI,
            json!({"model":"claude-opus-5-5","messages":hi}),
            16384,
        ),
        (
            "chat null",
            Format::OPENAI,
            json!({"model":"claude-opus-5-5","max_tokens":null,"messages":hi}),
            16384,
        ),
        (
            "chat explicit max_completion_tokens",
            Format::OPENAI,
            json!({"model":"claude-opus-5-5","max_completion_tokens":4096,"messages":hi}),
            4096,
        ),
        (
            "responses omitted",
            Format::OPENAI_RESPONSE,
            json!({"model":"claude-opus-5-5","input":"hi"}),
            16384,
        ),
        (
            "responses explicit",
            Format::OPENAI_RESPONSE,
            json!({"model":"claude-opus-5-5","max_output_tokens":4096,"input":"hi"}),
            4096,
        ),
        (
            "gemini omitted",
            Format::GEMINI,
            json!({"contents":parts}),
            16384,
        ),
        (
            "gemini explicit",
            Format::GEMINI,
            json!({"contents":parts,"generationConfig":{"maxOutputTokens":4096}}),
            4096,
        ),
    ] {
        for stream in [false, true] {
            let mock = Mock::start(Reply::sse(SSE)).await;
            let executor = mock.executor().with_config(Arc::clone(&rules));
            if stream {
                let response = executor
                    .execute_stream(
                        api_key_auth(),
                        opus(payload.clone()),
                        stream_options(format.clone()),
                    )
                    .await
                    .unwrap();
                collect(response).await;
            } else {
                executor
                    .execute(
                        api_key_auth(),
                        opus(payload.clone()),
                        options(format.clone()),
                    )
                    .await
                    .unwrap();
            }
            assert_eq!(
                mock.last().json()["max_tokens"],
                want,
                "{name} (stream: {stream}): {}",
                mock.last().body
            );
        }
    }
}

// Not upstream's: a Claude client's own body is left to the rules as it is
// (stripTranslatedDefaultMaxTokens returns it untouched), so a default rule
// does not replace the limit it sent.
#[tokio::test]
async fn a_default_rule_keeps_a_claude_clients_max_tokens() {
    let rules = Arc::new(
        Config::parse(
            r"
payload:
  default:
    - models:
        - name: claude-sonnet-4-5
          protocol: claude
      params:
        max_tokens: 16384
",
        )
        .unwrap(),
    );
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    mock.executor()
        .with_config(rules)
        .execute(
            api_key_auth(),
            request(claude_payload()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap();
    assert_eq!(mock.last().json()["max_tokens"], 256);
}

// Ported from claude_executor_test.go (v8.0.32): preservedThinkingTestSignature,
// preservedThinkingReplayPayload and runClaudeExecutorCapture. The signature
// is a syntactically valid Claude one that survives signature sanitization.
const PRESERVED_THINKING_SIGNATURE: &str = "CAISqwIKiAEIEBgCKkBHRlRBsNiptQUWfPoOhuQKwi5LnncZVO9bB5jqOs76D7uBtgktML0zqJtNmLHXHHcgD6lk4MQu4QBXzFd1lbC3Mg5jbGF1ZGUtZmFibGUtNTgBQgh0aGlua2luZ1okZDk3NDM5NzUtNGJiMC00OTM2LTllMjgtZDViMGQyMWJkYzQ4EgxCGh+XVFFFeySAjtAaDL/A1LltGu6MMJ+eXSIwsN0oBpDrqLv22UBfkMnTotnIbkvkOyb9xZHgigG6OZVHaI3gThm+maLKmgO5PrFLKlDFYp+YZksy/wKwszJlnLTPzAK+NUlfzagOE1ymtZTXhAYK260XyFYmg/te/C231+Fr/hoX+EJoUBnrn0gD7hqMISOT+TaFEuOXYsN517GfaxgB";

fn preserved_thinking_replay_payload(model: &str) -> Value {
    thinking_replay_payload(model, json!({"type": "adaptive"}))
}

fn thinking_replay_payload(model: &str, thinking: Value) -> Value {
    let mut payload = json!({
        "model": model,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "earlier reasoning",
                 "signature": PRESERVED_THINKING_SIGNATURE},
                {"type": "text", "text": "Answer"}]},
            {"role": "user", "content": [{"type": "text", "text": "continue"}]}]
    });
    if !thinking.is_null() {
        payload["thinking"] = thinking;
    }
    payload
}

/// `runClaudeExecutorCapture`: sends the payload for `model` through the
/// executor, streaming or not, and returns the body and `anthropic-beta` the
/// upstream saw.
async fn capture_claude_request(
    rules: &str,
    stream: bool,
    model: &str,
    payload: Value,
) -> (Value, String) {
    let mock = Mock::start(if stream {
        Reply::sse(SSE)
    } else {
        Reply::json(MESSAGE)
    })
    .await;
    let executor = mock.executor().with_config(Arc::new(if rules.is_empty() {
        Config::default()
    } else {
        Config::parse(rules).unwrap()
    }));
    let request = Request {
        model: model.into(),
        payload: Bytes::from(payload.to_string()),
    };
    if stream {
        let out = executor
            .execute_stream(api_key_auth(), request, stream_options(Format::CLAUDE))
            .await
            .unwrap();
        for chunk in out.chunks.collect::<Vec<_>>().await {
            chunk.unwrap();
        }
    } else {
        executor
            .execute(api_key_auth(), request, options(Format::CLAUDE))
            .await
            .unwrap();
    }
    let sent = mock.last();
    let betas = sent.header("anthropic-beta").unwrap_or_default().to_owned();
    (sent.json(), betas)
}

const THINKING_BINDING_BETA_NAME: &str = "thinking-binding-controls-2026-08-01";

fn drop_block_behavior(body: &Value) -> &Value {
    &body["thinking"]["block_binding"]["prefix_mismatch_behavior"]
}

// Ported from TestClaudeExecutor_PreservedThinkingReplayInjectsDropBlock.
#[tokio::test]
async fn preserved_thinking_replay_injects_drop_block() {
    for model in [
        "claude-haiku-5-5",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-fable-5-1",
    ] {
        let (body, betas) =
            capture_claude_request("", false, model, preserved_thinking_replay_payload(model))
                .await;
        assert_eq!(drop_block_behavior(&body), "drop_block", "{model}: {body}");
        assert!(
            betas.contains(THINKING_BINDING_BETA_NAME),
            "{model}: {betas}"
        );
        assert!(
            body.to_string().contains(PRESERVED_THINKING_SIGNATURE),
            "{model}: replayed thinking signature must survive injection: {body}"
        );
    }
}

// Not upstream's: upstream's executor tests only replay a signed `thinking`
// block. A `redacted_thinking` block with data is also signed history, so it
// needs the binding and the beta too.
#[tokio::test]
async fn redacted_thinking_replay_injects_drop_block() {
    let model = "claude-opus-5-5";
    let mut payload = preserved_thinking_replay_payload(model);
    payload["messages"][1]["content"][0] = json!({"type": "redacted_thinking", "data": "ZGF0YQ=="});
    let (body, betas) = capture_claude_request("", false, model, payload).await;
    assert_eq!(drop_block_behavior(&body), "drop_block", "{body}");
    assert!(betas.contains(THINKING_BINDING_BETA_NAME), "{betas}");
    assert_eq!(
        body["messages"][1]["content"][0]["type"], "redacted_thinking",
        "{body}"
    );
}

// Ported from TestClaudeExecutor_PreservedThinkingReplayInjectsDropBlockOnStream.
#[tokio::test]
async fn preserved_thinking_replay_injects_drop_block_on_stream() {
    let model = "claude-haiku-5-5";
    let (body, betas) =
        capture_claude_request("", true, model, preserved_thinking_replay_payload(model)).await;
    assert_eq!(drop_block_behavior(&body), "drop_block", "{body}");
    assert!(betas.contains(THINKING_BINDING_BETA_NAME), "{betas}");
}

// Ported from TestClaudeExecutor_PreservedThinkingInjectionSkipsNonPreservedModel.
#[tokio::test]
async fn preserved_thinking_injection_skips_non_preserved_model() {
    let model = "claude-opus-5";
    let (body, _) =
        capture_claude_request("", false, model, preserved_thinking_replay_payload(model)).await;
    assert!(body["thinking"].get("block_binding").is_none(), "{body}");
}

// Ported from TestClaudeExecutor_PreservedThinkingInjectionRequiresSignedBlock.
#[tokio::test]
async fn preserved_thinking_injection_requires_signed_block() {
    let model = "claude-haiku-5-5";
    let payload = json!({"model": model, "thinking": {"type": "adaptive"},
        "messages": [{"role": "user", "content": "hi"}]});
    let (body, _) = capture_claude_request("", false, model, payload).await;
    assert!(body["thinking"].get("block_binding").is_none(), "{body}");
}

// Ported from TestClaudeExecutor_PreservedThinkingInjectionRequiresThinkingType.
#[tokio::test]
async fn preserved_thinking_injection_requires_thinking_type() {
    let model = "claude-haiku-5-5";
    let (body, _) = capture_claude_request(
        "",
        false,
        model,
        thinking_replay_payload(model, Value::Null),
    )
    .await;
    assert!(body["thinking"].get("block_binding").is_none(), "{body}");
}

// Ported from TestClaudeExecutor_PreservedThinkingInjectionKeepsCallerError.
#[tokio::test]
async fn preserved_thinking_injection_keeps_caller_error() {
    let model = "claude-haiku-5-5";
    let thinking = json!({"type": "adaptive",
        "block_binding": {"prefix_mismatch_behavior": "error"}});
    let (body, _) =
        capture_claude_request("", false, model, thinking_replay_payload(model, thinking)).await;
    assert_eq!(drop_block_behavior(&body), "error", "{body}");
}

// Ported from TestClaudeExecutor_PayloadOverrideWinsOverPreservedThinkingInjection.
#[tokio::test]
async fn payload_override_wins_over_preserved_thinking_injection() {
    let model = "claude-haiku-5-5";
    let rules = r"
payload:
  override:
    - models:
        - name: claude-haiku-5-5
          protocol: claude
      params:
        thinking.block_binding.prefix_mismatch_behavior: error
";
    let (body, _) = capture_claude_request(
        rules,
        false,
        model,
        preserved_thinking_replay_payload(model),
    )
    .await;
    assert_eq!(
        drop_block_behavior(&body),
        "error",
        "payload override must be the final barrier: {body}"
    );
}

// Ported from TestClaudeExecutor_PreservedThinkingInjectionSkipsDisabledHaikuThinking.
#[tokio::test]
async fn preserved_thinking_injection_skips_disabled_haiku_thinking() {
    let model = "claude-haiku-5-5";
    let (body, _) = capture_claude_request(
        "",
        false,
        model,
        thinking_replay_payload(model, json!({"type": "disabled"})),
    )
    .await;
    assert!(body["thinking"].get("block_binding").is_none(), "{body}");
}

// Ported from TestClaudeExecutor_PreservedThinkingInjectionSkipsBetweenToolsSonnetThinking.
#[tokio::test]
async fn preserved_thinking_injection_skips_between_tools_sonnet_thinking() {
    let model = "claude-sonnet-5-5";
    let (body, _) = capture_claude_request(
        "",
        false,
        model,
        thinking_replay_payload(model, json!({"type": "between_tools"})),
    )
    .await;
    assert!(body["thinking"].get("block_binding").is_none(), "{body}");
}

// Ported from TestClaudeExecutor_PreservedThinkingInjectionSkipsMythosModel.
#[tokio::test]
async fn preserved_thinking_injection_skips_mythos_model() {
    let model = "claude-mythos-5-1";
    let (body, _) =
        capture_claude_request("", false, model, preserved_thinking_replay_payload(model)).await;
    assert!(body["thinking"].get("block_binding").is_none(), "{body}");
}

// Ported from TestClaudeExecutor_PayloadFilterRemovesPreservedThinkingInjection.
#[tokio::test]
async fn payload_filter_removes_preserved_thinking_injection() {
    let model = "claude-haiku-5-5";
    let rules = r"
payload:
  filter:
    - models:
        - name: claude-haiku-5-5
          protocol: claude
      params:
        - thinking.block_binding
";
    let (body, _) = capture_claude_request(
        rules,
        false,
        model,
        preserved_thinking_replay_payload(model),
    )
    .await;
    assert!(
        body["thinking"].get("block_binding").is_none(),
        "payload filter must remove injected block_binding: {body}"
    );
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
async fn compact_returns_a_sealed_summary_with_responses_usage() {
    let mock = Mock::start(Reply::json(r#"{"content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"Build summary."}],"usage":{"input_tokens":11,"cache_creation_input_tokens":3,"cache_read_input_tokens":100,"output_tokens":7}}"#)).await;
    let options = Options {
        alt: COMPACT_ALT.into(),
        ..options(Format::OPENAI_RESPONSE)
    };
    let response = mock
        .executor()
        .execute(
            api_key_auth(),
            request(json!({"input":"Check the build"})),
            options,
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(body["object"], "response.compaction");
    assert_eq!(body["output"].as_array().unwrap().len(), 1);
    assert_eq!(body["output"][0]["type"], "compaction");
    let summary =
        super::super::compaction::unseal(body["output"][0]["encrypted_content"].as_str().unwrap())
            .unwrap();
    assert_eq!(summary, "Build summary.");
    assert!(!summary.contains("hidden"));
    assert_eq!(body["usage"]["input_tokens"], 114);
    assert_eq!(body["usage"]["output_tokens"], 7);
    assert_eq!(body["usage"]["total_tokens"], 121);
    assert_eq!(body["usage"]["input_tokens_details"]["cached_tokens"], 100);
    let seen = mock.last();
    assert!(
        seen.body
            .contains("Please provide a concise and comprehensive summary")
    );
    assert!(seen.body.contains("Check the build"));
    assert_eq!(seen.json()["stream"], false);
    assert!(!seen.body.contains("resp_ag_compact_"));
}

#[tokio::test]
async fn compaction_stream_expands_previous_capsules_and_drops_foreign_ones() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let capsule =
        crate::claude::compaction::seal("Earlier build summary", "claude-sonnet-4-5").unwrap();
    let payload = json!({"input":[
        {"type":"compaction","encrypted_content":capsule},
        {"type":"compaction","encrypted_content":"gAAAAA-native"},
        {"type":"message","role":"user","content":"Continue"},
        {"type":"compaction_trigger"}
    ],"previous_response_id":"resp-old","tool_choice":"required","metadata":{"x":1}});
    let response = mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(payload),
            stream_options(Format::OPENAI_RESPONSE),
        )
        .await
        .unwrap();
    assert_eq!(response.headers["content-type"], "text/event-stream");
    let frames = response.chunks.collect::<Vec<_>>().await;
    let events: Vec<Value> = frames
        .iter()
        .map(|frame| {
            let frame = std::str::from_utf8(frame.as_ref().unwrap()).unwrap();
            serde_json::from_str(
                frame
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.output_item.done",
            "response.completed"
        ]
    );
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], index);
    }
    assert_eq!(
        events[2]["item"]["encrypted_content"],
        events[3]["item"]["encrypted_content"]
    );
    assert_eq!(events[3]["item"], events[4]["response"]["output"][0]);
    assert_eq!(events[4]["response"]["usage"]["total_tokens"], 17);
    let seen = mock.last();
    assert!(seen.body.contains("Earlier build summary"));
    assert!(!seen.body.contains("gAAAAA-native"));
    assert!(!seen.body.contains("compaction_trigger"));
    assert!(!seen.body.contains("previous_response_id"));
}

#[tokio::test]
async fn corrupt_capsules_in_either_payload_fail_before_sending() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let bad = json!({"input":[{"type":"compaction","encrypted_content":"cpa-ag-compact-v1:AA"}]});
    for original in [false, true] {
        for streaming in [false, true] {
            let mut opts = options(Format::OPENAI_RESPONSE);
            let req = if original {
                opts.original_request = Bytes::from(bad.to_string());
                request(json!({"input":"continue"}))
            } else {
                request(bad.clone())
            };
            let error = if streaming {
                mock.executor()
                    .execute_stream(api_key_auth(), req, opts)
                    .await
                    .unwrap_err()
            } else {
                mock.executor()
                    .execute(api_key_auth(), req, opts)
                    .await
                    .unwrap_err()
            };
            assert_eq!(error.status, 400);
            assert!(error.message.starts_with("invalid compaction capsule:"));
        }
    }
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn ordinary_replay_expands_our_summary_and_ignores_foreign_capsules() {
    let capsule = crate::claude::compaction::seal("Replay context", "claude-sonnet-4-5").unwrap();
    let mock = Mock::start(Reply::sse(SSE)).await;
    let payload = json!({"input":[{"type":"compaction","encrypted_content":capsule},
        {"type":"compaction","encrypted_content":"foreign"},{"type":"message","role":"user","content":"continue"}]});
    mock.executor()
        .execute(
            api_key_auth(),
            request(payload.clone()),
            options(Format::OPENAI_RESPONSE),
        )
        .await
        .unwrap();
    mock.executor()
        .execute_stream(
            api_key_auth(),
            request(payload),
            stream_options(Format::OPENAI_RESPONSE),
        )
        .await
        .unwrap()
        .chunks
        .collect::<Vec<_>>()
        .await;
    for seen in mock.requests() {
        assert!(seen.body.contains("Replay context"));
        assert!(!seen.body.contains("encrypted_content"));
        assert!(!seen.body.contains("foreign"));
    }
}

#[tokio::test]
async fn summary_keeps_tool_history_with_definitions_and_flattens_it_without() {
    for definitions in [false, true] {
        let mock = Mock::start(Reply::json(MESSAGE)).await;
        let mut payload = json!({"input":[
            {"type":"message","role":"user","content":"check build"},
            {"type":"function_call","name":"check_build","call_id":"call:1","arguments":"{}"},
            {"type":"function_call_output","call_id":"call:1","output":"build ok"},
            {"type":"compaction_trigger"}]});
        if definitions {
            payload["tools"] = json!([{"type":"function","name":"check_build","parameters":{"type":"object","properties":{}}}]);
        }
        mock.executor()
            .execute(
                api_key_auth(),
                request(payload),
                options(Format::OPENAI_RESPONSE),
            )
            .await
            .unwrap();
        let seen = mock.last();
        assert!(seen.body.contains("check_build") && seen.body.contains("build ok"));
        let body = seen.json();
        let blocks: Vec<&Value> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .collect();
        assert_eq!(
            blocks.iter().any(|block| block["type"] == "tool_use"),
            definitions
        );
        assert_eq!(
            blocks.iter().any(|block| block["type"] == "tool_result"),
            definitions
        );
        if definitions {
            assert_eq!(body["tool_choice"]["type"], "none");
        }
    }
}

#[tokio::test]
async fn original_trigger_selects_original_history_for_summary() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let mut opts = options(Format::OPENAI_RESPONSE);
    opts.original_request = Bytes::from(json!({"input":[{"type":"message","role":"user","content":"Original history"},{"type":"compaction_trigger"}]}).to_string());
    mock.executor()
        .execute(
            api_key_auth(),
            request(json!({"input":"Changed history"})),
            opts,
        )
        .await
        .unwrap();
    assert!(mock.last().body.contains("Original history"));
    assert!(!mock.last().body.contains("Changed history"));
}

#[tokio::test]
async fn summary_payload_rules_apply_after_tool_choice_and_token_defaults() {
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    let config = Config::parse(
        r"
payload:
  override:
    - models:
        - name: claude-sonnet-4-5
      params:
        max_tokens: 1234
        tool_choice.type: auto
",
    )
    .unwrap();
    let payload = json!({"tools":[{"type":"function","name":"check","parameters":{"type":"object"}}],"input":[{"type":"compaction_trigger"}]});
    mock.executor()
        .with_config(Arc::new(config))
        .execute(
            api_key_auth(),
            request(payload),
            options(Format::OPENAI_RESPONSE),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(body["max_tokens"], 1234);
    assert_eq!(body["tool_choice"]["type"], "auto");
}

#[tokio::test]
async fn compaction_propagates_upstream_errors_and_rejects_missing_summary_text() {
    let mut opts = options(Format::OPENAI_RESPONSE);
    opts.alt = COMPACT_ALT.into();
    let mock = Mock::start(Reply::error(429, "busy").header("retry-after", "60")).await;
    let error = mock
        .executor()
        .execute(api_key_auth(), request(json!({})), opts.clone())
        .await
        .unwrap_err();
    assert_eq!(error.status, 429);
    assert_eq!(error.message, "busy");
    assert!(error.retry_after.is_some());
    let mock = Mock::start(Reply::json(
        r#"{"content":[{"type":"thinking","thinking":"only reasoning"}]}"#,
    ))
    .await;
    let error = mock
        .executor()
        .execute(api_key_auth(), request(json!({})), opts)
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "extract summary: no summary text found in upstream response"
    );
}

#[tokio::test]
async fn tool_ids_are_repaired_in_execute_stream_and_count() {
    let payload = json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call:one","name":"check","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call:one","content":"ok"}]}]});
    let mock = Mock::start(Reply::json(MESSAGE)).await;
    mock.executor()
        .execute(
            api_key_auth(),
            request(payload.clone()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap();
    let stream_mock = Mock::start(Reply::sse(SSE)).await;
    stream_mock
        .executor()
        .execute_stream(
            api_key_auth(),
            request(payload.clone()),
            stream_options(Format::CLAUDE),
        )
        .await
        .unwrap()
        .chunks
        .collect::<Vec<_>>()
        .await;
    let count_mock = Mock::start(Reply::json(r#"{"input_tokens":42}"#)).await;
    count_mock
        .executor()
        .count_tokens(api_key_auth(), request(payload), options(Format::CLAUDE))
        .await
        .unwrap();
    for seen in [mock.last(), stream_mock.last(), count_mock.last()] {
        let body = seen.json();
        let id = body["messages"][0]["content"][0]["id"].as_str().unwrap();
        assert_eq!(id, "cpa_tid_v1_Y2FsbDpvbmU");
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], id);
    }
}

// issue6478_tool_ids_test.go: ClaudeToolIDRepairPrecedesPayloadRules and
// issue6478_tool_ids_edges_test.go: ClaudeUpstreamCountTokensRepairBeforePayload.
#[tokio::test]
async fn tool_id_repair_precedes_final_payload_rules_on_all_paths() {
    let rules = Arc::new(
        Config::parse(
            r#"
payload:
  override:
    - models:
        - name: '*'
          exist: [messages.0.content.0.id]
          not-match:
            - messages.0.content.0.id: 'call:one'
      params:
        temperature: 0.25
    - models:
        - name: '*'
      params:
        messages.0.content.0.id: 'configured:id#override'
        messages.1.content.0.tool_use_id: 'configured:id#override'
  filter:
    - models:
        - name: '*'
      params: [messages.0.content.1.id, messages.1.content.1.tool_use_id]
"#,
        )
        .unwrap(),
    );
    let payload = json!({"messages":[
        {"role":"assistant","content":[
            {"type":"tool_use","id":"call:one","name":"check","input":{}},
            {"type":"tool_use","id":"call:two","name":"check","input":{}}]},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"call:one","content":"ok"},
            {"type":"tool_result","tool_use_id":"call:two","content":"ok"}]}]});
    for path in ["execute", "stream", "count"] {
        let reply = match path {
            "stream" => Reply::sse(SSE),
            "count" => Reply::json(r#"{"input_tokens":42}"#),
            _ => Reply::json(MESSAGE),
        };
        let mock = Mock::start(reply).await;
        let executor = mock.executor().with_config(Arc::clone(&rules));
        match path {
            "stream" => {
                assert!(
                    collect(
                        executor
                            .execute_stream(
                                api_key_auth(),
                                request(payload.clone()),
                                stream_options(Format::CLAUDE)
                            )
                            .await
                            .unwrap()
                    )
                    .await
                    .iter()
                    .all(Result::is_ok)
                );
            }
            "count" => {
                executor
                    .count_tokens(
                        api_key_auth(),
                        request(payload.clone()),
                        options(Format::CLAUDE),
                    )
                    .await
                    .unwrap();
            }
            _ => {
                executor
                    .execute(
                        api_key_auth(),
                        request(payload.clone()),
                        options(Format::CLAUDE),
                    )
                    .await
                    .unwrap();
            }
        }
        let body = mock.last().json();
        assert_eq!(
            body["temperature"], 0.25,
            "{path}: condition must see repaired ID"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["id"], "configured:id#override",
            "{path}"
        );
        assert_eq!(
            body["messages"][1]["content"][0]["tool_use_id"], "configured:id#override",
            "{path}"
        );
        assert!(
            body["messages"][0]["content"][1].get("id").is_none(),
            "{path}"
        );
        assert!(
            body["messages"][1]["content"][1]
                .get("tool_use_id")
                .is_none(),
            "{path}"
        );
    }
}

#[tokio::test]
async fn compaction_usage_sums_saturate() {
    let mock = Mock::start(Reply::json(r#"{"content":[{"type":"text","text":"summary"}],"usage":{"input_tokens":9223372036854775807,"cache_creation_input_tokens":3,"cache_read_input_tokens":100,"output_tokens":7}}"#)).await;
    let response = mock
        .executor()
        .execute(
            api_key_auth(),
            request(json!({})),
            Options {
                alt: COMPACT_ALT.into(),
                ..options(Format::OPENAI_RESPONSE)
            },
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(body["usage"]["input_tokens"], i64::MAX);
    assert_eq!(body["usage"]["total_tokens"], i64::MAX);
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
// credentials), nor the password of a proxy that answers 407; and the
// error log, with `request-log` off, has the answer's status and body,
// scrubbed, for a call that isn't streamed and for a stream.
#[tokio::test]
async fn errors_hide_every_secret_sent() {
    for case in crate::secret_echo::cases(|base_url| (*gateway_auth(base_url)).clone()).await {
        for stream in [false, true] {
            let log = crate::secret_echo::ErrorLog::start();
            let options = log.tapped(Options {
                headers: case.headers.clone(),
                stream,
                ..options(Format::CLAUDE)
            });
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
            case.check_logged(&error.expect("the call went through"), log);
        }
    }
}

/// The first `=== API RESPONSE n ===` block of the error log `log`.
fn first_answer(log: &str) -> &str {
    log.split_once("=== API RESPONSE 1 ===\n")
        .and_then(|(_, rest)| rest.split("\n=== ").next())
        .unwrap_or_else(|| panic!("no API RESPONSE block: {log}"))
}

// Not upstream's: with `request-log` off, the error log of a failed call
// has the attempt's answer, as Anthropic's 429 to a sign-in didn't show:
// the status, the headers and the body of an error status, for a call and
// a stream, in Claude's format and translated for a Chat Completions
// client. Upstream's executors record no answer with the request log off.
#[tokio::test]
async fn error_logs_have_an_error_status_answer() {
    let refused =
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}"#;
    for (format, stream, payload) in [
        (Format::CLAUDE, false, claude_payload()),
        (Format::CLAUDE, true, claude_payload()),
        (Format::OPENAI, false, openai_payload(false)),
        (Format::OPENAI, true, openai_payload(true)),
    ] {
        let mock = Mock::start(Reply::error(429, refused).header("retry-after", "30")).await;
        let log = crate::secret_echo::ErrorLog::start();
        let options = log.tapped(if stream {
            stream_options(format)
        } else {
            options(format)
        });
        let executor = mock.executor();
        let error = if stream {
            executor
                .execute_stream(oauth_auth(), request(payload), options)
                .await
                .err()
        } else {
            executor
                .execute(oauth_auth(), request(payload), options)
                .await
                .err()
        };
        let error = error.expect("the call failed");
        assert_eq!(error.status, 429);
        let log = log.failed(&error);
        let answer = first_answer(&log);
        assert!(answer.contains("\nStatus: 429\nHeaders:\n"), "{log}");
        assert!(answer.contains("\nRetry-After: 30\n"), "{log}");
        assert!(
            answer.trim_end().ends_with(&format!("\nBody:\n{refused}")),
            "{log}"
        );
        assert!(!log.contains(OAUTH_TOKEN), "{log}");
        assert!(!log.contains("API RESPONSE 2"), "{log}");
    }
}

// Not upstream's: with `request-log` off, the error log of a stream cut
// off after its head has the answer's status and headers and the read
// error, once, without the lines the stream sent; that of a send that
// couldn't connect has the call's error.
#[tokio::test]
async fn error_logs_have_a_broken_attempts_error() {
    let partial = SSE.split("event: message_delta").next().unwrap();
    let mock = Mock::start(Reply::sse(partial).cut_off()).await;
    let log = crate::secret_echo::ErrorLog::start();
    let response = mock
        .executor()
        .execute_stream(
            oauth_auth(),
            request(claude_payload()),
            log.tapped(stream_options(Format::CLAUDE)),
        )
        .await
        .unwrap();
    let error = collect(response)
        .await
        .into_iter()
        .find_map(Result::err)
        .expect("the stream failed");
    let log = log.failed(&error);
    let answer = first_answer(&log);
    assert!(answer.contains("\nStatus: 200\nHeaders:\n"), "{log}");
    assert!(
        answer.contains("\nContent-Type: text/event-stream\n"),
        "{log}"
    );
    assert_eq!(answer.matches("\nError: ").count(), 1, "{log}");
    assert!(!answer.contains("message_start"), "{log}");

    // Bound but never listening, so a connection is refused, and the port
    // stays ours.
    let closed = tokio::net::TcpSocket::new_v4().unwrap();
    closed.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    let log = crate::secret_echo::ErrorLog::start();
    let error = ClaudeExecutor::new("direct")
        .with_base_url(url)
        .execute(
            oauth_auth(),
            request(claude_payload()),
            log.tapped(options(Format::CLAUDE)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 0);
    let log = log.failed(&error);
    assert!(log.contains("=== API REQUEST 1 ===\n"), "{log}");
    let answer = first_answer(&log);
    assert!(!answer.contains("Status:"), "{log}");
    assert!(answer.contains(&format!("\nError: {error}\n")), "{log}");
    assert!(!log.contains(OAUTH_TOKEN), "{log}");
}

/// The key, quoted where a model could echo it back.
fn quoting_the_key(text: &str) -> String {
    text.replace("Hello", &format!("key {API_KEY}"))
}

// Not upstream's: a successful answer that quotes the key, as a model can
// echo it back, reaches the client with it redacted: whole for a call that
// isn't streamed, a 200 JSON body with an `error` object among them, for a
// Claude client and for one that gets Claude's stream translated.
#[tokio::test]
async fn a_successful_answer_that_echoes_the_key_hides_it() {
    let error = format!(
        r#"{{"type":"error","error":{{"type":"authentication_error","message":"bad key {API_KEY}"}}}}"#
    );
    // What a call that isn't streamed gets: its payload.
    for (name, reply, format, payload) in [
        (
            "a message",
            Reply::json(&quoting_the_key(MESSAGE)),
            Format::CLAUDE,
            claude_payload(),
        ),
        (
            "an error object",
            Reply::json(&error),
            Format::CLAUDE,
            claude_payload(),
        ),
        (
            "a stream read for a translation",
            Reply::sse(&quoting_the_key(SSE)),
            Format::OPENAI,
            openai_payload(false),
        ),
    ] {
        let mock = Mock::start(reply).await;
        let response = mock
            .executor()
            .execute(api_key_auth(), request(payload), options(format))
            .await
            .unwrap();
        let shown = String::from_utf8_lossy(&response.payload);
        assert!(!shown.contains(API_KEY), "{name}: {shown}");
        assert!(shown.contains("[redacted]"), "{name}: {shown}");
    }
}

// Not upstream's: the same for a stream, whose lines are redacted one at a
// time, for a Claude client and for one that gets Claude's stream
// translated.
#[tokio::test]
async fn a_stream_that_echoes_the_key_hides_it() {
    for (name, format, payload) in [
        ("a Claude client", Format::CLAUDE, claude_payload()),
        ("a translated client", Format::OPENAI, openai_payload(true)),
    ] {
        let mock = Mock::start(Reply::sse(&quoting_the_key(SSE))).await;
        let response = mock
            .executor()
            .execute_stream(api_key_auth(), request(payload), stream_options(format))
            .await
            .unwrap();
        let joined: String = collect(response).await.iter().map(text).collect();
        assert!(!joined.contains(API_KEY), "{name}: {joined}");
        assert!(joined.contains("[redacted]"), "{name}: {joined}");
    }
}

// Not upstream's: a token count's answer that quotes the key has it
// redacted, as any other body.
#[tokio::test]
async fn a_token_count_that_echoes_the_key_hides_it() {
    let mock = Mock::start(Reply::json(&format!(
        r#"{{"input_tokens":42,"echo":"{API_KEY}"}}"#
    )))
    .await;
    let response = mock
        .executor()
        .count_tokens(
            api_key_auth(),
            request(claude_payload()),
            options(Format::CLAUDE),
        )
        .await
        .unwrap();
    let shown = String::from_utf8_lossy(&response.payload);
    assert_eq!(shown, r#"{"input_tokens":42,"echo":"[redacted]"}"#);
}

// Not upstream's: the redaction is of what the client gets; the taps, which
// write to disk with their own redaction, read Claude's answer as it came,
// for a call that isn't streamed and for a stream.
#[tokio::test]
async fn the_taps_see_the_answer_as_it_came() {
    for stream in [false, true] {
        let reply = if stream {
            Reply::sse(&quoting_the_key(SSE))
        } else {
            Reply::json(&quoting_the_key(MESSAGE))
        };
        let mock = Mock::start(reply).await;
        let (observation, raw) = crate::secret_echo::Raw::observe();
        let options = Options {
            observation: Some(observation),
            stream,
            ..options(Format::CLAUDE)
        };
        let shown = if stream {
            let response = mock
                .executor()
                .execute_stream(api_key_auth(), request(claude_payload()), options)
                .await
                .unwrap();
            collect(response).await.iter().map(text).collect()
        } else {
            let response = mock
                .executor()
                .execute(api_key_auth(), request(claude_payload()), options)
                .await
                .unwrap();
            String::from_utf8_lossy(&response.payload).into_owned()
        };
        assert!(!shown.contains(API_KEY), "{shown}");
        assert!(raw.seen().contains(API_KEY), "{}", raw.seen());
    }
}

/// Options for a `format` client whose request, as it came, is `payload`.
fn options_with_original(format: Format, payload: &Value) -> Options {
    Options {
        original_request: Bytes::from(payload.to_string()),
        ..options(format)
    }
}

/// The body an OpenAI client's request reaches Claude with.
async fn sent_for_openai(payload: Value, stream: bool) -> Value {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let options = Options {
        stream,
        ..options_with_original(Format::OPENAI, &payload)
    };
    let executor = mock.executor();
    if stream {
        let response = executor
            .execute_stream(api_key_auth(), request(payload), options)
            .await
            .unwrap();
        assert!(collect(response).await.iter().all(Result::is_ok));
    } else {
        executor
            .execute(api_key_auth(), request(payload), options)
            .await
            .unwrap();
    }
    mock.last().json()
}

fn explicit(mut payload: Value) -> Value {
    payload["prompt_cache_options"] = json!({"mode": "explicit"});
    payload
}

// Upstream's TestClaudeExecutor_PromptCacheOptionsMode_Explicit_SuppressesAutoCacheControl
// and its _Stream_ twin: no breakpoint is placed for a client in the
// explicit cache mode, and `prompt_cache_options` isn't sent on.
#[tokio::test]
async fn explicit_cache_mode_places_no_breakpoints() {
    let payload = explicit(json!({"messages": [
        {"role": "system", "content": "You are a helpful assistant."},
        {"role": "user", "content": "Hello"}
    ]}));
    for stream in [false, true] {
        let sent = sent_for_openai(payload.clone(), stream).await;
        assert_eq!(count_cache_controls(&sent), 0, "{sent}");
        assert!(sent.get("prompt_cache_options").is_none(), "{sent}");
    }
}

// Upstream's TestClaudeExecutor_PromptCacheOptionsMode_Explicit_PreservesClientBreakpoints.
#[tokio::test]
async fn explicit_cache_mode_keeps_the_clients_breakpoints() {
    let sent = sent_for_openai(
        explicit(json!({"messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": [
                {"type": "text", "text": "Cached part", "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "user", "content": "Uncached part"}
        ]})),
        false,
    )
    .await;
    assert_eq!(count_cache_controls(&sent), 1, "{sent}");
    assert!(
        json::exists(&sent, "messages.0.content.0.cache_control"),
        "{sent}"
    );
    assert!(!json::exists(&sent, "system.0.cache_control"), "{sent}");
    assert!(
        !json::exists(&sent, "messages.1.content.0.cache_control"),
        "{sent}"
    );
}

// Upstream's TestClaudeExecutor_PromptCacheOptionsMode_ImplicitOrMissing_PreservesAutoCacheControl.
#[tokio::test]
async fn other_cache_modes_get_breakpoints() {
    let messages = json!([
        {"role": "system", "content": "You are helpful."},
        {"role": "user", "content": "Hello"}
    ]);
    for payload in [
        json!({"messages": messages}),
        json!({"prompt_cache_options": {"mode": "implicit"}, "messages": messages}),
    ] {
        let sent = sent_for_openai(payload, false).await;
        assert!(json::exists(&sent, "system.0.cache_control"), "{sent}");
        assert!(
            json::exists(&sent, "messages.0.content.0.cache_control"),
            "{sent}"
        );
    }
}

fn openai_tool(name: &str) -> Value {
    json!({"type": "function", "function": {"name": name, "description": name, "parameters": {"type": "object"}}})
}

// Upstream's TestClaudeExecutor_PromptCacheOptionsMode_Explicit_ToolsFallbackSuppressed
// and _Explicit_ClientToolBreakpointPreserved.
#[tokio::test]
async fn explicit_cache_mode_leaves_tools_as_the_client_marked_them() {
    let sent = sent_for_openai(
        explicit(json!({
            "tools": [openai_tool("tool1"), openai_tool("tool2")],
            "messages": [{"role": "user", "content": "Hello without system"}]
        })),
        false,
    )
    .await;
    assert_eq!(count_cache_controls(&sent), 0, "{sent}");

    let mut marked = openai_tool("tool1");
    marked["cache_control"] = json!({"type": "ephemeral"});
    let sent = sent_for_openai(
        explicit(json!({
            "tools": [marked, openai_tool("tool2")],
            "messages": [{"role": "user", "content": "Hello"}]
        })),
        false,
    )
    .await;
    assert_eq!(count_cache_controls(&sent), 1, "{sent}");
    assert!(json::exists(&sent, "tools.0.cache_control"), "{sent}");
    assert!(!json::exists(&sent, "tools.1.cache_control"), "{sent}");
    assert!(
        !json::exists(&sent, "messages.0.content.0.cache_control"),
        "{sent}"
    );
}

// Upstream's TestClaudeExecutor_PromptCacheOptionsMode_Explicit_ProbePreservesClient1hTTL,
// and, not upstream's, a one-hour TTL after a five-minute breakpoint (the
// two turns are merged into one), which is kept only in the explicit mode.
#[tokio::test]
async fn explicit_cache_mode_keeps_ttls_as_written() {
    let hour = |text: &str| json!({"type": "text", "text": text, "cache_control": {"type": "ephemeral", "ttl": "1h"}});
    let sent = sent_for_openai(
        explicit(json!({"messages": [{"role": "user", "content": [hour("Hi")]}]})),
        false,
    )
    .await;
    assert_eq!(
        json::str_at(&sent, "messages.0.content.0.cache_control.ttl"),
        "1h",
        "{sent}"
    );

    let short_then_long = json!({"messages": [
        {"role": "user", "content": [
            {"type": "text", "text": "short", "cache_control": {"type": "ephemeral"}}
        ]},
        {"role": "user", "content": [hour("long")]}
    ]});
    let sent = sent_for_openai(explicit(short_then_long.clone()), false).await;
    assert_eq!(
        json::str_at(&sent, "messages.0.content.1.cache_control.ttl"),
        "1h",
        "{sent}"
    );
    let sent = sent_for_openai(short_then_long, false).await;
    assert!(
        !json::exists(&sent, "messages.0.content.1.cache_control.ttl"),
        "{sent}"
    );
}

// Upstream's TestClaudeExecutor_CountTokens_StripsPromptCacheOptions and
// TestClaudeExecutor_CountTokensUpstream_StripsPromptCacheOptions_EvenWithPayloadRule,
// and, not upstream's, a call when a rule writes them.
#[tokio::test]
async fn prompt_cache_options_are_never_sent() {
    async fn count(executor: ClaudeExecutor, payload: Value) {
        let options = options_with_original(Format::OPENAI, &payload);
        let response = executor
            .count_tokens(api_key_auth(), request(payload), options)
            .await
            .unwrap();
        assert!(!response.payload.is_empty());
    }
    let plain = json!({"messages": [{"role": "user", "content": "How many tokens?"}]});

    let mock = Mock::start(Reply::json(r#"{"input_tokens":15}"#)).await;
    count(mock.executor(), explicit(plain.clone())).await;
    let sent = mock.last().json();
    assert!(sent.get("prompt_cache_options").is_none(), "{sent}");

    let rules = Arc::new(
        Config::parse(
            r"
payload:
  override:
    - models:
        - name: claude-sonnet-4-5
      params:
        prompt_cache_options.mode: explicit
        top_k: 7
",
        )
        .unwrap(),
    );
    let mock = Mock::start(Reply::json(r#"{"input_tokens":15}"#)).await;
    count(
        mock.executor().with_config(Arc::clone(&rules)),
        plain.clone(),
    )
    .await;
    let sent = mock.last().json();
    assert!(sent.get("prompt_cache_options").is_none(), "{sent}");
    // The rule did apply.
    assert_eq!(sent["top_k"], 7, "{sent}");

    let mock = Mock::start(Reply::sse(SSE)).await;
    mock.executor()
        .with_config(rules)
        .execute(
            api_key_auth(),
            request(plain.clone()),
            options_with_original(Format::OPENAI, &plain),
        )
        .await
        .unwrap();
    let sent = mock.last().json();
    assert!(sent.get("prompt_cache_options").is_none(), "{sent}");
    // The rule did apply.
    assert_eq!(sent["top_k"], 7, "{sent}");
}
