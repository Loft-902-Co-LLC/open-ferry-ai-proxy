// Ported from CLIProxyAPI internal/runtime/executor/gemini_vertex_executor_interactions_test.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Native Interactions and the default bridge against loopback fakes.

use super::*;

const ANSWER: &str = r#"{"id":"interaction_vertex_1","object":"interaction","status":"completed","steps":[{"type":"model_output","content":[{"text":"vertex ok"}]}],"usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30}}"#;
const INPUT: &str = r#"{"model":"gemini-2.5-flash","input":"hi vertex"}"#;

#[tokio::test]
async fn api_key_execute_uses_native_interactions_with_opt_in() {
    let mock = Mock::start(Reply::json(ANSWER)).await;
    let mut auth = Auth::clone(&auth(&mock));
    auth.attributes.insert("interactions".into(), "true".into());
    // Forbidden client identity headers remain suppressed on this path.
    auth.attributes
        .insert("header:x-goog-api-client".into(), "fake-client".into());
    let response = executor()
        .execute(
            Arc::new(auth),
            request("gemini-2.5-flash", INPUT),
            options(&Format::INTERACTIONS),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.target(), "/v1beta1/interactions");
    assert_eq!(seen.header("x-goog-api-key"), Some("test-vertex-key"));
    assert_eq!(seen.header("api-revision"), Some("2026-05-20"));
    assert_eq!(seen.header("x-goog-api-client"), None);
    assert_eq!(seen.json()["input"], "hi vertex");
    assert_eq!(
        serde_json::from_slice::<Value>(&response.payload).unwrap()["id"],
        "interaction_vertex_1"
    );
}

#[tokio::test]
async fn api_key_with_project_uses_project_scoped_native_interactions() {
    let mock = Mock::start(Reply::json(ANSWER)).await;
    let mut auth = Auth::clone(&auth(&mock));
    auth.attributes.insert("interactions".into(), "true".into());
    auth.metadata
        .insert("project_id".into(), "proxy-test".into());
    executor()
        .execute(
            Arc::new(auth),
            request("gemini-2.5-flash", INPUT),
            options(&Format::INTERACTIONS),
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(
        seen.target(),
        "/v1beta1/projects/proxy-test/locations/global/interactions"
    );
    assert_eq!(seen.header("x-goog-api-key"), Some("test-vertex-key"));
    assert_eq!(seen.json()["input"], "hi vertex");
}

#[tokio::test]
async fn service_account_execute_uses_global_native_interactions() {
    let mock = Mock::start(Reply::json(ANSWER)).await;
    let token = token_endpoint().await;
    for location in [Some("global"), Some("europe-west1"), None] {
        let mut auth = service_account_auth(&token.url, location.unwrap_or("global"));
        if location.is_none() {
            auth.metadata.remove("location");
        }
        auth.metadata.insert("interactions".into(), true.into());
        let response = executor()
            .with_service_account_base_url(&mock.url)
            .execute(
                Arc::new(auth),
                request("gemini-2.5-flash", INPUT),
                options(&Format::INTERACTIONS),
            )
            .await
            .unwrap();
        let seen = mock.last();
        assert_eq!(
            seen.target(),
            "/v1beta1/projects/proxy-test/locations/global/interactions"
        );
        assert_eq!(seen.header("authorization"), Some("Bearer sa-token"));
        assert_eq!(seen.header("api-revision"), Some("2026-05-20"));
        assert_eq!(seen.json()["input"], "hi vertex");
        assert_eq!(
            serde_json::from_slice::<Value>(&response.payload).unwrap()["id"],
            "interaction_vertex_1"
        );
        assert_eq!(token.last().target(), "/");
    }
}

#[tokio::test]
async fn native_stream_preserves_large_frames_and_requests_sse() {
    let large = "A".repeat(1024 * 1024);
    let frames = format!(
        "event: interaction.created\ndata: {{\"id\":\"interaction_vertex_stream\",\"status\":\"in_progress\"}}\n\nevent: interaction.completed\ndata: {{\"id\":\"interaction_vertex_stream\",\"status\":\"completed\",\"steps\":[{{\"type\":\"model_output\",\"content\":[{{\"text\":\"{large}\"}}]}}]}}\n\n"
    );
    let mock = Mock::start(Reply::sse(&frames)).await;
    let token = token_endpoint().await;
    for service_account in [false, true] {
        let mut auth = if service_account {
            service_account_auth(&token.url, "global")
        } else {
            Auth::clone(&auth(&mock))
        };
        auth.metadata.insert("interactions".into(), true.into());
        let mut opts = stream_options(&Format::INTERACTIONS);
        opts.headers.insert(
            "api-revision",
            http::HeaderValue::from_static("custom-revision"),
        );
        let response = executor()
            .with_service_account_base_url(&mock.url)
            .execute_stream(Arc::new(auth), request("gemini-2.5-flash", INPUT), opts)
            .await
            .unwrap();
        let (chunks, error) = collect(response).await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(chunks.concat(), frames);
        let seen = mock.last();
        assert_eq!(
            seen.target(),
            if service_account {
                "/v1beta1/projects/proxy-test/locations/global/interactions?alt=sse"
            } else {
                "/v1beta1/interactions?alt=sse"
            }
        );
        assert_eq!(seen.header("api-revision"), Some("custom-revision"));
        assert_eq!(seen.json()["stream"], true);
    }
}

#[tokio::test]
async fn credentials_without_opt_in_use_the_existing_bridge() {
    let mock = Mock::start(Reply::json(OK_ANSWER)).await;
    let token = token_endpoint().await;
    for (location, enabled) in [("global", Some(false)), ("europe-west1", None)] {
        let mut auth = service_account_auth(&token.url, location);
        if let Some(enabled) = enabled {
            auth.metadata.insert("interactions".into(), enabled.into());
        }
        executor()
            .with_service_account_base_url(&mock.url)
            .execute(
                Arc::new(auth),
                request("gemini-2.5-flash", INPUT),
                options(&Format::INTERACTIONS),
            )
            .await
            .unwrap();
        assert_eq!(
            mock.last().target(),
            format!(
                "/v1/projects/proxy-test/locations/{location}/publishers/google/models/gemini-2.5-flash:generateContent"
            )
        );
    }
    executor()
        .execute(
            auth(&mock),
            request("gemini-2.5-flash", INPUT),
            options(&Format::INTERACTIONS),
        )
        .await
        .unwrap();
    assert_eq!(
        mock.last().target(),
        "/v1/publishers/google/models/gemini-2.5-flash:generateContent"
    );
}

#[tokio::test]
async fn native_payload_rules_run_after_input_sanitization() {
    let mock = Mock::start(Reply::json(ANSWER)).await;
    let mut auth = Auth::clone(&auth(&mock));
    auth.attributes.insert("interactions".into(), "true".into());
    let config = Config::parse(
        r#"
payload:
  override:
    - models:
        - name: gemini-2.5-flash
          protocol: interactions
          from-protocol: interactions
      params:
        custom_rule_param: enforced_after_all_transforms
        input.0.id: rule_id
"#,
    )
    .unwrap();
    executor().with_config(Arc::new(config)).execute(Arc::new(auth), request("gemini-2.5-flash", r#"{"model":"gemini-2.5-flash","input":[{"type":"message","id":"unsupported","role":"user","content":[{"type":"text","id":"unsupported_part","text":"hi"}]},{"type":"function_call","call_id":"call_1","name":"test","arguments":{}},{"type":"function_result","id":"unsupported_result","call_id":"call_1","result":"ok"}]}"#), options(&Format::INTERACTIONS)).await.unwrap();
    let body = mock.last().json();
    assert_eq!(body["custom_rule_param"], "enforced_after_all_transforms");
    assert_eq!(body["input"][0]["id"], "rule_id");
    assert!(body["input"][0]["content"][0].get("id").is_none());
    assert_eq!(body["input"][1]["id"], "call_1");
    assert!(body["input"][1].get("call_id").is_none());
    assert!(body["input"][2].get("id").is_none());
}

#[test]
fn native_routing_accepts_only_interactions_and_explicit_opt_in() {
    let mut auth = Auth::default();
    auth.attributes.insert("api_key".into(), "test-key".into());
    for value in ["true", " TRUE ", "1", "false", "0", ""] {
        auth.attributes.insert("interactions".into(), value.into());
        assert_eq!(
            interactions::should_execute_native(&auth, &options(&Format::INTERACTIONS)),
            ["true", " TRUE ", "1"].contains(&value)
        );
        assert!(!interactions::should_execute_native(
            &auth,
            &options(&Format::OPENAI_RESPONSE)
        ));
    }
    auth.metadata
        .insert("native_interactions".into(), true.into());
    assert!(interactions::should_execute_native(
        &auth,
        &options(&Format::INTERACTIONS)
    ));
}

#[tokio::test]
async fn native_errors_keep_the_upstream_status_for_execute_and_stream() {
    let mock = Mock::start(Reply::error(
        429,
        r#"{"error":{"message":"quota exhausted"}}"#,
    ))
    .await;
    let mut auth = Auth::clone(&auth(&mock));
    auth.attributes.insert("interactions".into(), "true".into());
    let auth = Arc::new(auth);
    let executor = executor();
    let error = executor
        .execute(
            Arc::clone(&auth),
            request("gemini-2.5-flash", INPUT),
            options(&Format::INTERACTIONS),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, 429);
    let error = executor
        .execute_stream(
            auth,
            request("gemini-2.5-flash", INPUT),
            stream_options(&Format::INTERACTIONS),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, 429);
}

#[tokio::test]
async fn native_opt_in_keeps_token_counts_on_the_gemini_api() {
    let mock = Mock::start(Reply::json(r#"{"totalTokens":42}"#)).await;
    let mut auth = Auth::clone(&auth(&mock));
    auth.attributes.insert("interactions".into(), "true".into());
    executor()
        .count_tokens(
            Arc::new(auth),
            request("gemini-2.5-flash", INPUT),
            options(&Format::INTERACTIONS),
        )
        .await
        .unwrap();
    assert_eq!(
        mock.last().target(),
        "/v1/publishers/google/models/gemini-2.5-flash:countTokens"
    );
}
