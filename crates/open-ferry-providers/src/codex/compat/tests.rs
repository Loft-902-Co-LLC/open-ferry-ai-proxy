//! Ported from CLIProxyAPI internal/runtime/executor/
//! codex_executor_spawn_agent_test.go, helps/codex_multi_agent_v2_test.go,
//! helps/codex_multi_agent_v2_summary_test.go, and the translation test of
//! internal/client/codex/optimize-multi-agent-v2/orphan_delegation_test.go
//! (v8.0.10, MIT).
//!
//! The executor tests run the Codex executor against a mock on 127.0.0.1
//! that names, in its reply, the namespace the request's first tool went
//! out under, as upstream's does.
//!
//! Changed:
//! - Upstream's executor tests send the Codex client's `User-Agent` in the
//!   Gin request and another in the call's headers, and the Gin request's
//!   wins. The executor here has only the call's headers, which are the
//!   client's, so the tests send the Codex client's `User-Agent` there.
//! - `TestCodexExecutorOptimizeMultiAgentV2` checks that `spawn_agent`'s
//!   description is left as the client sent it, in place of the model list
//!   step 2 writes there; step 2 isn't ported.
//! - `TestTranslateRequestCompatibilityForExecutorToolIntegerTypes` runs the
//!   Codex targets through the Codex executor's translation, with and
//!   without a compatibility model, and the other targets through what the
//!   other executors do before they translate, without one: they don't
//!   resolve compatibility models (see the module docs). It doesn't check
//!   that the payload and headers are left alone, as both are borrowed.
//! - `TestCompatibilityTranslationPreservesExplicitClaudeVisibility` runs
//!   without a compatibility model only, for the same reason.
//!
//! Dropped:
//! - `TestCodexExecutorsMultiAgentV2UsesSelectedHomeModel`: the Home
//!   service isn't ported.
//! - `TestCodexExecutorOptimizeMultiAgentV2`'s model descriptions (step 2).
//! - `TestNormalizesCodexToolTypes`' Gemini half: there is no Responses to
//!   Gemini translator here.
//! - `TestTranslateRequestPair*`, `TestSameByteSlice` and
//!   `TestTranslateRequestEnvelopePairWithCodexMultiAgentV2UsesModelInfo`:
//!   the executors here translate one payload, and Antigravity and model
//!   capabilities in requests aren't ported.
//! - `TestTranslateRequestWithAPIKeyModelCompatibility_InvokesPluginNormalizers`:
//!   the translator plugin hooks aren't ported.
//!
//! Added: how compatibility models are found, orphan delegation in the
//! executor and in token counts, the other executors without a config, and
//! the Claude, Gemini, Vertex AI and OpenAI-compatible executors readying a
//! Codex client's request.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::http::Uri;
use bytes::Bytes;
use futures_util::StreamExt as _;
use http::HeaderValue;
use open_ferry_core::config::CodexModel;
use open_ferry_core::exec::Response;
use open_ferry_core::executor::ProviderExecutor;
use serde_json::{Value, json};

use super::*;
use crate::codex::CodexExecutor;
use crate::json::{exists, get, str_at};

const CODEX_TUI: &str = "codex-tui/0.154.0";
const CODEX_DESKTOP: &str = "Codex Desktop/0.154.0";
const COMPACT_ALT: &str = "responses/compact";
const COMPLETED_EMPTY: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[]}}\n\n";

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// What the mock answers a request to `path` with `body`: a content type
/// and a body.
type Answer = Arc<dyn Fn(&str, &Value) -> (&'static str, String) + Send + Sync>;

/// A mock Codex bound to an ephemeral port on 127.0.0.1, which records the
/// body of each request.
struct Mock {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl Mock {
    async fn start(
        answer: impl Fn(&str, &Value) -> (&'static str, String) + Send + Sync + 'static,
    ) -> Self {
        let answer: Answer = Arc::new(answer);
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let recorder = bodies.clone();
        let app = Router::new().fallback(move |uri: Uri, body: Bytes| {
            let answer = answer.clone();
            let recorder = recorder.clone();
            async move {
                let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let (content_type, reply) = answer(uri.path(), &body);
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(body);
                ([(http::header::CONTENT_TYPE, content_type)], reply)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, bodies }
    }

    /// A mock that completes every request without output.
    async fn completing() -> Self {
        Self::start(|_, _| ("text/event-stream", COMPLETED_EMPTY.to_owned())).await
    }

    /// A mock that calls `spawn_agent` in the namespace the request's first
    /// input item's first tool has.
    async fn echoing_namespace() -> Self {
        Self::start(|path, body| {
            let namespace = Value::from(str_at(body, "input.0.tools.0.name"));
            if path == "/responses/compact" {
                let reply = format!(
                    r#"{{"id":"resp_1","object":"response.compaction","output":[{{"type":"function_call","name":"spawn_agent","namespace":{namespace},"arguments":"{{}}","call_id":"call_1"}}]}}"#
                );
                return ("application/json", reply);
            }
            let reply = format!(
                "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{{\"type\":\"function_call\",\"name\":\"spawn_agent\",\"namespace\":{namespace},\"arguments\":\"{{}}\",\"call_id\":\"call_1\"}}]}}}}\n\n"
            );
            ("text/event-stream", reply)
        })
        .await
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

/// A `codex-api-key` entry for `base_url` with key `test`.
fn codex_key(base_url: &str, models: &[(&str, &str, bool)]) -> CodexKey {
    CodexKey {
        api_key: "test".into(),
        base_url: base_url.into(),
        models: models
            .iter()
            .map(|&(name, alias, is_compat)| CodexModel {
                name: name.into(),
                alias: alias.into(),
                is_compat,
                ..CodexModel::default()
            })
            .collect(),
        ..CodexKey::default()
    }
}

fn config(optimize: bool, keys: Vec<CodexKey>) -> Config {
    let mut config = Config::default();
    config.client.codex.optimize_multi_agent_v2 = optimize;
    config.codex_api_key = keys;
    config
}

/// An API key credential of provider `codex` for `base_url`.
fn auth(base_url: &str) -> Auth {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), "test".into());
    auth
}

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_owned()),
    }
}

fn options(format: &str, headers: &[(&'static str, &str)]) -> Options {
    let mut options = Options::new(Format::from(format.to_owned()));
    for &(name, value) in headers {
        options
            .headers
            .append(name, HeaderValue::from_str(value).unwrap());
    }
    options
}

fn context<'a>(config: &'a Config, auth: &'a Auth) -> Context<'a> {
    Context {
        auth: Some(auth),
        config: Some(config),
        models: None,
    }
}

/// The payload of upstream's `codexSpawnAgentTestPayload`.
const SPAWN_AGENT_PAYLOAD: &str = r#"{
    "model":"gpt-5.4",
    "input":[{
        "type":"additional_tools",
        "role":"developer",
        "tools":[{
            "type":"namespace",
            "name":"collaboration",
            "tools":[{
                "type":"function",
                "name":"spawn_agent",
                "description":"Available model overrides (optional; inherited parent model is preferred):\n- old-model\nSpawns an agent.",
                "parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}
            }]
        }]
    },{
        "type":"agent_message",
        "id":"amsg_1",
        "author":"/root",
        "recipient":"/root/worker",
        "content":[
            {"type":"input_text","text":"Payload:\n"},
            {"type":"encrypted_content","encrypted_content":"delegated task"}
        ],
        "internal_chat_message_metadata_passthrough":{"turn_id":"turn_1"}
    }]
}"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Execute,
    Stream,
    Compact,
}

/// Calls the executor in `mode` and returns what the client gets.
async fn call(
    executor: &CodexExecutor,
    auth: Auth,
    request: Request,
    mut options: Options,
    mode: Mode,
) -> String {
    let auth = Arc::new(auth);
    match mode {
        Mode::Stream => {
            options.stream = true;
            let response = executor.execute_stream(auth, request, options).await;
            let mut chunks = response.expect("the stream didn't start").chunks;
            let mut text = String::new();
            while let Some(chunk) = chunks.next().await {
                text.push_str(&String::from_utf8_lossy(&chunk.expect("a stream error")));
                text.push('\n');
            }
            text
        }
        Mode::Execute | Mode::Compact => {
            if mode == Mode::Compact {
                options.alt = COMPACT_ALT.into();
            }
            let response: Response = executor.execute(auth, request, options).await.unwrap();
            String::from_utf8_lossy(&response.payload).into_owned()
        }
    }
}

/// The JSON of each line of what a client got, without any `data:` prefix.
fn events(text: &str) -> Vec<Value> {
    text.lines()
        .map(|line| line.strip_prefix("data:").unwrap_or(line).trim())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// `assertCodexSpawnAgentClientNamespace`.
fn assert_client_namespace(payload: &str) {
    assert!(
        !payload.contains("collaboration-optimize"),
        "optimized namespace leaked to client: {payload}"
    );
    assert!(
        payload.contains(r#""namespace":"collaboration""#),
        "restored collaboration namespace missing from client payload: {payload}"
    );
}

/// `assertCodexSpawnAgentRequestMessage`.
fn assert_request_message(body: &Value, enabled: bool) {
    let message = &body["input"][1];
    assert_eq!(message["type"], "agent_message", "{body}");
    assert!(!exists(message, "role"), "{body}");
    assert_eq!(message["author"], "/root", "{body}");
    assert_eq!(message["recipient"], "/root/worker", "{body}");
    assert_eq!(
        message["internal_chat_message_metadata_passthrough"]["turn_id"], "turn_1",
        "{body}"
    );
    if enabled {
        assert_eq!(message["content"][1]["type"], "input_text", "{body}");
        assert_eq!(message["content"][1]["text"], "delegated task", "{body}");
        assert!(!exists(message, "content.1.encrypted_content"), "{body}");
        return;
    }
    assert_eq!(message["content"][1]["type"], "encrypted_content", "{body}");
    assert_eq!(
        message["content"][1]["encrypted_content"], "delegated task",
        "{body}"
    );
}

/// `assertCodexSpawnAgentOptimization`, without step 2's model list.
fn assert_optimization(body: &Value, enabled: bool) {
    let namespace = str_at(body, "input.0.tools.0.name");
    let description = str_at(body, "input.0.tools.0.tools.0.description");
    let encrypted = get(
        body,
        "input.0.tools.0.tools.0.parameters.properties.message.encrypted",
    );
    assert!(description.contains("- old-model"), "{description:?}");
    if enabled {
        assert_eq!(namespace, "collaboration-optimize");
        assert!(encrypted.is_none(), "message encrypted was not removed");
        return;
    }
    assert_eq!(namespace, "collaboration");
    assert_eq!(encrypted, Some(&json!(true)));
}

/// Checks that `message` is the agent message as a compatibility model gets
/// it: a user message without its routing metadata.
fn assert_compat_message(message: &Value) {
    assert_eq!(message["type"], "message", "{message}");
    assert_eq!(message["role"], "user", "{message}");
    for field in [
        "author",
        "recipient",
        "internal_chat_message_metadata_passthrough",
    ] {
        assert!(!exists(message, field), "{field} was kept: {message}");
    }
}

// TestCodexExecutorOptimizeMultiAgentV2

#[tokio::test]
async fn executor_optimizes_multi_agent_v2() {
    let mock = Mock::echoing_namespace().await;
    for mode in [Mode::Execute, Mode::Stream, Mode::Compact] {
        for enabled in [true, false] {
            let executor =
                CodexExecutor::new("direct").with_config(Arc::new(config(enabled, Vec::new())));
            let options = options("openai-response", &[("user-agent", CODEX_TUI)]);
            let client = call(
                &executor,
                auth(&mock.url),
                request("gpt-5.4", SPAWN_AGENT_PAYLOAD),
                options,
                mode,
            )
            .await;
            let body = mock.last();
            assert_optimization(&body, enabled);
            assert_request_message(&body, enabled);
            assert_client_namespace(&client);
        }
    }
}

#[tokio::test]
async fn executor_leaves_other_clients_alone() {
    let mock = Mock::echoing_namespace().await;
    let executor = CodexExecutor::new("direct").with_config(Arc::new(config(true, Vec::new())));
    let options = options("openai-response", &[("user-agent", "curl/8.7.1")]);
    call(
        &executor,
        auth(&mock.url),
        request("gpt-5.4", SPAWN_AGENT_PAYLOAD),
        options,
        Mode::Execute,
    )
    .await;
    let body = mock.last();
    assert_optimization(&body, false);
    assert_request_message(&body, false);
}

// TestCodexExecutorIsCompatConvertsAgentMessage

#[tokio::test]
async fn executor_converts_agent_message_for_compatibility_models() {
    let mock = Mock::completing().await;
    let keys = vec![codex_key(
        &mock.url,
        &[
            ("deepseek-v4-flash", "deepseek-alias", true),
            ("gpt-5.4", "codex-native", false),
        ],
    )];
    for (model, enabled, converted) in [
        ("deepseek-v4-flash", true, true),
        ("gpt-5.4", true, false),
        ("deepseek-v4-flash", false, true),
        ("gpt-5.4", false, false),
    ] {
        let executor =
            CodexExecutor::new("direct").with_config(Arc::new(config(enabled, keys.clone())));
        let options = options("openai-response", &[("user-agent", CODEX_TUI)]);
        call(
            &executor,
            auth(&mock.url),
            request(model, SPAWN_AGENT_PAYLOAD),
            options,
            Mode::Execute,
        )
        .await;
        let body = mock.last();
        let message = &body["input"][1];
        if converted {
            assert_eq!(message["type"], "message", "{model} {enabled}: {body}");
            assert_eq!(message["role"], "user", "{model} {enabled}: {body}");
            assert_eq!(message["content"][1]["type"], "input_text", "{body}");
            assert_eq!(message["content"][1]["text"], "delegated task", "{body}");
        } else {
            assert_eq!(message["type"], "agent_message", "{model} {enabled}");
            assert!(!exists(message, "role"), "{body}");
        }
    }
}

// TestCodexExecutor_IsCompat_StripsAuthorAndRecipient_Issue6136

#[tokio::test]
async fn executor_strips_routing_metadata_for_compatibility_models() {
    let mock = Mock::completing().await;
    let keys = vec![codex_key(
        &mock.url,
        &[
            ("compat-model", "compat-alias", true),
            ("native-model", "native-alias", false),
        ],
    )];
    for (mode, enabled) in [
        (Mode::Execute, true),
        (Mode::Stream, true),
        (Mode::Compact, true),
        (Mode::Execute, false),
    ] {
        let executor =
            CodexExecutor::new("direct").with_config(Arc::new(config(enabled, keys.clone())));
        let options = options("openai-response", &[("user-agent", CODEX_DESKTOP)]);
        call(
            &executor,
            auth(&mock.url),
            request("compat-model", SPAWN_AGENT_PAYLOAD),
            options,
            mode,
        )
        .await;
        assert_compat_message(&mock.last()["input"][1]);
    }

    let executor = CodexExecutor::new("direct").with_config(Arc::new(config(true, keys)));
    let options = options("openai-response", &[("user-agent", CODEX_DESKTOP)]);
    call(
        &executor,
        auth(&mock.url),
        request("native-model", SPAWN_AGENT_PAYLOAD),
        options,
        Mode::Execute,
    )
    .await;
    let body = mock.last();
    let message = &body["input"][1];
    assert_eq!(message["type"], "agent_message");
    assert_eq!(message["author"], "/root");
    assert_eq!(message["recipient"], "/root/worker");
    assert_eq!(
        message["internal_chat_message_metadata_passthrough"]["turn_id"],
        "turn_1"
    );
}

// TestCodexExecutor_IsCompat_V8Layout_ClientScope_Issue6233

#[tokio::test]
async fn executor_reads_the_historical_oauth_layout_as_client_wide() {
    let mock = Mock::completing().await;
    let mut parsed = Config::parse(
        "oauth: {providers: {codex: {optimize-multi-agent-v2: true, orphan-delegation-compatibility: true}}}",
    )
    .unwrap();
    parsed.codex_api_key = vec![codex_key(
        &mock.url,
        &[("compat-model", "compat-alias", true)],
    )];
    assert!(
        parsed.for_api_key().client.codex.optimize_multi_agent_v2,
        "API-key scoping cleared client optimization"
    );
    let executor = CodexExecutor::new("direct").with_config(Arc::new(parsed));
    for mode in [Mode::Execute, Mode::Stream] {
        let options = options(
            "openai-response",
            &[("user-agent", "Codex Desktop/0.158.0-alpha.2.1")],
        );
        call(
            &executor,
            auth(&mock.url),
            request("compat-model", SPAWN_AGENT_PAYLOAD),
            options,
            mode,
        )
        .await;
        assert_compat_message(&mock.last()["input"][1]);
    }
}

// TestCodexExecutorOptimizeMultiAgentV2RestoresDottedFlatToolName

#[tokio::test]
async fn executor_restores_dotted_flat_tool_name() {
    let mock = Mock::start(|_, _| {
        (
            "text/event-stream",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"name\":\"collaboration-optimize.spawn_agent\",\"namespace\":null,\"arguments\":\"{}\",\"call_id\":\"call_1\"}]}}\n\n".to_owned(),
        )
    })
    .await;
    let executor = CodexExecutor::new("direct").with_config(Arc::new(config(true, Vec::new())));

    let streamed = call(
        &executor,
        auth(&mock.url),
        request("gpt-5.4", SPAWN_AGENT_PAYLOAD),
        options("openai-response", &[("user-agent", CODEX_TUI)]),
        Mode::Stream,
    )
    .await;
    assert_client_namespace(&streamed);
    let completed = events(&streamed)
        .into_iter()
        .find(|event| event["type"] == "response.completed")
        .expect("no completed event");
    assert_eq!(completed["response"]["output"][0]["name"], "spawn_agent");

    let executed = call(
        &executor,
        auth(&mock.url),
        request("gpt-5.4", SPAWN_AGENT_PAYLOAD),
        options("openai-response", &[("user-agent", CODEX_TUI)]),
        Mode::Execute,
    )
    .await;
    assert_client_namespace(&executed);
    let executed = json(&executed);
    let name = get(&executed, "output.0.name").or_else(|| get(&executed, "response.output.0.name"));
    assert_eq!(name, Some(&json!("spawn_agent")), "{executed}");
}

const ORPHAN_PAYLOAD: &str = r#"{
    "model": "test-model",
    "stream": false,
    "input": [
        {
            "type": "function_call_output",
            "name": "create_thread",
            "namespace": "codex_app",
            "output": "<codex_delegation><message>handoff</message></codex_delegation>"
        },
        {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "please continue"}]
        }
    ]
}"#;

const ORPHAN_TEXT: &str = "Tool output from codex_app__create_thread:\n<codex_delegation><message>handoff</message></codex_delegation>";

fn orphan_config(enabled: bool) -> Config {
    let mut config = Config::default();
    config.codex.orphan_delegation_compatibility = enabled;
    config
}

#[tokio::test]
async fn executor_rewrites_orphan_delegation_for_sub_agents() {
    let mock = Mock::completing().await;
    for (enabled, subagent, rewritten) in [
        (true, Some("collab_spawn"), true),
        (true, None, false),
        (false, Some("collab_spawn"), false),
    ] {
        let executor = CodexExecutor::new("direct").with_config(Arc::new(orphan_config(enabled)));
        let mut headers = vec![("user-agent", CODEX_TUI)];
        headers.extend(subagent.map(|value| ("x-openai-subagent", value)));
        for mode in [Mode::Execute, Mode::Compact] {
            call(
                &executor,
                auth(&mock.url),
                request("gpt-5.4", ORPHAN_PAYLOAD),
                options("openai-response", &headers),
                mode,
            )
            .await;
            let body = mock.last();
            let item = &body["input"][0];
            if rewritten {
                assert_eq!(item["type"], "message", "{body}");
                assert_eq!(item["role"], "user", "{body}");
                assert_eq!(item["content"][0]["text"], ORPHAN_TEXT, "{body}");
            } else {
                assert_eq!(item["type"], "function_call_output", "{body}");
            }
        }
    }
}

#[test]
fn token_counts_rewrite_orphan_delegation() {
    let config = orphan_config(true);
    let auth = auth("http://127.0.0.1:1");
    let options = options("openai-response", &[("x-openai-subagent", "collab_spawn")]);
    let body = translate(
        Kind::CountTokens,
        context(&config, &auth),
        &request("gpt-5.4", ORPHAN_PAYLOAD),
        &options,
        &Format::CODEX,
        false,
        json(ORPHAN_PAYLOAD),
    );
    assert_eq!(body["input"][0]["type"], "message", "{body}");
    assert_eq!(body["input"][0]["content"][0]["text"], ORPHAN_TEXT);

    // The other calls rewrite it after translation, in `prepare`.
    let body = translate(
        Kind::Execute,
        context(&config, &auth),
        &request("gpt-5.4", ORPHAN_PAYLOAD),
        &options,
        &Format::CODEX,
        false,
        json(ORPHAN_PAYLOAD),
    );
    assert_eq!(body["input"][0]["type"], "function_call_output");
}

// TestTranslateRequestWithCodexMultiAgentV2OrphanDelegation

/// What another executor sends for `payload` (`TranslateRequestWithCodexMultiAgentV2`).
fn other_executor(config: Option<&Config>, options: &Options, to: &Format, payload: &str) -> Value {
    let mut payload = json(payload);
    before_translation(config, options, to, &mut payload);
    Registry::global().translate_request(&options.source_format, to, "test-model", payload, false)
}

#[test]
fn other_executors_rewrite_orphan_delegation() {
    let collab = [("x-openai-subagent", "collab_spawn")];
    let responses = Format::OPENAI_RESPONSE;

    let missing = other_executor(
        Some(&orphan_config(true)),
        &options("openai-response", &[]),
        &responses,
        ORPHAN_PAYLOAD,
    );
    assert_eq!(missing["input"][0]["type"], "function_call_output");

    let wrong = other_executor(
        Some(&orphan_config(true)),
        &options("openai-response", &[("x-openai-subagent", "other_agent")]),
        &responses,
        ORPHAN_PAYLOAD,
    );
    assert_eq!(wrong["input"][0]["type"], "function_call_output");

    let rewritten = other_executor(
        Some(&orphan_config(true)),
        &options("openai-response", &collab),
        &responses,
        ORPHAN_PAYLOAD,
    );
    assert_eq!(rewritten["input"][0]["type"], "message");
    assert_eq!(rewritten["input"][0]["role"], "user");
    assert_eq!(rewritten["input"][0]["content"][0]["text"], ORPHAN_TEXT);

    let chat = other_executor(
        Some(&orphan_config(true)),
        &options("openai-response", &collab),
        &Format::OPENAI,
        ORPHAN_PAYLOAD,
    );
    let messages = chat["messages"].as_array().unwrap();
    assert!(messages.len() >= 2, "{chat}");
    assert_eq!(messages[0]["role"], "user", "{chat}");
    let text = match &messages[0]["content"] {
        Value::Array(_) => str_at(&messages[0], "content.0.text"),
        content => content.as_str().unwrap_or_default().to_owned(),
    };
    assert_eq!(text, ORPHAN_TEXT);

    let disabled = other_executor(
        Some(&orphan_config(false)),
        &options("openai-response", &collab),
        &responses,
        ORPHAN_PAYLOAD,
    );
    assert_eq!(disabled["input"][0]["type"], "function_call_output");

    let interleaved = other_executor(
        Some(&orphan_config(true)),
        &options("openai-response", &collab),
        &responses,
        r#"{
            "model": "test-model",
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"test\"}"},
                {"type": "function_call_output", "call_id": "call_1", "name": "lookup", "output": "result1"},
                {"type": "function_call_output", "name": "create_thread", "namespace": "codex_app", "output": "delegated"}
            ]
        }"#,
    );
    assert_eq!(interleaved["input"][0]["type"], "function_call");
    assert_eq!(interleaved["input"][1]["type"], "function_call_output");
    assert_eq!(interleaved["input"][1]["call_id"], "call_1");
    assert_eq!(interleaved["input"][2]["type"], "message");
    assert_eq!(interleaved["input"][2]["role"], "user");
}

#[test]
fn other_executors_convert_agent_message_for_other_formats() {
    let config = config(true, Vec::new());
    let headers = [("user-agent", CODEX_TUI)];
    let claude = other_executor(
        Some(&config),
        &options("openai-response", &headers),
        &Format::CLAUDE,
        SPAWN_AGENT_PAYLOAD,
    );
    let text = claude.to_string();
    assert!(!text.contains("agent_message"), "{text}");
    assert!(!text.contains("encrypted_content"), "{text}");
    assert!(text.contains("delegated task"), "{text}");

    // A Responses target keeps it, as does a client that isn't Codex's.
    let responses = other_executor(
        Some(&config),
        &options("openai-response", &headers),
        &Format::OPENAI_RESPONSE,
        SPAWN_AGENT_PAYLOAD,
    );
    assert_eq!(responses["input"][1]["type"], "agent_message");
    let mut payload = json(SPAWN_AGENT_PAYLOAD);
    before_translation(
        Some(&config),
        &options("openai-response", &[("user-agent", "curl/8.7.1")]),
        &Format::CLAUDE,
        &mut payload,
    );
    assert_eq!(payload, json(SPAWN_AGENT_PAYLOAD));
}

// TestTranslateRequestCompatibilityForExecutorToolIntegerTypes

const RESPONSES_TOOLS: &str = r#"{"input":"hi","tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"yield_time_ms":{"type":"number"},"unrelated":{"type":"number"}}}}]}"#;
const CLAUDE_TOOLS: &str = r#"{"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"exec_command","input_schema":{"type":"object","properties":{"yield_time_ms":{"type":"number"},"unrelated":{"type":"number"}}}}]}"#;

#[test]
fn tool_integer_types_by_target_executor() {
    let routes = [
        (
            "responses_to_codex",
            Format::OPENAI_RESPONSE,
            Format::CODEX,
            RESPONSES_TOOLS,
            "tools.0.parameters.properties",
        ),
        (
            "claude_to_codex",
            Format::CLAUDE,
            Format::CODEX,
            CLAUDE_TOOLS,
            "tools.0.parameters.properties",
        ),
        (
            "responses_to_claude",
            Format::OPENAI_RESPONSE,
            Format::CLAUDE,
            RESPONSES_TOOLS,
            "tools.0.input_schema.properties",
        ),
        (
            "responses_passthrough",
            Format::OPENAI_RESPONSE,
            Format::OPENAI_RESPONSE,
            RESPONSES_TOOLS,
            "tools.0.parameters.properties",
        ),
    ];
    let auth = auth("http://127.0.0.1:1");
    for (route, from, to, payload, properties) in routes {
        for user_agent in ["codex_cli_rs/0.1", "curl/8.7.1", ""] {
            let mut headers = vec![("x-openai-subagent", "collab_spawn")];
            if !user_agent.is_empty() {
                headers.push(("user-agent", user_agent));
            }
            let options = options(from.as_str(), &headers);
            let check = |body: &Value, want: &str, target: &str| {
                let field = |name: &str| str_at(body, &format!("{properties}.{name}.type"));
                assert_eq!(
                    field("yield_time_ms"),
                    want,
                    "{route}/{target}/{user_agent:?}: {body}"
                );
                assert_eq!(field("unrelated"), "number", "{route}/{target}: {body}");
            };

            // The Codex executor keeps the client's types.
            for compat in [false, true] {
                let config = config(
                    false,
                    vec![codex_key("http://127.0.0.1:1", &[("model", "", compat)])],
                );
                let body = translate(
                    Kind::Execute,
                    context(&config, &auth),
                    &request("model", payload),
                    &options,
                    &to,
                    false,
                    json(payload),
                );
                check(&body, "number", &format!("codex/compat={compat}"));
            }

            // The others make a Codex client's integers integers.
            let want = if user_agent == "codex_cli_rs/0.1" {
                "integer"
            } else {
                "number"
            };
            let config = Config::default();
            for config in [Some(&config), None] {
                let mut body = json(payload);
                before_translation(config, &options, &to, &mut body);
                let body = Registry::global().translate_request(&from, &to, "model", body, false);
                check(&body, want, "other");
            }
        }
    }
}

// TestTranslateRequestWithCodexMultiAgentV2_NormalizesCodexToolTypes

#[test]
fn other_executors_normalize_codex_tool_types() {
    let payload = r#"{
        "model": "gpt-5.5",
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": [{
            "type": "function",
            "name": "exec_command",
            "parameters": {"type": "object", "properties": {
                "yield_time_ms": {"type": "number"},
                "timeout_ms": {"type": "number"}
            }}
        }]
    }"#;
    let body = other_executor(
        Some(&Config::default()),
        &options("openai-response", &[("user-agent", CODEX_TUI)]),
        &Format::CLAUDE,
        payload,
    );
    let properties = &body["tools"][0]["input_schema"]["properties"];
    assert_eq!(properties["yield_time_ms"]["type"], "integer", "{body}");
    assert_eq!(properties["timeout_ms"]["type"], "integer", "{body}");
}

// TestCompatibilityTranslationPreservesExplicitClaudeVisibility

#[test]
fn other_executors_keep_explicit_claude_visibility() {
    let cases = [
        (
            "chat effort only",
            "openai",
            r#"{"reasoning_effort":"high","messages":[{"role":"user","content":"hi"}]}"#,
            None,
        ),
        (
            "chat show",
            "openai",
            r#"{"reasoning_effort":"high","include_reasoning":true,"messages":[{"role":"user","content":"hi"}]}"#,
            Some("summarized"),
        ),
        (
            "chat hide",
            "openai",
            r#"{"reasoning_effort":"high","reasoning":{"exclude":true},"messages":[{"role":"user","content":"hi"}]}"#,
            Some("omitted"),
        ),
        (
            "responses effort only",
            "openai-response",
            r#"{"reasoning":{"effort":"high"},"input":"hi"}"#,
            None,
        ),
        (
            "responses show",
            "openai-response",
            r#"{"reasoning":{"effort":"high","summary":"auto"},"input":"hi"}"#,
            Some("summarized"),
        ),
        (
            "responses hide",
            "openai-response",
            r#"{"reasoning":{"effort":"high","summary":null},"input":"hi"}"#,
            Some("omitted"),
        ),
    ];
    for stream in [false, true] {
        for (name, from, payload, want) in cases {
            let options = options(from, &[]);
            let mut body = json(payload);
            before_translation(
                Some(&Config::default()),
                &options,
                &Format::CLAUDE,
                &mut body,
            );
            let body = Registry::global().translate_request(
                &options.source_format,
                &Format::CLAUDE,
                "claude-opus-5-5",
                body,
                stream,
            );
            let display = get(&body, "thinking.display");
            assert_eq!(
                display.and_then(Value::as_str),
                want,
                "{name} stream={stream}: {body}"
            );
            if want.is_none() {
                assert!(display.is_none(), "{name}: {body}");
            }
        }
    }
}

// How compatibility models are found.

#[test]
fn compatibility_models_by_name_alias_and_suffix() {
    let keys = vec![codex_key(
        "https://compat.example",
        &[
            ("deepseek-v4-flash", "deepseek-alias", true),
            ("gpt-5.4", "codex-native", false),
        ],
    )];
    let config = config(false, keys);
    let auth = auth("https://compat.example");
    for (model, want) in [
        ("deepseek-v4-flash", true),
        ("DeepSeek-V4-Flash", true),
        ("deepseek-alias", true),
        ("deepseek-v4-flash(high)", true),
        (" deepseek-alias ", true),
        ("gpt-5.4", false),
        ("codex-native", false),
        ("unknown", false),
    ] {
        assert_eq!(
            is_compat(context(&config, &auth), &request(model, "{}")),
            want,
            "{model}"
        );
    }

    // Without a config, a credential or a matching entry there is none.
    let compat = request("deepseek-v4-flash", "{}");
    assert!(!is_compat(Context::default(), &compat));
    assert!(!is_compat(
        Context {
            auth: None,
            config: Some(&config),
            models: None
        },
        &compat
    ));
    assert!(!is_compat(
        context(&config, &auth_with(&[("api_key", "other")])),
        &compat
    ));
}

fn auth_with(attributes: &[(&str, &str)]) -> Auth {
    let mut auth = Auth {
        provider: "codex".into(),
        ..Auth::default()
    };
    for &(key, value) in attributes {
        auth.attributes.insert(key.into(), value.into());
    }
    auth
}

#[test]
fn compatibility_models_follow_the_credentials_entry() {
    let mut first = codex_key("https://one.example", &[("model", "", false)]);
    first.api_key = "shared".into();
    let mut second = codex_key("https://two.example", &[("model", "", true)]);
    second.api_key = "shared".into();
    let config = config(false, vec![first, second]);
    let model = request("model", "{}");

    // By key and base URL, regardless of case.
    let by_url = auth_with(&[("api_key", "SHARED"), ("base_url", "https://TWO.example")]);
    assert!(is_compat(context(&config, &by_url), &model));
    // By its index in the config, when its key and URL agree.
    let by_index = auth_with(&[("api_key", "shared"), ("config_index", " 1 ")]);
    assert!(is_compat(context(&config, &by_index), &model));
    let disagreeing = auth_with(&[
        ("api_key", "shared"),
        ("base_url", "https://one.example"),
        ("config_index", "1"),
    ]);
    assert!(!is_compat(context(&config, &disagreeing), &model));
    // By key alone, the first with it.
    let by_key = auth_with(&[("api_key", "shared")]);
    assert!(!is_compat(context(&config, &by_key), &model));
}

#[test]
fn compatibility_models_fall_back_to_the_credential_managers_entry() {
    // The executor's lookup finds an entry without models; the credential
    // manager's prefers the one with the credential's prefix.
    let mut without_models = codex_key("https://compat.example", &[]);
    without_models.prefix = "other".into();
    let with_models = codex_key("https://compat.example", &[("", "compat-alias", true)]);
    let config = config(false, vec![without_models, with_models]);
    let auth = auth("https://compat.example");
    assert!(is_compat(
        context(&config, &auth),
        &request("compat-alias", "{}")
    ));
    assert!(is_compat(
        context(&config, &auth),
        &request("compat-alias(low)", "{}")
    ));

    // Only for a `codex` credential.
    let mut other = auth.clone();
    other.provider = "openai".into();
    assert!(!is_compat(
        context(&config, &other),
        &request("compat-alias", "{}")
    ));
}

#[test]
fn claude_requests_to_compatibility_models_use_their_translator() {
    // An assistant thinking block without a signature reaches only a
    // compatibility model.
    let keys = vec![codex_key("https://compat.example", &[("model", "", true)])];
    let auth = auth("https://compat.example");
    let payload = r#"{"model":"model","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"thinking","thinking":"pondering","signature":""},{"type":"text","text":"ok"}]},{"role":"user","content":"again"}]}"#;
    let options = options("claude", &[]);
    let compat_input =
        convert_claude_request_to_codex_with_compat("model", &json(payload))["input"].clone();
    let plain = Registry::global().translate_request(
        &Format::CLAUDE,
        &Format::CODEX,
        "model",
        json(payload),
        false,
    );
    assert_ne!(compat_input, plain["input"]);
    for compat in [true, false] {
        let mut config = config(false, keys.clone());
        config.codex_api_key[0].models[0].is_compat = compat;
        let body = translate(
            Kind::Execute,
            context(&config, &auth),
            &request("model", payload),
            &options,
            &Format::CODEX,
            false,
            json(payload),
        );
        if compat {
            assert_eq!(body["input"], compat_input);
        } else {
            assert_eq!(body, plain);
        }
    }
}

// The hook in the other executors.

/// The `type` of the first `yield_time_ms` property anywhere in `value`.
fn yield_time_type(value: &Value) -> Option<&Value> {
    match value {
        Value::Object(fields) => fields
            .get("yield_time_ms")
            .and_then(|property| property.get("type"))
            .or_else(|| fields.values().find_map(yield_time_type)),
        Value::Array(items) => items.iter().find_map(yield_time_type),
        _ => None,
    }
}

/// A Responses request from a Codex client with a tool of its own, an agent
/// message, and an orphan delegation output.
const CODEX_CLIENT_RESPONSES: &str = r#"{
    "model": "model",
    "input": [
        {"type": "function_call_output", "name": "create_thread", "namespace": "codex_app", "output": "handoff"},
        {"type": "agent_message", "author": "/root", "recipient": "/root/worker", "content": [{"type": "encrypted_content", "encrypted_content": "delegated task"}]}
    ],
    "tools": [{"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"yield_time_ms": {"type": "number"}}}}]
}"#;

/// The same tool in a Chat Completions request, which Gemini can translate.
const CODEX_CLIENT_CHAT: &str = r#"{
    "model": "model",
    "messages": [{"role": "user", "content": "hi"}],
    "tools": [{"type": "function", "function": {"name": "exec_command", "parameters": {"type": "object", "properties": {"yield_time_ms": {"type": "number"}}}}}]
}"#;

fn codex_client_config() -> Arc<Config> {
    let mut config = config(true, Vec::new());
    config.codex.orphan_delegation_compatibility = true;
    Arc::new(config)
}

fn codex_client_options(format: &str) -> Options {
    options(
        format,
        &[
            ("user-agent", CODEX_TUI),
            ("x-openai-subagent", "collab_spawn"),
        ],
    )
}

/// Checks that a Responses request reached another provider readied.
fn assert_readied(body: &Value) {
    assert_eq!(yield_time_type(body), Some(&json!("integer")), "{body}");
    let text = body.to_string();
    assert!(
        text.contains("Tool output from codex_app__create_thread"),
        "{text}"
    );
    assert!(!text.contains("agent_message"), "{text}");
    assert!(text.contains("delegated task"), "{text}");
}

#[tokio::test]
async fn claude_executor_readies_codex_clients_requests() {
    let mock = Mock::completing().await;
    let mut auth = auth(&mock.url);
    auth.provider = "claude".into();
    let executor = crate::claude::ClaudeExecutor::new("direct").with_config(codex_client_config());
    let _ = executor
        .execute(
            Arc::new(auth),
            request("claude-sonnet-4-5", CODEX_CLIENT_RESPONSES),
            codex_client_options("openai-response"),
        )
        .await;
    assert_readied(&mock.last());
}

#[tokio::test]
async fn gemini_executors_ready_codex_clients_requests() {
    let mock = Mock::completing().await;
    let mut auth = auth(&mock.url);
    auth.provider = "gemini".into();
    let executor = crate::gemini::GeminiExecutor::new("direct").with_config(codex_client_config());
    let _ = executor
        .execute(
            Arc::new(auth.clone()),
            request("gemini-2.5-flash", CODEX_CLIENT_CHAT),
            codex_client_options("openai"),
        )
        .await;
    assert_eq!(
        yield_time_type(&mock.last()),
        Some(&json!("integer")),
        "{}",
        mock.last()
    );

    auth.provider = "vertex".into();
    let executor = crate::gemini::VertexExecutor::new("direct").with_config(codex_client_config());
    let _ = executor
        .execute(
            Arc::new(auth),
            request("gemini-2.5-flash", CODEX_CLIENT_CHAT),
            codex_client_options("openai"),
        )
        .await;
    assert_eq!(
        yield_time_type(&mock.last()),
        Some(&json!("integer")),
        "{}",
        mock.last()
    );
}

#[tokio::test]
async fn openai_compatible_executor_readies_codex_clients_requests() {
    let mock = Mock::completing().await;
    let mut auth = auth(&mock.url);
    auth.provider = "openai-compatibility".into();
    let executor = crate::openai_compat::OpenAiCompatExecutor::new(
        "openai-compatibility",
        codex_client_config(),
    );
    let _ = executor
        .execute(
            Arc::new(auth.clone()),
            request("model", CODEX_CLIENT_RESPONSES),
            codex_client_options("openai-response"),
        )
        .await;
    assert_readied(&mock.last());

    // A compact call stays in the Responses format, so its agent message
    // stays too.
    let mut options = codex_client_options("openai-response");
    options.alt = COMPACT_ALT.into();
    let _ = executor
        .execute(
            Arc::new(auth),
            request("model", CODEX_CLIENT_RESPONSES),
            options,
        )
        .await;
    let body = mock.last();
    assert_eq!(yield_time_type(&body), Some(&json!("integer")), "{body}");
    assert_eq!(body["input"][0]["type"], "message", "{body}");
    assert_eq!(body["input"][1]["type"], "agent_message", "{body}");
}
