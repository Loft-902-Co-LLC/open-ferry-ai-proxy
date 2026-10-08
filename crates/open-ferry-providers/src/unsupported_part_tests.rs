// Ported from CLIProxyAPI internal/runtime/executor/unsupported_part_executor_test.go
// (TestExecutorsRefuseAFileOnlyTurnBeforeCallingUpstream) and
// helps/request_pair_error_test.go (TestResponsesFileIDOnlyReturns400OnEveryPath,
// TestResponsesAudioAndTextBesideFileIDOnCompatPath) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A turn whose only content is a part the provider can't receive is
//! refused with a 400 naming the part, before any call to the provider,
//! instead of going out as an empty turn. Each executor runs against a mock
//! on an ephemeral port of 127.0.0.1 that counts the requests it gets, with
//! a dummy key.
//!
//! Changed:
//! - Upstream's targets are kept, and Vertex AI, Gemini Interactions, Meta
//!   and xAI are added, as their executors translate too.
//! - The error is checked by its status and text, as the executor's error
//!   doesn't keep upstream's `UnsupportedPartError` type.
//! - The compatibility-path cases of the `helps` tests run where the path
//!   exists here: through the Codex executor for a Claude request to a
//!   compatibility model. A Responses request to Claude takes the registry's
//!   translator, which is the one both of upstream's paths reach.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::{CodexKey, CodexModel, Config};
use open_ferry_core::exec::{ExecError, Format, Options, Request};
use open_ferry_core::executor::ProviderExecutor;

use crate::claude::ClaudeExecutor;
use crate::codex::CodexExecutor;
use crate::gemini::{GeminiExecutor, InteractionsExecutor, VertexExecutor};
use crate::meta::MetaExecutor;
use crate::openai_compat::OpenAiCompatExecutor;
use crate::xai::XaiExecutor;

const CLAUDE_FILE: &str = r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":[{"type":"container_upload","file_id":"file_not_stored"}]}]}"#;
const OPENAI_FILE: &str = r#"{"model":"m","messages":[{"role":"user","content":[{"type":"file","file":{"file_id":"file-not-stored"}}]}]}"#;
/// The attachment-only user turn is followed by a developer step that must
/// not hide it.
const INTERACTIONS_DEVELOPER: &str = r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"text","text":"hello"}]},{"type":"model_output","content":[{"type":"text","text":"hi"}]},{"type":"user_input","content":[{"type":"document","uri":"gs://b/a.pdf"}]},{"type":"user_input","role":"developer","content":[{"type":"text","text":"note"}]}]}"#;
/// The same emptied audio turn, followed by an instruction step that names
/// itself by role or by type alone.
const INTERACTIONS_AUDIO_PREFIX: &str = r#"{"model":"m","input":[{"type":"user_input","content":[{"type":"text","text":"hello"}]},{"type":"model_output","content":[{"type":"text","text":"hi"}]},{"type":"user_input","content":[{"type":"audio","uri":"gs://b/a.wav"}]},"#;
/// The only new user turn is inline audio, which Claude can't read and must
/// not get as placeholder text.
const GEMINI_AUDIO: &str = r#"{"model":"m","contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"hi"}]},{"role":"user","parts":[{"inlineData":{"mimeType":"audio/wav","data":"UklGRg=="}}]}]}"#;
/// The only new user turn is a remote image URL, so the earlier turn must
/// not be answered in its place.
const OPENAI_IMAGE_URL: &str = r#"{"model":"m","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://x.test/a.png"}}]}]}"#;
/// A Responses request whose only user content is a file ID.
const RESPONSES_FILE_ID: &str = r#"{"model":"claude-sonnet-4","input":[{"type":"message","role":"user","content":[{"type":"input_file","file_id":"file-1"}]}]}"#;

/// A mock provider on an ephemeral port of 127.0.0.1 that answers every
/// request with an error and counts them.
struct Mock {
    url: String,
    calls: Arc<AtomicUsize>,
}

impl Mock {
    async fn start() -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let app = Router::new().fallback(move |_: Bytes| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (http::StatusCode::INTERNAL_SERVER_ERROR, "reached")
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, calls }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// A case: its label, the executor, the client's format, its body and the
/// part that is refused.
type Target<'a> = (
    &'a str,
    &'a Arc<dyn ProviderExecutor>,
    Format,
    &'a str,
    &'a str,
);

/// An API key credential of `executor` for `base_url`.
fn auth(executor: &dyn ProviderExecutor, base_url: &str) -> Arc<Auth> {
    let mut auth = Auth {
        id: format!("unsupported-part-{}", executor.id()),
        provider: executor.id().to_owned(),
        ..Auth::default()
    };
    auth.attributes.insert("api_key".into(), "test".into());
    auth.attributes.insert("base_url".into(), base_url.into());
    Arc::new(auth)
}

fn compat_executor() -> OpenAiCompatExecutor {
    let mut config = Config::default();
    config.proxy_url = "direct".into();
    OpenAiCompatExecutor::new(crate::apply_patch_bridge_tests::COMPAT, Arc::new(config))
}

/// Checks that `error` is the 400 refusing a `part` part.
fn assert_refused(error: Option<ExecError>, part: &str, label: &str) {
    let error = error.unwrap_or_else(|| panic!("{label}: the call went on"));
    assert_eq!(error.http_status(), 400, "{label}: {error:?}");
    assert_eq!(
        error.to_string(),
        format!("unsupported content part: {part}"),
        "{label}"
    );
    assert!(error.request_scoped, "{label}: the credential was blamed");
}

/// Runs a call, a stream and a token count of `payload` from `source`
/// through `executor`, and gives each call's error, if it failed.
async fn calls(
    executor: &dyn ProviderExecutor,
    auth: &Arc<Auth>,
    source: &Format,
    payload: &str,
) -> Vec<(&'static str, Option<ExecError>)> {
    let request = || Request {
        model: "m".into(),
        payload: Bytes::from(payload.to_owned()),
    };
    let options = |stream: bool| Options {
        stream,
        ..Options::new(source.clone())
    };
    let execute = executor
        .execute(Arc::clone(auth), request(), options(false))
        .await
        .err();
    let stream = match executor
        .execute_stream(Arc::clone(auth), request(), options(true))
        .await
    {
        Err(error) => Some(error),
        // A refusal must come before the stream starts; the first chunk of
        // one that started tells what reached the provider.
        Ok(response) => {
            let first = response.chunks.into_future().await.0;
            panic!("the stream started: {:?}", first.map(|chunk| chunk.err()))
        }
    };
    let count = executor
        .count_tokens(Arc::clone(auth), request(), options(false))
        .await
        .err();
    vec![("execute", execute), ("stream", stream), ("count", count)]
}

/// `TestExecutorsRefuseAFileOnlyTurnBeforeCallingUpstream`.
#[tokio::test]
async fn executors_refuse_a_file_only_turn_before_calling_upstream() {
    let mock = Mock::start().await;
    let interactions_audio = |last: &str| format!("{INTERACTIONS_AUDIO_PREFIX}{last}]}}");
    let audio_role_developer = interactions_audio(
        r#"{"type":"user_input","role":"developer","content":[{"type":"text","text":"note"}]}"#,
    );
    let audio_type_system =
        interactions_audio(r#"{"type":"system","content":[{"type":"text","text":"note"}]}"#);
    let audio_type_developer =
        interactions_audio(r#"{"type":"developer","content":[{"type":"text","text":"note"}]}"#);

    let compat: Arc<dyn ProviderExecutor> = Arc::new(compat_executor());
    let codex: Arc<dyn ProviderExecutor> = Arc::new(CodexExecutor::new("direct"));
    let claude: Arc<dyn ProviderExecutor> = Arc::new(ClaudeExecutor::new("direct"));
    let gemini: Arc<dyn ProviderExecutor> = Arc::new(GeminiExecutor::new("direct"));
    let vertex: Arc<dyn ProviderExecutor> = Arc::new(VertexExecutor::new("direct"));
    let interactions: Arc<dyn ProviderExecutor> = Arc::new(InteractionsExecutor::new("direct"));
    let meta: Arc<dyn ProviderExecutor> = Arc::new(MetaExecutor::new("direct"));
    let xai: Arc<dyn ProviderExecutor> = Arc::new(XaiExecutor::new("direct"));
    let targets: Vec<Target<'_>> = vec![
        (
            "claude to openai-compat",
            &compat,
            Format::CLAUDE,
            CLAUDE_FILE,
            "container_upload",
        ),
        (
            "claude to codex",
            &codex,
            Format::CLAUDE,
            CLAUDE_FILE,
            "container_upload",
        ),
        (
            "openai to claude",
            &claude,
            Format::OPENAI,
            OPENAI_FILE,
            "file",
        ),
        (
            "openai to gemini",
            &gemini,
            Format::OPENAI,
            OPENAI_FILE,
            "file",
        ),
        (
            "openai image_url to gemini",
            &gemini,
            Format::OPENAI,
            OPENAI_IMAGE_URL,
            "image_url",
        ),
        (
            "gemini audio to claude",
            &claude,
            Format::GEMINI,
            GEMINI_AUDIO,
            "inlineData",
        ),
        (
            "interactions developer step to claude",
            &claude,
            Format::INTERACTIONS,
            INTERACTIONS_DEVELOPER,
            "document",
        ),
        (
            "interactions developer step to gemini",
            &gemini,
            Format::INTERACTIONS,
            INTERACTIONS_DEVELOPER,
            "document",
        ),
        (
            "interactions developer role to codex",
            &codex,
            Format::INTERACTIONS,
            &audio_role_developer,
            "audio",
        ),
        (
            "interactions system type to codex",
            &codex,
            Format::INTERACTIONS,
            &audio_type_system,
            "audio",
        ),
        (
            "interactions developer type to codex",
            &codex,
            Format::INTERACTIONS,
            &audio_type_developer,
            "audio",
        ),
        // Not upstream's: the other executors that translate.
        (
            "openai to vertex",
            &vertex,
            Format::OPENAI,
            OPENAI_FILE,
            "file",
        ),
        (
            "claude to gemini interactions",
            &interactions,
            Format::CLAUDE,
            CLAUDE_FILE,
            "container_upload",
        ),
        (
            "claude to meta",
            &meta,
            Format::CLAUDE,
            CLAUDE_FILE,
            "container_upload",
        ),
        (
            "claude to xai",
            &xai,
            Format::CLAUDE,
            CLAUDE_FILE,
            "container_upload",
        ),
        (
            "responses file id to claude",
            &claude,
            Format::OPENAI_RESPONSE,
            RESPONSES_FILE_ID,
            "input_file",
        ),
    ];
    for (name, executor, source, payload, part) in targets {
        let auth = auth(executor.as_ref(), &mock.url);
        for (call, error) in calls(executor.as_ref(), &auth, &source, payload).await {
            assert_refused(error, part, &format!("{name} {call}"));
        }
    }
    assert_eq!(mock.calls(), 0, "the provider was called");
}

/// `TestClaudeContainerUploadReturns400`'s compatibility half, and
/// `TestPairTranslationReportsOnlyTheWorkingError` at an executor: a Codex
/// compatibility model refuses the file through its own translator, and a
/// client's original request that holds the file doesn't refuse a payload
/// that translated.
#[tokio::test]
async fn the_codex_compatibility_path_refuses_too() {
    let mock = Mock::start().await;
    let mut config = Config::default();
    config.codex_api_key = vec![CodexKey {
        api_key: "test".into(),
        base_url: mock.url.clone(),
        models: vec![CodexModel {
            name: "m".into(),
            is_compat: true,
            ..CodexModel::default()
        }],
        ..CodexKey::default()
    }];
    let executor = CodexExecutor::new("direct").with_config(Arc::new(config));
    let auth = auth(&executor, &mock.url);
    for (call, error) in calls(&executor, &auth, &Format::CLAUDE, CLAUDE_FILE).await {
        assert_refused(error, "container_upload", &format!("compat {call}"));
    }
    assert_eq!(mock.calls(), 0, "the provider was called");

    // The payload translated; only the original holds the file.
    let request = Request {
        model: "m".into(),
        payload: Bytes::from_static(
            br#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":[{"type":"text","text":"keep me"}]}]}"#,
        ),
    };
    let options = Options {
        original_request: Bytes::from_static(CLAUDE_FILE.as_bytes()),
        ..Options::new(Format::CLAUDE)
    };
    let error = executor
        .execute(Arc::clone(&auth), request, options)
        .await
        .expect_err("the mock answers 500");
    assert_eq!(error.http_status(), 500, "{error:?}");
    assert_eq!(mock.calls(), 1, "the payload wasn't sent");
}
