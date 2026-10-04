//! Tests of the routes of `crate::model_definitions`. Upstream has no tests
//! of `GetStaticModelDefinitions`; the expected models are upstream's
//! answers for the same channels.

use http::{Method, StatusCode};
use open_ferry_core::registry::StaticCatalog;
use serde_json::Value;

use super::{Api, LOCAL, request_from};

/// The answer's models' IDs.
fn ids(body: &Value) -> Vec<&str> {
    body["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect()
}

/// Not upstream's: each channel lists the catalog's models for it, each
/// written as upstream writes it.
#[tokio::test]
async fn channels_list_their_models() {
    let api = Api::new();
    for (channel, first) in [
        (
            "claude",
            concat!(
                r#"{"id":"claude-haiku-4-5-20251001","object":"model","created":1759276800,"#,
                r#""owned_by":"anthropic","type":"claude","display_name":"Claude 4.5 Haiku","#,
                r#""context_length":200000,"max_completion_tokens":64000,"#,
                r#""supportedInputModalities":["text","image"],"supportedOutputModalities":["text"],"#,
                r#""thinking":{"min":1024,"max":128000,"zero_allowed":true}}"#,
            ),
        ),
        (
            "gemini",
            concat!(
                r#"{"id":"gemini-2.5-pro","object":"model","created":1750118400,"#,
                r#""owned_by":"google","type":"gemini","display_name":"Gemini 2.5 Pro","#,
                r#""name":"models/gemini-2.5-pro","version":"2.5","#,
                r#""description":"Stable release (June 17th, 2025) of Gemini 2.5 Pro","#,
                r#""inputTokenLimit":1048576,"outputTokenLimit":65536,"#,
                r#""supportedGenerationMethods":["generateContent","countTokens","#,
                r#""createCachedContent","batchGenerateContent"],"#,
                r#""supportedInputModalities":["text","image","audio","video"],"#,
                r#""supportedOutputModalities":["text"],"#,
                r#""thinking":{"min":128,"max":32768,"dynamic_allowed":true}}"#,
            ),
        ),
        (
            "codex",
            concat!(
                r#"{"id":"gpt-5.5","object":"model","created":1776902400,"owned_by":"openai","#,
                r#""type":"openai","display_name":"GPT 5.5","version":"gpt-5.5","#,
                r#""description":"Frontier model for complex coding, research, and real-world work.","#,
                r#""context_length":272000,"max_completion_tokens":128000,"#,
                r#""supported_parameters":["tools"],"supportedInputModalities":["text","image"],"#,
                r#""supportedOutputModalities":["text"],"#,
                r#""thinking":{"levels":["low","medium","high","xhigh"]}}"#,
            ),
        ),
    ] {
        let answer = api
            .get(&format!("/v0/management/model-definitions/{channel}"))
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{channel}");
        let start = format!(r#"{{"channel":"{channel}","models":[{first}"#);
        assert!(
            answer.body.starts_with(&start),
            "{channel}: {}",
            answer.body
        );
    }
    for channel in ["claude", "gemini", "gemini-interactions", "vertex", "codex"] {
        let body = api
            .get(&format!("/v0/management/model-definitions/{channel}"))
            .await
            .expect(StatusCode::OK);
        let want = StaticCatalog::embedded().models_for_channel(channel);
        assert!(!want.is_empty(), "{channel}");
        let want: Vec<&str> = want.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids(&body), want, "{channel}");
        assert_eq!(body["channel"], channel);
    }
}

/// Not upstream's: the channel is trimmed and taken in any case, and named
/// in lower case; a blank one is taken from the query.
#[tokio::test]
async fn channels_are_trimmed_and_taken_in_any_case() {
    let api = Api::new();
    let codex = StaticCatalog::embedded().models_for_channel("codex");
    let codex: Vec<&str> = codex.iter().map(|model| model.id.as_str()).collect();
    for path in [
        "/v0/management/model-definitions/%20Codex%20",
        "/v0/management/model-definitions/CODEX",
        "/v0/management/model-definitions/%20?channel=codex",
    ] {
        let body = api.get(path).await.expect(StatusCode::OK);
        assert_eq!(body["channel"], "codex", "{path}");
        assert_eq!(ids(&body), codex, "{path}");
    }
    let body = api
        .get("/v0/management/model-definitions/%20?channel=Vertex")
        .await
        .expect(StatusCode::OK);
    assert_eq!(body["channel"], "vertex");
}

/// Not upstream's: a blank channel and one the catalog doesn't know are
/// answered with a 400; the unknown one is named as it was sent, trimmed.
#[tokio::test]
async fn unknown_channels_are_refused() {
    let api = Api::new();
    for path in [
        "/v0/management/model-definitions/%20",
        "/v0/management/model-definitions/%20?channel=%20",
    ] {
        let answer = api.get(path).await;
        answer.assert(
            StatusCode::BAD_REQUEST,
            r#"{"error":"channel is required"}"#,
        );
    }
    for (path, channel) in [
        ("Codex-Pro", "Codex-Pro"),
        ("gemini-cli", "gemini-cli"),
        ("%20xai%20", "xai"),
        ("kimi", "kimi"),
        ("aistudio", "aistudio"),
        ("antigravity", "antigravity"),
    ] {
        let answer = api
            .get(&format!("/v0/management/model-definitions/{path}"))
            .await;
        let want = format!(r#"{{"channel":"{channel}","error":"unknown channel"}}"#);
        answer.assert(StatusCode::BAD_REQUEST, &want);
    }
}

/// Not upstream's: the v8 route answers as the v0 one, both take the key,
/// and writes aren't routes.
#[tokio::test]
async fn both_routes_answer_alike() {
    let api = Api::new();
    let v0 = api.get("/v0/management/model-definitions/claude").await;
    let v8 = api
        .get("/v8/management/routing/model-definitions/claude")
        .await;
    assert_eq!(v0.status, StatusCode::OK);
    assert_eq!((v8.status, &v8.body), (v0.status, &v0.body));
    for path in [
        "/v0/management/model-definitions/claude",
        "/v8/management/routing/model-definitions/claude",
    ] {
        let request = request_from(LOCAL, Method::GET, path, "");
        assert_eq!(api.send(request).await.status, StatusCode::UNAUTHORIZED);
        let answer = api.send(super::keyed(Method::PUT, path, "{}")).await;
        assert_eq!(
            (answer.status, answer.body.as_str()),
            (StatusCode::NOT_FOUND, "")
        );
    }
}
