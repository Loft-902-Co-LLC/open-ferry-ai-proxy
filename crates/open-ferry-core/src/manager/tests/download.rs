// Modelled on CLIProxyAPI sdk/api/handlers/openai/openai_videos_handlers.go
// (videoContentHTTPClient, videoContentDownloadAuth) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The manager's downloads: the provider's executor fetches the file with
//! the credential that made it, or none.
//!
//! Deviations from upstream:
//! - Upstream's handler fetches the file itself, so upstream has no test of
//!   this; these check the lookup the handler's `videoContentDownloadAuth`
//!   does, and that a download leaves the credential's state alone.

use std::sync::{Arc, Mutex};

use futures_core::future::BoxFuture;
use futures_util::StreamExt as _;
use http::HeaderMap;

use super::support::*;
use crate::auth::Auth;
use crate::exec::{
    Dispatcher, Download, Downloaded, ErrorKind, ExecError, Request, Response, StreamResponse,
};
use crate::executor::ProviderExecutor;
use crate::manager::{Settings, lock};

/// An executor that records each download's credential and URL, and
/// answers with a 200, or with `fail` when it is set.
struct Fetcher {
    seen: Mutex<Vec<(Option<String>, String)>>,
    fail: Mutex<Option<ExecError>>,
}

impl Fetcher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
        })
    }

    fn seen(&self) -> Vec<(Option<String>, String)> {
        lock(&self.seen).clone()
    }
}

fn unused<T: Send + 'static>() -> BoxFuture<'static, Result<T, ExecError>> {
    Box::pin(async { Err(ExecError::new(ErrorKind::Upstream, "unused")) })
}

impl ProviderExecutor for Fetcher {
    fn id(&self) -> &str {
        "xai"
    }

    fn execute(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: crate::exec::Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        unused()
    }

    fn execute_stream(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: crate::exec::Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        unused()
    }

    fn count_tokens(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: crate::exec::Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        unused()
    }

    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move { Ok((*auth).clone()) })
    }

    fn download(
        &self,
        auth: Option<Arc<Auth>>,
        url: String,
    ) -> BoxFuture<'_, Result<Downloaded, ExecError>> {
        lock(&self.seen).push((auth.map(|auth| auth.id.clone()), url));
        let fail = lock(&self.fail).take();
        Box::pin(async move {
            if let Some(error) = fail {
                return Err(error);
            }
            Ok(Downloaded {
                status: 200,
                status_text: "200 OK".into(),
                headers: HeaderMap::new(),
                body: futures_util::stream::iter([Ok(bytes::Bytes::from_static(b"mp4"))]).boxed(),
            })
        })
    }
}

fn download(provider: &str, auth_id: &str) -> Download {
    Download {
        provider: provider.into(),
        auth_id: auth_id.into(),
        url: "http://127.0.0.1:9/v.mp4".into(),
    }
}

// Not upstream's: the credential that made the file is handed over by its
// trimmed ID, and an empty or unknown one hands over none.
#[tokio::test(start_paused = true)]
async fn download_passes_the_credential_that_made_the_file() {
    let h = Harness::new(Settings::default());
    let fetcher = Fetcher::new();
    h.manager.register_executor(fetcher.clone());
    h.add(auth("xai-1", "xai"), &["grok-imagine-video"]);

    for (provider, auth_id, want) in [
        ("xai", "xai-1", Some("xai-1")),
        (" xai ", " xai-1 ", Some("xai-1")),
        ("xai", "", None),
        ("xai", "  ", None),
        ("xai", "gone", None),
    ] {
        let downloaded = match Dispatcher::download(&h.manager, download(provider, auth_id)).await {
            Ok(downloaded) => downloaded,
            Err(error) => panic!("download {auth_id:?}: {error}"),
        };
        assert_eq!(downloaded.status, 200);
        let body: Vec<_> = downloaded.body.collect().await;
        assert_eq!(body.len(), 1);
        let last = fetcher.seen().pop();
        assert_eq!(
            last,
            Some((
                want.map(str::to_owned),
                "http://127.0.0.1:9/v.mp4".to_owned()
            )),
            "{auth_id:?}"
        );
    }
}

// Not upstream's: a provider with no executor is a 502, and nothing is
// fetched.
#[tokio::test(start_paused = true)]
async fn download_without_an_executor_is_a_bad_gateway() {
    let h = Harness::new(Settings::default());
    let fetcher = Fetcher::new();
    h.manager.register_executor(fetcher.clone());

    for provider in ["codex", ""] {
        let error = match Dispatcher::download(&h.manager, download(provider, "")).await {
            Ok(_) => panic!("download from {provider:?} succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.http_status(), 502, "{provider:?}");
        assert!(
            error.to_string().contains("no executor for provider"),
            "{error}"
        );
    }
    assert!(fetcher.seen().is_empty());
}

// Not upstream's: an executor's error keeps its status, gets a 502 when it
// has none, and leaves the credential as it was.
#[tokio::test(start_paused = true)]
async fn download_errors_are_bad_gateways_and_leave_the_credential_alone() {
    let h = Harness::new(Settings::default());
    let fetcher = Fetcher::new();
    h.manager.register_executor(fetcher.clone());
    h.add(auth("xai-1", "xai"), &["grok-imagine-video"]);
    let before = h.versions("xai-1");

    for (error, want) in [
        (ExecError::new(ErrorKind::Upstream, "refused"), 502),
        (
            ExecError::new(ErrorKind::Upstream, "bad URL").with_status(400),
            400,
        ),
    ] {
        *lock(&fetcher.fail) = Some(error);
        let error = match Dispatcher::download(&h.manager, download("xai", "xai-1")).await {
            Ok(_) => panic!("download succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.http_status(), want);
    }

    assert_eq!(h.versions("xai-1"), before);
    let auth = h.get("xai-1");
    assert!(!auth.unavailable, "{auth:?}");
    assert!(auth.model_states.is_empty(), "{auth:?}");
}
