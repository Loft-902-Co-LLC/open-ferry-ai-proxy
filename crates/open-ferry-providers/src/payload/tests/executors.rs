//! The executors' payload tests, against a mock on 127.0.0.1 that records
//! each request's body: ported from upstream's
//! internal/runtime/executor/gemini_executor_test.go and
//! openai_compat_executor_compact_test.go, with tests of our own for the
//! Claude and Codex executors.
//!
//! Upstream tests not ported:
//! - `TestClaudeExecutorPayloadOverrideDisabledThinking` and
//!   `TestClaudeExecutorPayloadOverrideReenablesThinking` run with cloaking,
//!   and test the `context_management` it adds, which isn't ported;
//!   `claude_payload_override_reaches_messages` tests that the rules apply.
//! - `TestOpenAICompatExecutorPromptCacheKeyCallerValueWinsPayloadOverride`
//!   tests `applyPromptCacheKey`, which derives a `prompt_cache_key` and
//!   isn't ported.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use bytes::Bytes;
use futures_util::StreamExt as _;
use http::HeaderValue;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_core::executor::ProviderExecutor;
use serde_json::Value;

/// A mock provider bound to an ephemeral port on 127.0.0.1, answering every
/// request with one content type and body, and recording each body.
struct Mock {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl Mock {
    async fn start(content_type: &'static str, reply: &str) -> Self {
        let reply = reply.to_owned();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&bodies);
        let app = Router::new().fallback(move |body: Bytes| {
            let reply = reply.clone();
            let recorder = Arc::clone(&recorder);
            async move {
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                ([(http::header::CONTENT_TYPE, content_type)], reply)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, bodies }
    }

    /// The body of the last request.
    fn last(&self) -> Value {
        self.bodies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last()
            .cloned()
            .expect("no request reached the mock")
    }
}

fn config(yaml: &str) -> Arc<Config> {
    Arc::new(Config::parse(yaml).expect("config parses"))
}

/// An API key credential of `provider` for `base_url`.
fn auth(provider: &str, base_url: &str) -> Arc<Auth> {
    let mut auth = Auth {
        provider: provider.into(),
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), "test".into());
    Arc::new(auth)
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

/// Runs a call, as a stream or not, through `executor`.
async fn call(
    executor: &dyn ProviderExecutor,
    auth: Arc<Auth>,
    request: Request,
    options: Options,
    stream: bool,
) {
    if stream {
        let response = executor.execute_stream(auth, request, options).await;
        let mut chunks = response.expect("the stream didn't start").chunks;
        while let Some(chunk) = chunks.next().await {
            chunk.expect("a stream error");
        }
    } else {
        executor
            .execute(auth, request, options)
            .await
            .expect("the call failed");
    }
}

/// `TestGeminiExecutorAppliesPayloadRulesBeforeLeadingUserNormalization`.
#[tokio::test]
async fn gemini_applies_payload_rules_before_leading_user_normalization() {
    let mock = Mock::start(
        "application/json",
        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#,
    )
    .await;
    let executor = crate::gemini::GeminiExecutor::new("direct").with_config(config(
        r#"
payload:
  override:
    - models:
        - name: gemini-3.7-flash
          protocol: gemini
      params:
        contents.0.parts.0.text: payload override
"#,
    ));
    call(
        &executor,
        auth("gemini", &mock.url),
        request(
            "gemini-3.7-flash",
            r#"{"contents":[{"role":"model","parts":[{"text":"prior output"}]},{"role":"user","parts":[{"text":"continue"}]}]}"#,
        ),
        options(&Format::GEMINI),
        false,
    )
    .await;
    let body = mock.last();
    let contents = body["contents"].as_array().expect("contents");
    assert_eq!(contents.len(), 3, "{body}");
    assert_eq!(contents[0]["role"], "user", "{body}");
    assert_eq!(contents[0]["parts"][0]["text"], "", "{body}");
    assert_eq!(contents[1]["role"], "model", "{body}");
    assert_eq!(
        contents[1]["parts"][0]["text"], "payload override",
        "{body}"
    );
}

/// `TestOpenAICompatExecutorPayloadOverrideWinsOverThinkingSuffix`.
#[tokio::test]
async fn openai_compat_payload_override_wins_over_thinking_suffix() {
    let mock = Mock::start(
        "application/json",
        r#"{"id":"chatcmpl_1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    )
    .await;
    let executor = crate::openai_compat::OpenAiCompatExecutor::new(
        "openai-compatibility",
        config(
            r#"
payload:
  override:
    - models:
        - name: custom-openai
          protocol: openai
      params:
        reasoning_effort: low
"#,
        ),
    );
    call(
        &executor,
        auth("openai-compatibility", &format!("{}/v1", mock.url)),
        request(
            "custom-openai(high)",
            r#"{"model":"custom-openai(high)","messages":[{"role":"user","content":"hi"}]}"#,
        ),
        options(&Format::OPENAI),
        false,
    )
    .await;
    let body = mock.last();
    assert_eq!(body["reasoning_effort"], "low", "{body}");
}

/// Not upstream's: the Claude executor applies the rules to a call and a
/// stream, the rules matching the client's protocol and headers.
#[tokio::test]
async fn claude_payload_override_reaches_messages() {
    let message = r#"{"id":"msg_01","type":"message","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
    let events = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let rules = config(
        r#"
payload:
  override:
    - models:
        - name: claude-opus-5
          protocol: claude
          from-protocol: claude
          headers:
            X-Tenant: blue
      params:
        thinking.type: disabled
  filter:
    - models:
        - name: claude-*
          protocol: claude
      params:
        - metadata.note
"#,
    );
    let payload = r#"{"model":"claude-opus-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"metadata":{"note":"x","user_id":"caller"}}"#;
    for (stream, content_type, reply) in [
        (false, "application/json", message),
        (true, "text/event-stream", events),
    ] {
        let mock = Mock::start(content_type, reply).await;
        let executor = crate::claude::ClaudeExecutor::new("direct").with_config(Arc::clone(&rules));
        let mut with_tenant = options(&Format::CLAUDE);
        with_tenant
            .headers
            .insert("x-tenant", HeaderValue::from_static("blue"));
        call(
            &executor,
            auth("claude", &mock.url),
            request("claude-opus-5", payload),
            with_tenant,
            stream,
        )
        .await;
        let body = mock.last();
        assert_eq!(body["thinking"]["type"], "disabled", "{body}");
        assert_eq!(
            body["metadata"],
            serde_json::json!({"user_id": "caller"}),
            "{body}"
        );

        call(
            &executor,
            auth("claude", &mock.url),
            request("claude-opus-5", payload),
            options(&Format::CLAUDE),
            stream,
        )
        .await;
        let body = mock.last();
        assert!(body.get("thinking").is_none(), "{body}");
    }
}

/// Not upstream's: the Codex executors check a default rule's path in the
/// client's request as they translate it, and skip a Codex client's integer
/// pass.
#[tokio::test]
async fn codex_defaults_check_the_client_request() {
    let completed = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[]}}\n\n";
    let mock = Mock::start("text/event-stream", completed).await;
    let executor = crate::codex::CodexExecutor::new("direct").with_config(config(
        r#"
payload:
  default:
    - models:
        - name: gpt-*
          protocol: codex
      params:
        text.verbosity: low
"#,
    ));
    let tools = r#""tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"yield_time_ms":{"type":"number"}}}}]"#;
    for (payload, want) in [
        (
            format!(r#"{{"model":"gpt-5","input":"hi","text":{{"verbosity":"high"}},{tools}}}"#),
            "high",
        ),
        (
            format!(r#"{{"model":"gpt-5","input":"hi",{tools}}}"#),
            "low",
        ),
    ] {
        let mut options = options(&Format::OPENAI_RESPONSE);
        options
            .headers
            .insert("user-agent", HeaderValue::from_static("codex_cli_rs/0.1"));
        call(
            &executor,
            auth("codex", &mock.url),
            request("gpt-5", &payload),
            options,
            true,
        )
        .await;
        let body = mock.last();
        assert_eq!(body["text"]["verbosity"], want, "{body}");
        assert_eq!(
            body["tools"][0]["parameters"]["properties"]["yield_time_ms"]["type"], "number",
            "{body}"
        );
    }
}
