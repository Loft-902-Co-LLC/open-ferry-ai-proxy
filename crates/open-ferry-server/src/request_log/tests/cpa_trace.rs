//! Ports CLIProxyAPI internal/logging/cpa_trace_test.go (v8.0.10, MIT);
//! `TestFormatCPATraceID` is in open-ferry-core's `request_log/tests.rs`.
//!
//! Upstream's handlers set the trace through a callback; here a handler
//! records the credential its call was given in the request's context, as
//! the dispatcher does.
//!
//! Changed: the "skips committed response" case of
//! `TestCPATraceIDMiddlewareRequiresAuthIndexBeforeResponseCommit` picks the
//! credential while the answer's body is sent, as the head is sent before
//! the body.
//!
//! Added: the trace is set on an answer that isn't logged, and an answer's
//! own trace header is kept.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Request};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::stream;
use http::StatusCode;
use open_ferry_core::auth::Auth;
use open_ferry_core::observe::request_log::CPA_TRACE_ID_HEADER;
use open_ferry_core::observe::{RequestContext, SelectedAuth};

use super::{Harness, send};
use crate::state::AppState;

fn select(context: &RequestContext, index: &str) {
    context.select(SelectedAuth::new(Arc::new(Auth {
        index: index.to_owned(),
        ..Auth::default()
    })));
}

fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/selected",
            post(
                |Extension(context): Extension<Arc<RequestContext>>| async move {
                    select(&context, "auth-index");
                    StatusCode::OK
                },
            ),
        )
        .route(
            "/unselected",
            post(
                |Extension(context): Extension<Arc<RequestContext>>| async move {
                    select(&context, "");
                    StatusCode::OK
                },
            ),
        )
        .route(
            "/committed",
            post(
                |Extension(context): Extension<Arc<RequestContext>>| async move {
                    let body = stream::once(async move {
                        select(&context, "auth-index");
                        Ok::<_, std::io::Error>("late")
                    });
                    Body::from_stream(body).into_response()
                },
            ),
        )
        .route(
            "/v0/management/config",
            get(
                |Extension(context): Extension<Arc<RequestContext>>| async move {
                    select(&context, "auth-index");
                    StatusCode::OK
                },
            ),
        )
        .route(
            "/own",
            post(
                |Extension(context): Extension<Arc<RequestContext>>| async move {
                    select(&context, "auth-index");
                    let mut response = Response::new(Body::empty());
                    response
                        .headers_mut()
                        .insert(CPA_TRACE_ID_HEADER, "mine".parse().unwrap());
                    response
                },
            ),
        )
}

// Ports TestCPATraceIDMiddlewareRequiresAuthIndexBeforeResponseCommit.
#[tokio::test]
async fn cpa_trace_id_middleware_requires_auth_index_before_response_commit() {
    for request_log in [false, true] {
        let harness = Harness::new(request_log);
        let app = harness.app(routes());

        let (_, headers, _) = send(
            &app,
            Request::post("/selected").body(Body::empty()).unwrap(),
        )
        .await;
        let trace = headers.get(CPA_TRACE_ID_HEADER).unwrap().to_str().unwrap();
        // `<yyyymmddHHMMSS>-auth-index-<UUIDv7>`
        assert_eq!(trace.len(), 14 + 1 + "auth-index".len() + 1 + 36, "{trace}");
        let suffix = trace.get(15..).unwrap();
        let (index, id) = suffix.split_at("auth-index-".len());
        assert_eq!(index, "auth-index-");
        assert!(uuid::Uuid::parse_str(id).is_ok(), "{trace}");

        let (_, headers, _) = send(
            &app,
            Request::post("/unselected").body(Body::empty()).unwrap(),
        )
        .await;
        assert!(headers.get(CPA_TRACE_ID_HEADER).is_none());

        let (_, headers, body) = send(
            &app,
            Request::post("/committed").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(body, "late");
        assert!(headers.get(CPA_TRACE_ID_HEADER).is_none());
    }
}

// Ports TestCPATraceIDConcurrentSelectionAndResponseCommit.
#[tokio::test]
async fn cpa_trace_id_concurrent_selection_and_response_commit() {
    let harness = Harness::new(true);
    let app = harness.app(Router::new().route(
        "/race",
        post(
            |Extension(context): Extension<Arc<RequestContext>>| async move {
                let selecting = tokio::spawn(async move { select(&context, "auth-index") });
                let response = "\n".into_response();
                selecting.await.unwrap();
                response
            },
        ),
    ));
    for _ in 0..100 {
        let (status, _, _) = send(&app, Request::post("/race").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
    }
}

// Not upstream's: an answer that isn't logged gets the trace too, and an
// answer's own trace header is kept.
#[tokio::test]
async fn sets_the_trace_on_every_answer() {
    let harness = Harness::new(true);
    let app = harness.app(routes());
    let (_, headers, _) = send(
        &app,
        Request::get("/v0/management/config")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(headers.get(CPA_TRACE_ID_HEADER).is_some());

    let (_, headers, _) = send(&app, Request::post("/own").body(Body::empty()).unwrap()).await;
    assert_eq!(headers.get(CPA_TRACE_ID_HEADER).unwrap(), "mine");
}
