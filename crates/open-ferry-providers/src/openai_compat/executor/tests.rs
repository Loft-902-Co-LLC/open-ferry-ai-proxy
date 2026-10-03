//! The executor against a mock provider on 127.0.0.1, ported from upstream's
//! `openai_compat_executor_compact_test.go`,
//! `openai_compat_executor_max_tokens_test.go`,
//! `openai_compat_executor_retry_test.go` (`TestOpenAICompatExecutorPropagatesRetryAfter`)
//! and `openai_compat_executor_tool_results_test.go`, with checks of the
//! request headers, credentials, refresh, token counts and entry lookup.
//!
//! Dropped:
//! - `openai_compat_executor_images_test.go`, the image tests in the compact
//!   test file (`ImagesGenerationsPassthrough`, `ImagesGenerationsStreamsUpstream`,
//!   `ImagesEditsMultipartRewritesModel`,
//!   `RewriteOpenAICompatImagesMultipartPayloadPreservesStreamAndFileContentType`)
//!   and `openai_compat_executor_video_test.go`: the image endpoints aren't
//!   ported, and the video test checks how the Claude and OpenAI request
//!   translators pass video, which isn't this executor's.
//! - `openai_compat_home_options_test.go`: the Home service isn't ported.
//! - `openai_compat_executor_reasoning_test.go`: the `is-compat` flag isn't
//!   passed to translators.
//! - `PayloadOverrideWinsOverThinkingSuffix`: payload rules and thinking
//!   suffixes aren't applied.
//! - `PromptCacheKeyIsModelAndProtocolScoped`: it checks derived keys.
//!
//! Changed:
//! - No `prompt_cache_key` is derived, by policy. In `ApplyPromptCacheKey`
//!   the derived cases expect none. `UsesConfigIndex`,
//!   `IgnoresConfigIndexForNonConfigAuth`, `PromptCacheKeyExecute`,
//!   `PromptCacheKeyExecuteStream` and `PromptCacheKeyStreamCompactSkipped`
//!   give the client's own key (in the original request where the payload
//!   would carry it through anyway) instead of a session to derive one from.
//! - An extra tool result test sends the same tool result as a Chat
//!   Completions request.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Uri;
use futures_util::StreamExt as _;
use http::{HeaderMap, HeaderValue};
use open_ferry_core::config::OpenAiCompatibilityModel;
use serde_json::{Value, json};

use super::*;
use crate::json::{exists, get};

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
    headers: Vec<(&'static str, &'static str)>,
    body: String,
}

impl Reply {
    fn sse(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "text/event-stream")],
            body: body.to_owned(),
        }
    }

    fn json(body: &str) -> Self {
        Self {
            headers: vec![("content-type", "application/json")],
            ..Self::sse(body)
        }
    }

    fn error(status: u16, body: &str) -> Self {
        Self {
            status,
            ..Self::json(body)
        }
    }

    fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }
}

/// A mock provider bound to an ephemeral port on 127.0.0.1.
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
                let parts = vec![Ok::<_, io::Error>(Bytes::from(reply.body))];
                let mut response = axum::response::Response::builder().status(reply.status);
                for (name, value) in reply.headers {
                    response = response.header(name, value);
                }
                response
                    .body(Body::from_stream(futures_util::stream::iter(parts)))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, seen }
    }

    /// The provider's base URL, as upstream's tests configure it.
    fn base_url(&self) -> String {
        format!("{}/v1", self.url)
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

/// An executor for `entries` that doesn't use the environment's proxy.
fn executor(entries: Vec<OpenAiCompatibility>) -> OpenAiCompatExecutor {
    let mut config = Config::default();
    config.proxy_url = "direct".into();
    config.openai_compatibility = entries;
    OpenAiCompatExecutor::new("openai-compatibility", Arc::new(config))
}

fn entry(name: &str, support_prompt_cache_key: bool) -> OpenAiCompatibility {
    OpenAiCompatibility {
        name: name.into(),
        support_prompt_cache_key,
        ..OpenAiCompatibility::default()
    }
}

/// A credential with only a base URL and an API key.
fn plain_auth(base_url: &str) -> Arc<Auth> {
    let mut auth = Auth::default();
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), "test".into());
    Arc::new(auth)
}

/// A credential of the entry `name`, as upstream's tests make them.
fn compat_auth(base_url: &str, name: &str) -> Auth {
    let mut auth = Auth {
        provider: "openai-compatibility".into(),
        ..Auth::default()
    };
    for (key, value) in [
        ("base_url", base_url),
        ("api_key", "test"),
        ("compat_name", name),
        ("provider_key", name),
    ] {
        if !value.is_empty() {
            auth.attributes.insert(key.into(), value.into());
        }
    }
    auth
}

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_owned()),
    }
}

fn options(format: &Format) -> Options {
    Options::new(format.clone())
}

fn stream_options(format: &Format) -> Options {
    Options {
        stream: true,
        ..options(format)
    }
}

/// Options for an OpenAI Responses client whose original request was
/// `original`.
fn responses_stream_options(original: &str) -> Options {
    Options {
        response_format: Format::OPENAI_RESPONSE,
        original_request: Bytes::from(original.to_owned()),
        ..stream_options(&Format::OPENAI_RESPONSE)
    }
}

/// The chunks of a stream, and its error if it failed.
async fn collect(response: StreamResponse) -> (Vec<String>, Option<ExecError>) {
    let mut chunks = response.chunks;
    let mut out = Vec::new();
    let mut error = None;
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => {
                assert!(error.is_none(), "a chunk after the error");
                out.push(String::from_utf8_lossy(&chunk).into_owned());
            }
            Err(failure) => {
                assert!(error.is_none(), "a second error: {failure:?}");
                error = Some(failure);
            }
        }
    }
    (out, error)
}

const CHAT_ANSWER: &str = r#"{"id":"chatcmpl_1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
const EMPTY_CHUNK_STREAM: &str = "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[]}\n\ndata: [DONE]\n\n";
const PARTIAL_CHUNK: &str = r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","created":1773896263,"model":"deepseek-v4-flash","choices":[{"index":0,"delta":{"role":"assistant","content":"partial"},"finish_reason":null}]}"#;
const RESPONSES_REQUEST: &str = r#"{"model":"deepseek-v4-flash","input":"hi","stream":true}"#;

#[tokio::test]
async fn compact_passthrough() {
    let answer = r#"{"id":"resp_1","object":"response.compaction","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}"#;
    let mock = Mock::start(Reply::json(answer)).await;
    let executor = executor(vec![entry("compat", true)]);
    let auth = compat_auth(&mock.base_url(), "compat");
    let options = Options {
        alt: COMPACT_ALT.into(),
        ..options(&Format::OPENAI_RESPONSE)
    };
    let response = executor
        .execute(
            Arc::new(auth),
            request(
                "gpt-5.1-codex-max",
                r#"{"model":"gpt-5.1-codex-max","input":[{"role":"user","content":"hi"}],"stream":true}"#,
            ),
            options,
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.path, "/v1/responses/compact");
    let body = seen.json();
    assert!(exists(&body, "input"), "expected input in body");
    assert!(!exists(&body, "messages"), "unexpected messages in body");
    assert!(
        !exists(&body, "prompt_cache_key"),
        "unexpected prompt_cache_key in responses compact body: {}",
        seen.body
    );
    assert!(
        !exists(&body, "stream"),
        "compact drops stream: {}",
        seen.body
    );
    assert_eq!(String::from_utf8_lossy(&response.payload), answer);
}

#[test]
fn apply_prompt_cache_key_table() {
    // (name, support, from, payload, want); upstream's derived keys are
    // absent here.
    let claude_session =
        r#"{"model":"gpt-5.6","metadata":{"user_id":"{\"session_id\":\"cache-session\"}"}}"#;
    let cases = [
        ("disabled", false, "claude", claude_session, None),
        ("derived", true, "claude", claude_session, None),
        (
            "explicit caller key wins",
            true,
            "claude",
            r#"{"model":"gpt-5.6","prompt_cache_key":"caller-key","metadata":{"user_id":"{\"session_id\":\"cache-session\"}"}}"#,
            Some("caller-key"),
        ),
        (
            "non Claude source without identity",
            true,
            "openai",
            r#"{"model":"gpt-5.6","messages":[{"role":"user","content":"hello"}]}"#,
            None,
        ),
        (
            "OpenAI",
            true,
            "openai",
            r#"{"model":"gpt-5.6","messages":[{"role":"user","content":"hello"}]}"#,
            None,
        ),
        (
            "OpenAI responses",
            true,
            "openai-response",
            r#"{"model":"gpt-5.6","input":"hello"}"#,
            None,
        ),
        (
            "Gemini",
            true,
            "gemini",
            r#"{"model":"gemini-3","contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
            None,
        ),
        (
            "Interactions",
            true,
            "interactions",
            r#"{"model":"gpt-5.6","input":"hello"}"#,
            None,
        ),
        (
            "Codex",
            true,
            "codex",
            r#"{"model":"gpt-5.6","input":"hello"}"#,
            None,
        ),
        (
            "Antigravity",
            true,
            "antigravity",
            r#"{"model":"gpt-5.6","input":"hello"}"#,
            None,
        ),
        (
            "a blank caller key counts as none",
            true,
            "openai",
            r#"{"model":"gpt-5.6","prompt_cache_key":"  "}"#,
            None,
        ),
        (
            "the caller key is trimmed",
            true,
            "openai",
            r#"{"model":"gpt-5.6","prompt_cache_key":" key "}"#,
            Some("key"),
        ),
    ];
    for (name, support, from, payload, want) in cases {
        let executor = executor(vec![entry("compat", support)]);
        let auth = compat_auth("", "compat");
        let mut body = json!({"model": "gpt-5.6", "messages": []});
        apply_prompt_cache_key(
            executor.compat_config(&auth),
            &parse_object(payload.as_bytes()),
            &options(&Format::from(from.to_owned())),
            &mut body,
        );
        assert_eq!(
            get(&body, "prompt_cache_key").and_then(Value::as_str),
            want,
            "{name}: {body}"
        );
    }
}

#[test]
fn prompt_cache_key_caller_value_wins_payload_override() {
    let executor = executor(vec![entry("compat", true)]);
    let auth = compat_auth("", "compat");
    let caller = r#"{"model":"gpt-5.6","prompt_cache_key":"caller-key"}"#;
    for (name, payload, original) in [
        ("request payload", caller, ""),
        ("original request", "", caller),
    ] {
        let mut body = json!({"model": "gpt-5.6", "prompt_cache_key": "payload-override"});
        let options = Options {
            original_request: Bytes::from(original),
            ..options(&Format::OPENAI)
        };
        apply_prompt_cache_key(
            executor.compat_config(&auth),
            &parse_object(payload.as_bytes()),
            &options,
            &mut body,
        );
        assert_eq!(body["prompt_cache_key"], "caller-key", "{name}");
    }
}

#[test]
fn prompt_cache_key_uses_config_index() {
    let executor = executor(vec![entry("duplicate", false), entry("duplicate", true)]);
    let payload = parse_object(br#"{"model":"gpt-5.6","prompt_cache_key":"caller-key"}"#);
    for (name, index, want_present) in [("first config", "0", false), ("second config", "1", true)]
    {
        let mut auth = compat_auth("", "duplicate");
        auth.attributes.insert("config_index".into(), index.into());
        auth.attributes
            .insert("source".into(), "config:duplicate[0]".into());
        let mut body = json!({"model": "gpt-5.6", "messages": []});
        apply_prompt_cache_key(
            executor.compat_config(&auth),
            &payload,
            &options(&Format::CLAUDE),
            &mut body,
        );
        assert_eq!(
            exists(&body, "prompt_cache_key"),
            want_present,
            "{name}: {body}"
        );
    }
}

#[test]
fn prompt_cache_key_ignores_config_index_for_non_config_auth() {
    let executor = executor(vec![entry("duplicate", false), entry("duplicate", true)]);
    let mut auth = compat_auth("", "duplicate");
    auth.attributes.insert("config_index".into(), "1".into());
    let mut body = json!({"model": "gpt-5.6", "messages": []});
    apply_prompt_cache_key(
        executor.compat_config(&auth),
        &parse_object(
            br#"{"messages":[{"role":"user","content":"hello"}],"prompt_cache_key":"caller-key"}"#,
        ),
        &options(&Format::OPENAI),
        &mut body,
    );
    assert!(
        !exists(&body, "prompt_cache_key"),
        "unexpected prompt_cache_key for non-config auth: {body}"
    );
}

#[tokio::test]
async fn prompt_cache_key_execute() {
    let mock = Mock::start(Reply::json(CHAT_ANSWER)).await;
    let executor = executor(vec![entry("compat", true)]);
    let auth = Arc::new(compat_auth(&mock.base_url(), "compat"));
    let payload = r#"{"model":"gpt-5.6","messages":[{"role":"user","content":"hello"}]}"#;
    let with_key = Options {
        original_request: Bytes::from_static(br#"{"prompt_cache_key":"caller-key"}"#),
        ..options(&Format::OPENAI)
    };
    executor
        .execute(auth.clone(), request("gpt-5.6", payload), with_key)
        .await
        .unwrap();
    assert_eq!(mock.last().json()["prompt_cache_key"], "caller-key");

    executor
        .execute(auth, request("gpt-5.6", payload), options(&Format::OPENAI))
        .await
        .unwrap();
    let seen = mock.last();
    assert!(
        !exists(&seen.json(), "prompt_cache_key"),
        "none is made up: {}",
        seen.body
    );
}

#[tokio::test]
async fn prompt_cache_key_execute_stream() {
    let mock = Mock::start(Reply::sse(EMPTY_CHUNK_STREAM)).await;
    let executor = executor(vec![entry("compat", true)]);
    let auth = Arc::new(compat_auth(&mock.base_url(), "compat"));
    let payload =
        r#"{"model":"gpt-5.6","messages":[{"role":"user","content":"hello"}],"stream":true}"#;
    let with_key = Options {
        original_request: Bytes::from_static(br#"{"prompt_cache_key":"caller-key"}"#),
        ..stream_options(&Format::OPENAI)
    };
    let response = executor
        .execute_stream(auth.clone(), request("gpt-5.6", payload), with_key)
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "stream chunk error: {error:?}");
    assert_eq!(mock.last().json()["prompt_cache_key"], "caller-key");

    let response = executor
        .execute_stream(
            auth,
            request("gpt-5.6", payload),
            stream_options(&Format::OPENAI),
        )
        .await
        .unwrap();
    collect(response).await;
    let seen = mock.last();
    assert!(
        !exists(&seen.json(), "prompt_cache_key"),
        "none is made up: {}",
        seen.body
    );
}

#[tokio::test]
async fn prompt_cache_key_stream_compact_skipped() {
    let mock = Mock::start(Reply::sse(EMPTY_CHUNK_STREAM)).await;
    let executor = executor(vec![entry("compat", true)]);
    let auth = Arc::new(compat_auth(&mock.base_url(), "compat"));
    let options = Options {
        alt: COMPACT_ALT.into(),
        original_request: Bytes::from_static(br#"{"prompt_cache_key":"caller-key"}"#),
        ..stream_options(&Format::OPENAI)
    };
    let response = executor
        .execute_stream(
            auth,
            request(
                "gpt-5.6",
                r#"{"model":"gpt-5.6","messages":[{"role":"user","content":"hello"}],"stream":true}"#,
            ),
            options,
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "stream chunk error: {error:?}");
    let seen = mock.last();
    assert_eq!(seen.path, "/v1/chat/completions");
    assert!(
        !exists(&seen.json(), "prompt_cache_key"),
        "unexpected prompt_cache_key in streaming compact body: {}",
        seen.body
    );
}

/// Streams `body` from the mock to an OpenAI client.
async fn openai_stream(body: &str, model: &str) -> (Vec<String>, Option<ExecError>) {
    let mock = Mock::start(Reply::sse(body)).await;
    let payload = format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"stream":true}}"#
    );
    let response = executor(Vec::new())
        .execute_stream(
            plain_auth(&mock.base_url()),
            request(model, &payload),
            stream_options(&Format::OPENAI),
        )
        .await
        .unwrap();
    collect(response).await
}

/// Streams `body` from the mock to an OpenAI Responses client.
async fn responses_stream(body: &str) -> (Vec<String>, Option<ExecError>) {
    let mock = Mock::start(Reply::sse(body)).await;
    let response = executor(Vec::new())
        .execute_stream(
            plain_auth(&mock.base_url()),
            request("deepseek-v4-flash", RESPONSES_REQUEST),
            responses_stream_options(RESPONSES_REQUEST),
        )
        .await
        .unwrap();
    collect(response).await
}

#[tokio::test]
async fn stream_rejects_plain_json_after_blank_lines() {
    let body = "\n\n: openrouter processing\n\nevent: error\n{\"error\":{\"message\":\"upstream failed\",\"type\":\"server_error\"}}\n";
    let (_, error) = openai_stream(body, "openrouter-model").await;
    let error = error.expect("expected plain JSON stream error");
    assert_eq!(error.status, 502, "{error:?}");
    assert!(error.message.contains("upstream failed"), "{error:?}");
}

#[tokio::test]
async fn stream_skips_keep_alive_until_data_line() {
    let data = r#"{"id":"chatcmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#;
    let body =
        format!("\n\n: openrouter processing\n\nevent: ping\nid: 1\nretry: 1000\ndata: {data}\n");
    let (chunks, error) = openai_stream(&body, "openrouter-model").await;
    assert!(error.is_none(), "unexpected stream error: {error:?}");
    let got: Value = serde_json::from_str(&chunks.concat()).unwrap_or(Value::Null);
    assert_eq!(got["choices"][0]["delta"]["content"], "hello", "{chunks:?}");
}

#[tokio::test]
async fn responses_stream_fails_on_eof_without_done() {
    let (chunks, error) = responses_stream(&format!("{PARTIAL_CHUNK}\n\n")).await;
    let streamed = chunks.concat();
    assert!(
        streamed.contains("response.output_text.delta"),
        "stream did not forward partial assistant output: {streamed:?}"
    );
    assert!(
        !streamed.contains("response.completed"),
        "clean EOF without [DONE] was finalized as response.completed: {streamed:?}"
    );
    let error = error.expect("clean EOF without [DONE] did not produce a terminal stream error");
    assert_eq!(error.status, 502, "{error:?}");
    assert!(error.message.contains("closed before [DONE]"), "{error:?}");
}

#[tokio::test]
async fn responses_stream_preserves_upstream_data_error() {
    for with_done in [false, true] {
        let mut body = format!(
            "{PARTIAL_CHUNK}\n\ndata: {{\"error\":{{\"type\":\"server_error\",\"code\":\"upstream_failed\",\"message\":\"upstream failed\"}}}}\n\n"
        );
        if with_done {
            body.push_str("data: [DONE]\n\n");
        }
        let (chunks, error) = responses_stream(&body).await;
        let streamed = chunks.concat();
        assert!(
            !streamed.contains("response.completed"),
            "with_done={with_done}: upstream data error was finalized as response.completed: {streamed:?}"
        );
        let error = error.expect("terminal stream error");
        assert!(
            error.message.contains("upstream failed"),
            "with_done={with_done}: {error:?}"
        );
    }
}

#[tokio::test]
async fn responses_stream_preserves_named_error_event() {
    let body = format!(
        "{PARTIAL_CHUNK}\n\nevent: error\ndata: {{\"code\":\"upstream_failed\",\ndata: \"message\":\"upstream failed\"}}\n\ndata: [DONE]\n\n"
    );
    let (chunks, error) = responses_stream(&body).await;
    let streamed = chunks.concat();
    assert!(
        !streamed.contains("response.completed"),
        "named upstream error event was finalized as response.completed: {streamed:?}"
    );
    let error = error.expect("terminal stream error");
    assert!(error.message.contains("upstream failed"), "{error:?}");
}

#[tokio::test]
async fn responses_stream_handles_additional_error_shapes() {
    let cases: [(&str, &[&str], &str); 5] = [
        (
            "response failed payload",
            &[
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"server_error\",\"code\":\"upstream_failed\",\"message\":\"response failed upstream\"}}}\n\n",
                "data: [DONE]\n\n",
            ],
            "response failed upstream",
        ),
        (
            "data before named error",
            &[
                "data: {\"detail\":\"data before event failure\"}\n",
                "event: error\n\n",
                "data: [DONE]\n\n",
            ],
            "data before event failure",
        ),
        (
            "done after incomplete error data",
            &[
                "event: error\n",
                "data: {\"message\":\"incomplete upstream failure\"\n",
                "data: [DONE]\n\n",
            ],
            "incomplete data before [DONE]",
        ),
        (
            "done immediately after error event",
            &["event: error\n", "data: [DONE]\n\n"],
            "error event ended before [DONE]",
        ),
        (
            "incomplete data cannot cross frame boundary",
            &[
                "data: {\n\n",
                "data: \"id\":\"chatcmpl_2\",\"object\":\"chat.completion.chunk\",\"choices\":[]}\n\n",
                "data: [DONE]\n\n",
            ],
            "incomplete SSE data frame",
        ),
    ];
    for (name, lines, want) in cases {
        let body = format!("{PARTIAL_CHUNK}\n\n{}", lines.concat());
        let (chunks, error) = responses_stream(&body).await;
        let streamed = chunks.concat();
        assert!(
            !streamed.contains("response.completed"),
            "{name}: upstream error was finalized as response.completed: {streamed:?}"
        );
        let error = error.unwrap_or_else(|| panic!("{name}: no terminal stream error"));
        assert_eq!(error.status, 502, "{name}: {error:?}");
        assert!(
            error.message.contains(want),
            "{name}: {error:?}, want {want:?}"
        );
    }
}

#[tokio::test]
async fn stream_drops_chunks_after_done() {
    // Some providers (OpenCode zen, for one) send metadata after
    // data: [DONE], which mustn't reach the client.
    let first = r#"{"id":"c1a4ba22","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#;
    let second = r#"{"id":"c1a4ba22","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
    let body = format!(
        "data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\ndata: {{\"choices\":[],\"cost\":\"0\"}}\n\n"
    );
    let (chunks, error) = openai_stream(&body, "deepseek-v4-flash-free").await;
    assert!(error.is_none(), "unexpected stream error: {error:?}");
    assert!(
        chunks.iter().all(|chunk| !chunk.contains(r#""cost""#)),
        "post-DONE cost chunk was forwarded: {chunks:?}"
    );
    let payloads: Vec<&String> = chunks.iter().filter(|chunk| !chunk.is_empty()).collect();
    assert_eq!(payloads.len(), 2, "want content + finish: {payloads:?}");
    let parsed: Vec<Value> = payloads
        .iter()
        .map(|payload| serde_json::from_str(payload).unwrap())
        .collect();
    assert!(
        parsed.iter().all(|payload| exists(payload, "id")),
        "{payloads:?}"
    );
    assert_eq!(parsed[0]["choices"][0]["delta"]["content"], "hi");
    assert_eq!(parsed[1]["choices"][0]["finish_reason"], "stop");
}

fn max_tokens_models() -> Vec<OpenAiCompatibility> {
    let model =
        |name: &str, alias: &str, use_max_completion_tokens: bool| OpenAiCompatibilityModel {
            name: name.into(),
            alias: alias.into(),
            use_max_completion_tokens,
            ..OpenAiCompatibilityModel::default()
        };
    vec![OpenAiCompatibility {
        models: vec![
            model("upstream-new", "alias-new", true),
            model("upstream-legacy", "alias-legacy", false),
        ],
        ..entry("test-compat", false)
    }]
}

/// (name, client format, model, payload, field wanted, its value).
type MaxTokensCase = (
    &'static str,
    Format,
    &'static str,
    &'static str,
    &'static str,
    Value,
);

/// Checks that the provider got `want` set to `value` and not the other
/// limit field.
fn check_max_tokens(name: &str, seen: &Seen, want: &str, value: &Value) {
    let body = seen.json();
    let other = if want == "max_tokens" {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    assert_eq!(get(&body, want), Some(value), "{name}: body={}", seen.body);
    assert!(
        !exists(&body, other),
        "{name}: {other} should be absent; body={}",
        seen.body
    );
}

#[tokio::test]
async fn max_tokens_normalization() {
    let mock = Mock::start(Reply::json(
        r#"{"id":"chatcmpl-1","choices":[{"message":{"role":"assistant","content":"hello"}}]}"#,
    ))
    .await;
    let executor = executor(max_tokens_models());
    let auth = Arc::new(compat_auth(&mock.base_url(), "test-compat"));
    let responses = Format::OPENAI_RESPONSE;
    let chat = Format::OPENAI;
    let cases: [MaxTokensCase; 8] = [
        (
            "Execute responses request with use-max-completion-tokens=true sets max_completion_tokens",
            responses.clone(),
            "alias-new",
            r#"{"model":"alias-new","input":[{"role":"user","content":"hi"}],"max_output_tokens":1024}"#,
            "max_completion_tokens",
            json!(1024),
        ),
        (
            "Execute chat request with max_tokens and use-max-completion-tokens=true converts to max_completion_tokens",
            chat.clone(),
            "alias-new",
            r#"{"model":"alias-new","messages":[{"role":"user","content":"hi"}],"max_tokens":512}"#,
            "max_completion_tokens",
            json!(512),
        ),
        (
            "Execute responses request with use-max-completion-tokens=false sets max_tokens",
            responses.clone(),
            "alias-legacy",
            r#"{"model":"alias-legacy","input":[{"role":"user","content":"hi"}],"max_output_tokens":1024}"#,
            "max_tokens",
            json!(1024),
        ),
        (
            "Execute chat request with max_completion_tokens and use-max-completion-tokens=false converts to max_tokens",
            chat.clone(),
            "alias-legacy",
            r#"{"model":"alias-legacy","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":512}"#,
            "max_tokens",
            json!(512),
        ),
        (
            "Execute responses request with max_output_tokens=null and use-max-completion-tokens=true preserves null",
            responses.clone(),
            "alias-new",
            r#"{"model":"alias-new","input":[{"role":"user","content":"hi"}],"max_output_tokens":null}"#,
            "max_completion_tokens",
            Value::Null,
        ),
        (
            "Execute responses request with max_output_tokens=null and use-max-completion-tokens=false preserves null",
            responses.clone(),
            "alias-legacy",
            r#"{"model":"alias-legacy","input":[{"role":"user","content":"hi"}],"max_output_tokens":null}"#,
            "max_tokens",
            Value::Null,
        ),
        (
            "Execute chat request with max_tokens=null and use-max-completion-tokens=true preserves null",
            chat.clone(),
            "alias-new",
            r#"{"model":"alias-new","messages":[{"role":"user","content":"hi"}],"max_tokens":null}"#,
            "max_completion_tokens",
            Value::Null,
        ),
        (
            "Execute chat request with max_completion_tokens=null and use-max-completion-tokens=false preserves null",
            chat.clone(),
            "alias-legacy",
            r#"{"model":"alias-legacy","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":null}"#,
            "max_tokens",
            Value::Null,
        ),
    ];
    for (name, format, model, payload, want, value) in cases {
        executor
            .execute(auth.clone(), request(model, payload), options(&format))
            .await
            .unwrap_or_else(|error| panic!("{name}: Execute error: {error:?}"));
        check_max_tokens(name, &mock.last(), want, &value);
    }
}

#[tokio::test]
async fn max_tokens_normalization_stream() {
    let mock = Mock::start(Reply::sse(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
    ))
    .await;
    let executor = executor(max_tokens_models());
    let auth = Arc::new(compat_auth(&mock.base_url(), "test-compat"));
    let responses = Format::OPENAI_RESPONSE;
    let chat = Format::OPENAI;
    let cases: [MaxTokensCase; 6] = [
        (
            "ExecuteStream with use-max-completion-tokens=true emits max_completion_tokens",
            chat.clone(),
            "alias-new",
            r#"{"model":"alias-new","messages":[{"role":"user","content":"hi"}],"max_tokens":256}"#,
            "max_completion_tokens",
            json!(256),
        ),
        (
            "ExecuteStream with use-max-completion-tokens=false emits max_tokens",
            chat.clone(),
            "alias-legacy",
            r#"{"model":"alias-legacy","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":256}"#,
            "max_tokens",
            json!(256),
        ),
        (
            "ExecuteStream responses request with max_output_tokens=null and use-max-completion-tokens=true preserves null",
            responses.clone(),
            "alias-new",
            r#"{"model":"alias-new","input":[{"role":"user","content":"hi"}],"max_output_tokens":null}"#,
            "max_completion_tokens",
            Value::Null,
        ),
        (
            "ExecuteStream responses request with max_output_tokens=null and use-max-completion-tokens=false preserves null",
            responses.clone(),
            "alias-legacy",
            r#"{"model":"alias-legacy","input":[{"role":"user","content":"hi"}],"max_output_tokens":null}"#,
            "max_tokens",
            Value::Null,
        ),
        (
            "ExecuteStream chat request with max_tokens=null and use-max-completion-tokens=true preserves null",
            chat.clone(),
            "alias-new",
            r#"{"model":"alias-new","messages":[{"role":"user","content":"hi"}],"max_tokens":null}"#,
            "max_completion_tokens",
            Value::Null,
        ),
        (
            "ExecuteStream chat request with max_completion_tokens=null and use-max-completion-tokens=false preserves null",
            chat.clone(),
            "alias-legacy",
            r#"{"model":"alias-legacy","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":null}"#,
            "max_tokens",
            Value::Null,
        ),
    ];
    for (name, format, model, payload, want, value) in cases {
        let response = executor
            .execute_stream(
                auth.clone(),
                request(model, payload),
                stream_options(&format),
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: ExecuteStream error: {error:?}"));
        collect(response).await;
        check_max_tokens(name, &mock.last(), want, &value);
    }
}

#[tokio::test]
async fn propagates_retry_after() {
    let mock = Mock::start(
        Reply::error(
            429,
            r#"{"error":{"code":"rate_limit","message":"try later"}}"#,
        )
        .with_header("retry-after", "7"),
    )
    .await;
    let executor = executor(Vec::new());
    let auth = plain_auth(&mock.base_url());
    let payload = r#"{"model":"compatible-model","messages":[{"role":"user","content":"hi"}]}"#;

    let error = executor
        .execute(
            auth.clone(),
            request("compatible-model", payload),
            options(&Format::OPENAI),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 429, "nonstream: {error:?}");
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)), "nonstream");
    assert_eq!(
        error.message,
        r#"{"error":{"code":"rate_limit","message":"try later"}}"#
    );

    let Err(error) = executor
        .execute_stream(
            auth,
            request("compatible-model", payload),
            stream_options(&Format::OPENAI),
        )
        .await
    else {
        panic!("stream bootstrap: the stream started");
    };
    assert_eq!(error.status, 429, "stream bootstrap: {error:?}");
    assert_eq!(
        error.retry_after,
        Some(Duration::from_secs(7)),
        "stream bootstrap"
    );
}

/// A provider whose `mapped-model` (alias `claude-client`) takes
/// `input_modalities`.
fn modalities_executor(input_modalities: &[&str]) -> OpenAiCompatExecutor {
    executor(vec![OpenAiCompatibility {
        models: vec![OpenAiCompatibilityModel {
            name: "mapped-model".into(),
            alias: "claude-client".into(),
            input_modalities: input_modalities
                .iter()
                .map(|&modality| modality.into())
                .collect(),
            ..OpenAiCompatibilityModel::default()
        }],
        ..entry("compat", false)
    }])
}

const CLAUDE_TOOL_RESULT: &str = r#"{"model":"claude-client","max_tokens":64,"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"inspect_image","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":[{"type":"text","text":"image inspected"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}]}]}]}"#;
const OMITTED_IMAGE_RESULT: &str = "image inspected\n\n[image omitted: unsupported by upstream]";
const NON_STREAM_ANSWER: &str = r#"{"id":"chatcmpl_1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

/// Sends `payload` from a `source` client as a stream or not, and returns
/// what the provider got.
async fn send_tool_result(
    executor: &OpenAiCompatExecutor,
    source: &Format,
    payload: &str,
    stream: bool,
) -> Seen {
    let reply = if stream {
        Reply::sse("data: [DONE]\n\n")
    } else {
        Reply::json(NON_STREAM_ANSWER)
    };
    let mock = Mock::start(reply).await;
    let auth = Arc::new(compat_auth(&mock.base_url(), "compat"));
    let options = Options {
        stream,
        response_format: Format::OPENAI,
        ..options(source)
    };
    let request = request("mapped-model", payload);
    if stream {
        let response = executor
            .execute_stream(auth, request, options)
            .await
            .unwrap();
        let (_, error) = collect(response).await;
        assert!(error.is_none(), "stream chunk error: {error:?}");
    } else {
        executor.execute(auth, request, options).await.unwrap();
    }
    mock.last()
}

#[tokio::test]
async fn tool_result_content_by_input_modalities() {
    let cases: [(&str, bool, &[&str], bool); 4] = [
        ("non-stream text-only", false, &["text"], true),
        ("stream text-only", true, &["text"], true),
        ("non-stream multimodal", false, &["text", "image"], false),
        ("non-stream unspecified", false, &[], false),
    ];
    for (name, stream, modalities, want_text_only) in cases {
        let executor = modalities_executor(modalities);
        let seen = send_tool_result(&executor, &Format::CLAUDE, CLAUDE_TOOL_RESULT, stream).await;
        let content = &seen.json()["messages"][1]["content"];
        if want_text_only {
            assert_eq!(content, OMITTED_IMAGE_RESULT, "{name}: body={}", seen.body);
            assert!(
                !seen.body.contains("image_url"),
                "{name}: text-only model still received an image_url part: {}",
                seen.body
            );
        } else {
            assert_eq!(content, "image inspected", "{name}: body={}", seen.body);
            assert!(
                seen.body.contains("image_url"),
                "{name}: multimodal model did not receive relayed image_url part: {}",
                seen.body
            );
        }
    }
}

#[tokio::test]
async fn text_only_model_does_not_receive_image_url() {
    let executor = modalities_executor(&["text"]);
    for stream in [false, true] {
        let seen = send_tool_result(&executor, &Format::CLAUDE, CLAUDE_TOOL_RESULT, stream).await;
        assert!(
            !seen.body.contains("image_url"),
            "stream={stream}: text-only model still received an image_url part: {}",
            seen.body
        );
    }
}

#[tokio::test]
async fn text_only_model_gets_chat_tool_results_as_text() {
    let payload = r#"{"model":"claude-client","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"inspect_image","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_1","content":[{"type":"text","text":"image inspected"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}]}"#;
    for stream in [false, true] {
        let seen = send_tool_result(
            &modalities_executor(&["text"]),
            &Format::OPENAI,
            payload,
            stream,
        )
        .await;
        assert_eq!(
            seen.json()["messages"][1]["content"],
            OMITTED_IMAGE_RESULT,
            "stream={stream}"
        );
        assert!(
            !seen.body.contains("image_url"),
            "stream={stream}: {}",
            seen.body
        );

        let seen = send_tool_result(
            &modalities_executor(&["text", "image"]),
            &Format::OPENAI,
            payload,
            stream,
        )
        .await;
        assert!(
            seen.json()["messages"][1]["content"].is_array(),
            "stream={stream}"
        );
    }
}

#[tokio::test]
async fn chat_request_goes_out_as_sent() {
    let mock = Mock::start(Reply::json(CHAT_ANSWER).with_header("x-upstream", "yes")).await;
    let executor = executor(Vec::new());
    let auth = plain_auth(&format!("{}/", mock.base_url()));
    let payload = r#"{"model":"alias","messages":[{"role":"user","content":"hi"}],"max_tokens":5,"stream":false}"#;
    let response = executor
        .execute(
            auth.clone(),
            request("upstream-model(high)", payload),
            options(&Format::OPENAI),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(
        seen.path, "/v1/chat/completions",
        "one trailing slash is dropped"
    );
    assert_eq!(
        seen.body,
        r#"{"model":"upstream-model","messages":[{"role":"user","content":"hi"}],"max_tokens":5,"stream":false}"#
    );
    assert_eq!(response.headers.get("x-upstream").unwrap(), "yes");
    let answer: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(answer["choices"][0]["message"]["content"], "ok");

    let mock = Mock::start(Reply::sse(EMPTY_CHUNK_STREAM)).await;
    let response = executor
        .execute_stream(
            plain_auth(&mock.base_url()),
            request("upstream-model", payload),
            stream_options(&Format::OPENAI),
        )
        .await
        .unwrap();
    collect(response).await;
    assert_eq!(
        mock.last().body,
        r#"{"model":"upstream-model","messages":[{"role":"user","content":"hi"}],"max_tokens":5,"stream":false,"stream_options":{"include_usage":true}}"#
    );
}

#[tokio::test]
async fn request_headers() {
    let mock = Mock::start(Reply::json(CHAT_ANSWER)).await;
    let executor = executor(Vec::new());
    let payload = r#"{"model":"m","messages":[]}"#;
    executor
        .execute(
            plain_auth(&mock.base_url()),
            request("m", payload),
            options(&Format::OPENAI),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("authorization"), Some("Bearer test"));
    assert_eq!(seen.header("user-agent"), Some(USER_AGENT));
    assert!(USER_AGENT.starts_with("open-ferry/"));
    assert_eq!(seen.header("cache-control"), None);

    let mock = Mock::start(Reply::sse(EMPTY_CHUNK_STREAM)).await;
    let mut auth = Auth::default();
    auth.attributes.insert("base_url".into(), mock.base_url());
    auth.attributes
        .insert("header:X-Custom".into(), "custom".into());
    auth.attributes
        .insert("header:User-Agent".into(), "claude-cli/2.1".into());
    let options = Options {
        headers: HeaderMap::from_iter([(
            header::USER_AGENT,
            HeaderValue::from_static(" my-client/1.0 "),
        )]),
        ..stream_options(&Format::OPENAI)
    };
    let response = executor
        .execute_stream(Arc::new(auth), request("m", payload), options)
        .await
        .unwrap();
    collect(response).await;
    let seen = mock.last();
    assert_eq!(seen.header("authorization"), None, "no API key, no token");
    assert_eq!(seen.header("user-agent"), Some("my-client/1.0"));
    assert_eq!(seen.header("x-custom"), Some("custom"));
    assert_eq!(seen.header("accept"), Some("text/event-stream"));
    assert_eq!(seen.header("cache-control"), Some("no-cache"));
}

#[tokio::test]
async fn missing_base_url_is_unauthorized() {
    let executor = executor(Vec::new());
    let mut auth = Auth::default();
    auth.attributes.insert("base_url".into(), "  ".into());
    auth.attributes
        .insert("api_key".into(), "secret-key".into());
    let auth = Arc::new(auth);
    let error = executor
        .execute(auth.clone(), request("m", "{}"), options(&Format::OPENAI))
        .await
        .unwrap_err();
    assert_eq!(
        (error.status, error.message.as_str()),
        (401, "missing provider baseURL")
    );
    let Err(error) = executor
        .execute_stream(auth, request("m", "{}"), stream_options(&Format::OPENAI))
        .await
    else {
        panic!("the stream started");
    };
    assert_eq!(error.status, 401);
}

#[tokio::test]
async fn errors_hide_the_api_key() {
    let mut auth = Auth::default();
    let key = "sk-test-secret-key";
    let mock = Mock::start(Reply::error(
        401,
        r#"{"error":{"message":"Incorrect API key provided: sk-test-secret-key"}}"#,
    ))
    .await;
    auth.attributes.insert("base_url".into(), mock.base_url());
    auth.attributes.insert("api_key".into(), key.into());
    let auth = Arc::new(auth);
    let error = executor(Vec::new())
        .execute(auth.clone(), request("m", "{}"), options(&Format::OPENAI))
        .await
        .unwrap_err();
    assert_eq!(error.status, 401);
    assert_eq!(
        error.message,
        r#"{"error":{"message":"Incorrect API key provided: [redacted]"}}"#
    );

    let mock = Mock::start(Reply::sse(
        "data: {\"error\":{\"message\":\"bad key sk-test-secret-key\"}}\n\n",
    ))
    .await;
    let mut auth = (*auth).clone();
    auth.attributes.insert("base_url".into(), mock.base_url());
    let response = executor(Vec::new())
        .execute_stream(
            Arc::new(auth),
            request("m", "{}"),
            stream_options(&Format::OPENAI),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    let error = error.expect("the stream failed");
    assert!(!error.message.contains(key), "{error:?}");
    assert!(error.message.contains("bad key [redacted]"), "{error:?}");
}

#[tokio::test]
async fn a_frame_too_large_fails() {
    let line = format!("data: {}\n", "x".repeat(1 << 20));
    let mock = Mock::start(Reply::sse(&line.repeat(51))).await;
    let response = executor(Vec::new())
        .execute_stream(
            plain_auth(&mock.base_url()),
            request("m", "{}"),
            stream_options(&Format::OPENAI),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(chunks.is_empty(), "{}", chunks.len());
    let error = error.expect("the stream failed");
    assert_eq!(
        (error.status, error.message.as_str()),
        (502, "upstream SSE data frame is too large")
    );
}

#[tokio::test]
async fn a_control_character_in_the_base_url_fails() {
    let mock = Mock::start(Reply::json("{}")).await;
    let auth = plain_auth(&format!("{}/v\t1", mock.url));
    let error = executor(Vec::new())
        .execute(auth, request("m", "{}"), options(&Format::OPENAI))
        .await
        .unwrap_err();
    assert_eq!(error.message, "net/url: invalid control character in URL");
    assert!(mock.requests().is_empty(), "nothing is sent");
}

#[tokio::test]
async fn a_configured_content_length_is_ignored() {
    let mock = Mock::start(Reply::json(CHAT_ANSWER)).await;
    let mut auth = (*plain_auth(&mock.base_url())).clone();
    auth.attributes
        .insert("header:Content-Length".into(), "1".into());
    executor(Vec::new())
        .execute(
            Arc::new(auth),
            request("m", r#"{"model":"m","messages":[]}"#),
            options(&Format::OPENAI),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.json()["model"], "m", "{}", seen.body);
    assert_eq!(
        seen.header("content-length"),
        Some(seen.body.len().to_string().as_str())
    );
}

#[tokio::test]
async fn error_status_keeps_the_body() {
    let mock = Mock::start(Reply::error(400, r#"{"error":{"message":"bad"}}"#)).await;
    let error = executor(Vec::new())
        .execute(
            plain_auth(&mock.base_url()),
            request("m", "{}"),
            options(&Format::OPENAI),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(error.message, r#"{"error":{"message":"bad"}}"#);
    assert_eq!(error.retry_after, None);
}

#[tokio::test]
async fn refresh() {
    let executor = executor(Vec::new());
    let auth = plain_auth("http://127.0.0.1:9");
    let refreshed = executor.refresh(auth.clone()).await.unwrap();
    assert_eq!(refreshed.attributes, auth.attributes);

    for key in ["refresh_token", "refreshToken"] {
        let mut auth = Auth {
            provider: "openai-compatible-x".into(),
            ..Auth::default()
        };
        auth.metadata.insert(key.into(), "rt".into());
        let error = executor.refresh(Arc::new(auth)).await.unwrap_err();
        assert_eq!(
            error.message,
            "openai compat executor cannot refresh oauth credentials for provider openai-compatibility"
        );
    }
    let mut auth = Auth::default();
    auth.metadata.insert("refresh_token".into(), " ".into());
    assert!(
        executor.refresh(Arc::new(auth)).await.is_ok(),
        "a blank token is none"
    );
}

#[tokio::test]
async fn count_tokens() {
    let executor = executor(Vec::new());
    let payload = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hello world"}]}"#;
    let response = executor
        .count_tokens(
            plain_auth(""),
            request("gpt-4o(high)", payload),
            options(&Format::OPENAI),
        )
        .await
        .unwrap();
    let count = tiktoken_rs::o200k_base_singleton()
        .encode_ordinary("user\nhello world")
        .len();
    assert_eq!(
        String::from_utf8_lossy(&response.payload),
        format!(
            r#"{{"usage":{{"prompt_tokens":{count},"completion_tokens":0,"total_tokens":{count}}}}}"#
        )
    );
    assert!(response.headers.is_empty());
}

#[tokio::test]
async fn count_tokens_for_a_claude_client() {
    let payload = r#"{"model":"gpt-4o","system":"be brief","messages":[{"role":"user","content":"hello world"}]}"#;
    let response = executor(Vec::new())
        .count_tokens(
            plain_auth(""),
            request("gpt-4o", payload),
            options(&Format::CLAUDE),
        )
        .await
        .unwrap();
    let count = tiktoken_rs::o200k_base_singleton()
        .encode_ordinary("system\nbe brief\nuser\nhello world")
        .len();
    assert_eq!(
        String::from_utf8_lossy(&response.payload),
        format!(r#"{{"input_tokens":{count}}}"#)
    );
}

#[tokio::test]
async fn claude_client_gets_a_claude_message() {
    let mock = Mock::start(Reply::json(CHAT_ANSWER)).await;
    let payload = r#"{"model":"m","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let response = executor(Vec::new())
        .execute(
            plain_auth(&mock.base_url()),
            request("m", payload),
            options(&Format::CLAUDE),
        )
        .await
        .unwrap();
    let sent = mock.last();
    assert_eq!(sent.path, "/v1/chat/completions");
    assert_eq!(sent.json()["messages"][0]["role"], "user", "{}", sent.body);
    assert_eq!(sent.json()["max_tokens"], 64, "{}", sent.body);
    let message: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(message["type"], "message", "{message}");
    assert_eq!(message["role"], "assistant", "{message}");
    assert_eq!(message["content"][0]["type"], "text", "{message}");
    assert_eq!(message["content"][0]["text"], "ok", "{message}");
    assert_eq!(message["stop_reason"], "end_turn", "{message}");
}

#[tokio::test]
async fn claude_client_streams_claude_events() {
    let body = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hel\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let mock = Mock::start(Reply::sse(body)).await;
    let payload = r#"{"model":"m","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let response = executor(Vec::new())
        .execute_stream(
            plain_auth(&mock.base_url()),
            request("m", payload),
            stream_options(&Format::CLAUDE),
        )
        .await
        .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    let streamed = chunks.concat();
    for event in [
        "event: message_start",
        "event: content_block_start",
        "event: content_block_delta",
        "event: message_delta",
        "event: message_stop",
    ] {
        assert!(streamed.contains(event), "no {event}: {streamed}");
    }
    assert!(streamed.contains(r#""text":"hel""#), "{streamed}");
    assert!(streamed.contains(r#""text":"lo""#), "{streamed}");
    let sent = mock.last().json();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
}

#[test]
fn compat_config_lookup() {
    let disabled = OpenAiCompatibility {
        disabled: true,
        ..entry("Prov", true)
    };
    let executor = executor(vec![disabled, entry("prov", false), entry("other", true)]);
    let name = |auth: &Auth| {
        executor
            .compat_config(auth)
            .map(|compat| (compat.name.clone(), compat.support_prompt_cache_key))
    };

    let mut auth = Auth {
        provider: " PROV ".into(),
        ..Auth::default()
    };
    assert_eq!(
        name(&auth),
        Some(("prov".into(), false)),
        "disabled entries are skipped"
    );
    auth.attributes.insert("compat_name".into(), "other".into());
    assert_eq!(
        name(&auth),
        Some(("prov".into(), false)),
        "entries go in order"
    );
    auth.provider = "openai-compatible-other".into();
    assert_eq!(name(&auth), Some(("other".into(), true)));

    let mut auth = Auth::default();
    auth.attributes
        .insert("source".into(), "config:other[0]".into());
    for (index, want) in [
        ("2", Some(("other".into(), true))),
        (" 1 ", Some(("prov".into(), false))),
        ("0", None),
        ("3", None),
        ("-1", None),
        ("x", None),
    ] {
        auth.attributes.insert("config_index".into(), index.into());
        assert_eq!(name(&auth), want, "config_index {index:?}");
    }
    assert_eq!(name(&Auth::default()), None);
}
