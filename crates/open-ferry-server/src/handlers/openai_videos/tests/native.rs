// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_videos_handlers_test.go
// (videoAuthCaptureExecutor, newVideoAuthBindingTestHandler,
// newVideoAuthTestHandler, assertPreviewAliasRouting,
// TestWriteVideoContentFromURL,
// TestVideosContentUsesSelectedAuthProxyForDownload,
// TestVideosCreateBindsRetrieveToSelectedAuth,
// TestXAIVideosNativeCreateBindsRetrieveToSelectedAuth,
// TestXAIVideosNativeRetrieveUsesCanonicalBoundModel,
// TestVideosCreatePreviewAliasUsesPreviewAuthWithGAPayload,
// TestVideosCreatePreviewAliasUsesDefaultXAIModelsWithGAPayload,
// TestXAIVideosNativePreviewAliasUsesPreviewAuthWithGAPayload) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The video routes through the credential manager and the real xAI
//! executor, against a mock xAI API, a file server and a proxy, each on a
//! loopback port.
//!
//! Changed from upstream:
//! - Upstream's capture executor stands in for xAI's; here the real one
//!   calls a mock of xAI's API, which records what it is sent. A
//!   credential is told by the dummy key it sends, `key-<credential ID>`.
//! - The model a credential is picked by is told by which credential a
//!   call goes with, where upstream's executor records it; the registry
//!   is the catalog, as upstream's global registry is.
//! - The requests go through the router, with a client key.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Body;
use axum::http::Uri;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, header};
use open_ferry_core::auth::Auth;
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::{ModelRegistry, StaticCatalog};
use open_ferry_providers::xai::XaiExecutor;
use serde_json::{Value, json};

use super::{DEFAULT_MODEL, KEY, MODEL_15, OPENAI_PATH, PREVIEW, binding, get, post, send};
use crate::config::ServerConfig;
use crate::router;
use crate::state::AppState;

/// One request the mock xAI API received.
#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    path: String,
    /// The bearer token, which names the credential.
    key: String,
    /// The body's `model`, trimmed, or empty.
    model: String,
}

/// A mock of xAI's video API: every request is answered as a finished
/// video, the one a `GET /videos/<id>` names, else `request_id`, its file
/// at `content_url` (upstream's `videoAuthCaptureExecutor`).
struct Xai {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Xai {
    async fn start(request_id: &'static str, content_url: &str) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let content_url = content_url.to_owned();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let recorder = Arc::clone(&recorder);
                let content_url = content_url.clone();
                async move {
                    let key = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.strip_prefix("Bearer "))
                        .unwrap_or_default()
                        .to_owned();
                    let sent: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let model = sent["model"].as_str().unwrap_or_default().trim().to_owned();
                    let id = match uri.path().strip_prefix("/videos/") {
                        Some(id) if method == Method::GET => id.to_owned(),
                        _ => request_id.to_owned(),
                    };
                    recorder
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(Seen {
                            method,
                            path: uri.path().to_owned(),
                            key,
                            model,
                        });
                    let answer = json!({
                        "request_id": id,
                        "status": "completed",
                        "progress": 100,
                        "video": {"url": content_url, "duration": 4},
                    });
                    axum::response::Response::builder()
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(answer.to_string()))
                        .unwrap()
                }
            },
        );
        Self {
            url: serve(app).await,
            seen,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The credentials the requests went with, in turn.
    fn keys(&self) -> Vec<String> {
        self.seen().into_iter().map(|seen| seen.key).collect()
    }

    /// The models the request bodies named, in turn.
    fn models(&self) -> Vec<String> {
        self.seen().into_iter().map(|seen| seen.model).collect()
    }
}

/// Serves `app` on a loopback port, giving its URL.
async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

/// A server of the finished video, giving its URL.
async fn file_server() -> String {
    let app = Router::new().fallback(|| async {
        axum::response::Response::builder()
            .header(header::CONTENT_TYPE, "video/mp4")
            .header(
                header::CONTENT_DISPOSITION,
                r#"attachment; filename="video.mp4""#,
            )
            .body(Body::from("video-bytes"))
            .unwrap()
    });
    format!("{}/video.mp4", serve(app).await)
}

/// A proxy that counts what it is sent and answers each with a 502, giving
/// its URL.
async fn counting_proxy(hits: &Arc<AtomicUsize>) -> String {
    let hits = Arc::clone(hits);
    let app = Router::new().fallback(move || {
        let hits = Arc::clone(&hits);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            (StatusCode::BAD_GATEWAY, "unexpected proxy")
        }
    });
    serve(app).await
}

/// The dummy key credential `auth_id` sends.
fn key(auth_id: &str) -> String {
    format!("key-{auth_id}")
}

/// A model by its ID.
fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        ..ModelInfo::default()
    }
}

/// A credential: its ID, its proxy and the models it serves.
struct Credential<'a> {
    id: &'a str,
    proxy_url: &'a str,
    models: Vec<ModelInfo>,
}

impl<'a> Credential<'a> {
    fn serving(id: &'a str, models: &[&str]) -> Self {
        Self {
            id,
            proxy_url: "",
            models: models.iter().map(|id| model(id)).collect(),
        }
    }
}

/// A proxy and the state it serves.
struct Proxy {
    app: Router,
    state: AppState,
}

/// The router, with the client key `sk-test`, over a manager with the xAI
/// executor, its global proxy `global_proxy`, and `credentials`, API keys
/// calling `xai`, each registered for its models (upstream's
/// `newVideoAuthBindingTestHandler` and `newVideoAuthTestHandler`).
fn proxy(xai: &Xai, global_proxy: &str, credentials: Vec<Credential<'_>>) -> Proxy {
    let registry = Arc::new(ModelRegistry::new());
    let manager = Arc::new(Manager::new(Settings::default(), registry.clone(), None));
    manager.register_executor(Arc::new(XaiExecutor::new(global_proxy)));
    for credential in credentials {
        let mut auth = Auth {
            id: credential.id.into(),
            provider: "xai".into(),
            proxy_url: credential.proxy_url.into(),
            ..Auth::default()
        };
        auth.attributes.insert("api_key".into(), key(credential.id));
        auth.attributes.insert("base_url".into(), xai.url.clone());
        manager.register_unsaved(auth).unwrap();
        registry.register_client(credential.id, "xai", &credential.models);
    }
    let config = ServerConfig {
        api_keys: vec![KEY.into()],
        ..ServerConfig::default()
    };
    let state = AppState::new(config, manager, registry);
    Proxy {
        app: router(state.clone()),
        state,
    }
}

/// The two credentials of upstream's `newVideoAuthBindingTestHandler`,
/// serving the default model.
fn two_credentials(request_id: &str) -> (String, String) {
    (
        format!("{request_id}-auth-a"),
        format!("{request_id}-auth-b"),
    )
}

#[tokio::test]
async fn write_video_content_from_url() {
    // TestWriteVideoContentFromURL: the finished video is sent with its
    // type and disposition.
    let xai = Xai::start("video_123", &file_server().await).await;
    let proxy = proxy(
        &xai,
        "direct",
        vec![Credential::serving("video-auth", &[DEFAULT_MODEL])],
    );
    let (status, headers, body) =
        send(&proxy.app, get("/openai/v1/videos/video_123/content")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(super::content_type(&headers), "video/mp4");
    assert_eq!(
        super::header_of(&headers, "content-disposition"),
        r#"attachment; filename="video.mp4""#
    );
    assert_eq!(body, "video-bytes");
}

#[tokio::test]
async fn videos_content_uses_selected_auth_proxy_for_download() {
    // TestVideosContentUsesSelectedAuthProxyForDownload: the download goes
    // through the proxy of the credential the call went with, `direct`,
    // not the global one.
    let hits = Arc::new(AtomicUsize::new(0));
    let global_proxy = counting_proxy(&hits).await;
    let video_id = "video-content-selected";
    let auth_id = "video-content-selected-auth";
    let xai = Xai::start(video_id, &file_server().await).await;
    let proxy = proxy(
        &xai,
        &global_proxy,
        vec![Credential {
            proxy_url: "direct",
            ..Credential::serving(auth_id, &[DEFAULT_MODEL])
        }],
    );
    let uri = format!("{OPENAI_PATH}/{video_id}/content");
    let (status, _, body) = send(&proxy.app, get(&uri)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "video-bytes");
    assert_eq!(xai.keys(), [key(auth_id)]);
    let bound = proxy.state.video_bindings().get(video_id);
    assert_eq!(bound.map(|bound| bound.auth_id).as_deref(), Some(auth_id));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

// Not upstream's: without a proxy of its own, the credential's call goes
// through the global proxy, so the count above means something.
#[tokio::test]
async fn the_global_proxy_is_used_without_a_credentials_own() {
    let hits = Arc::new(AtomicUsize::new(0));
    let global_proxy = counting_proxy(&hits).await;
    let xai = Xai::start("vid-1", &file_server().await).await;
    let proxy = proxy(
        &xai,
        &global_proxy,
        vec![Credential::serving("xai-auth", &[DEFAULT_MODEL])],
    );
    let (status, _, body) = send(&proxy.app, get("/openai/v1/videos/vid-1/content")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(xai.seen().is_empty());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn videos_create_binds_retrieve_to_selected_auth() {
    // TestVideosCreateBindsRetrieveToSelectedAuth.
    let request_id = "video-openai-bound";
    let (a, b) = two_credentials(request_id);
    let xai = Xai::start(request_id, "https://vidgen.x.ai/video.mp4").await;
    let proxy = proxy(
        &xai,
        "direct",
        vec![
            Credential::serving(&a, &[DEFAULT_MODEL]),
            Credential::serving(&b, &[DEFAULT_MODEL]),
        ],
    );
    let (status, _, body) = send(
        &proxy.app,
        post(OPENAI_PATH, r#"{"model":"sora-2","prompt":"make a video"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let created = super::parse(body.as_bytes());
    assert_eq!(created["id"], request_id);
    assert_eq!(created["model"], DEFAULT_MODEL);

    let uri = format!("{OPENAI_PATH}/{request_id}");
    let (status, _, body) = send(&proxy.app, get(&uri)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let keys = xai.keys();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert_eq!(keys[1], keys[0], "{keys:?}");
}

#[tokio::test]
async fn xai_videos_native_create_binds_retrieve_to_selected_auth() {
    // TestXAIVideosNativeCreateBindsRetrieveToSelectedAuth. Also checks
    // what xAI was asked: a post of the create, then a `GET` of the video.
    let request_id = "video-xai-bound";
    let (a, b) = two_credentials(request_id);
    let xai = Xai::start(request_id, "https://vidgen.x.ai/video.mp4").await;
    let proxy = proxy(
        &xai,
        "direct",
        vec![
            Credential::serving(&a, &[DEFAULT_MODEL]),
            Credential::serving(&b, &[DEFAULT_MODEL]),
        ],
    );
    let (status, _, body) = send(
        &proxy.app,
        post(
            "/v1/videos/generations",
            r#"{"model":"grok-imagine-video","prompt":"make a video"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(super::parse(body.as_bytes())["request_id"], request_id);

    let (status, _, body) = send(&proxy.app, get(&format!("/v1/videos/{request_id}"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = xai.seen();
    let [create, retrieve] = seen.as_slice() else {
        panic!("requests: {seen:?}");
    };
    assert_eq!(retrieve.key, create.key);
    assert_eq!(
        (&create.method, create.path.as_str()),
        (&Method::POST, "/videos/generations")
    );
    assert_eq!(
        (&retrieve.method, retrieve.path.as_str()),
        (&Method::GET, "/videos/video-xai-bound")
    );
}

#[tokio::test]
async fn xai_videos_native_retrieve_uses_canonical_bound_model() {
    // TestXAIVideosNativeRetrieveUsesCanonicalBoundModel: only the 1.5
    // credential serves the model, so both calls go with it.
    let request_id = "video-xai-1.5-bound";
    let auth_15 = "video-xai-1.5-auth";
    let xai = Xai::start(request_id, "https://vidgen.x.ai/video.mp4").await;
    let proxy = proxy(
        &xai,
        "direct",
        vec![
            Credential::serving("video-xai-1.5-default-auth", &[DEFAULT_MODEL]),
            Credential::serving(auth_15, &[MODEL_15]),
        ],
    );
    let (status, _, body) = send(
        &proxy.app,
        post(
            "/v1/videos/generations",
            r#"{"model":"grok-imagine-video-1.5","prompt":"make a video"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(super::parse(body.as_bytes())["request_id"], request_id);

    let (status, _, body) = send(&proxy.app, get(&format!("/v1/videos/{request_id}"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(xai.keys(), [key(auth_15), key(auth_15)]);
    assert_eq!(xai.models(), [MODEL_15, ""]);
    assert_eq!(
        proxy.state.video_bindings().get(request_id),
        binding(auth_15, MODEL_15)
    );
}

/// Both calls went with `auth_id`, picked by the preview alias, which only
/// it serves; xAI was sent the model it maps to; and the video is held with
/// the alias (upstream's `assertPreviewAliasRouting`).
fn assert_preview_alias_routing(xai: &Xai, proxy: &Proxy, video_id: &str, auth_id: &str) {
    assert_eq!(xai.keys(), [key(auth_id), key(auth_id)]);
    assert_eq!(xai.models(), [MODEL_15, ""]);
    assert_eq!(
        proxy.state.video_bindings().get(video_id),
        binding(auth_id, PREVIEW)
    );
}

/// An OpenAI create of the preview alias, then a retrieve of its video,
/// over a credential `auth_id` serving `models` beside one serving only the
/// default model.
async fn openai_preview_alias(request_id: &'static str, auth_id: &str, models: Vec<ModelInfo>) {
    let xai = Xai::start(request_id, "https://vidgen.x.ai/video.mp4").await;
    let proxy = proxy(
        &xai,
        "direct",
        vec![
            Credential {
                models,
                ..Credential::serving(auth_id, &[])
            },
            Credential::serving("video-default-only-auth", &[DEFAULT_MODEL]),
        ],
    );
    let (status, _, body) = send(
        &proxy.app,
        post(
            OPENAI_PATH,
            r#"{"model":"grok-imagine-video-1.5-preview","prompt":"make a video"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let created = super::parse(body.as_bytes());
    assert_eq!(created["model"], MODEL_15);
    let video_id = created["id"].as_str().unwrap();

    let (status, _, body) = send(&proxy.app, get(&format!("{OPENAI_PATH}/{video_id}"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_preview_alias_routing(&xai, &proxy, video_id, auth_id);
}

#[tokio::test]
async fn videos_create_preview_alias_uses_preview_auth_with_ga_payload() {
    // TestVideosCreatePreviewAliasUsesPreviewAuthWithGAPayload. Changed
    // from upstream: a second credential, serving only the default model,
    // shows the alias is what the credential is picked by.
    openai_preview_alias(
        "video-openai-preview-alias",
        "video-openai-preview-auth",
        vec![model(PREVIEW)],
    )
    .await;
}

#[tokio::test]
async fn videos_create_preview_alias_uses_default_xai_models_with_ga_payload() {
    // TestVideosCreatePreviewAliasUsesDefaultXAIModelsWithGAPayload: the
    // xAI models every credential serves include the alias.
    openai_preview_alias(
        "video-openai-preview-default-models",
        "video-openai-preview-default-auth",
        StaticCatalog::embedded().xai_models(),
    )
    .await;
}

#[tokio::test]
async fn xai_videos_native_preview_alias_uses_preview_auth_with_ga_payload() {
    // TestXAIVideosNativePreviewAliasUsesPreviewAuthWithGAPayload. Changed
    // from upstream as the OpenAI create's is.
    let request_id = "video-native-preview-alias";
    let auth_id = "video-native-preview-auth";
    let xai = Xai::start(request_id, "https://vidgen.x.ai/video.mp4").await;
    let proxy = proxy(
        &xai,
        "direct",
        vec![
            Credential::serving(auth_id, &[PREVIEW]),
            Credential::serving("video-default-only-auth", &[DEFAULT_MODEL]),
        ],
    );
    let (status, _, body) = send(
        &proxy.app,
        post(
            "/v1/videos/generations",
            r#"{"model":"grok-imagine-video-1.5-preview","prompt":"make a video"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let video_id = super::parse(body.as_bytes())["request_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, _, body) = send(&proxy.app, get(&format!("/v1/videos/{video_id}"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_preview_alias_routing(&xai, &proxy, &video_id, auth_id);
}
