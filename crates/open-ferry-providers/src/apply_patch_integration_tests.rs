// Ported from CLIProxyAPI internal/runtime/executor/apply_patch_integration_test.go
// (task6PatchRequest, task6ProviderFixture, task6Executor,
// assertTask6PatchError, assertTask6FailedStream, task6CaptureFailureUsage,
// task6Gateway) and internal/runtime/executor/apply_patch_repair_test.go
// (task6RepairSource, task6RepairSSE, task6RepairChunkType,
// task6RepairReadStream) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `apply_patch` bridge end to end: each served executor against a mock
//! upstream on 127.0.0.1 that sends a bad, cut-off or unfinished call to
//! `apply_patch`, and what the client and the usage records get.
//! [`integration`] ports `apply_patch_integration_test.go` and [`repair`]
//! ports `apply_patch_repair_test.go`; what they share is here.
//!
//! The mocks are the executors' own: Gemini's [`Mock`] for HTTP, the
//! scripted body of Codex's bootstrap tests where a test must see the client
//! go, and Codex's WebSocket mock, with the xAI WebSocket tests' helpers,
//! for xAI's Responses WebSocket.
//!
//! How the tests are adapted:
//! - Usage: upstream registers a usage plugin and counts the records of the
//!   credential's ID up to a barrier record. Here each test has a usage
//!   queue of its own ([`Usage`]), turned on as it is when the management
//!   API serves, and a direct call reports to it as the server's handlers
//!   have it do, through a [`CallReport`] (the manager makes its own).
//! - The gateway (`task6Gateway`): upstream serves the call through its
//!   Responses handler on gin and reads the HTTP body. Here the call goes
//!   through the credential manager with the request that handler gives it,
//!   and the test reads what the manager returns. What the server writes of
//!   that is the server's: once a terminal event such as `response.failed`
//!   is written it writes no `error` event and no `[DONE]`
//!   (`does_not_append_failure_after_terminal_event` in the server's
//!   `handlers/responses/tests.rs`), so upstream's "one `event:
//!   response.failed` and no `event: error`" is one `response.failed`
//!   followed by the error here. The client's `User-Agent` upstream's
//!   gateway request carries is left off: none of these executors reads it.
//! - `scanner` mode: upstream promises `Content-Length: 999999` and sends
//!   less, so that reading the body fails. The mock here sends the body and
//!   then fails the connection ([`Reply::cut_off`]).
//! - A direct call of the xAI executor takes the Responses WebSocket only
//!   for a client on that WebSocket and a credential with websockets on (see
//!   the xAI WebSocket tests); upstream's WebSocket executor takes any call.
//!   The `raw=true` cases of upstream's WebSocket tests are such a client.
//!   Their `raw=false` cases, a client over HTTP, go over HTTP here, where
//!   the xAI rows of the HTTP tests cover them, so they are dropped.
//!
//! Dropped, with why:
//! - The Kimi, Devin and Antigravity cases
//!   (`TestApplyPatchDevinErrorAndEOF`,
//!   `TestApplyPatchDevinLegacyOtherToolsPreserved`,
//!   `TestApplyPatchDevinLateFailureStopsConsumption`, and the `kimi`,
//!   `kimi-chat`, `devin` and `antigravity` rows of the others): those
//!   providers aren't served.
//! - `TestApplyPatchRepairGatewayInitializesGinTestMode`: gin's test mode is
//!   upstream's test plumbing.
//! - `TestApplyPatchNonStreamNativeNilWithoutErrorIs502`: it swaps the
//!   registered Chat Completions to Responses translator for one that gives
//!   nothing and no error. The port's translators give a translation or
//!   `None` for a failed `apply_patch` call, so there is no such state to
//!   reach, and they are one global registry shared by tests that run at
//!   once. Every executor's non-stream call turns a missing or empty
//!   translation into the clean 502 (`.filter(|out| !out.is_empty())
//!   .ok_or_else(..)` after `translate_non_stream`), which
//!   `integration::sdk_original_request_fallback` drives through the
//!   translator's own `None`.
//! - The `account_uuid` metadata upstream gives its Claude OAuth credential:
//!   the port sends no made-up user or session ID built from it, and the
//!   key alone makes the credential OAuth's.
//! - In `TestApplyPatchRepairXAIWebsocketPersistentFailureRetry`, the
//!   `translator*` branches (they inject a translator into the global
//!   registry, and only run with `raw=false`), the `detach=true` cases and
//!   the check that the call's lifecycle ended as `invalid_tool_arguments`:
//!   execution lifecycles aren't ported.
//!
//! What the tests found, and what fixed it: the OpenAI-compatible and Gemini
//! Interactions executors didn't ready their stream's translator for a
//! request that declares `apply_patch` (`InitializeApplyPatchStream`), so a
//! stream to a Responses client that ended before sending anything ended
//! with "upstream stream closed before [DONE]" (OpenAI-compatible) or with
//! no error at all (Interactions), not with the patch error. They now call
//! `apply_patch_responses::initialize_stream`; the `custom-compat/empty` and
//! `gemini-interactions/empty` cases of `actual_provider_error_and_eof` fail
//! without it.
//!
//! Deviations from upstream: the mocks, usage capture and gateway above.

use std::any::Any;
use std::sync::Arc;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use http::Method;
use open_ferry_core::auth::{Auth, Status};
use open_ferry_core::config::Config;
use open_ferry_core::exec::{
    Dispatcher as _, ExecError, Format, Options, Request, Response, StreamResponse,
};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::observe::usage;
use open_ferry_core::observe::{CallReport, Observation, RequestContext};
use open_ferry_core::registry::ModelRegistry;
use serde_json::Value;

use crate::codex::terminal::APPLY_PATCH_ERROR_MESSAGE;
use crate::gemini::testing::{Mock, Reply};
use crate::xai::websocket_tests::{collect, within};

mod integration;
mod repair;

/// The client's request: input, and `apply_patch` as a custom tool
/// (`task6PatchRequest`).
const PATCH_REQUEST: &str = r#"{"input":"patch","tools":[{"type":"custom","name":"apply_patch"}]}"#;

/// The model the gateway serves.
const GATEWAY_MODEL: &str = "task6-public-patch";

/// A provider whose executor a test drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    CustomCompat,
    Claude,
    /// Claude with an OAuth token for its key.
    ClaudeOauth,
    Gemini,
    GeminiInteractions,
    Vertex,
    Xai,
    Meta,
}

impl Provider {
    /// The name upstream's subtests give it.
    fn name(self) -> &'static str {
        match self {
            Self::CustomCompat => "custom-compat",
            Self::Claude => "claude",
            Self::ClaudeOauth => "claude-oauth",
            Self::Gemini => "gemini",
            Self::GeminiInteractions => "gemini-interactions",
            Self::Vertex => "vertex",
            Self::Xai => "xai",
            Self::Meta => "meta",
        }
    }

    fn is_claude(self) -> bool {
        matches!(self, Self::Claude | Self::ClaudeOauth)
    }

    /// Its executor, which doesn't use the environment's proxy
    /// (`task6Executor`).
    fn executor(self) -> Arc<dyn ProviderExecutor> {
        match self {
            Self::CustomCompat => {
                let mut config = Config::default();
                config.proxy_url = "direct".into();
                Arc::new(crate::openai_compat::OpenAiCompatExecutor::new(
                    "custom-compat",
                    Arc::new(config),
                ))
            }
            Self::Claude | Self::ClaudeOauth => {
                Arc::new(crate::claude::ClaudeExecutor::new("direct"))
            }
            Self::Gemini => Arc::new(crate::gemini::GeminiExecutor::new("direct")),
            Self::GeminiInteractions => {
                Arc::new(crate::gemini::InteractionsExecutor::new("direct"))
            }
            Self::Vertex => Arc::new(crate::gemini::VertexExecutor::new("direct")),
            Self::Xai => Arc::new(crate::xai::XaiExecutor::new("direct")),
            Self::Meta => Arc::new(crate::meta::MetaExecutor::new("direct")),
        }
    }

    /// A credential `id` with a dummy key for the mock at `base_url`.
    fn auth(self, id: &str, base_url: &str) -> Auth {
        let mut auth = Auth {
            id: id.into(),
            provider: self.executor().id().into(),
            ..Auth::default()
        };
        let key = if self == Self::ClaudeOauth {
            "sk-ant-oat-test"
        } else {
            "test"
        };
        auth.attributes.insert("api_key".into(), key.into());
        auth.attributes.insert("base_url".into(), base_url.into());
        auth
    }

    /// The model upstream's tests ask for.
    fn model(self) -> &'static str {
        match self {
            Self::Claude | Self::ClaudeOauth => "claude-sonnet-4-6",
            Self::Xai | Self::Meta => "grok-4",
            _ => "gemini-3.1-pro-preview",
        }
    }
}

/// `text` as a JSON string (Go's `%q` for the text quoted here).
fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

/// `events` as an SSE body (`task6RepairSSE`).
fn sse(events: &[String]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

/// What `provider` answers in `mode` with a call to `tool_name`
/// (`task6ProviderFixture`): `nonstream`, a whole answer whose call has
/// invalid arguments; `stream`, the same as a stream; `eof`, a stream that
/// ends inside the call; `empty`, nothing.
fn fixture(provider: Provider, mode: &str, tool_name: &str) -> String {
    if mode == "empty" {
        return String::new();
    }
    let args = if mode == "eof" {
        r#"{"input":""#
    } else {
        r#"{"input":7,"secret":"RAW_SECRET"}"#
    };
    match provider {
        Provider::Claude | Provider::ClaudeOauth => {
            let name = quote(tool_name);
            if mode == "nonstream" {
                return format!(
                    r#"{{"id":"r","type":"message","role":"assistant","content":[{{"type":"tool_use","id":"c","name":{name},"input":{args}}}],"usage":{{"input_tokens":5,"output_tokens":3}}}}"#
                );
            }
            let mut events = vec![
                r#"{"type":"message_start","message":{"id":"r","model":"claude-sonnet-4-6","usage":{"input_tokens":5}}}"#.to_owned(),
                format!(
                    r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"c","name":{name},"input":{{}}}}}}"#
                ),
                format!(
                    r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
                    quote(args)
                ),
            ];
            if mode != "eof" {
                events.extend([
                    r#"{"type":"content_block_stop","index":0}"#.to_owned(),
                    r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#.to_owned(),
                    r#"{"type":"message_stop"}"#.to_owned(),
                ]);
            }
            sse(&events)
        }
        Provider::Gemini | Provider::Vertex => {
            let (args, finish) = if mode == "eof" {
                (r#"{"input":"unfinished"}"#, "")
            } else {
                (
                    r#"{"input":7,"secret":"RAW_SECRET"}"#,
                    r#","finishReason":"STOP""#,
                )
            };
            let body = format!(
                r#"{{"responseId":"r","candidates":[{{"content":{{"parts":[{{"functionCall":{{"name":"apply_patch","args":{args}}}}}]}}{finish}}}],"usageMetadata":{{"promptTokenCount":5,"candidatesTokenCount":3}}}}"#
            );
            if mode == "nonstream" {
                body
            } else {
                sse(&[body])
            }
        }
        Provider::GeminiInteractions => {
            let arguments = quote(args);
            if mode == "nonstream" {
                return format!(
                    r#"{{"id":"r","steps":[{{"type":"function_call","id":"c","name":"apply_patch","arguments":{arguments}}}],"usage":{{"total_input_tokens":5,"total_output_tokens":3}}}}"#
                );
            }
            let mut events = vec![
                r#"{"event_type":"interaction.created","interaction":{"id":"r"}}"#.to_owned(),
                r#"{"event_type":"step.start","index":0,"step":{"type":"function_call","id":"c","name":"apply_patch"}}"#.to_owned(),
                format!(
                    r#"{{"event_type":"step.delta","index":0,"delta":{{"type":"function_call","arguments":{arguments}}}}}"#
                ),
            ];
            if mode != "eof" {
                events.extend([
                    r#"{"event_type":"interaction.completed","interaction":{"id":"r"}}"#.to_owned(),
                    "[DONE]".to_owned(),
                ]);
            }
            sse(&events)
        }
        Provider::CustomCompat | Provider::Xai | Provider::Meta => {
            let arguments = quote(args);
            if mode == "nonstream" {
                return format!(
                    r#"{{"id":"r","object":"chat.completion","choices":[{{"message":{{"tool_calls":[{{"id":"c","type":"function","function":{{"name":"apply_patch","arguments":{arguments}}}}}]}},"finish_reason":"tool_calls"}}],"usage":{{"prompt_tokens":5,"completion_tokens":3}}}}"#
                );
            }
            let mut events = vec![format!(
                r#"{{"id":"r","object":"chat.completion.chunk","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"c","type":"function","function":{{"name":"apply_patch","arguments":{arguments}}}}}]}}}}]}}"#
            )];
            if mode != "eof" {
                events.extend([
                    r#"{"id":"r","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":3}}"#.to_owned(),
                    "[DONE]".to_owned(),
                ]);
            }
            sse(&events)
        }
    }
}

/// A request for `model` with `payload`.
fn request(model: &str, payload: &str) -> Request {
    crate::gemini::testing::request(model, payload)
}

/// The options of a Responses client's call, streaming or not, with no
/// original request.
fn options(stream: bool) -> Options {
    Options {
        stream,
        ..Options::new(Format::OPENAI_RESPONSE)
    }
}

/// [`options`] with `request`'s payload as the client's request.
fn options_for(request: &Request, stream: bool) -> Options {
    Options {
        original_request: request.payload.clone(),
        ..options(stream)
    }
}

/// Asserts `error` is the clean 502 (`assertTask6PatchError`).
fn assert_patch_error(error: &ExecError) {
    assert_eq!(error.status, 502, "{error:?}");
    assert_eq!(error.message, APPLY_PATCH_ERROR_MESSAGE, "{error:?}");
}

/// Asserts a non-streaming call failed with the clean 502 and gave the
/// client nothing.
fn assert_patch_failure(result: Result<Response, ExecError>) {
    match result {
        Ok(response) => panic!(
            "the call answered {}",
            String::from_utf8_lossy(&response.payload)
        ),
        Err(error) => assert_patch_error(&error),
    }
}

/// The events of a chunk: the JSON of its `data:` lines, or its own when it
/// is one JSON event (as on the Responses WebSocket).
fn chunk_events(chunk: &str) -> Vec<Value> {
    if let Ok(event) = serde_json::from_str::<Value>(chunk.trim()) {
        return vec![event];
    }
    chunk
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect()
}

/// How many events of `chunks` are of `event_type`.
fn count_type(chunks: &[String], event_type: &str) -> usize {
    chunks
        .iter()
        .flat_map(|chunk| chunk_events(chunk))
        .filter(|event| event["type"] == event_type)
        .count()
}

/// Asserts a stream failed cleanly (`assertTask6FailedStream`): one
/// `response.failed`, then the clean 502, and nothing of the invalid call,
/// no completion and no `[DONE]`.
fn assert_failed_stream(chunks: &[String], error: Option<ExecError>) {
    let output = chunks.concat();
    let error = error.unwrap_or_else(|| panic!("the stream ended without an error: {output}"));
    assert_patch_error(&error);
    assert_eq!(count_type(chunks, "response.failed"), 1, "{output}");
    for leak in [
        r#""type":"response.completed""#,
        "[DONE]",
        "RAW_SECRET",
        r#""input":7"#,
    ] {
        assert!(!output.contains(leak), "{leak} in {output}");
    }
}

/// A usage queue of a test's own, on (what upstream's
/// `task6CaptureFailureUsage` plugin sees).
struct Usage(usage::Usage);

impl Usage {
    fn new() -> Self {
        let mut config = Config::default();
        config.usage_statistics_enabled = true;
        let queue = usage::Usage::new(&config);
        usage::reconfigure(&queue, None, &config, true);
        Self(queue)
    }

    /// Has the call of `request` with `options` report to the queue, as a
    /// server handler's taps do.
    fn observe(&self, request: &Request, options: &mut Options) {
        let context = Arc::new(RequestContext::new(
            Method::POST,
            "/v1/responses".to_owned(),
        ));
        let tap = self
            .0
            .tap(&context, request, options)
            .expect("the usage tap");
        options.observation = Some(Arc::new(Observation::new(context, vec![tap])));
    }

    /// The records queued so far, taken.
    fn records(&self) -> Vec<Value> {
        self.0
            .pop_oldest(usize::MAX)
            .iter()
            .map(|record| serde_json::from_slice(record).expect("a JSON record"))
            .collect()
    }

    /// Asserts the calls left one record, a failure (`checkUsage`).
    fn assert_one_failure(&self) {
        let records = self.records();
        assert_eq!(records.len(), 1, "usage records: {records:?}");
        assert_eq!(records[0]["failed"], true, "usage records: {records:?}");
    }
}

/// Makes a non-streaming call of `executor` as a server handler does,
/// telling its taps how it ended.
async fn execute(
    executor: &dyn ProviderExecutor,
    auth: Arc<Auth>,
    request: Request,
    options: Options,
) -> Result<Response, ExecError> {
    let report = CallReport::start(&options);
    let result = within("the call", executor.execute(auth, request, options)).await;
    report.finish(&result);
    result
}

/// Starts a streaming call of `executor` as a server handler does, its
/// taps told how the stream ends.
async fn start_stream(
    executor: &dyn ProviderExecutor,
    auth: Arc<Auth>,
    request: Request,
    options: Options,
) -> StreamResponse {
    let report = CallReport::start(&options);
    let result = within(
        "the stream to start",
        executor.execute_stream(auth, request, options),
    )
    .await;
    report
        .stream(result)
        .unwrap_or_else(|error| panic!("the stream didn't start: {error:?}"))
}

/// [`start_stream`], read to its end: its chunks and its error.
async fn stream(
    executor: &dyn ProviderExecutor,
    auth: Arc<Auth>,
    request: Request,
    options: Options,
) -> (Vec<String>, Option<ExecError>) {
    collect(start_stream(executor, auth, request, options).await).await
}

/// A credential manager with `executor` and `auth`, active, serving
/// [`GATEWAY_MODEL`]; and the provider to call.
fn gateway(executor: Arc<dyn ProviderExecutor>, mut auth: Auth) -> (Manager, String) {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Manager::new(Settings::default(), Arc::clone(&registry) as _, None);
    let provider = executor.id().to_owned();
    manager.register_executor(executor);
    registry.register_client(
        &auth.id,
        &provider,
        &[ModelInfo {
            id: GATEWAY_MODEL.into(),
            ..ModelInfo::default()
        }],
    );
    auth.status = Status::Active;
    manager.register(auth).unwrap();
    (manager, provider)
}

/// The call upstream's Responses handler gives the manager for the
/// client's request (`task6Gateway`'s), reporting to `usage`.
fn gateway_call(stream: bool, usage: &Usage) -> (Request, Options) {
    let payload = format!(
        r#"{{"model":"{GATEWAY_MODEL}","input":"patch","stream":{stream},"tools":[{{"type":"custom","name":"apply_patch"}}]}}"#
    );
    let request = request(GATEWAY_MODEL, &payload);
    let mut options = options_for(&request, stream);
    options.metadata.request_path = "/v1/responses".into();
    options.metadata.requested_model = GATEWAY_MODEL.into();
    usage.observe(&request, &mut options);
    (request, options)
}

/// A non-streaming call through the gateway (`task6Gateway` with `stream`
/// off).
async fn gateway_execute(
    executor: Arc<dyn ProviderExecutor>,
    auth: Auth,
    usage: &Usage,
) -> Result<Response, ExecError> {
    let (manager, provider) = gateway(executor, auth);
    let (request, options) = gateway_call(false, usage);
    within(
        "the gateway call",
        manager.execute(&[provider], request, options),
    )
    .await
}

/// A streaming call through the gateway (`task6Gateway` with `stream` on),
/// read to its end.
async fn gateway_stream(
    executor: Arc<dyn ProviderExecutor>,
    auth: Auth,
    usage: &Usage,
) -> (Vec<String>, Option<ExecError>) {
    let (manager, provider) = gateway(executor, auth);
    let (request, options) = gateway_call(true, usage);
    let response = within(
        "the gateway stream to start",
        manager.execute_stream(&[provider], request, options),
    )
    .await
    .unwrap_or_else(|error| panic!("the gateway stream didn't start: {error:?}"));
    collect(response).await
}

/// One of a test's cases: its name and its body.
type Case = (String, BoxFuture<'static, ()>);

/// A case named `name`.
fn case(name: String, body: impl Future<Output = ()> + Send + 'static) -> Case {
    (name, body.boxed())
}

/// Runs each case on a task of its own, as upstream's subtests, and fails
/// with every case that failed.
async fn subtests(cases: Vec<Case>) {
    let mut failed = Vec::new();
    for (name, body) in cases {
        if let Err(error) = tokio::spawn(body).await {
            let message = error
                .try_into_panic()
                .map(|panic| panic_message(&*panic))
                .unwrap_or_else(|error| error.to_string());
            failed.push(format!("{name}: {message}"));
        }
    }
    assert!(failed.is_empty(), "failed cases:\n{}", failed.join("\n"));
}

/// What a panic said.
fn panic_message(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_default()
}
