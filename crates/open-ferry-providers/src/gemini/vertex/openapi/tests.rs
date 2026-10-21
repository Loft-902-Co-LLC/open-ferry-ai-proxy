// Ported from CLIProxyAPI vertex_openapi_test.go and
// gemini_vertex_executor_openapi_test.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI
use super::*;
use crate::gemini::testing::{
    Mock, Reply, collect, key_auth, options, request, service_account_auth, stream_options,
};

#[test]
fn models_overrides_and_normalization() {
    for (model, partner, normalized) in [
        ("meta/llama-4", true, "meta/llama-4"),
        (
            "publishers/Meta/models/llama-4(high)",
            true,
            "meta/llama-4(high)",
        ),
        ("google/gemini-2.5-flash", false, "google/gemini-2.5-flash"),
        ("models/gemini-2.5-flash", false, "google/gemini-2.5-flash"),
        (
            " gemini-2.5-flash(high) ",
            false,
            "google/gemini-2.5-flash(high)",
        ),
        ("publishers/meta/models/a/b", false, "meta/a/b"),
        ("publishers/", false, "google/publishers/"),
        ("", false, ""),
        ("(high)", false, "google/(high)(high)"),
    ] {
        assert_eq!(is_partner(model), partner, "{model}");
        assert_eq!(normalize_model(model), normalized, "{model}");
    }
    let mut auth = Auth::default();
    assert!(should_execute(&auth, "meta/llama"));
    auth.metadata.insert("openapi".into(), Value::Bool(false));
    assert!(!should_execute(&auth, "meta/llama"));
    auth.attributes.insert("openapi".into(), "1".into());
    assert!(should_execute(&auth, "gemini-2.5-flash"));
    assert!(!should_execute(&auth, "imagen-4"));
    auth.attributes
        .insert("openapi_location".into(), " global ".into());
    assert_eq!(location(&auth, "europe-west4"), "global");
    auth.attributes.clear();
    auth.metadata
        .insert("location".into(), Value::from("global"));
    assert_eq!(location(&auth, "europe-west4"), "europe-west4");
}

#[test]
fn urls_and_official_hosts() {
    for (base, project, expected) in [
        (
            "",
            "p",
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/endpoints/openapi",
        ),
        (
            "https://example.test",
            "",
            "https://example.test/v1/endpoints/openapi",
        ),
        (
            "https://example.test/v1beta1",
            "p",
            "https://example.test/v1beta1/projects/p/locations/us-central1/endpoints/openapi",
        ),
        (
            "https://example.test/v1/projects/p/locations/l/publishers/google/models",
            "p",
            "https://example.test/v1/projects/p/locations/l/endpoints/openapi",
        ),
        (
            "https://example.test/v1/projects/p/locations/l",
            "p",
            "https://example.test/v1/projects/p/locations/l/endpoints/openapi",
        ),
        (
            "https://example.test/v1/endpoints/openapi/",
            "p",
            "https://example.test/v1/endpoints/openapi",
        ),
    ] {
        assert_eq!(base_url(base, project, DEFAULT_LOCATION), expected);
    }
    assert_eq!(
        base_url("", "p", "global"),
        "https://aiplatform.googleapis.com/v1/projects/p/locations/global/endpoints/openapi"
    );
    for base in [
        "",
        "https://aiplatform.googleapis.com",
        "https://us-central1-aiplatform.googleapis.com",
        "https://x.aiplatform.googleapis.com:443/v1",
    ] {
        assert!(google_host(base));
    }
    for base in [
        "https://aiplatform.googleapis.com.example.test",
        "http://127.0.0.1:1234",
    ] {
        assert!(!google_host(base));
    }
}

const CHAT: &str = r#"{"id":"test","object":"chat.completion","model":"meta/llama","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#;
const PAYLOAD: &str = r#"{"model":"meta/llama","messages":[{"role":"user","content":"hello"}]}"#;

#[tokio::test]
async fn third_party_key_and_stream_use_openapi() {
    let mock = Mock::start(Reply::json(CHAT)).await;
    let auth = key_auth("vertex", "dummy-partner-key", &mock.url);
    let executor = VertexExecutor::new("direct");
    let response = executor
        .execute(
            auth.clone(),
            request("publishers/meta/models/llama", PAYLOAD),
            options(&Format::OPENAI),
        )
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&response.payload).contains("hello"));
    let seen = mock.last();
    assert_eq!(seen.target(), "/v1/endpoints/openapi/chat/completions");
    assert_eq!(
        seen.header("authorization"),
        Some("Bearer dummy-partner-key")
    );
    assert_eq!(seen.header("x-goog-api-key"), None);
    assert!(
        seen.header("user-agent")
            .unwrap()
            .starts_with("open-ferry/")
    );
    assert_eq!(seen.json()["model"], "meta/llama");
    let stream = Mock::start(Reply::sse("data: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n")).await;
    let response = executor
        .execute_stream(
            key_auth("vertex", "dummy", &stream.url),
            request("meta/llama", PAYLOAD),
            stream_options(&Format::OPENAI),
        )
        .await
        .unwrap();
    collect(response).await;
    assert_eq!(
        stream.last().target(),
        "/v1/endpoints/openapi/chat/completions"
    );
    assert_eq!(
        stream.last().json()["stream_options"]["include_usage"],
        true
    );
}

#[tokio::test]
async fn service_account_wins_and_tokens_are_cached() {
    let token = Mock::start(Reply::json(
        r#"{"access_token":"dummy-sa-token","expires_in":3600,"token_type":"Bearer"}"#,
    ))
    .await;
    let mock = Mock::start(Reply::json(CHAT)).await;
    let mut auth = service_account_auth(&token.url, "us-central1");
    auth.attributes
        .insert("api_key".into(), "unused-dummy".into());
    auth.attributes
        .insert("openapi_location".into(), "global".into());
    let auth = Arc::new(auth.clone());
    let executor = VertexExecutor::new("direct").with_service_account_base_url(&mock.url);
    for _ in 0..2 {
        executor
            .execute(
                auth.clone(),
                request("meta/llama", PAYLOAD),
                options(&Format::OPENAI),
            )
            .await
            .unwrap();
    }
    assert_eq!(token.hits(), 1);
    assert_eq!(
        mock.last().header("authorization"),
        Some("Bearer dummy-sa-token")
    );
    assert!(
        mock.last()
            .target()
            .contains("/locations/global/endpoints/openapi/chat/completions")
    );
}

#[tokio::test]
async fn official_api_key_is_rejected_and_counts_are_local() {
    let executor = VertexExecutor::new("direct");
    for url in [
        "",
        "https://aiplatform.googleapis.com",
        "https://us-central1-aiplatform.googleapis.com",
    ] {
        let auth = key_auth("vertex", "dummy-key", url);
        let error = executor
            .execute(
                auth,
                request("meta/llama", PAYLOAD),
                options(&Format::OPENAI),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);
    }
    let response = executor
        .count_tokens(
            Arc::new(Auth::default()),
            request("meta/llama", PAYLOAD),
            options(&Format::OPENAI),
        )
        .await
        .unwrap();
    assert!(!response.payload.is_empty());
}

#[tokio::test]
async fn static_key_metadata_cannot_override_the_openapi_destination() {
    let mock = Mock::start(Reply::json(CHAT)).await;
    let executor = VertexExecutor::new("direct");
    for (key, destination) in [
        ("base_url", "https://relay.example"),
        ("base_url", mock.url.as_str()),
        ("base-url", mock.url.as_str()),
    ] {
        let mut auth = Auth {
            provider: "vertex".into(),
            // Confine even a regressed HTTPS request to this local proxy mock.
            proxy_url: mock.url.clone(),
            metadata: serde_json::json!({"type":"vertex", "access_token":"dummy-key"})
                .as_object()
                .unwrap()
                .clone(),
            ..Auth::default()
        };
        auth.metadata.insert(key.into(), destination.into());
        let error = executor
            .execute(
                Arc::new(auth),
                request("meta/llama", PAYLOAD),
                options(&Format::OPENAI),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert!(
            error
                .message
                .contains("requires service account credentials")
        );
        assert_eq!(mock.hits(), 0);
    }
}
