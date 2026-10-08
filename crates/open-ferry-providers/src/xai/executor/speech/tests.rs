// Ported from CLIProxyAPI internal/runtime/executor/xai_executor_speech_test.go
// (TestXAISpeechRequestURLStaysOnOfficialAPI,
// TestXAIExecutorExecuteSpeechPostsAudioRequest,
// TestXAIExecutorExecuteStreamRejectsSpeech,
// TestXAIExecutorExecuteSpeechPayloadRulesMatchOpenAIProtocol,
// TestXAIExecutorExecuteSpeechUpstreamErrorScope,
// TestXAIExecutorSpeechUnknownVoiceDoesNotRotateOrCoolCredentials,
// TestXAIExecutorSpeechModelNotFoundRotatesCredentials) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Speech calls against a mock xAI server on 127.0.0.1. The mock records
//! each request's method, target, headers and body. Upstream's tests sign
//! in with OAuth; these use a dummy API key, the only credential served.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::http::{Method as AxumMethod, Uri};
use bytes::Bytes;
use chrono::Utc;
use http::HeaderMap;
use open_ferry_core::auth::Status;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Dispatcher, ExecError, Format, Options, Request};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;

use super::*;
use crate::xai::executor::tests::{assert_no_tokens, execute_observed, records, usage_queue};
use crate::xai::request::MEDIA_REFUSED;

/// The dummy API key the tests send.
const API_KEY: &str = "xai-test-key";
/// The body the speech handler sends for "hello".
const BODY: &str = r#"{"text":"hello","voice_id":"eve","language":"auto"}"#;

/// One request the mock received.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }
}

/// What the mock answers every request with.
#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Bytes,
}

impl Reply {
    /// An MP3's first bytes, as upstream's tests answer.
    fn audio() -> Self {
        Self {
            status: 200,
            content_type: "audio/mpeg",
            body: Bytes::from_static(b"ID3audio"),
        }
    }

    fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: Bytes::from(body.to_owned()),
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
        let app = Router::new().fallback(
            move |method: AxumMethod, uri: Uri, headers: HeaderMap, body: Bytes| {
                let reply = reply.clone();
                let recorder = Arc::clone(&recorder);
                async move {
                    recorder
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(Seen {
                            method: method.to_string(),
                            target: uri.to_string(),
                            headers,
                            body: body.to_vec(),
                        });
                    axum::response::Response::builder()
                        .status(reply.status)
                        .header("content-type", reply.content_type)
                        .body(axum::body::Body::from(reply.body))
                        .unwrap()
                }
            },
        );
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

/// An API key credential `id` for `base_url`.
fn api_key_auth_with_id(id: &str, base_url: &str) -> Auth {
    let mut auth = Auth {
        id: id.into(),
        provider: "xai".into(),
        status: Status::Active,
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), API_KEY.into());
    auth
}

/// An API key credential for `base_url`.
fn api_key_auth(base_url: &str) -> Arc<Auth> {
    Arc::new(api_key_auth_with_id("xai-1", base_url))
}

fn request(payload: &str) -> Request {
    Request {
        model: "grok-tts".into(),
        payload: Bytes::from(payload.to_owned()),
    }
}

/// Options for a call from the speech endpoint.
fn speech_options() -> Options {
    let mut options = Options::new(Format::OPENAI_SPEECH);
    options.metadata.request_path = "/v1/audio/speech".into();
    options
}

async fn execute(
    executor: &XaiExecutor,
    auth: Arc<Auth>,
    payload: &str,
) -> Result<Response, ExecError> {
    ProviderExecutor::execute(executor, auth, request(payload), speech_options()).await
}

// TestXAISpeechRequestURLStaysOnOfficialAPI: a credential on Grok's CLI chat
// proxy speaks through xAI's API, and any other under its own base URL.
#[test]
fn speech_url_stays_on_the_official_api() {
    let auth = |base: &str| api_key_auth_with_id("xai-1", base);
    let url = speech_url(&auth(CLI_CHAT_PROXY_BASE_URL));
    assert_eq!(url, format!("{DEFAULT_BASE_URL}/tts"));
    assert!(!url.starts_with(CLI_CHAT_PROXY_BASE_URL));
    assert_eq!(
        speech_url(&auth("https://gateway.example/v1")),
        "https://gateway.example/v1/tts"
    );
    // Not upstream's: a trailing slash, and no base URL at all.
    assert_eq!(
        speech_url(&auth(&format!("{CLI_CHAT_PROXY_BASE_URL}/"))),
        format!("{DEFAULT_BASE_URL}/tts")
    );
    assert_eq!(
        speech_url(&auth("https://gateway.example/v1/")),
        "https://gateway.example/v1/tts"
    );
    let mut bare = auth("");
    bare.attributes.remove("base_url");
    assert_eq!(speech_url(&bare), format!("{DEFAULT_BASE_URL}/tts"));
}

// TestXAIExecutorExecuteSpeechPostsAudioRequest: the body is posted as it
// came to /tts with the key, `Accept: */*` and no Grok CLI header, and the
// audio comes back with its content type. Not upstream's: the usage record
// names the model and no tokens.
#[tokio::test]
async fn speech_posts_the_body_and_returns_the_audio() {
    let mock = Mock::start(Reply::audio()).await;
    let queue = usage_queue();
    let response = execute_observed(
        &executor(),
        &queue,
        api_key_auth(&format!("{}/v1", mock.url)),
        request(BODY),
        speech_options(),
    )
    .await
    .unwrap();

    let seen = mock.last();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.target, "/v1/tts");
    assert_eq!(
        seen.header("authorization"),
        Some(format!("Bearer {API_KEY}").as_str())
    );
    assert_eq!(seen.header("accept"), Some("*/*"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    for name in ["x-xai-token-auth", "x-grok-client-version"] {
        assert_eq!(seen.header(name), None, "{name}");
    }
    assert_eq!(seen.body, BODY.as_bytes());
    assert_eq!(&response.payload[..], b"ID3audio");
    assert_eq!(response.headers.get("content-type").unwrap(), "audio/mpeg");

    let records = records(&queue);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["model"], "grok-tts", "{record}");
    assert_eq!(record["provider"], "xai", "{record}");
    assert_eq!(record["failed"], false, "{record}");
    assert_eq!(record.get("response_model"), None, "{record}");
    assert_no_tokens(record);
}

// TestXAIExecutorExecuteStreamRejectsSpeech: a speech stream is refused with
// a 400 before anything is sent.
#[tokio::test]
async fn speech_streams_are_refused() {
    let mock = Mock::start(Reply::audio()).await;
    let options = Options {
        stream: true,
        ..speech_options()
    };
    let result = executor()
        .execute_stream(api_key_auth(&mock.url), request(BODY), options)
        .await;
    let Err(error) = result else {
        panic!("the speech stream started");
    };
    assert_eq!(error.http_status(), 400, "{error:?}");
    assert!(
        error.message.contains("streaming not supported"),
        "{error:?}"
    );
    assert_eq!(error.message, "streaming not supported for /audio/speech");
    assert!(mock.requests().is_empty());
}

// Not upstream's: a speech compaction, which upstream compacts, is refused
// before anything is sent, streamed or not, as an image or video one is.
#[tokio::test]
async fn speech_compactions_are_refused() {
    let mock = Mock::start(Reply::audio()).await;
    let options = Options {
        alt: crate::xai::executor::COMPACT_ALT.into(),
        ..speech_options()
    };
    let error = executor()
        .execute(api_key_auth(&mock.url), request(BODY), options.clone())
        .await
        .unwrap_err();
    assert_eq!(error.http_status(), 400, "{error:?}");
    assert_eq!(error.message, MEDIA_REFUSED);
    let result = executor()
        .execute_stream(
            api_key_auth(&mock.url),
            request(BODY),
            Options {
                stream: true,
                ..options
            },
        )
        .await;
    let Err(error) = result else {
        panic!("the compaction stream started");
    };
    assert_eq!(error.message, MEDIA_REFUSED);
    assert!(mock.requests().is_empty());
}

// TestXAIExecutorExecuteSpeechPayloadRulesMatchOpenAIProtocol: the payload
// rules apply for the model with protocol `openai`.
#[tokio::test]
async fn speech_payload_rules_match_the_openai_protocol() {
    let mock = Mock::start(Reply::audio()).await;
    let config = Config::parse(
        "payload:\n  override:\n    - models:\n        - name: grok-tts\n          protocol: openai\n      params:\n        voice_id: ara\n        language: en\n",
    )
    .unwrap();
    let executor = executor().with_config(Arc::new(config));
    execute(&executor, api_key_auth(&format!("{}/v1", mock.url)), BODY)
        .await
        .unwrap();
    let sent: Value = serde_json::from_slice(&mock.last().body).unwrap();
    assert_eq!(sent["voice_id"], "ara", "{sent}");
    assert_eq!(sent["language"], "en", "{sent}");
    assert_eq!(sent["text"], "hello", "{sent}");
}

// TestXAIExecutorExecuteSpeechUpstreamErrorScope: xAI's status and body come
// back, a bad-credentials 403 as a 401, and a 404 is the request's alone
// unless its body says the model is unknown or unavailable.
#[tokio::test]
async fn speech_errors_keep_xais_status_and_scope_unknown_voices() {
    let cases: [(u16, &str, u16, bool); 15] = [
        (400, r#"{"error":"invalid language"}"#, 400, false),
        (
            400,
            r#"{"error":"requested model is not supported"}"#,
            400,
            false,
        ),
        (404, r#"{"error":"voice not found"}"#, 404, true),
        (
            404,
            r#"{"error":{"code":"model_not_found","message":"The model grok-tts does not exist"}}"#,
            404,
            false,
        ),
        (
            404,
            r#"{"code":"model_not_found","error":"model unavailable"}"#,
            404,
            false,
        ),
        (
            404,
            r#"{"error":"The model grok-tts is not available for your account"}"#,
            404,
            false,
        ),
        (404, "model is not available", 404, false),
        (
            404,
            r#"{"error":{"message":"Unsupported model: grok-tts"}}"#,
            404,
            false,
        ),
        (422, r#"{"error":"text too long"}"#, 422, false),
        (
            422,
            r#"{"code":"model_not_supported","error":"model is not supported"}"#,
            422,
            false,
        ),
        (401, r#"{"error":"unauthorized"}"#, 401, false),
        (
            403,
            r#"{"code":"bad-credentials","error":"access token could not be validated"}"#,
            401,
            false,
        ),
        (403, r#"{"error":"forbidden"}"#, 403, false),
        (429, r#"{"error":"rate limited"}"#, 429, false),
        (500, r#"{"error":"boom"}"#, 500, false),
    ];
    for (status, body, want_status, want_scoped) in cases {
        let mock = Mock::start(Reply::status(status, body)).await;
        let error = execute(
            &executor(),
            api_key_auth(&format!("{}/v1", mock.url)),
            r#"{"text":"hello","voice_id":"nope","language":"auto"}"#,
        )
        .await
        .unwrap_err();
        assert_eq!(error.http_status(), want_status, "{status} {body}");
        assert_eq!(error.message, body, "{status} {body}");
        assert_eq!(error.request_scoped, want_scoped, "{status} {body}");
    }
}

// Not upstream's: the patterns are looked for in the JSON body's fields, or
// in a body that isn't JSON as text, ignoring case.
#[test]
fn model_unavailable_reads_the_fields_or_the_text() {
    assert!(model_unavailable(br#"{"detail":"Model Unavailable"}"#));
    assert!(model_unavailable(br#"{"type":"MODEL_NOT_FOUND"}"#));
    assert!(model_unavailable(
        br#"{"error":{"type":"model_not_supported"}}"#
    ));
    assert!(model_unavailable(b"Not available for your plan"));
    assert!(!model_unavailable(br#"{"error":"voice not found"}"#));
    assert!(!model_unavailable(br#"{"hint":"model unavailable"}"#));
    assert!(!model_unavailable(b""));
}

/// The ID of the `index`th credential the manager tests register.
fn credential(index: usize) -> String {
    format!("xai-speech-scope-{index}")
}

/// A manager with no retries and two xAI API keys for `url`, each serving
/// `grok-tts`, and their registry.
fn start_manager(url: &str) -> (Manager, Arc<ModelRegistry>) {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Manager::new(Settings::default(), Arc::clone(&registry) as _, None);
    manager.register_executor(Arc::new(executor()));
    let models = [ModelInfo {
        id: "grok-tts".into(),
        ..ModelInfo::default()
    }];
    for index in 0..2 {
        let id = credential(index);
        registry.register_client(&id, "xai", &models);
        let mut auth = api_key_auth_with_id(&id, &format!("{url}/v1"));
        auth.metadata
            .insert("disable_cooling".into(), Value::Bool(false));
        manager.register(auth).unwrap();
    }
    (manager, registry)
}

/// Runs a speech call for `grok-tts` with `payload` through `manager`.
async fn run(manager: &Manager, payload: &str) -> Result<Response, ExecError> {
    manager
        .execute(
            &["xai".to_owned()],
            request(payload),
            Options::new(Format::OPENAI_SPEECH),
        )
        .await
}

// TestXAIExecutorSpeechUnknownVoiceDoesNotRotateOrCoolCredentials: an
// unknown voice's 404 is tried on one credential only, and cools neither
// credential nor the model, nor suspends it.
#[tokio::test]
async fn unknown_voice_neither_rotates_nor_cools_credentials() {
    let mock = Mock::start(Reply::status(404, r#"{"error":"voice not found"}"#)).await;
    let (manager, registry) = start_manager(&mock.url);
    let result = run(
        &manager,
        r#"{"text":"hello","voice_id":"nope","language":"auto"}"#,
    )
    .await;
    assert!(result.is_err(), "the call succeeded");
    assert_eq!(mock.requests().len(), 1, "a credential was retried");

    let now = Utc::now();
    let cooling = |unavailable: bool, next: Option<chrono::DateTime<Utc>>| {
        unavailable || next.is_some_and(|next| next > now)
    };
    for index in 0..2 {
        let id = credential(index);
        let auth = manager.get(&id).unwrap();
        assert!(
            !cooling(auth.unavailable, auth.next_retry_after),
            "{id} cooled down: {:?}",
            auth.next_retry_after
        );
        if let Some(state) = auth.model_states.get("grok-tts") {
            assert!(
                !cooling(state.unavailable, state.next_retry_after),
                "{id}'s grok-tts cooled down: {state:?}"
            );
        }
        assert!(
            !registry.is_model_suspended_for_client(&id, "grok-tts"),
            "{id}'s grok-tts suspended"
        );
    }
}

// TestXAIExecutorSpeechModelNotFoundRotatesCredentials: a 404 for an unknown
// model is the credential's, so the manager tries the other one.
#[tokio::test]
async fn model_not_found_rotates_credentials() {
    let mock = Mock::start(Reply::status(
        404,
        r#"{"error":{"code":"model_not_found","message":"The model grok-tts does not exist"}}"#,
    ))
    .await;
    let (manager, _registry) = start_manager(&mock.url);
    let result = run(&manager, BODY).await;
    assert!(result.is_err(), "the call succeeded");
    assert_eq!(
        mock.requests().len(),
        2,
        "the other credential wasn't tried"
    );
}
