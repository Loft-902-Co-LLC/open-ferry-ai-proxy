//! The executors' payload tests, against a mock on 127.0.0.1 that records
//! each request's body: ported from upstream's
//! internal/runtime/executor/gemini_executor_test.go,
//! openai_compat_executor_compact_test.go, payload_barrier_test.go and
//! payload_review_test.go, with tests of our own for the Claude and Codex
//! executors and for token counts. Upstream's Claude tests run with
//! cloaking, which adds a system prompt and `metadata`; here the client
//! sends them.
//!
//! Upstream tests not ported:
//! - `TestClaudeExecutorPayloadOverrideDisabledThinking` and
//!   `TestClaudeExecutorPayloadOverrideReenablesThinking` run with cloaking,
//!   and test the `context_management` it adds, which isn't ported;
//!   `claude_payload_override_reaches_messages` tests that the rules apply.
//! - `TestOpenAICompatExecutorPromptCacheKeyCallerValueWinsPayloadOverride`
//!   tests `applyPromptCacheKey`, which derives a `prompt_cache_key` and
//!   isn't ported.
//! - `TestPayloadBarrierAntigravityRebuiltAttempts`,
//!   `TestPayloadBarrierAIStudio` and `TestAIStudioCountPayloadAfterCleanup`
//!   test Antigravity and AI Studio, which aren't ported.
//! - `TestCodexDuplexSteerPayloadBarrier` tests steering a Codex WebSocket
//!   response (`response.steer`), which isn't ported.
//! - `TestPayloadBarrierXAIWebsocketRetry` is `crate::xai::websocket`'s,
//!   and the WebSocket half of `TestPayloadBarrierCodexImageFilter` is
//!   `crate::codex::websocket`'s.

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

/// `TestGeminiExecutorAppliesPayloadRulesAfterLeadingUserNormalization`.
#[tokio::test]
async fn gemini_applies_payload_rules_after_leading_user_normalization() {
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
    // The rule targets the leading user turn the executor added.
    assert_eq!(
        contents[0]["parts"][0]["text"], "payload override",
        "{body}"
    );
    assert_eq!(contents[1]["role"], "model", "{body}");
    assert_eq!(contents[1]["parts"][0]["text"], "prior output", "{body}");
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

/// The Messages answer the Claude tests' mock gives a call.
const CLAUDE_MESSAGE: &str = r#"{"id":"msg_test","type":"message","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

/// The Messages stream the Claude tests' mock gives a stream.
const CLAUDE_EVENTS: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Upstream's `executeClaudeContextManagementRequest`, without cloaking:
/// the body the Claude executor sends for a Claude client's `payload`, with
/// the rules of the config document `yaml`, as a call or a stream.
async fn claude_body(yaml: &str, payload: &str, stream: bool) -> Value {
    let (content_type, reply) = if stream {
        ("text/event-stream", CLAUDE_EVENTS)
    } else {
        ("application/json", CLAUDE_MESSAGE)
    };
    let mock = Mock::start(content_type, reply).await;
    let executor = crate::claude::ClaudeExecutor::new("direct").with_config(config(yaml));
    call(
        &executor,
        auth("claude", &mock.url),
        request("claude-opus-5", payload),
        options(&Format::CLAUDE),
        stream,
    )
    .await;
    mock.last()
}

/// `TestPayloadBarrierClaudeDoesNotReplayFiltersOrReinjectIdentity`: what
/// a filter removes stays removed, and a filter of a list's first item
/// removes one item.
#[tokio::test]
async fn claude_barrier_does_not_replay_filters_or_reinject_identity() {
    let yaml = r#"
payload:
  filter:
    - models:
        - name: "*"
      params:
        - system
        - metadata
        - diagnostics
        - context_management
        - messages.0
  override:
    - models:
        - name: "*"
      params:
        max_tokens: 1
"#;
    let payload = r#"{"model":"claude-opus-5","system":"be brief","metadata":{"user_id":"caller"},"thinking":{"type":"adaptive"},"messages":[{"role":"user","content":"first"},{"role":"assistant","content":"second"},{"role":"user","content":"third"}]}"#;
    for stream in [false, true] {
        let body = claude_body(yaml, payload, stream).await;
        for path in ["system", "metadata", "diagnostics", "context_management"] {
            assert!(body.get(path).is_none(), "{path} came back: {body}");
        }
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(2), "{body}");
        assert_eq!(body["max_tokens"], 1, "{body}");
    }
}

/// `TestClaudePayloadConditionsEvaluateOnceAtFinalBarrier`: each rule's
/// conditions read the body as the rules before it left it, and a filter
/// applies once.
#[tokio::test]
async fn claude_conditions_evaluate_once_at_the_final_barrier() {
    let yaml = r#"
payload:
  override:
    - models:
        - name: "*"
          match:
            - max_tokens: 100
      params:
        max_tokens: 200
        temperature: 0.2
        diagnostics:
          user: true
    - models:
        - name: "*"
          match:
            - max_tokens: 200
      params:
        top_p: 0.4
  filter:
    - models:
        - name: "*"
          match:
            - max_tokens: 200
      params:
        - messages.0
"#;
    let payload = r#"{"model":"claude-opus-5","max_tokens":100,"messages":[{"role":"user","content":"first"},{"role":"assistant","content":"second"},{"role":"user","content":"third"}]}"#;
    for stream in [false, true] {
        let body = claude_body(yaml, payload, stream).await;
        assert_eq!(body["max_tokens"], 200, "stream={stream}: {body}");
        assert_eq!(body["temperature"], 0.2, "stream={stream}: {body}");
        assert_eq!(body["top_p"], 0.4, "stream={stream}: {body}");
        assert_eq!(body["diagnostics"]["user"], true, "stream={stream}: {body}");
        assert_eq!(
            body["messages"].as_array().map(Vec::len),
            Some(2),
            "stream={stream}: {body}"
        );
    }
}

/// `TestClaudePayloadConditionsObserveBuiltinThinkingRemoval`: the rules'
/// conditions see the body after the executor drops `thinking` for a forced
/// tool choice.
#[tokio::test]
async fn claude_conditions_observe_builtin_thinking_removal() {
    let rule = |kind: &str, field: &str, value: &str| {
        format!(
            "    - models:\n        - name: \"*\"\n          {kind}:\n            - thinking\n      params:\n        {field}: {value}\n"
        )
    };
    let mut yaml = String::from("payload:\n");
    for (section, prefix, value) in [
        ("default", "default", "true"),
        ("default-raw", "default_raw", "'true'"),
        ("override", "override", "true"),
        ("override-raw", "override_raw", "'true'"),
    ] {
        yaml.push_str(&format!("  {section}:\n"));
        yaml.push_str(&rule("not-exist", &format!("{prefix}_matched"), value));
        yaml.push_str(&rule("exist", &format!("{prefix}_unmatched"), value));
    }
    yaml.push_str("  filter:\n");
    for (kind, field) in [("not-exist", "metadata"), ("exist", "system")] {
        yaml.push_str(&format!(
            "    - models:\n        - name: \"*\"\n          {kind}:\n            - thinking\n      params:\n        - {field}\n"
        ));
    }
    let payload = r#"{"model":"claude-opus-5","max_tokens":100,"system":"be brief","metadata":{"user_id":"caller"},"thinking":{"type":"adaptive"},"tool_choice":{"type":"any"},"tools":[{"name":"lookup","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":"hi"}]}"#;
    for stream in [false, true] {
        let body = claude_body(&yaml, payload, stream).await;
        assert!(
            body.get("thinking").is_none(),
            "the forced tool kept thinking: {body}"
        );
        for prefix in ["default", "default_raw", "override", "override_raw"] {
            assert_eq!(
                body[format!("{prefix}_matched")],
                true,
                "{prefix} (stream={stream}): {body}"
            );
            assert!(
                body.get(format!("{prefix}_unmatched")).is_none(),
                "{prefix} matched the body before the executor's changes (stream={stream}): {body}"
            );
        }
        assert!(body.get("metadata").is_none(), "stream={stream}: {body}");
        assert!(body.get("system").is_some(), "stream={stream}: {body}");
    }
}

/// `TestPayloadBarrierCodexImageFilter`, over HTTP: a filter removes the
/// `image_generation` tool, the `instructions` and the `prompt_cache_key`
/// the executor sets after the translation, from a call and a stream.
#[tokio::test]
async fn codex_barrier_filter() {
    let completed = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_barrier\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n";
    let rules = config(
        r#"
payload:
  filter:
    - models:
        - name: gpt-5.6-sol
      params:
        - 'tools.#(type=="image_generation")#'
        - instructions
        - prompt_cache_key
"#,
    );
    for stream in [false, true] {
        for model in ["gpt-5.6-sol", "gpt-5.6-luna"] {
            let mock = Mock::start("text/event-stream", completed).await;
            let executor =
                crate::codex::CodexExecutor::new("direct").with_config(Arc::clone(&rules));
            call(
                &executor,
                auth("codex", &mock.url),
                request(
                    model,
                    r#"{"input":"hello","prompt_cache_key":"injected-cache"}"#,
                ),
                options(&Format::OPENAI_RESPONSE),
                stream,
            )
            .await;
            let body = mock.last();
            let filtered = model == "gpt-5.6-sol";
            let case = format!("{model} stream={stream}: {body}");
            let image = body
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| {
                    tools.iter().any(|tool| {
                        tool.get("type").and_then(Value::as_str) == Some("image_generation")
                    })
                });
            assert_eq!(image, !filtered, "image tool presence: {case}");
            assert_eq!(body.get("instructions").is_none(), filtered, "{case}");
            assert_eq!(body.get("prompt_cache_key").is_none(), filtered, "{case}");
        }
    }
}

/// Not upstream's: a token count's body has the rules applied last, as
/// upstream's `CountTokens` do. Claude's count matches a rule against the
/// body without the `metadata` it drops, and Gemini's sends what a rule
/// writes back into the fields it drops; the local counts of the
/// OpenAI-compatible executor and xAI count what a rule writes.
#[tokio::test]
async fn token_counts_apply_the_rules_last() {
    // Claude's own count, for a credential without a base URL.
    let mock = Mock::start("application/json", r#"{"input_tokens":3}"#).await;
    let executor = crate::claude::ClaudeExecutor::new("direct")
        .with_base_url(mock.url.clone())
        .with_config(config(
            r#"
payload:
  override:
    - models:
        - name: claude-opus-5
          not-exist:
            - metadata
      params:
        messages.0.content: counted
"#,
        ));
    let mut key = Auth {
        provider: "claude".into(),
        ..Auth::default()
    };
    key.attributes.insert("api_key".into(), "test".into());
    executor
        .count_tokens(
            Arc::new(key),
            request(
                "claude-opus-5",
                r#"{"model":"claude-opus-5","metadata":{"user_id":"caller"},"messages":[{"role":"user","content":"hi"}]}"#,
            ),
            options(&Format::CLAUDE),
        )
        .await
        .expect("the Claude count failed");
    let body = mock.last();
    assert_eq!(body["messages"][0]["content"], "counted", "{body}");
    assert!(body.get("metadata").is_none(), "{body}");

    // Gemini and Vertex AI drop the generation config from a count.
    let rules = config(
        r#"
payload:
  override:
    - models:
        - name: gemini-3.7-flash
      params:
        generationConfig.temperature: 0.2
"#,
    );
    let gemini = crate::gemini::GeminiExecutor::new("direct").with_config(Arc::clone(&rules));
    let vertex = crate::gemini::VertexExecutor::new("direct").with_config(Arc::clone(&rules));
    for (provider, executor) in [
        ("gemini", &gemini as &dyn ProviderExecutor),
        ("vertex", &vertex),
    ] {
        let mock = Mock::start("application/json", r#"{"totalTokens":1}"#).await;
        executor
            .count_tokens(
                auth(provider, &mock.url),
                request(
                    "gemini-3.7-flash",
                    r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"temperature":1}}"#,
                ),
                options(&Format::GEMINI),
            )
            .await
            .unwrap_or_else(|error| panic!("the {provider} count failed: {error:?}"));
        let body = mock.last();
        assert_eq!(
            body["generationConfig"]["temperature"], 0.2,
            "{provider}: {body}"
        );
    }

    // A rule that writes the counted text counts as the same text sent.
    let compat = |yaml: &str| {
        crate::openai_compat::OpenAiCompatExecutor::new("openai-compatibility", config(yaml))
    };
    let rewrite = "payload:\n  override:\n    - models:\n        - name: custom-openai\n      params:\n        messages.0.content: hello there world\n";
    let chat = |text: &str| {
        request(
            "custom-openai",
            &format!(
                r#"{{"model":"custom-openai","messages":[{{"role":"user","content":"{text}"}}]}}"#
            ),
        )
    };
    let unconfigured = "payload: {}\n";
    let by_rule = count(&compat(rewrite), chat("x"), &Format::OPENAI).await;
    let sent = count(
        &compat(unconfigured),
        chat("hello there world"),
        &Format::OPENAI,
    )
    .await;
    let unrewritten = count(&compat(unconfigured), chat("x"), &Format::OPENAI).await;
    assert_eq!(by_rule, sent);
    assert_ne!(by_rule, unrewritten);

    let xai = |yaml: &str| crate::xai::XaiExecutor::new("direct").with_config(config(yaml));
    let rewrite = "payload:\n  override:\n    - models:\n        - name: grok-4.3\n      params:\n        input: hello there world\n";
    let responses = |text: &str| {
        request(
            "grok-4.3",
            &format!(r#"{{"model":"grok-4.3","input":"{text}"}}"#),
        )
    };
    let by_rule = count(&xai(rewrite), responses("x"), &Format::OPENAI_RESPONSE).await;
    let sent = count(
        &xai(unconfigured),
        responses("hello there world"),
        &Format::OPENAI_RESPONSE,
    )
    .await;
    let unrewritten = count(&xai(unconfigured), responses("x"), &Format::OPENAI_RESPONSE).await;
    assert_eq!(by_rule, sent);
    assert_ne!(by_rule, unrewritten);
}

/// The answer to a token count through `executor`, which counts locally.
async fn count(executor: &dyn ProviderExecutor, request: Request, format: &Format) -> String {
    let response = executor
        .count_tokens(Arc::new(Auth::default()), request, options(format))
        .await
        .expect("the count failed");
    String::from_utf8_lossy(&response.payload).into_owned()
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
