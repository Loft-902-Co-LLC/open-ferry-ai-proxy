//! Bodies nested too deeply, on every route that takes a model's request:
//! each answers 400 in its own error format and calls nothing, and a body
//! at the limit is called as usual.
//!
//! Not upstream's: upstream forwards a body of any depth, which the
//! translators and executors here would read as an empty one.

use axum::Router;
use http::{HeaderMap, Method, StatusCode};
use open_ferry_core::exec::ExecError;
use serde_json::Value;

use super::{app, authed, content_type, send};
use crate::body::MAX_DEPTH;
use crate::config::ServerConfig;
use crate::testing::Outcome;

/// How deep a refused body is nested: just past the limit, further, and so
/// far a recursive reader would overflow its stack.
const TOO_DEEP: [usize; 4] = [MAX_DEPTH + 1, MAX_DEPTH + 2, 5_000, 100_000];

/// How a route words an error.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// `{"error":{"message":..,"type":..}}`.
    OpenAi,
    /// `{"type":"error","error":{"type":..,"message":..}}`.
    Claude,
}

/// A JSON object with `fields` and one more member nested so that the object
/// is `depth` levels deep.
fn nested(fields: &str, depth: usize) -> String {
    format!(
        r#"{{{fields},"deep":{}0{}}}"#,
        "[".repeat(depth - 1),
        "]".repeat(depth - 1)
    )
}

async fn post(
    app: &Router,
    uri: &str,
    fields: &str,
    depth: usize,
) -> (StatusCode, HeaderMap, String) {
    send(app, authed(Method::POST, uri, &nested(fields, depth))).await
}

/// Checks that `uri` calls with a body of `fields` at the limit, which
/// `accepted` answers, and that it refuses one a level deeper, or far deeper,
/// in `shape`, calling nothing more.
async fn refuses_past_the_limit(uri: &str, fields: &str, accepted: Outcome, shape: Shape) {
    let (app, dispatcher) = app(ServerConfig::default(), vec![accepted]);
    let (status, _, body) = post(&app, uri, fields, MAX_DEPTH).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    assert_eq!(dispatcher.calls().len(), 1, "{uri}");

    for depth in TOO_DEEP {
        let (status, headers, body) = post(&app, uri, fields, depth).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {depth}: {body}");
        assert_eq!(content_type(&headers), "application/json", "{uri}");
        let error: Value = serde_json::from_str(&body).unwrap();
        let (kind, message) = match shape {
            Shape::OpenAi => (&error["error"]["type"], &error["error"]["message"]),
            Shape::Claude => {
                assert_eq!(error["type"], "error", "{uri}: {body}");
                (&error["error"]["type"], &error["error"]["message"])
            }
        };
        assert_eq!(kind, "invalid_request_error", "{uri}: {body}");
        let message = message.as_str().unwrap_or_default();
        assert!(message.contains("nested more than 127"), "{uri}: {body}");
    }
    assert_eq!(
        dispatcher.calls().len(),
        1,
        "{uri}: refused bodies were called"
    );
}

fn reply() -> Outcome {
    Outcome::reply(r#"{"id":"r1"}"#)
}

fn events() -> Outcome {
    Outcome::chunks(&[r#"{"n":1}"#])
}

/// A Responses stream, which wants whole events.
fn response_events() -> Outcome {
    Outcome::chunks(&[
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\"}}\n\n",
    ])
}

#[tokio::test]
async fn chat_completions_refuse_deep_bodies() {
    let chat = r#""model":"gpt-5","messages":[]"#;
    refuses_past_the_limit("/v1/chat/completions", chat, reply(), Shape::OpenAi).await;
    // The stream isn't told by a body that can't be read.
    let stream = r#""model":"gpt-5","messages":[],"stream":true"#;
    refuses_past_the_limit("/v1/chat/completions", stream, events(), Shape::OpenAi).await;
    // A body in the Responses format is converted when it can be read.
    let responses = r#""model":"gpt-5","input":"hi""#;
    refuses_past_the_limit("/v1/chat/completions", responses, reply(), Shape::OpenAi).await;
}

#[tokio::test]
async fn completions_refuse_deep_bodies() {
    // The call is made with the body converted to Chat Completions, which
    // has none of the depth the client's had.
    let completion = r#""model":"gpt-5","prompt":"hi""#;
    refuses_past_the_limit("/v1/completions", completion, reply(), Shape::OpenAi).await;
    let stream = r#""model":"gpt-5","prompt":"hi","stream":true"#;
    refuses_past_the_limit("/v1/completions", stream, events(), Shape::OpenAi).await;
}

#[tokio::test]
async fn responses_refuse_deep_bodies() {
    let request = r#""model":"gpt-5","input":"hi""#;
    let stream = r#""model":"gpt-5","input":"hi","stream":true"#;
    for uri in ["/v1/responses", "/backend-api/codex/responses"] {
        refuses_past_the_limit(uri, request, reply(), Shape::OpenAi).await;
        refuses_past_the_limit(uri, stream, response_events(), Shape::OpenAi).await;
    }
    for uri in [
        "/v1/responses/compact",
        "/backend-api/codex/responses/compact",
    ] {
        refuses_past_the_limit(uri, request, reply(), Shape::OpenAi).await;
    }
}

#[tokio::test]
async fn claude_routes_refuse_deep_bodies() {
    let message = r#""model":"claude-sonnet","max_tokens":1,"messages":[]"#;
    refuses_past_the_limit("/v1/messages", message, reply(), Shape::Claude).await;
    let stream = r#""model":"claude-sonnet","max_tokens":1,"messages":[],"stream":true"#;
    refuses_past_the_limit("/v1/messages", stream, events(), Shape::Claude).await;
    let count = r#""model":"claude-sonnet","messages":[]"#;
    refuses_past_the_limit(
        "/v1/messages/count_tokens",
        count,
        Outcome::reply(r#"{"input_tokens":3}"#),
        Shape::Claude,
    )
    .await;
}

#[tokio::test]
async fn gemini_routes_refuse_deep_bodies() {
    // The model is in the path, so the body's depth is no hindrance to
    // routing it, and a stream is asked for by the path.
    let contents = r#""contents":[{"parts":[{"text":"hi"}]}]"#;
    refuses_past_the_limit(
        "/v1beta/models/gpt-5:generateContent",
        contents,
        reply(),
        Shape::OpenAi,
    )
    .await;
    refuses_past_the_limit(
        "/v1beta/models/gpt-5:streamGenerateContent",
        contents,
        events(),
        Shape::OpenAi,
    )
    .await;
    refuses_past_the_limit(
        "/v1beta/models/gpt-5:streamGenerateContent?alt=sse",
        contents,
        events(),
        Shape::OpenAi,
    )
    .await;
    refuses_past_the_limit(
        "/v1beta/models/gpt-5:countTokens",
        contents,
        Outcome::reply(r#"{"totalTokens":3}"#),
        Shape::OpenAi,
    )
    .await;
}

// Only a body that is JSON, and too deep, is refused for it. One that isn't
// JSON is left to the route, as before, however many arrays it opens; and a
// body of any other kind counts the same, so an array is as deep as an
// object.
#[tokio::test]
async fn only_json_nested_too_deeply_is_refused() {
    let (app, _) = app(
        ServerConfig::default(),
        vec![Outcome::Fail(ExecError::upstream(400, "cut short"))],
    );
    let cut = format!(
        r#"{{"model":"gpt-5","messages":[],"deep":{}0"#,
        "[".repeat(MAX_DEPTH + 10)
    );
    let (status, _, body) = send(&app, authed(Method::POST, "/v1/chat/completions", &cut)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(!body.contains("nested more than"), "{body}");

    let array = format!("{}{}", "[".repeat(200), "]".repeat(200));
    let (status, _, body) = send(&app, authed(Method::POST, "/v1/chat/completions", &array)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("nested more than 127"), "{body}");
}
