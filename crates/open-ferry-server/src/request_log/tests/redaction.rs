//! Not upstream's: what the request log keeps out of its files. Credential
//! headers are masked both ways, the client's key and every upstream
//! secret are scrubbed from the whole log, key-like query parameters and
//! upstream user info are masked, and the management API and OAuth
//! callbacks are never logged.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::routing::post;
use http::StatusCode;
use open_ferry_core::observe::RequestContext;

use super::{Harness, Upstream, post as post_request, send};
use crate::state::AppState;

/// Credentials a client sends in headers, by header name.
const REQUEST_CREDENTIALS: [(&str, &str); 6] = [
    ("authorization", "Bearer client-bearer-0123456789"),
    ("cookie", "session=cookie-value-0123456789"),
    ("x-management-key", "management-key-0123456789"),
    ("x-api-key", "x-api-key-value-0123456789"),
    ("x-goog-api-key", "goog-api-key-value-0123456789"),
    ("proxy-authorization", "Basic proxy-credential-0123456789"),
];

const SET_COOKIE: &str = "session=answer-cookie-0123456789; Path=/";

// Not upstream's: credential headers are masked in the request's headers
// and the answer's.
#[tokio::test]
async fn masks_credential_headers() {
    let harness = Harness::new(true);
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(|| async { ([("set-cookie", SET_COOKIE)], "{}") }),
    ));
    let mut request = Request::post("/v1/responses").header("content-type", "application/json");
    for (name, value) in REQUEST_CREDENTIALS {
        request = request.header(name, value);
    }
    send(&app, request.body(Body::from("{}")).unwrap()).await;
    let (_, log) = harness.only_log();
    for (name, value) in REQUEST_CREDENTIALS {
        let secret = value.rsplit(' ').next().unwrap();
        assert!(!log.contains(secret), "{name} leaked: {log}");
    }
    assert!(log.contains("Authorization: Bearer "), "{log}");
    assert!(log.contains("X-Management-Key: "), "{log}");
    assert!(!log.contains("answer-cookie-0123456789"), "{log}");
    assert!(log.contains("Set-Cookie: "), "{log}");
}

// Not upstream's: the client's key is scrubbed wherever it appears, and an
// upstream secret too, in the answer and the upstream sections.
#[tokio::test]
async fn scrubs_the_client_key_and_upstream_secrets() {
    const CLIENT_KEY: &str = "client-key-in-body-0123456789";
    const SECRET: &str = "sk-upstream-secret-0123456789";
    let harness = Harness::new(true);
    let app = harness.app(Router::new().route(
        "/v1/responses",
        post(
            |State(state): State<AppState>,
             Extension(context): Extension<Arc<RequestContext>>| async move {
                context.set_client_key(CLIENT_KEY);
                Upstream {
                    url: "https://user:upstream-password@api.example.com/v1/responses?key=query-key-0123456789",
                    body: "{\"note\":\"sk-upstream-secret-0123456789\"}",
                    secret: SECRET,
                    chunks: &["{\"echo\":\"sk-upstream-secret-0123456789\"}"],
                    ..Upstream::default()
                }
                .run(&state, &context);
                format!("{{\"echo\":\"{SECRET}\",\"client\":\"{CLIENT_KEY}\"}}")
            },
        ),
    ));
    let body = format!("{{\"key\":\"{CLIENT_KEY}\"}}");
    let (status, _, answer) = send(
        &app,
        post_request(
            "/v1/responses?key=client-query-key-0123456789&model=gpt-5",
            body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(answer.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()));
    let (_, log) = harness.only_log();
    for leaked in [
        CLIENT_KEY,
        SECRET,
        "client-query-key-0123456789",
        "query-key-0123456789",
        "upstream-password",
    ] {
        assert!(!log.contains(leaked), "{leaked} leaked: {log}");
    }
    assert!(log.contains("=== API REQUEST 1 ==="), "{log}");
    assert!(log.contains("=== API RESPONSE 1 ==="), "{log}");
    assert!(log.contains("model=gpt-5"), "{log}");
}

// Not upstream's: the management API and OAuth callbacks are never
// logged, whatever the method.
#[tokio::test]
async fn never_logs_management_or_callbacks() {
    let harness = Harness::new(true);
    let paths = [
        "/v0/management/config",
        "/v8/management/config",
        "/management/config",
        "/codex/callback",
    ];
    let mut routes = Router::new();
    for path in paths {
        routes = routes.route(path, post(|| async { StatusCode::INTERNAL_SERVER_ERROR }));
    }
    let app = harness.app(routes);
    for path in paths {
        let request = Request::post(path)
            .header("x-management-key", "management-key-0123456789")
            .body(Body::from("{}"))
            .unwrap();
        let (status, _, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{path}");
    }
    assert!(harness.logs().is_empty());
}
