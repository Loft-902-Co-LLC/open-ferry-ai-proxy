//! The executor against a mock xAI server on 127.0.0.1, ported from
//! upstream's `xai_executor_test.go` where it tests what is ported. The
//! mock records each request's path, headers and body, and replies with
//! recorded-style SSE. Upstream's tests sign in with OAuth; these use a
//! dummy API key, the only credential served.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::http::Uri;
use bytes::Bytes;
use http::HeaderMap;
use serde_json::{Value, json};

use super::*;
use crate::codex::client::USER_AGENT;
use crate::codex::request::CONTROL_CHARACTER;
use crate::json::{exists, get};
use crate::xai::request::MEDIA_REFUSED;

mod replay;
mod secrets;
mod tools;

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
}

impl Reply {
    fn sse(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.to_owned(),
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
}

/// A mock server bound to an ephemeral port on 127.0.0.1.
struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    async fn start(reply: Reply) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let reply = reply.clone();
            let recorder = Arc::clone(&recorder);
            async move {
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Seen {
                        path: uri.path().to_owned(),
                        headers,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });
                axum::response::Response::builder()
                    .status(reply.status)
                    .header("content-type", reply.content_type)
                    .body(axum::body::Body::from(reply.body))
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
fn executor() -> XaiExecutor {
    XaiExecutor::new("direct")
}

/// [`executor`] following the config `yaml`.
fn executor_with(yaml: &str) -> XaiExecutor {
    executor().with_config(Arc::new(Config::parse(yaml).expect("config parses")))
}

/// An executor that injects X search, as `xai.inject-x-search` asks.
fn injecting_x_search() -> XaiExecutor {
    executor_with("xai:\n  inject-x-search: true\n")
}

/// The dummy API key the tests send.
const API_KEY: &str = "xai-test-key";

/// An API key credential for `base_url`.
fn api_key_auth(base_url: &str) -> Arc<Auth> {
    let mut auth = Auth {
        provider: "xai".into(),
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), API_KEY.into());
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

/// `options` answering in Codex's own format, as upstream's tests that set
/// `ResponseFormat: FormatCodex` do.
fn codex_response(mut options: Options) -> Options {
    options.response_format = Format::CODEX;
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

/// The text of a stream that must end without an error.
async fn streamed(result: Result<StreamResponse, ExecError>) -> String {
    let (text, error) = collect(result.expect("the stream starts")).await;
    assert!(error.is_none(), "{error:?}\n{text}");
    text
}

/// The error of a stream that didn't start.
fn refused(result: Result<StreamResponse, ExecError>) -> ExecError {
    match result {
        Ok(_) => panic!("the stream started"),
        Err(error) => error,
    }
}

/// The JSON of each `data:` line in SSE text.
fn sse_events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect()
}

/// The last event of `event_type` in SSE text.
fn last_event(text: &str, event_type: &str) -> Value {
    sse_events(text)
        .into_iter()
        .rfind(|event| event["type"] == event_type)
        .unwrap_or_else(|| panic!("no {event_type} in {text}"))
}

fn payload_json(response: &Response) -> Value {
    serde_json::from_slice(&response.payload).unwrap()
}

/// Grok CLI identity headers, never sent.
const GROK_CLI_HEADERS: [&str; 5] = [
    "x-xai-token-auth",
    "x-grok-client-version",
    "x-grok-client-identifier",
    "x-grok-client-anything",
    "x-authenticateresponse",
];

/// Asserts the request names this proxy alone: its own user agent, and none
/// of the Grok CLI's identity headers.
fn assert_own_identity(seen: &Seen) {
    assert_eq!(seen.header("user-agent"), Some(USER_AGENT));
    assert!(USER_AGENT.starts_with("open-ferry/"));
    for name in GROK_CLI_HEADERS {
        assert!(seen.header(name).is_none(), "{name} was sent");
    }
}

const COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":0,\"status\":\"completed\",\"model\":\"grok-4.3\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";

/// A credential that tries to send every Grok CLI identity header, a Grok
/// CLI user agent and a conversation of its own.
fn impersonating_auth(base_url: &str) -> Arc<Auth> {
    let mut auth = (*api_key_auth(base_url)).clone();
    for (name, value) in [
        ("header:X-XAI-Token-Auth", "xai-grok-cli"),
        ("header:x-grok-client-version", "1.0.44"),
        ("header:x-grok-client-identifier", "grok-shell"),
        ("header:x-grok-client-anything", "1"),
        ("header:x-authenticateresponse", "authenticate-response"),
        ("header:User-Agent", "xai-grok-workspace/1.0"),
        ("header:x-grok-conv-id", "made-up-conversation"),
    ] {
        auth.attributes.insert(name.into(), value.into());
    }
    Arc::new(auth)
}

// Not upstream's: the executor's name, and a key that refresh leaves alone.
#[tokio::test]
async fn identifies_and_returns_the_key_unchanged_on_refresh() {
    let executor = executor();
    assert_eq!(executor.id(), "xai");
    assert_eq!(executor.refresh_lead(), None);
    let auth = api_key_auth("http://127.0.0.1:9");
    let refreshed = executor.refresh(Arc::clone(&auth)).await.unwrap();
    assert_eq!(refreshed.attributes, auth.attributes);
    assert_eq!(refreshed.metadata, auth.metadata);
}

// Not upstream's (upstream sends the Grok CLI's identity on its chat
// proxy): the request names this proxy alone, whatever the credential's
// custom headers say, and without a client prompt_cache_key there is no
// conversation at all.
#[tokio::test]
async fn sends_no_grok_cli_identity_and_no_made_up_conversation() {
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    let payload = r#"{"model":"grok-4.3","input":"hello"}"#;
    executor()
        .execute(
            impersonating_auth(&mock.url),
            request("grok-4.3", payload),
            options("openai-response"),
        )
        .await
        .unwrap();
    streamed(
        executor()
            .execute_stream(
                impersonating_auth(&mock.url),
                request("grok-4.3", payload),
                stream_options("openai-response"),
            )
            .await,
    )
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    for seen in requests {
        assert_eq!(seen.path, "/responses");
        assert_eq!(seen.header("authorization"), Some("Bearer xai-test-key"));
        assert_eq!(seen.header("accept"), Some("text/event-stream"));
        assert_eq!(seen.header("content-type"), Some("application/json"));
        assert_own_identity(&seen);
        assert!(seen.header("x-grok-conv-id").is_none(), "{seen:?}");
        let body = seen.json();
        assert!(!exists(&body, "prompt_cache_key"), "{body}");
        assert_eq!(body["model"], "grok-4.3");
        assert_eq!(body["stream"], true);
    }
}

// Not upstream's: x-grok-conv-id and prompt_cache_key are the client's own
// prompt_cache_key, trimmed, on every kind of call, and a credential's
// header can't change them.
#[tokio::test]
async fn the_conversation_is_the_clients_prompt_cache_key() {
    let payload =
        r#"{"model":"grok-4.3","input":"hello","prompt_cache_key":"  client-session-1 "}"#;
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    executor()
        .execute(
            impersonating_auth(&mock.url),
            request("grok-4.3", payload),
            options("openai-response"),
        )
        .await
        .unwrap();
    streamed(
        executor()
            .execute_stream(
                impersonating_auth(&mock.url),
                request("grok-4.3", payload),
                stream_options("openai-response"),
            )
            .await,
    )
    .await;
    let compact = Mock::start(Reply::json(r#"{"id":"resp_1","output":[]}"#)).await;
    executor()
        .execute(
            impersonating_auth(&compact.url),
            request("grok-4.3", payload),
            compact_options("openai-response"),
        )
        .await
        .unwrap();
    let mut requests = mock.requests();
    requests.extend(compact.requests());
    assert_eq!(requests.len(), 3);
    for seen in requests {
        assert_eq!(seen.header("x-grok-conv-id"), Some("client-session-1"));
        assert_eq!(seen.json()["prompt_cache_key"], "client-session-1");
        assert_own_identity(&seen);
    }
}

// Not upstream's (upstream's image and video handlers aren't ported): an
// image or video request is refused with a 400 before anything is sent.
#[tokio::test]
async fn image_and_video_requests_are_refused_unsent() {
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    let payload = r#"{"model":"grok-imagine-image","prompt":"a cat"}"#;
    for format in ["openai-image", "openai-video"] {
        for alt in ["", COMPACT_ALT] {
            let options = Options {
                alt: alt.into(),
                ..options(format)
            };
            let error = executor()
                .execute(
                    api_key_auth(&mock.url),
                    request("grok-imagine-image", payload),
                    options.clone(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.http_status(), 400, "{error:?}");
            assert_eq!(error.message, MEDIA_REFUSED);
            let error = refused(
                executor()
                    .execute_stream(
                        api_key_auth(&mock.url),
                        request("grok-imagine-image", payload),
                        Options {
                            stream: true,
                            ..options
                        },
                    )
                    .await,
            );
            assert_eq!(error.http_status(), 400, "{error:?}");
            assert_eq!(error.message, MEDIA_REFUSED);
        }
    }
    assert!(mock.requests().is_empty());
}

// Not upstream's: an upstream or proxy that echoes what it was sent in its
// error gets none of the request's secrets back to the client.
#[tokio::test]
async fn errors_hide_every_secret_sent() {
    let payload = r#"{"model":"grok-4.3","input":"hello"}"#;
    for case in crate::secret_echo::cases(|base_url| (*api_key_auth(base_url)).clone()).await {
        for options in [
            options("openai-response"),
            stream_options("openai-response"),
            compact_options("openai-response"),
        ] {
            let options = Options {
                headers: case.headers.clone(),
                ..options
            };
            let auth = Arc::clone(&case.auth);
            let error = if options.stream {
                executor()
                    .execute_stream(auth, request("grok-4.3", payload), options)
                    .await
                    .err()
            } else {
                executor()
                    .execute(auth, request("grok-4.3", payload), options)
                    .await
                    .err()
            };
            let error = error.expect("the call went through");
            assert!(!error.message.contains(API_KEY), "{}", error.message);
            case.check(&error);
        }
    }
}

// Not upstream's, the executor's half of upstream's xaiStatusErr tests: an
// expired key's 403 is a 401 on every kind of call, and the free tier's
// exhausted usage waits 24 hours.
#[tokio::test]
async fn maps_bad_credentials_and_free_usage_errors() {
    let bad = r#"{"code":"unauthenticated:bad-credentials","error":"The OAuth2 access token could not be validated."}"#;
    let mock = Mock::start(Reply::error(403, bad)).await;
    let payload = r#"{"model":"grok-4.3","input":"hello"}"#;
    for options in [
        options("openai-response"),
        compact_options("openai-response"),
    ] {
        let error = executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-4.3", payload),
                options,
            )
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), 401);
        assert_eq!(error.message, bad);
    }
    let error = refused(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request("grok-4.3", payload),
                stream_options("openai-response"),
            )
            .await,
    );
    assert_eq!(error.http_status(), 401);

    let free = r#"{"code":"subscription:free-usage-exhausted","error":"used up"}"#;
    let mock = Mock::start(Reply::error(429, free)).await;
    let error = executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.3", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.http_status(), 429);
    assert_eq!(
        error.retry_after,
        Some(errors::FREE_USAGE_EXHAUSTED_COOLDOWN)
    );
}

// Not upstream's: a stream that ends before its terminal event fails a
// non-streaming call with upstream's 408, and a failure event is no
// terminal event, as upstream reads it.
#[tokio::test]
async fn missing_completion_is_a_timeout() {
    for body in [
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"status\":\"failed\"}}\n\n",
        "",
    ] {
        let mock = Mock::start(Reply::sse(body)).await;
        let error = executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-4.3", r#"{"model":"grok-4.3","input":"hello"}"#),
                options("openai-response"),
            )
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), 408, "{body}");
        assert_eq!(error.message, DISCONNECTED_MESSAGE);
    }
}

// Not upstream's: Go's url.Parse refuses a control character before
// anything is sent.
#[tokio::test]
async fn a_url_with_a_control_character_is_refused_unsent() {
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    let auth = api_key_auth(&format!("{}/v{}1", mock.url, char::from(0x7f_u8)));
    let payload = r#"{"model":"grok-4.3","input":"hello"}"#;
    let error = executor()
        .execute(
            Arc::clone(&auth),
            request("grok-4.3", payload),
            options("openai-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, CONTROL_CHARACTER);
    let error = refused(
        executor()
            .execute_stream(
                auth,
                request("grok-4.3", payload),
                stream_options("openai-response"),
            )
            .await,
    );
    assert_eq!(error.message, CONTROL_CHARACTER);
    assert!(mock.requests().is_empty());
}

// TestXAIExecutorExecuteFiltersInternalXSearchCalls.
#[tokio::test]
async fn execute_filters_internal_x_search_calls() {
    let mock = Mock::start(Reply::sse(concat!(
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"ctc_1\",\"type\":\"custom_tool_call\",\"call_id\":\"xs_call-1\",\"name\":\"x_user_search\",\"input\":\"{}\",\"status\":\"completed\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}],\"status\":\"completed\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"id\":\"ctc_1\",\"type\":\"custom_tool_call\",\"call_id\":\"xs_call-1\",\"name\":\"x_user_search\",\"input\":\"{}\"},{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
    )))
    .await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "grok-4.5",
                r#"{"model":"grok-4.5","input":"search X","tools":[{"type":"x_search"}]}"#,
            ),
            options("openai-response"),
        )
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&response.payload);
    assert!(
        !text.contains("x_user_search") && !text.contains("custom_tool_call"),
        "{text}"
    );
    let payload = payload_json(&response);
    assert_eq!(payload["output"].as_array().map(Vec::len), Some(1));
    assert_eq!(payload["output"][0]["content"][0]["text"], "answer");
}

// TestXAIExecutorExecuteStreamFiltersInternalXSearchCalls.
#[tokio::test]
async fn stream_filters_internal_x_search_calls() {
    let names = [
        "x_user_search",
        "x_semantic_search",
        "x_keyword_search",
        "x_thread_fetch",
    ];
    let mut completed = json!({"type": "response.completed", "response": {"id": "resp_1", "object": "response", "status": "completed", "output": [], "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}});
    let mut output = Vec::new();
    let mut body = String::new();
    for (index, name) in names.iter().enumerate() {
        let item_id = format!("ctc_{index}");
        let call_id = format!("xs_call-{index}");
        let added = json!({"type": "response.output_item.added", "output_index": index, "item": {"id": item_id, "type": "custom_tool_call", "call_id": call_id, "name": name, "input": "", "status": "in_progress"}});
        let input_done = json!({"type": "response.custom_tool_call_input.done", "output_index": index, "item_id": item_id, "input": "{}"});
        let item = json!({"id": item_id, "type": "custom_tool_call", "call_id": call_id, "name": name, "input": "{}", "status": "completed"});
        let done =
            json!({"type": "response.output_item.done", "output_index": index, "item": item});
        body.push_str(&format!(
            "event: response.output_item.added\ndata: {added}\n\nevent: response.custom_tool_call_input.done\ndata: {input_done}\n\nevent: response.output_item.done\ndata: {done}\n\n"
        ));
        output.push(item);
    }
    let message_index = names.len();
    let message = json!({"id": "msg_1", "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}], "status": "completed"});
    let added = json!({"type": "response.output_item.added", "output_index": message_index, "item": {"id": "msg_1", "type": "message", "role": "assistant", "content": [], "status": "in_progress"}});
    let delta = json!({"type": "response.output_text.delta", "output_index": message_index, "item_id": "msg_1", "content_index": 0, "delta": "answer"});
    let done = json!({"type": "response.output_item.done", "output_index": message_index, "item": message});
    output.push(message);
    completed["response"]["output"] = Value::Array(output);
    body.push_str(&format!(
        "event: response.output_item.added\ndata: {added}\n\nevent: response.output_text.delta\ndata: {delta}\n\nevent: response.output_item.done\ndata: {done}\n\nevent: response.completed\ndata: {completed}\n\n"
    ));
    let mock = Mock::start(Reply::sse(&body)).await;

    let text = streamed(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request(
                    "grok-4.5",
                    r#"{"model":"grok-4.5","input":"search X","tools":[{"type":"x_search"}]}"#,
                ),
                stream_options("openai-response"),
            )
            .await,
    )
    .await;
    for name in names {
        assert!(!text.contains(name), "{name} in {text}");
    }
    assert!(!text.contains("response.custom_tool_call_input"), "{text}");
    let message_events: Vec<Value> = sse_events(&text)
        .into_iter()
        .filter(|event| event["item"]["id"] == "msg_1" || event["item_id"] == "msg_1")
        .collect();
    assert!(!message_events.is_empty(), "no message events in {text}");
    for event in message_events {
        assert_eq!(event["output_index"], 0, "{event}");
    }
    let completed = last_event(&text, "response.completed");
    assert_eq!(
        completed["response"]["output"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(completed["response"]["output"][0]["type"], "message");
}

const INCOMPLETE: &str = "{\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[],\"usage\":{\"input_tokens\":8,\"output_tokens\":1,\"total_tokens\":9}}}";

const REASONING_DONE: &str = "{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[]}}";

// TestXAIExecutorExecuteAcceptsResponseIncomplete.
#[tokio::test]
async fn execute_accepts_response_incomplete() {
    let mock = Mock::start(Reply::sse(&format!(
        "data: {REASONING_DONE}\n\ndata: {INCOMPLETE}\n\n"
    )))
    .await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "grok-4.5",
                r#"{"model":"grok-4.5","input":"hi","max_output_tokens":1}"#,
            ),
            options("openai-response"),
        )
        .await
        .unwrap();
    let payload = payload_json(&response);
    assert_eq!(payload["status"], "incomplete");
    assert_eq!(payload["incomplete_details"]["reason"], "max_output_tokens");
    assert_eq!(payload["output"].as_array().map(Vec::len), Some(1));
}

// TestXAIExecutorExecuteStreamAcceptsResponseIncomplete.
#[tokio::test]
async fn stream_accepts_response_incomplete() {
    let mock = Mock::start(Reply::sse(&format!(
        "event: response.output_item.done\ndata: {REASONING_DONE}\n\nevent: response.incomplete\ndata: {INCOMPLETE}\n\n"
    )))
    .await;
    let text = streamed(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request(
                    "grok-4.5",
                    r#"{"model":"grok-4.5","input":"hi","max_output_tokens":1}"#,
                ),
                stream_options("openai-response"),
            )
            .await,
    )
    .await;
    let incomplete = last_event(&text, "response.incomplete");
    assert_eq!(
        incomplete["response"]["output"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(incomplete["response"]["usage"]["total_tokens"], 9);
}

/// `testValidGrokEncryptedContent`: 256 bytes that pass for xAI's.
fn valid_grok_encrypted_content() -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let mut buffer = Vec::with_capacity(256);
    for index in 0_u32..8 {
        let [low, middle, high, _] = index.to_le_bytes();
        buffer.extend_from_slice(&Sha256::digest([low, middle, high]));
    }
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(buffer)
}

// TestXAIExecutorCompactUsesCompactEndpoint.
#[tokio::test]
async fn compact_uses_compact_endpoint() {
    let answer = r#"{"id":"resp_1","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"opaque-out"}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;
    let mock = Mock::start(Reply::json(answer)).await;
    let executor = executor_with(
        "payload:\n  override:\n    - models:\n        - name: grok-4.3\n      params:\n        top_k: 10\n",
    );
    let encrypted = valid_grok_encrypted_content();
    let payload = json!({"model": "grok-4.3", "stream": true, "max_output_tokens": 64, "temperature": 0.3, "top_p": 0.8, "stop": ["END"], "input": [{"type": "compaction", "encrypted_content": encrypted}, {"role": "user", "content": "hello"}]});
    let response = executor
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.3", &payload.to_string()),
            compact_options("openai-response"),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.path, "/responses/compact");
    assert_eq!(seen.header("authorization"), Some("Bearer xai-test-key"));
    assert_eq!(seen.header("accept"), Some("application/json"));
    assert_own_identity(&seen);
    let body = seen.json();
    for field in [
        "stream",
        "max_output_tokens",
        "temperature",
        "top_p",
        "top_k",
        "stop",
    ] {
        assert!(!exists(&body, field), "{field} in {body}");
    }
    assert_eq!(body["input"][0]["encrypted_content"], encrypted);
    assert_eq!(body["input"][1]["role"], "user");
    assert_eq!(
        payload_json(&response)["output"][0]["encrypted_content"],
        "opaque-out"
    );
}

// TestXAIExecutorCompactDropsOrphanedImageGenerationToolChoice.
#[tokio::test]
async fn compact_drops_orphaned_image_generation_tool_choice() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"resp_1","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"opaque-out"}]}"#,
    ))
    .await;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request(
                "grok-4.6",
                r#"{"model":"grok-4.6","input":"compact this","tools":[{"type":"image_generation","action":"generate"}],"tool_choice":{"type":"image_generation"}}"#,
            ),
            compact_options("openai-response"),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    for field in ["tools", "tool_choice", "parallel_tool_calls"] {
        assert!(!exists(&body, field), "{field} in {body}");
    }
}

// Not upstream's: a compact call keeps the client's previous_response_id,
// which every other call drops, and sends no compaction_trigger item.
#[tokio::test]
async fn compact_keeps_previous_response_id() {
    let mock = Mock::start(Reply::json(r#"{"id":"resp_2","output":[]}"#)).await;
    let payload = r#"{"model":"grok-4.3","previous_response_id":" resp_1 ","input":[{"role":"user","content":"hi"},{"type":"compaction_trigger"}]}"#;
    executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.3", payload),
            compact_options("openai-response"),
        )
        .await
        .unwrap();
    let body = mock.last().json();
    assert_eq!(body["previous_response_id"], "resp_1");
    assert_eq!(body["input"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["input"][0]["role"], "user");

    let chat = Mock::start(Reply::sse(COMPLETED)).await;
    executor()
        .execute(
            api_key_auth(&chat.url),
            request(
                "grok-4.3",
                r#"{"model":"grok-4.3","previous_response_id":"resp_1","input":"hi"}"#,
            ),
            options("openai-response"),
        )
        .await
        .unwrap();
    assert!(!exists(&chat.last().json(), "previous_response_id"));
}

// Not upstream's: a numeric previous_response_id is sent as the string
// gjson's `String` makes of the number the client wrote, an integer as
// written, `-0` included, and any other number in plain notation.
#[tokio::test]
async fn compact_sends_a_numeric_previous_response_id_as_written() {
    for (written, sent) in [("-0", "-0"), ("1E20", "100000000000000000000")] {
        let mock = Mock::start(Reply::json(r#"{"id":"resp_2","output":[]}"#)).await;
        let payload =
            format!(r#"{{"model":"grok-4.3","previous_response_id":{written},"input":"hi"}}"#);
        executor()
            .execute(
                api_key_auth(&mock.url),
                request("grok-4.3", &payload),
                compact_options("openai-response"),
            )
            .await
            .unwrap_or_else(|error| panic!("{written}: {error:?}"));
        let body = mock.last().body;
        assert!(
            body.contains(&format!(r#""previous_response_id":"{sent}""#)),
            "{written}: {body}"
        );
    }
}

// Not upstream's: a compact call can't stream.
#[tokio::test]
async fn compact_cannot_stream() {
    let mock = Mock::start(Reply::json("{}")).await;
    let error = refused(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request("grok-4.3", r#"{"model":"grok-4.3","input":"hi"}"#),
                Options {
                    stream: true,
                    ..compact_options("openai-response")
                },
            )
            .await,
    );
    assert_eq!(error.http_status(), 400);
    assert_eq!(
        error.message,
        "streaming not supported for /responses/compact"
    );
    assert!(mock.requests().is_empty());
}

// TestXAIExecutorExecuteStreamCompactionTriggerUsesCompactEndpoint.
#[tokio::test]
async fn stream_compaction_trigger_uses_compact_endpoint() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"resp_xai_1","model":"grok-4.3","output":[{"type":"compaction","encrypted_content":"opaque"}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#,
    ))
    .await;
    let result = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request(
                "grok-4.3",
                r#"{"model":"grok-4.3","stream":true,"input":[{"role":"user","content":"hello"},{"type":"compaction_trigger"}]}"#,
            ),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    assert_eq!(result.headers["content-type"], "text/event-stream");
    let (output, error) = collect(result).await;
    assert!(error.is_none(), "{error:?}");
    let seen = mock.last();
    assert_eq!(seen.path, "/responses/compact");
    assert_eq!(seen.header("accept"), Some("application/json"));
    assert!(!compact::input_has_item_type(
        seen.body.as_bytes(),
        compact::COMPACTION_TRIGGER
    ));
    assert!(!exists(&seen.json(), "stream"));
    for event in [
        "response.created",
        "response.in_progress",
        "response.output_item.added",
        "response.output_item.done",
        "response.completed",
    ] {
        assert!(
            output.contains(&format!("event: {event}\n")),
            "{event} in {output}"
        );
    }
    assert!(
        output.matches(r#""model":"grok-4.3""#).count() >= 2,
        "{output}"
    );
    assert!(
        output.contains(r#""type":"compaction""#)
            && output.contains(r#""encrypted_content":"opaque""#),
        "{output}"
    );
    assert!(
        output.contains(r#""output_tokens_details":{"reasoning_tokens":0}"#)
            && output.contains(r#""input_tokens_details":{"cached_tokens":0}"#),
        "{output}"
    );
}

// TestXAIExecutorExecuteStreamFiltersToolSearchTool.
#[tokio::test]
async fn stream_filters_tool_search_tool() {
    let mock = Mock::start(Reply::sse(
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":0,\"status\":\"completed\",\"model\":\"grok-4.3\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]},{\"type\":\"function_call\",\"id\":\"patch_item\",\"call_id\":\"cp\",\"name\":\"apply_patch\",\"arguments\":\"{\\\"input\\\":\\\"p\\\"}\"}]}}\n\n",
    ))
    .await;
    let payload = r#"{"model":"grok-4.3","input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"test"}],"content":null,"encrypted_content":null},{"type":"reasoning","summary":[{"type":"summary_text","text":"second"}]},{"role":"user","content":"hello"},{"type":"reasoning","summary":[{"type":"summary_text","text":"separate"}]}],"tools":[{"type":"tool_search"},{"type":"image_generation"},{"type":"custom","name":"apply_patch"},{"type":"custom","name":"custom_lookup"},{"type":"function","name":"lookup"},{"type":"web_search","external_web_access":true,"search_content_types":["text","image"]},{"type":"namespace","name":"codex_app","description":"Tools in the codex_app namespace.","tools":[{"type":"function","name":"automation_update"},{"type":"custom","name":"namespace_custom"},{"type":"tool_search"}]}]}"#;
    let output = streamed(
        injecting_x_search()
            .execute_stream(
                api_key_auth(&mock.url),
                request("grok-4.3", payload),
                stream_options("openai-response"),
            )
            .await,
    )
    .await;
    assert!(
        output.contains(r#""type":"custom_tool_call""#)
            && output.contains(r#""input":"p""#)
            && output.contains("response.custom_tool_call_input.done"),
        "{output}"
    );
    let body = mock.last().json();
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 7, "{body}");
    let input = &body["input"];
    assert!(input[0].get("content").is_none(), "{input}");
    assert!(input[0].get("encrypted_content").is_none(), "{input}");
    assert_eq!(input[0]["summary"][0]["text"], "test");
    assert_eq!(input[0]["summary"][1]["text"], "second");
    assert_eq!(input[1]["role"], "user");
    assert_eq!(input[2]["summary"][0]["text"], "separate");
    let mut names = Vec::new();
    let mut x_search = false;
    for tool in tools {
        let tool_type = tool["type"].as_str().unwrap_or_default();
        assert!(
            ["function", "web_search", "x_search"].contains(&tool_type),
            "{tool}"
        );
        if tool_type == "function" {
            assert!(tool.get("parameters").is_some(), "{tool}");
        }
        if tool["name"] == "apply_patch" {
            assert_eq!(tool["parameters"]["required"][0], "input");
            assert_ne!(tool["parameters"]["additionalProperties"], true);
            assert_eq!(tool["parameters"]["properties"]["input"]["type"], "string");
        }
        if tool_type == "web_search" {
            assert!(tool.get("external_web_access").is_none(), "{tool}");
            assert_eq!(tool["search_content_types"][1], "image");
        }
        x_search |= tool_type == "x_search";
        names.push(tool["name"].as_str().unwrap_or_default().to_owned());
    }
    assert!(
        names
            .iter()
            .any(|name| name == "codex_app__automation_update")
    );
    assert!(
        names
            .iter()
            .any(|name| name == "codex_app__namespace_custom")
    );
    assert!(x_search);
}

// TestXAIExecutorExecuteStreamNormalizesReasoningTextEvents.
#[tokio::test]
async fn stream_normalizes_reasoning_text_events() {
    let mock = Mock::start(Reply::sse(concat!(
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"status\":\"in_progress\",\"summary\":[]}}\n\n",
        "event: response.content_part.added\n",
        "data: {\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"rs_1\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"reasoning_text\",\"text\":\"\"}}\n\n",
        "event: response.reasoning_text.delta\n",
        "data: {\"type\":\"response.reasoning_text.delta\",\"sequence_number\":3,\"item_id\":\"rs_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"thinking\"}\n\n",
        "event: response.reasoning_text.done\n",
        "data: {\"type\":\"response.reasoning_text.done\",\"sequence_number\":4,\"item_id\":\"rs_1\",\"output_index\":0,\"content_index\":0,\"text\":\"thinking\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"sequence_number\":5,\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"status\":\"completed\",\"summary\":[],\"content\":[{\"type\":\"reasoning_text\",\"text\":\"thinking\"}]}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":6,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":0,\"status\":\"completed\",\"model\":\"grok-4.3\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
    )))
    .await;
    let output = streamed(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request("grok-4.3", r#"{"model":"grok-4.3","input":"hello"}"#),
                codex_response(stream_options("openai-response")),
            )
            .await,
    )
    .await;
    assert!(!output.contains("reasoning_text"), "{output}");
    for want in [
        "event: response.reasoning_summary_part.added",
        "event: response.reasoning_summary_text.delta",
        "event: response.reasoning_summary_text.done",
        "event: response.reasoning_summary_part.done",
        r#""type":"response.reasoning_summary_part.added""#,
        r#""type":"response.reasoning_summary_text.delta""#,
        r#""type":"response.reasoning_summary_text.done""#,
        r#""type":"response.reasoning_summary_part.done""#,
        r#""part":{"type":"summary_text","text":"thinking"}"#,
        r#""summary_index":0"#,
        r#""summary":[{"type":"summary_text","text":"thinking"}]"#,
    ] {
        assert!(output.contains(want), "{want} not in {output}");
    }
    let text_done = output.find(r#""type":"response.reasoning_summary_text.done""#);
    let part_done = output.find(r#""type":"response.reasoning_summary_part.done""#);
    assert!(text_done < part_done, "{output}");
}

// TestXAIExecutorExecuteNormalizesReasoningOutputForNonStreamTranslation.
#[tokio::test]
async fn execute_normalizes_reasoning_output_for_non_stream_translation() {
    let mock = Mock::start(Reply::sse(concat!(
        "data: {\"type\":\"response.output_item.done\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"status\":\"completed\",\"summary\":[],\"content\":[{\"type\":\"reasoning_text\",\"text\":\"thinking\"}]}}\n\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":0,\"status\":\"completed\",\"model\":\"grok-4.3\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
    )))
    .await;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.3", r#"{"model":"grok-4.3","input":"hello"}"#),
            codex_response(options("openai-response")),
        )
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&response.payload);
    assert!(!text.contains("reasoning_text"), "{text}");
    let payload = payload_json(&response);
    let item = &payload["response"]["output"][0];
    assert_eq!(item["summary"][0]["type"], "summary_text");
    assert_eq!(item["summary"][0]["text"], "thinking");
    assert!(get(item, "content").is_none(), "{item}");
}

// Not upstream's: a Chat Completions client gets xAI's answer in its own
// format.
#[tokio::test]
async fn translates_for_a_chat_completions_client() {
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    let payload = r#"{"model":"grok-4.3","messages":[{"role":"user","content":"hi"}]}"#;
    let response = executor()
        .execute(
            api_key_auth(&mock.url),
            request("grok-4.3", payload),
            options("openai"),
        )
        .await
        .unwrap();
    let payload_out = payload_json(&response);
    assert_eq!(payload_out["object"], "chat.completion");
    assert_eq!(payload_out["choices"][0]["message"]["content"], "ok");
    assert_eq!(mock.last().json()["input"][0]["role"], "user");

    let text = streamed(
        executor()
            .execute_stream(
                api_key_auth(&mock.url),
                request("grok-4.3", payload),
                stream_options("openai"),
            )
            .await,
    )
    .await;
    assert!(text.contains("chat.completion.chunk"), "{text}");
}

// Not upstream's: a token count is estimated locally, without a call, and
// answers in the client's format.
#[tokio::test]
async fn count_tokens_answers_in_the_clients_format_unsent() {
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    let payload = r#"{"model":"grok-4.3","instructions":"be brief","input":"hello there"}"#;
    let response = executor()
        .count_tokens(
            api_key_auth(&mock.url),
            request("grok-4.3", payload),
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

    let claude = r#"{"model":"grok-4.3","messages":[{"role":"user","content":"hello there"}]}"#;
    let response = executor()
        .count_tokens(
            api_key_auth(&mock.url),
            request("grok-4.3", claude),
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
    assert!(mock.requests().is_empty());
}
