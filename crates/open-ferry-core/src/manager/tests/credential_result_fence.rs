// Regression coverage for the API-key exception to upstream v8.0.42's
// unchanged-bearer result fence (conductor_cooldown.go).
use super::siwc::fixture;
use super::support::*;
use crate::auth::Auth;
use crate::exec::{Dispatcher, ExecError, Options, Request, Response, StreamResponse};
use crate::executor::ProviderExecutor;
use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::StreamExt;
use http::HeaderMap;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{Notify, Semaphore};

/// A mock reply held until the credential edit has landed. A stream opens
/// with one chunk, then waits before delivering its 429.
struct Deferred {
    entered: Notify,
    release: Arc<Semaphore>,
}

impl ProviderExecutor for Deferred {
    fn id(&self) -> &str {
        "codex"
    }
    fn execute(
        &self,
        auth: Arc<Auth>,
        _: Request,
        _: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        Box::pin(async move {
            assert!(auth.use_siwc() || auth.attribute("api_key") == Some("dummy-K1"));
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
            Err(ExecError::upstream(429, "deferred quota failure"))
        })
    }
    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.execute(auth, request, options)
    }
    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        _: Request,
        _: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        Box::pin(async move {
            assert!(auth.use_siwc() || auth.attribute("api_key") == Some("dummy-K1"));
            self.entered.notify_one();
            let release = self.release.clone();
            let chunks = futures_util::stream::once(async { Ok(Bytes::from_static(b"first")) })
                .chain(futures_util::stream::once(async move {
                    release.acquire().await.unwrap().forget();
                    Err(ExecError::upstream(429, "deferred quota failure"))
                }))
                .boxed();
            Ok(StreamResponse {
                headers: HeaderMap::new(),
                chunks,
            })
        })
    }
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move { Ok((*auth).clone()) })
    }
}

async fn deferred_result(streaming: bool, siwc: bool) {
    let h = Harness::new(Default::default());
    let executor = Arc::new(Deferred {
        entered: Notify::new(),
        release: Arc::new(Semaphore::new(0)),
    });
    h.manager.register_executor(executor.clone());
    let mut candidate = fixture();
    candidate.metadata["use_siwc"] = json!(siwc);
    candidate.metadata["expired"] = json!("2099-01-01T00:00:00Z");
    candidate.metadata["siwc"]["expires_at"] = json!("2099-01-01T00:00:00Z");
    candidate
        .attributes
        .insert("api_key".into(), "dummy-K1".into());
    let before = h.add(candidate, &["gpt-test"]);
    let worker = h.manager.clone();
    let task = tokio::spawn(async move {
        let request = Request {
            model: "gpt-test".into(),
            payload: Bytes::from_static(b"{}"),
        };
        if streaming {
            let mut stream = worker
                .execute_stream(&["codex".into()], request, options())
                .await
                .unwrap();
            assert!(stream.chunks.next().await.unwrap().is_ok());
            assert_eq!(stream.chunks.next().await.unwrap().unwrap_err().status, 429);
            while stream.chunks.next().await.is_some() {}
        } else {
            assert_eq!(
                worker
                    .execute(&["codex".into()], request, options())
                    .await
                    .unwrap_err()
                    .http_status(),
                429
            );
        }
    });
    executor.entered.notified().await;
    let mut changed = Auth::clone(&h.get("codex.json"));
    if siwc {
        changed.metadata["access_token"] = json!("dummy-outer-replacement");
    } else {
        changed
            .attributes
            .insert("api_key".into(), "dummy-K2".into());
    }
    h.manager.update_unsaved(changed).unwrap();
    let current = h.get("codex.json");
    assert!(current.credential_version > before.credential_version);
    assert_eq!(current.metadata["siwc"], before.metadata["siwc"]);
    if !siwc {
        assert_eq!(current.access_token(), before.access_token());
    }
    executor.release.add_permits(1);
    task.await.unwrap();
    let live = h.get("codex.json");
    assert_eq!(live.failed, i64::from(siwc));
    assert_eq!(live.unavailable, siwc);
    assert_eq!(live.next_retry_after.is_some(), siwc);
    assert_eq!(
        live.model_states
            .get("gpt-test")
            .is_some_and(|state| state.unavailable && state.next_retry_after.is_some()),
        siwc
    );
    if !siwc {
        assert!(live.last_error.is_none());
        assert!(!live.quota.exceeded);
        assert!(live.model_states.is_empty());
    }
}

#[tokio::test]
async fn api_key_replacement_fences_deferred_429() {
    deferred_result(false, false).await;
}

#[tokio::test]
async fn api_key_replacement_fences_open_stream_429() {
    deferred_result(true, false).await;
}

#[tokio::test]
async fn unchanged_siwc_bearer_keeps_results_after_outer_edit() {
    for streaming in [false, true] {
        deferred_result(streaming, true).await;
    }
}

#[test]
fn adding_api_key_fences_a_previous_oauth_call() {
    let mut before = fixture();
    before.metadata["use_siwc"] = json!(false);
    before.credential_version = 1;
    let result = crate::manager::CallResult {
        access_token: before.access_token().into(),
        credential_version: 1,
        ..Default::default()
    };
    let mut current = before.clone();
    current
        .attributes
        .insert("api_key".into(), "dummy-K2".into());
    current.credential_version = 2;
    assert!(crate::manager::cooldown::is_stale_result(&result, &current));
}
