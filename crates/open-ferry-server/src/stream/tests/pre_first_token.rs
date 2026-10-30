// Ported from CLIProxyAPI sdk/api/handlers/handlers_stream_bootstrap_test.go,
// claude/code_handlers_error_test.go, gemini/gemini_handlers_stream_error_test.go,
// openai/openai_handlers_stream_error_test.go and
// openai/openai_responses_handlers_stream_error_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, StreamExt, stream};
use http::{HeaderMap, HeaderValue, Request, StatusCode};
use http_body_util::BodyExt;
use open_ferry_core::exec::{
    Dispatcher, ExecError, Options, ProviderId, Request as ExecRequest, Response as ExecResponse,
    StreamResponse,
};
use open_ferry_core::models::ModelInfo;
use tokio::sync::oneshot;
use tower::ServiceExt;

use crate::AppState;
use crate::config::{ServerConfig, StreamingConfig};
use crate::testing::FakeCatalog;

const TICK: Duration = Duration::from_millis(10);
const HEARTBEAT: &str = ": keep-alive\n\n";
const OPENAI_ERROR: &str =
    r#"{"error":{"message":"busy","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#;
type ResultGate = oneshot::Receiver<Result<Bytes, ExecError>>;

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A channel gates either ExecuteStream itself or its first payload. No
/// background task runs, and the paused Tokio clock advances only a tick.
struct GatedDispatcher {
    gate: Mutex<Option<ResultGate>>,
    bootstrap: bool,
    dropped: Arc<AtomicBool>,
}

impl GatedDispatcher {
    fn take(&self) -> (ResultGate, Dropped) {
        (
            self.gate.lock().unwrap().take().unwrap(),
            Dropped(self.dropped.clone()),
        )
    }
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-late-upstream", HeaderValue::from_static("late"));
    headers
}

impl Dispatcher for GatedDispatcher {
    fn execute<'a>(
        &'a self,
        _: &'a [ProviderId],
        _: ExecRequest,
        _: Options,
    ) -> BoxFuture<'a, Result<ExecResponse, ExecError>> {
        let (gate, held) = self.take();
        async move {
            let _held = held;
            gate.await.unwrap().map(|payload| ExecResponse {
                payload,
                headers: headers(),
            })
        }
        .boxed()
    }

    fn count_tokens<'a>(
        &'a self,
        _: &'a [ProviderId],
        _: ExecRequest,
        _: Options,
    ) -> BoxFuture<'a, Result<ExecResponse, ExecError>> {
        panic!("streaming test does not count tokens")
    }

    fn execute_stream<'a>(
        &'a self,
        _: &'a [ProviderId],
        _: ExecRequest,
        _: Options,
    ) -> BoxFuture<'a, Result<StreamResponse, ExecError>> {
        let (gate, held) = self.take();
        let bootstrap = self.bootstrap;
        async move {
            if bootstrap {
                let _held = held;
                let payload = gate.await.unwrap()?;
                Ok(StreamResponse {
                    headers: headers(),
                    chunks: stream::iter([Ok(payload)]).boxed(),
                })
            } else {
                let chunks = stream::once(async move {
                    let _held = held;
                    gate.await.unwrap()
                })
                .boxed();
                Ok(StreamResponse {
                    headers: headers(),
                    chunks,
                })
            }
        }
        .boxed()
    }
}

fn server(
    config: StreamingConfig,
    provider: &str,
    model: &str,
    bootstrap: bool,
) -> (
    Router,
    oneshot::Sender<Result<Bytes, ExecError>>,
    Arc<AtomicBool>,
) {
    let (send, gate) = oneshot::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let dispatcher = Arc::new(GatedDispatcher {
        gate: Mutex::new(Some(gate)),
        bootstrap,
        dropped: dropped.clone(),
    });
    let catalog = FakeCatalog::new()
        .serve(model, &[provider])
        .info(ModelInfo {
            id: model.into(),
            model_type: open_ferry_core::registry::registration::OPENAI_IMAGE_MODEL_TYPE.into(),
            ..ModelInfo::default()
        });
    let state = AppState::new(
        ServerConfig {
            streaming: config,
            passthrough_headers: true,
            ..ServerConfig::default()
        },
        dispatcher,
        Arc::new(catalog),
    );
    (crate::router(state), send, dropped)
}

fn enabled() -> StreamingConfig {
    StreamingConfig {
        keepalive: Some(TICK),
        keepalive_before_first_token: true,
        ..StreamingConfig::default()
    }
}

fn request(path: &str, body: &str, codex: bool) -> Request<Body> {
    let mut request = Request::post(path).header("content-type", "application/json");
    if codex {
        // The caller's identity is read, never sent upstream by the handler.
        request = request.header("user-agent", "codex_cli_rs");
    }
    request.body(Body::from(body.to_owned())).unwrap()
}

pub(crate) async fn check_protocol(
    path: &str,
    payload: &str,
    provider: &str,
    model: &str,
    error_prefix: &str,
    codex: bool,
) {
    for bootstrap in [true, false] {
        let (app, release, dropped) = server(enabled(), provider, model, bootstrap);
        let response = app.oneshot(request(path, payload, codex)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        // Call::stream still waits for the first accepted payload before
        // exposing headers, even when execute_stream has already resolved.
        assert!(!response.headers().contains_key("x-late-upstream"));
        let mut body = response.into_body();
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            HEARTBEAT
        );
        release.send(Err(ExecError::upstream(429, "busy"))).unwrap();
        let error = String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
        if path.contains("responses") {
            assert!(error.starts_with(error_prefix), "{error}");
            let data = error
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            let json: serde_json::Value = serde_json::from_str(data).unwrap();
            let detail = if codex {
                &json["response"]["error"]
            } else {
                &json["error"]
            };
            assert_eq!(detail["message"], "busy");
            assert_eq!(detail["code"], "rate_limit_exceeded");
            assert_eq!(json["sequence_number"], 0);
        } else if provider == "claude" {
            assert_eq!(
                error,
                format!(
                    "{error_prefix}{{\"type\":\"error\",\"error\":{{\"type\":\"rate_limit_error\",\"message\":\"busy\"}}}}\n\n"
                )
            );
        } else {
            assert_eq!(error, format!("{error_prefix}{OPENAI_ERROR}\n\n"));
        }
        assert!(dropped.load(Ordering::SeqCst));
    }
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_claude_failure_is_an_sse_error_after_keepalive() {
    check_protocol(
        "/v1/messages",
        r#"{"model":"m","messages":[],"stream":true}"#,
        "claude",
        "m",
        "event: error\ndata: ",
        false,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_gemini_failure_is_an_sse_error_after_keepalive() {
    check_protocol(
        "/v1beta/models/m:streamGenerateContent",
        r#"{"contents":[]}"#,
        "gemini",
        "m",
        "event: error\ndata: ",
        false,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_chat_failure_is_a_data_error_after_keepalive() {
    check_protocol(
        "/v1/chat/completions",
        r#"{"model":"m","messages":[],"stream":true}"#,
        "openai",
        "m",
        "data: ",
        false,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_completions_failure_is_a_data_error_after_keepalive() {
    check_protocol(
        "/v1/completions",
        r#"{"model":"m","prompt":"hi","stream":true}"#,
        "openai",
        "m",
        "data: ",
        false,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_responses_failure_uses_the_clients_event() {
    for path in ["/v1/responses", "/backend-api/codex/responses"] {
        for codex in [false, true] {
            let prefix = if codex {
                "\nevent: response.failed\ndata: "
            } else {
                "\nevent: error\ndata: "
            };
            check_protocol(
                path,
                r#"{"model":"m","input":"hi","stream":true}"#,
                "openai",
                "m",
                prefix,
                codex,
            )
            .await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_images_failure_is_an_sse_error_after_keepalive() {
    for (model, provider) in [
        ("gpt-image-2", "codex"),
        ("compat-image", "compat"),
        ("grok-imagine-image", "xai"),
    ] {
        for path in ["/v1/images/generations", "/v1/images/edits"] {
            let payload = format!(
                r#"{{"model":"{model}","prompt":"cat","image":"data:image/png;base64,AA==","stream":true}}"#
            );
            check_protocol(
                path,
                &payload,
                provider,
                model,
                "event: error\ndata: ",
                false,
            )
            .await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_responses_success_follows_the_heartbeat() {
    let (app, release, _) = server(enabled(), "openai", "m", false);
    let response = app
        .oneshot(request(
            "/v1/responses",
            r#"{"model":"m","input":"hi","stream":true}"#,
            false,
        ))
        .await
        .unwrap();
    let mut body = response.into_body();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        HEARTBEAT
    );
    let complete = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
    release.send(Ok(Bytes::from(complete))).unwrap();
    let rest = String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert_eq!(rest, format!("{complete}\n"));
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_gates_preserve_http_failures_before_a_tick() {
    let cases = [
        (
            "/v1/messages",
            r#"{"model":"m","messages":[],"stream":true}"#,
            "claude",
            "m",
        ),
        (
            "/v1beta/models/m:streamGenerateContent",
            r#"{"contents":[]}"#,
            "gemini",
            "m",
        ),
        (
            "/v1beta/models/m:streamGenerateContent?alt=json",
            r#"{"contents":[]}"#,
            "gemini",
            "m",
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"m","stream":true}"#,
            "openai",
            "m",
        ),
        (
            "/v1/completions",
            r#"{"model":"m","stream":true}"#,
            "openai",
            "m",
        ),
        (
            "/v1/responses",
            r#"{"model":"m","stream":true}"#,
            "openai",
            "m",
        ),
        (
            "/v1beta/interactions",
            r#"{"model":"m","stream":true}"#,
            "gemini-interactions",
            "m",
        ),
        (
            "/v1/images/generations",
            r#"{"model":"gpt-image-2","prompt":"cat","stream":true}"#,
            "codex",
            "gpt-image-2",
        ),
        (
            "/v1/images/generations",
            r#"{"model":"grok-imagine-image","prompt":"cat","stream":true}"#,
            "xai",
            "grok-imagine-image",
        ),
    ];
    for (path, payload, provider, model) in cases {
        for config in [
            StreamingConfig::default(),
            StreamingConfig {
                keepalive: Some(TICK),
                ..StreamingConfig::default()
            },
            StreamingConfig {
                keepalive: Some(Duration::ZERO),
                keepalive_before_first_token: true,
                ..StreamingConfig::default()
            },
            enabled(),
        ] {
            let (app, release, _) = server(config, provider, model, true);
            release.send(Err(ExecError::upstream(429, "busy"))).unwrap();
            let response = app.oneshot(request(path, payload, false)).await.unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{path}");
            assert_eq!(response.headers()["content-type"], "application/json");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(!String::from_utf8_lossy(&body).contains(HEARTBEAT));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_dropping_the_response_cancels_bootstrap_and_payload_waits() {
    for bootstrap in [true, false] {
        let (app, release, dropped) = server(enabled(), "openai", "m", bootstrap);
        let response = app
            .oneshot(request(
                "/v1/responses",
                r#"{"model":"m","stream":true}"#,
                false,
            ))
            .await
            .unwrap();
        assert!(!dropped.load(Ordering::SeqCst));
        drop(response);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(release.send(Err(ExecError::upstream(429, "busy"))).is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_disabled_waits_keep_the_http_error_and_raw_gemini_never_ticks() {
    for (path, payload, provider, model) in [
        (
            "/v1/messages",
            r#"{"model":"m","stream":true}"#,
            "claude",
            "m",
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"m","stream":true}"#,
            "openai",
            "m",
        ),
        (
            "/v1/completions",
            r#"{"model":"m","stream":true}"#,
            "openai",
            "m",
        ),
        (
            "/v1/responses",
            r#"{"model":"m","stream":true}"#,
            "openai",
            "m",
        ),
        (
            "/v1beta/interactions",
            r#"{"model":"m","stream":true}"#,
            "gemini-interactions",
            "m",
        ),
        (
            "/v1beta/models/m:streamGenerateContent",
            "{}",
            "gemini",
            "m",
        ),
        (
            "/v1/images/generations",
            r#"{"model":"gpt-image-2","prompt":"cat","stream":true}"#,
            "codex",
            "gpt-image-2",
        ),
        (
            "/v1/images/generations",
            r#"{"model":"compat-image","prompt":"cat","stream":true}"#,
            "compat",
            "compat-image",
        ),
        (
            "/v1/images/generations",
            r#"{"model":"grok-imagine-image","prompt":"cat","stream":true}"#,
            "xai",
            "grok-imagine-image",
        ),
        (
            "/v1beta/models/m:streamGenerateContent?alt=json",
            "{}",
            "gemini",
            "m",
        ),
    ] {
        let configs = if path.contains("alt=") {
            vec![enabled()]
        } else {
            vec![
                StreamingConfig {
                    keepalive: Some(TICK),
                    ..StreamingConfig::default()
                },
                StreamingConfig {
                    keepalive_before_first_token: true,
                    ..StreamingConfig::default()
                },
                StreamingConfig {
                    keepalive: Some(Duration::ZERO),
                    keepalive_before_first_token: true,
                    ..StreamingConfig::default()
                },
            ]
        };
        for config in configs {
            let (app, release, _) = server(config, provider, model, true);
            let mut waiting = Box::pin(app.oneshot(request(path, payload, false)));
            assert!(waiting.as_mut().now_or_never().is_none(), "{path}");
            tokio::time::advance(TICK).await;
            assert!(waiting.as_mut().now_or_never().is_none(), "{path}");
            release.send(Err(ExecError::upstream(429, "busy"))).unwrap();
            let response = waiting.await.unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{path}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(!String::from_utf8_lossy(&body).contains(HEARTBEAT));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_success_follows_the_heartbeat_for_each_protocol() {
    // The upstream bootstrap holds the first payload through pre-FT ticks:
    // sdk/api/handlers/handlers.go (ExecuteStreamBootstrap), stream_forwarder.go.
    let cases = [
        (
            "/v1/messages",
            r#"{"model":"m","messages":[],"stream":true}"#,
            "claude",
            "m",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
        (
            "/v1beta/models/m:streamGenerateContent",
            r#"{"contents":[]}"#,
            "gemini",
            "m",
            r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}]}"#,
            "",
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"m","messages":[],"stream":true}"#,
            "openai",
            "m",
            r#"{"choices":[{"delta":{"content":"hello"},"finish_reason":"stop"}]}"#,
            "",
        ),
        (
            "/v1beta/interactions",
            r#"{"model":"m","input":"hi","stream":true}"#,
            "gemini-interactions",
            "m",
            "event: interaction.complete\ndata: {\"hello\":true}\n\n",
            "event: interaction.complete\ndata: {\"hello\":true}\n\n",
        ),
        (
            "/v1/images/generations",
            r#"{"model":"gpt-image-2","prompt":"cat","stream":true}"#,
            "codex",
            "gpt-image-2",
            "event: image_generation.completed\ndata: {\"b64_json\":\"AA==\"}\n\n",
            "event: image_generation.completed\ndata: {\"b64_json\":\"AA==\"}\n\n",
        ),
        (
            "/v1/images/edits",
            r#"{"model":"compat-image","prompt":"cat","image":"data:image/png;base64,AA==","stream":true}"#,
            "compat",
            "compat-image",
            "event: image_edit.completed\ndata: {\"b64_json\":\"AA==\"}\n\n",
            "event: image_edit.completed\ndata: {\"b64_json\":\"AA==\"}\n\n",
        ),
    ];
    for (path, payload, provider, model, first, expected) in cases {
        for bootstrap in [true, false] {
            let (app, release, dropped) = server(enabled(), provider, model, bootstrap);
            let response = app.oneshot(request(path, payload, false)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = response.into_body();
            assert_eq!(
                body.frame().await.unwrap().unwrap().into_data().unwrap(),
                HEARTBEAT
            );
            release.send(Ok(Bytes::from(first))).unwrap();
            let rest =
                String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
            let expected = if expected.is_empty() {
                format!(
                    "data: {first}\n\n{}",
                    if provider == "openai" {
                        "data: [DONE]\n\n"
                    } else {
                        ""
                    }
                )
            } else {
                expected.to_owned()
            };
            assert_eq!(rest, expected, "{path}, bootstrap={bootstrap}");
            assert!(dropped.load(Ordering::SeqCst));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pre_first_token_xai_images_success_follows_the_heartbeat() {
    for path in ["/v1/images/generations", "/v1/images/edits"] {
        let (app, release, _) = server(enabled(), "xai", "grok-imagine-image", true);
        let response = app.oneshot(request(path,
            r#"{"model":"grok-imagine-image","prompt":"cat","image":"data:image/png;base64,AA==","stream":true}"#,
            false)).await.unwrap();
        let mut body = response.into_body();
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            HEARTBEAT
        );
        release
            .send(Ok(Bytes::from_static(br#"{"data":[{"b64_json":"AA=="}]}"#)))
            .unwrap();
        let rest = String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
        let prefix = if path.ends_with("edits") {
            "image_edit"
        } else {
            "image_generation"
        };
        assert_eq!(
            rest,
            format!(
                "event: {prefix}.completed\ndata: {{\"type\":\"{prefix}.completed\",\"b64_json\":\"AA==\"}}\n\n"
            )
        );
    }
}
