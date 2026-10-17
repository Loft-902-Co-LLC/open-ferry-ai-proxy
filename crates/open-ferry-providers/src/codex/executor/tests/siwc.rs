// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_auth_test.go
// and codex_executor_compact_test.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use open_ferry_core::auth::codex_refresh::CodexPersistIntent;
use open_ferry_core::auth::{CodexCredentialPartition, CodexRefreshFailure};
use open_ferry_core::exec::HttpTarget;
use open_ferry_core::executor::{AdoptedPartitions, RefreshOptions};

use super::*;

const DEVICE: &str = "a3227b9a-6677-49b8-b08f-10245ed878f1";
const FORBIDDEN: &[&str] = &[
    "originator",
    "chatgpt-account-id",
    "x-codex-routing-hint",
    "conversation_id",
    "conversation-id",
    "x-codex-beta-features",
    "x-codex-custom",
    "version",
    "thread-id",
    "thread_id",
    "x-codex-window-id",
    "x-openai-internal-codex-responses-lite",
    "x-openai-client-request-time",
];
const SSE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_mock\",\"output\":[{\"type\":\"image_generation_call\",\"result\":\"QUJD\",\"output_format\":\"png\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";

fn siwc_auth(base: &str) -> Auth {
    // Isolate the process-wide refresh cache even if loopback ports are reused.
    static NEXT_CLIENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let client = NEXT_CLIENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut auth = (*api_key_auth(base)).clone();
    auth.metadata.insert("type".into(), json!("codex"));
    auth.metadata.insert("use_siwc".into(), json!(true));
    auth.metadata
        .insert("account_id".into(), json!("outer-mock-account"));
    auth.metadata
        .insert("access_token".into(), json!("outer-unused"));
    auth.metadata.insert(
        "siwc".into(),
        json!({
            "access_token": "siwc-mock-access", "refresh_token": "siwc-mock-refresh",
            "client_id": format!("issued-mock-client-{client}"), "id_token": "old-mock-id", "device_id": DEVICE,
            "expired": "2099-01-01T00:00:00Z", "extra": "preserved"
        }),
    );
    for name in FORBIDDEN.iter().copied().chain([
        "user-agent",
        "authorization",
        "session_id",
        "session-id",
        "x-client-request-id",
    ]) {
        auth.attributes
            .insert(format!("header:{name}"), "custom-attempt".into());
    }
    auth
}

fn attempted_headers(mut opts: Options, ids: bool) -> Options {
    for name in FORBIDDEN.iter().copied().chain(["user-agent"]) {
        opts = with_header(opts, name, "caller-Mac OS-attempt");
    }
    if ids {
        opts = with_header(opts, "session-id", "caller-session");
        opts = with_header(opts, "x-client-request-id", "independent-request");
    }
    opts
}

fn assert_identity(seen: &Seen, ids: bool) {
    assert_eq!(
        seen.header("user-agent"),
        Some(crate::codex::siwc::user_agent().as_str())
    );
    assert_eq!(
        seen.header("authorization"),
        Some("Bearer siwc-mock-access")
    );
    for name in FORBIDDEN {
        assert!(seen.header(name).is_none(), "forbidden header {name}");
    }
    assert!(seen.header("session-id").is_none());
    assert_eq!(seen.header("session_id"), ids.then_some("caller-session"));
    assert_eq!(
        seen.header("x-client-request-id"),
        ids.then_some("independent-request")
    );
    assert!(!seen.body.contains(DEVICE));
    assert!(seen.headers.values().all(|value| {
        !value
            .as_bytes()
            .windows(DEVICE.len())
            .any(|bytes| bytes == DEVICE.as_bytes())
    }));
}

// TestCodexExecutorUseSIWCExecuteAndStream and
// TestCodexExecutorUseSIWCImageRequestsRouteThroughResponsesSSE, with L1/L4/L5.
#[tokio::test]
async fn siwc_all_http_egress_has_fixed_identity_and_responses_body() {
    for path in [
        "execute",
        "stream",
        "compact",
        "generate",
        "generate-stream",
        "edit",
        "edit-stream",
    ] {
        for ids in [false, true] {
            let compact = path == "compact";
            let mock = Mock::start(if compact {
                Reply::json(r#"{"id":"resp_mock","output":[]}"#)
            } else {
                Reply::sse(SSE)
            })
            .await;
            let auth = Arc::new(siwc_auth(&mock.url));
            let image = path.starts_with("generate") || path.starts_with("edit");
            let streaming = path.ends_with("stream") || path == "stream";
            let mut opts = if compact {
                compact_options("openai-response")
            } else {
                options(if image {
                    "openai-image"
                } else {
                    "openai-response"
                })
            };
            opts.stream = streaming;
            if image {
                opts.metadata.request_path = if path.starts_with("edit") {
                    "/v1/images/edits"
                } else {
                    "/v1/images/generations"
                }
                .into();
            }
            opts = attempted_headers(opts, ids);
            let req = request(
                if image { "gpt-image-2" } else { "gpt-5" },
                r#"{"model":"gpt-image-2","input":"hello","prompt":"draw a fox","images":[{"image_url":"data:image/png;base64,AA=="}],"prompt_cache_key":"body-only","metadata":{"user_id":"caller-metadata"},"temperature":0.7,"max_output_tokens":10,"prompt_cache_options":{"x":true}}"#,
            );
            let mut config = Config::default();
            config.codex.signin_with_chatgpt_identifier = "config-attempt".into();
            let exec = executor().with_config(Arc::new(config));
            if streaming {
                let (_, error) = collect(exec.execute_stream(auth, req, opts).await.unwrap()).await;
                assert!(error.is_none(), "{path}");
            } else {
                exec.execute(auth, req, opts).await.unwrap();
            }
            let seen = mock.last();
            assert_eq!(
                seen.path,
                if compact {
                    "/responses/compact"
                } else {
                    "/responses"
                },
                "{path}"
            );
            assert_identity(&seen, ids);
            let body = seen.json();
            for absent in [
                "temperature",
                "max_output_tokens",
                "max_completion_tokens",
                "prompt_cache_options",
                "prompt_cache_retention",
                "instructions",
            ] {
                assert!(!exists(&body, absent), "{path}: {absent}");
            }
            if compact {
                assert!(!exists(&body, "stream"));
                assert!(!exists(&body, "store"));
            } else {
                assert_eq!(body["stream"], true);
                assert_eq!(body["store"], false);
            }
            if image {
                assert_eq!(body["tools"][0]["type"], "image_generation");
                assert_eq!(body["tools"][0]["model"], "gpt-image-2");
            } else {
                assert!(
                    body.get("tools")
                        .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
                );
                assert_eq!(body["prompt_cache_key"], "body-only");
            }
        }
    }
}

#[tokio::test]
async fn siwc_http_prefers_the_callers_hyphenated_session_header() {
    for streaming in [false, true] {
        for both in [false, true] {
            let mock = Mock::start(Reply::sse(SSE)).await;
            let auth = Arc::new(siwc_auth(&mock.url));
            let mut opts = attempted_headers(options("openai-response"), true);
            if both {
                opts = with_header(opts, "session_id", "underscore-loses");
            }
            let req = request("gpt-5", r#"{"input":"hello"}"#);
            let exec = executor();
            if streaming {
                assert!(
                    collect(exec.execute_stream(auth, req, opts).await.unwrap())
                        .await
                        .1
                        .is_none()
                );
            } else {
                exec.execute(auth, req, opts).await.unwrap();
            }
            assert_identity(&mock.last(), true);
        }
    }
}

#[tokio::test]
async fn siwc_websockets_enabled_routes_to_websocket_and_missing_token_sends_nothing() {
    use crate::codex::websocket::mock::Server;
    for streaming in [false, true] {
        let mock = Server::once(&[r#"{"type":"response.completed","response":{"id":"siwc-ws","output":[],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}"#]).await;
        let mut auth = siwc_auth(&mock.url);
        auth.attributes.insert("websockets".into(), "true".into());
        let mut opts = attempted_headers(options("openai-response"), true);
        opts.downstream_websocket = true;
        opts.stream = streaming;
        assert!(websocket::routes(&auth, &opts));
        let req = request("gpt-5", r#"{"input":"hello"}"#);
        let exec = executor();
        if streaming {
            let (_, error) = collect(
                exec.execute_stream(Arc::new(auth.clone()), req.clone(), opts.clone())
                    .await
                    .unwrap(),
            )
            .await;
            assert!(error.is_none());
        } else {
            exec.execute(Arc::new(auth.clone()), req.clone(), opts.clone())
                .await
                .unwrap();
        }
        let record = mock.record();
        assert_eq!(record.handshakes.len(), 1);
        let seen = &record.handshakes[0];
        assert_eq!(seen.path, "/responses");
        assert_eq!(seen.header("upgrade"), Some("websocket"));
        assert!(seen.header("sec-websocket-key").is_some());
        assert_eq!(
            seen.header("user-agent"),
            Some(crate::codex::siwc::user_agent().as_str())
        );
        let body: Value = serde_json::from_str(&record.messages[0]).unwrap();
        assert!(!exists(&body, "stream"));
        assert_eq!(body["store"], false);
        assert!(record.bodies.is_empty());

        auth.metadata["siwc"]["access_token"] = json!("");
        let error = if streaming {
            exec.execute_stream(Arc::new(auth), req, opts)
                .await
                .err()
                .unwrap()
        } else {
            exec.execute(Arc::new(auth), req, opts).await.err().unwrap()
        };
        assert_eq!(error.status, 401);
        assert_eq!(mock.record().handshakes.len(), 1);
        assert!(mock.record().bodies.is_empty());
    }
}

#[test]
fn siwc_ids_are_independent_and_the_default_base_is_openai() {
    let mut auth = siwc_auth("");
    assert_eq!(
        endpoint(&auth, DEFAULT_BASE_URL, false),
        "https://api.openai.com/v1/responses"
    );
    auth.attributes
        .insert("base_url".into(), "http://127.0.0.1:9/v1".into());
    assert_eq!(
        endpoint(&auth, DEFAULT_BASE_URL, true),
        "http://127.0.0.1:9/v1/responses/compact"
    );
    for name in ["session_id", "x-client-request-id"] {
        let mut caller = HeaderMap::new();
        caller.insert(name, HeaderValue::from_static("only-caller-value"));
        let headers = build_headers(&auth, &caller, true).unwrap();
        assert_eq!(headers[name], "only-caller-value");
        let other = if name == "session_id" {
            "x-client-request-id"
        } else {
            "session_id"
        };
        assert!(headers.get(other).is_none());
    }
}

// TestCodexExecutorUseSIWCFailsClosedWhenMissingSIWCToken, plus L6.
#[tokio::test]
async fn siwc_missing_token_and_passthrough_send_nothing() {
    let mock = Mock::start(Reply::sse(SSE)).await;
    let exec = executor().with_base_url(&mock.url);
    let mut auth = siwc_auth(&mock.url);
    auth.metadata["siwc"]
        .as_object_mut()
        .unwrap()
        .remove("access_token");
    let auth = Arc::new(auth);
    for compact in [false, true] {
        let opts = if compact {
            compact_options("openai-response")
        } else {
            options("openai-response")
        };
        let error = exec
            .execute(auth.clone(), request("gpt-5", "{}"), opts)
            .await
            .err()
            .unwrap();
        assert_eq!(error.http_status(), 401);
    }
    let error = exec
        .execute_stream(
            auth,
            request("gpt-5", "{}"),
            stream_options("openai-response"),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.http_status(), 401);
    for image_path in ["/v1/images/generations", "/v1/images/edits"] {
        for streaming in [false, true] {
            let mut missing = siwc_auth(&mock.url);
            missing.metadata["siwc"]["access_token"] = json!("   ");
            let mut opts = options("openai-image");
            opts.metadata.request_path = image_path.into();
            let req = request(
                "gpt-image-2",
                r#"{"prompt":"draw","images":[{"image_url":"data:image/png;base64,AA=="}]}"#,
            );
            let error = if streaming {
                exec.execute_stream(Arc::new(missing), req, opts)
                    .await
                    .err()
                    .unwrap()
            } else {
                exec.execute(Arc::new(missing), req, opts)
                    .await
                    .err()
                    .unwrap()
            };
            assert_eq!(error.http_status(), 401);
        }
    }
    for target in [
        HttpTarget::Path("/alpha/search".into()),
        HttpTarget::Url(format!("{}/passthrough", mock.url)),
    ] {
        let error = exec
            .http_request(
                Arc::new(siwc_auth(&mock.url)),
                HttpCall {
                    method: Method::POST,
                    target,
                    headers: HeaderMap::new(),
                    body: Bytes::new(),
                    client_headers: HeaderMap::new(),
                    response_limit: 1024,
                    observation: None,
                },
            )
            .await
            .err()
            .unwrap();
        assert!(error.message.contains("not supported with use_siwc"));
    }
    assert!(mock.requests().is_empty());
}

#[test]
fn siwc_compact_strips_before_payload_finalizer_and_keeps_explicit_instructions() {
    use open_ferry_core::config::{PayloadModelRule, PayloadRule};
    let auth = siwc_auth("");
    let mut config = Config::default();
    config.payload.r#override.push(PayloadRule {
        models: vec![PayloadModelRule {
            name: "gpt-5".into(),
            protocol: "openai-response".into(),
            ..Default::default()
        }],
        params: vec![(
            "temperature".into(),
            open_ferry_core::config::AnyValue::Float(0.25),
        )],
    });
    let req = request(
        "gpt-5",
        r#"{"input":"hello","instructions":"caller instructions","temperature":1,"store":true,"stream":true}"#,
    );
    let body = prepare_body(
        Kind::Compact,
        Context {
            auth: Some(&auth),
            config: Some(&config),
            models: None,
        },
        &req,
        &compact_options("openai-response"),
    )
    .unwrap();
    assert_eq!(body.body["temperature"], 0.25);
    assert!(!exists(&body.translated, "temperature"));
    assert_eq!(body.body["instructions"], "caller instructions");
    assert!(!exists(&body.body, "store"));
    assert!(!exists(&body.body, "stream"));
}

fn dual_auth(outer_due: bool, siwc_due: bool) -> Arc<Auth> {
    let mut auth = siwc_auth("");
    auth.metadata
        .insert("refresh_token".into(), json!("outer-mock-refresh"));
    auth.metadata
        .insert("id_token".into(), json!("old-outer-id"));
    auth.metadata.insert(
        "expired".into(),
        json!(if outer_due {
            "2000-01-01T00:00:00Z"
        } else {
            "2099-01-01T00:00:00Z"
        }),
    );
    auth.metadata["siwc"]["expired"] = json!(if siwc_due {
        "2000-01-01T00:00:00Z"
    } else {
        "2099-01-01T00:00:00Z"
    });
    Arc::new(auth)
}

async fn siwc_token_server(fail: bool) -> Mock {
    Mock::start(if fail { Reply::error(400, r#"{"error":"invalid_grant"}"#) } else { Reply::json(r#"{"access_token":"new-siwc-access","refresh_token":"new-siwc-refresh","expires_in":3600,"scope":"openid chatgpt.tokens.use.direct","token_type":"Bearer"}"#) }).await
}

fn dual_executor(outer: &Mock, siwc: &Mock) -> CodexExecutor {
    refresh_executor(outer).with_siwc_endpoints(crate::codex::siwc::Endpoints::with_base(&siwc.url))
}

// TestCodexExecutorRefreshSelectiveAndDual: due partitions, force, switch off.
#[tokio::test]
async fn siwc_refresh_selective_dual_forced_and_disabled() {
    for (outer_due, siwc_due, force, active) in [
        (true, false, false, true),
        (false, true, false, true),
        (true, true, false, true),
        (false, false, true, true),
        (false, true, false, false),
        (false, false, false, true),
    ] {
        let outer = token_server("").await;
        let siwc = siwc_token_server(false).await;
        let mut auth = (*dual_auth(outer_due, siwc_due)).clone();
        auth.metadata.insert("use_siwc".into(), json!(active));
        let before = auth.metadata.clone();
        let after = dual_executor(&outer, &siwc)
            .refresh_with_options(Arc::new(auth), RefreshOptions::forced(force))
            .await
            .unwrap();
        let explicit_unscheduled = !outer_due && !siwc_due;
        let rotated_outer = outer_due || force || explicit_unscheduled;
        let rotated_siwc = siwc_due || force || explicit_unscheduled;
        assert_eq!(outer.requests().len(), usize::from(rotated_outer));
        assert_eq!(siwc.requests().len(), usize::from(rotated_siwc));
        assert!(after.codex_refresh_failure.is_none());
        assert_eq!(after.metadata_str("id_token"), Some("old-outer-id"));
        assert_eq!(after.siwc_token_field("id_token"), "old-mock-id");
        assert_eq!(after.metadata["siwc"]["extra"], "preserved");
        if rotated_outer || rotated_siwc {
            let CodexPersistIntent::Refresh(patch) = &after.codex_persist_intent else {
                panic!("missing refresh patch");
            };
            let (applied, which) = patch.apply(&before);
            assert!(
                applied == after.metadata,
                "patch did not reproduce refreshed metadata"
            );
            assert_eq!(which.outer, rotated_outer.then_some(true));
            assert_eq!(which.siwc, rotated_siwc.then_some(true));
        }
        if rotated_siwc {
            let sent = siwc.last();
            assert!(sent.body.contains("client_id=issued-mock-client"));
            assert!(!sent.body.contains(DEVICE));
            assert_eq!(
                sent.header("user-agent"),
                Some(crate::codex::siwc::user_agent().as_str())
            );
        }
    }
}

#[tokio::test]
async fn siwc_unexpired_unauthorized_refreshes_both_partitions_through_manager() {
    use open_ferry_core::exec::Dispatcher;
    use open_ferry_core::manager::{Manager, Settings};
    use open_ferry_core::models::ModelInfo;
    use open_ferry_core::registry::ModelRegistry;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let app = Router::new().fallback(move |headers: HeaderMap| {
        let recorder = recorder.clone();
        async move {
            let fresh = headers
                .get("authorization")
                .is_some_and(|value| value == "Bearer new-siwc-access");
            recorder.lock().unwrap().push(headers);
            axum::response::Response::builder()
                .status(if fresh { 200 } else { 401 })
                .header(
                    "content-type",
                    if fresh {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .body(Body::from(if fresh {
                    SSE
                } else {
                    r#"{"error":{"message":"revoked token","type":"invalid_request_error"}}"#
                }))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let outer = token_server("").await;
    let siwc = siwc_token_server(false).await;
    let registry = Arc::new(ModelRegistry::new());
    let manager = Manager::new(Settings::default(), registry.clone(), None);
    manager.register_executor(Arc::new(dual_executor(&outer, &siwc)));
    let mut auth = (*dual_auth(false, false)).clone();
    auth.id = "unexpired-siwc-401".into();
    auth.attributes.insert("base_url".into(), base);
    auth.metadata
        .insert("refresh_token".into(), json!("  outer-mock-refresh  "));
    assert!(
        auth.outer_refresh_due_at(chrono::Utc::now(), Some(REFRESH_LEAD))
            .unwrap()
            > chrono::Utc::now()
    );
    assert!(auth.siwc_refresh_due_at(chrono::Utc::now()).unwrap() > chrono::Utc::now());
    registry.register_client(
        &auth.id,
        "codex",
        &[ModelInfo {
            id: "gpt-5".into(),
            ..ModelInfo::default()
        }],
    );
    manager.register(auth).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        manager.execute(
            &["codex".into()],
            request("gpt-5", r#"{"input":"hello"}"#),
            options("openai-response"),
        ),
    )
    .await
    .unwrap();
    server.abort();
    result.unwrap();
    assert_eq!(outer.requests().len(), 1);
    assert_eq!(siwc.requests().len(), 1);
    assert!(
        outer
            .last()
            .body
            .contains("refresh_token=outer-mock-refresh")
    );
    assert!(!outer.last().body.contains("+outer-mock-refresh"));
    let sent = seen.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[0]
            .get("authorization")
            .is_some_and(|value| value == "Bearer siwc-mock-access")
    );
    assert!(
        sent[1]
            .get("authorization")
            .is_some_and(|value| value == "Bearer new-siwc-access")
    );
    let updated = manager.get("unexpired-siwc-401").unwrap();
    assert_eq!(updated.siwc_token_field("access_token"), "new-siwc-access");
    assert_eq!(
        updated.metadata_str("access_token"),
        Some("new-mock-access-token")
    );
    assert!(updated.codex_refresh_failure.is_none());
}

#[tokio::test]
async fn siwc_disabled_api_key_noop_refresh_preserves_existing_failure_marker() {
    let mut auth = siwc_auth("");
    auth.metadata.insert("use_siwc".into(), json!(false));
    auth.metadata["siwc"]
        .as_object_mut()
        .unwrap()
        .remove("refresh_token");
    auth.codex_refresh_failure = Some(CodexRefreshFailure {
        partition: CodexCredentialPartition::Siwc,
        error: ExecError::new(ErrorKind::Upstream, "previous failure"),
    });
    let after = executor().refresh(Arc::new(auth)).await.unwrap();
    let failure = after
        .codex_refresh_failure
        .expect("unchanged API-key refresh lost the failure marker");
    assert_eq!(failure.partition, CodexCredentialPartition::Siwc);
    assert!(
        failure.error.message == "previous failure",
        "failure details changed"
    );
}

#[tokio::test]
async fn siwc_refresh_preserves_omitted_id_tokens_and_accepts_replacements() {
    for (replacement, account) in [(false, "mock-account"), (true, "mock-account"), (true, "")] {
        let new_id = id_token("plus", account);
        let new_claims = parse_jwt_token(&new_id).unwrap();
        let old_claims = json!({"email":"old-identity","https://api.openai.com/auth":{"chatgpt_account_id":"old-mock-account","chatgpt_plan_type":"team"}});
        let old_id = format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(old_claims.to_string())
        );
        let mut outer_body = json!({"access_token":"new-mock-access-token","refresh_token":"new-mock-refresh-token","expires_in":3600});
        let mut siwc_body = json!({"access_token":"new-siwc-access","refresh_token":"new-siwc-refresh","expires_in":3600,"scope":"chatgpt.tokens.use.direct","token_type":"Bearer"});
        if replacement {
            outer_body["id_token"] = json!(new_id);
            siwc_body["id_token"] = json!(new_id);
        }
        assert_eq!(outer_body.get("id_token").is_some(), replacement);
        assert_eq!(siwc_body.get("id_token").is_some(), replacement);
        let outer = Mock::start(Reply::json(&outer_body.to_string())).await;
        let siwc = Mock::start(Reply::json(&siwc_body.to_string())).await;
        let mut auth = (*dual_auth(true, true)).clone();
        auth.metadata.insert("id_token".into(), json!(old_id));
        auth.metadata
            .insert("email".into(), old_claims["email"].clone());
        auth.metadata
            .insert("account_id".into(), json!("old-mock-account"));
        auth.metadata.insert("plan_type".into(), json!("team"));
        auth.attributes.insert("plan_type".into(), "team".into());
        let after = dual_executor(&outer, &siwc)
            .refresh(Arc::new(auth))
            .await
            .unwrap();
        let expected_id = if replacement {
            new_id.as_str()
        } else {
            old_id.as_str()
        };
        assert!(
            after.metadata_str("id_token") == Some(expected_id),
            "outer ID-token handling changed"
        );
        let expected_siwc_id = if replacement {
            new_id.as_str()
        } else {
            "old-mock-id"
        };
        assert!(
            after.siwc_token_field("id_token") == expected_siwc_id,
            "SIWC ID-token handling changed"
        );
        let expected_email = if replacement {
            new_claims.email.as_str()
        } else {
            old_claims["email"].as_str().unwrap()
        };
        assert!(
            after.metadata_str("email") == Some(expected_email),
            "ID-token-derived email changed incorrectly"
        );
        let expected_account = if replacement && !account.is_empty() {
            account
        } else {
            "old-mock-account"
        };
        assert!(
            after.metadata_str("account_id") == Some(expected_account),
            "ID-token-derived account changed incorrectly"
        );
        let expected_plan = if replacement { "plus" } else { "team" };
        assert!(
            after.metadata_str("plan_type") == Some(expected_plan),
            "ID-token-derived plan changed incorrectly"
        );
        assert!(
            after.attribute("plan_type") == Some(expected_plan),
            "mirrored plan changed incorrectly"
        );
    }
}

#[tokio::test]
async fn siwc_disabled_dual_failure_returns_transient_outer_error_first() {
    let outer = Mock::start(Reply::error(503, "transient outer unavailable")).await;
    let siwc = siwc_token_server(true).await;
    let mut auth = (*dual_auth(true, true)).clone();
    auth.metadata.insert("use_siwc".into(), json!(false));
    auth.attributes.remove("api_key");
    let error = dual_executor(&outer, &siwc)
        .refresh(Arc::new(auth))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("transient outer unavailable"),
        "outer refresh error was not preferred"
    );
    assert!(
        !error.message.contains("invalid_grant"),
        "SIWC invalid grant masked the outer transient failure"
    );
    assert!(!outer.requests().is_empty());
    assert!(!siwc.requests().is_empty());
}

#[tokio::test]
async fn siwc_partial_refresh_patches_only_success_and_total_failure_is_error() {
    for failed in [
        CodexCredentialPartition::Outer,
        CodexCredentialPartition::Siwc,
    ] {
        let outer = if failed == CodexCredentialPartition::Outer {
            Mock::start(Reply::error(400, r#"{"error":"invalid_grant"}"#)).await
        } else {
            token_server("").await
        };
        let siwc = siwc_token_server(failed == CodexCredentialPartition::Siwc).await;
        let auth = dual_auth(true, true);
        let after = dual_executor(&outer, &siwc)
            .refresh(auth.clone())
            .await
            .unwrap();
        assert_eq!(
            after.codex_refresh_failure.as_ref().unwrap().partition,
            failed
        );
        let CodexPersistIntent::Refresh(patch) = &after.codex_persist_intent else {
            panic!("missing refresh patch");
        };
        let (applied, which) = patch.apply(&auth.metadata);
        assert!(
            applied == after.metadata,
            "patch did not reproduce refreshed metadata"
        );
        assert_eq!(
            which.outer,
            (failed != CodexCredentialPartition::Outer).then_some(true)
        );
        assert_eq!(
            which.siwc,
            (failed != CodexCredentialPartition::Siwc).then_some(true)
        );
    }
    let outer = Mock::start(Reply::error(400, r#"{"error":"invalid_grant"}"#)).await;
    let siwc = siwc_token_server(true).await;
    assert!(
        dual_executor(&outer, &siwc)
            .refresh(dual_auth(true, true))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn siwc_complete_refresh_clears_failure_and_ordinary_refresh_stays_unchanged() {
    let outer = token_server("").await;
    let siwc = siwc_token_server(false).await;
    let mut auth = (*dual_auth(true, true)).clone();
    auth.codex_refresh_failure = Some(CodexRefreshFailure {
        partition: CodexCredentialPartition::Siwc,
        error: ExecError::new(ErrorKind::Upstream, "previous failure"),
    });
    let after = dual_executor(&outer, &siwc)
        .refresh(Arc::new(auth))
        .await
        .unwrap();
    assert!(after.codex_refresh_failure.is_none());
    let after = refresh_executor(&outer)
        .refresh(refreshable(&[]))
        .await
        .unwrap();
    assert_eq!(
        after.metadata_str("access_token"),
        Some("new-mock-access-token")
    );
    assert!(after.metadata.get("siwc").is_none());
    assert_eq!(outer.requests().len(), 2);
    assert_eq!(siwc.requests().len(), 1);
}

// L15: manager's detached refresh-then-save owns this executor call.
#[tokio::test]
async fn siwc_manager_refresh_persists_after_the_waiter_is_cancelled() {
    use open_ferry_core::auth::AuthStore;
    use open_ferry_core::manager::{Manager, Settings};
    use open_ferry_core::registry::ModelRegistry;
    use tokio::sync::Notify;

    struct Store {
        saved: Mutex<Option<Auth>>,
        rotated: Notify,
    }
    impl AuthStore for Store {
        fn list(&self) -> io::Result<Vec<Auth>> {
            Ok(Vec::new())
        }
        fn delete(&self, _: &str) -> io::Result<()> {
            Ok(())
        }
        fn save(&self, auth: &Auth) -> io::Result<String> {
            *self.saved.lock().unwrap() = Some(auth.clone());
            if auth.siwc_token_field("access_token") == "new-siwc-access" {
                self.rotated.notify_one();
            }
            Ok("memory-record".into())
        }
    }
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (sent, allow) = (started.clone(), release.clone());
    let app = Router::new().fallback(move || {
        let (sent, allow) = (sent.clone(), allow.clone());
        async move {
            sent.notify_one();
            allow.notified().await;
            axum::Json(json!({"access_token":"new-siwc-access","refresh_token":"new-siwc-refresh","expires_in":3600,"scope":"chatgpt.tokens.use.direct","token_type":"Bearer"}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let outer = token_server("").await;
    let store = Arc::new(Store {
        saved: Mutex::new(None),
        rotated: Notify::new(),
    });
    let manager = Manager::new(
        Settings::default(),
        Arc::new(ModelRegistry::new()),
        Some(store.clone()),
    );
    manager.register_executor(Arc::new(
        refresh_executor(&outer)
            .with_siwc_endpoints(crate::codex::siwc::Endpoints::with_base(&base)),
    ));
    let mut auth = (*dual_auth(true, true)).clone();
    auth.id = "cancelled-siwc-refresh".into();
    manager.register(auth).unwrap();
    let caller = manager.clone();
    let waiter = tokio::spawn(async move { caller.force_refresh("cancelled-siwc-refresh").await });
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), store.rotated.notified())
        .await
        .unwrap();
    let saved = store.saved.lock().unwrap();
    let saved = saved.as_ref().unwrap();
    assert_eq!(saved.siwc_token_field("access_token"), "new-siwc-access");
    assert_eq!(
        saved.metadata_str("access_token"),
        Some("new-mock-access-token")
    );
    server.abort();
}

// C1-1: without adoption the decision is the pre-adoption formula, for every
// combination of force, refreshable partitions and due partitions.
#[test]
fn siwc_refresh_decision_without_adoption_is_the_unadopted_formula() {
    use crate::codex::executor::auth::refresh_decision;

    for bits in 0..32u8 {
        let [force, has_outer, has_siwc, outer_due, siwc_due] =
            std::array::from_fn(|bit| bits & (1 << bit) != 0);
        // Due implies refreshable, as in `refresh_inner`.
        if (outer_due && !has_outer) || (siwc_due && !has_siwc) {
            continue;
        }
        let explicit_unscheduled = !force && !outer_due && !siwc_due;
        let expected = (
            has_outer && (force || explicit_unscheduled || outer_due),
            has_siwc && (force || explicit_unscheduled || siwc_due),
        );
        assert_eq!(
            refresh_decision(
                RefreshOptions::forced(force),
                (has_outer, outer_due),
                (has_siwc, siwc_due)
            ),
            expected,
            "force={force} outer={has_outer}/{outer_due} siwc={has_siwc}/{siwc_due}"
        );
    }
}

fn adopted(outer: bool, siwc: bool, was_due: bool) -> Option<AdoptedPartitions> {
    Some(AdoptedPartitions {
        outer,
        siwc,
        was_due,
    })
}

// C1-2: the executor half of TestCodexExecutorRefreshSerializesUnderFileLockAndSkipsAlreadyRotatedToken.
// Both partitions were rotated by another process, so nothing is sent and the
// record comes back as passed with no persist intent.
#[tokio::test]
async fn siwc_adopted_partitions_that_were_not_due_send_nothing() {
    let outer = token_server("").await;
    let siwc = siwc_token_server(false).await;
    let auth = dual_auth(false, false);
    let after = dual_executor(&outer, &siwc)
        .refresh_with_options(
            auth.clone(),
            RefreshOptions {
                force: false,
                adopted: adopted(true, true, false),
            },
        )
        .await
        .unwrap();
    assert!(outer.requests().is_empty());
    assert!(siwc.requests().is_empty());
    assert!(after.metadata == auth.metadata);
    assert!(!matches!(
        after.codex_persist_intent,
        CodexPersistIntent::Refresh(_)
    ));
    assert!(after.codex_refresh_failure.is_none());
}

// C1-3: an adopted outer partition is left alone while a due SIWC partition
// on the adopted record is still refreshed.
#[tokio::test]
async fn siwc_adopted_outer_still_refreshes_a_due_siwc() {
    let outer = token_server("").await;
    let siwc = siwc_token_server(false).await;
    let auth = dual_auth(false, true);
    let after = dual_executor(&outer, &siwc)
        .refresh_with_options(
            auth.clone(),
            RefreshOptions {
                force: false,
                adopted: adopted(true, false, true),
            },
        )
        .await
        .unwrap();
    assert!(outer.requests().is_empty());
    assert_eq!(siwc.requests().len(), 1);
    assert_eq!(after.siwc_token_field("access_token"), "new-siwc-access");
    let CodexPersistIntent::Refresh(patch) = &after.codex_persist_intent else {
        panic!("missing refresh patch");
    };
    let (_, which) = patch.apply(&auth.metadata);
    assert_eq!(which.outer, None);
    assert_eq!(which.siwc, Some(true));
}

// C1-4: force overrides adoption and refreshes both partitions.
#[tokio::test]
async fn siwc_force_with_adoption_refreshes_both_partitions() {
    let outer = token_server("").await;
    let siwc = siwc_token_server(false).await;
    let auth = dual_auth(false, false);
    let after = dual_executor(&outer, &siwc)
        .refresh_with_options(
            auth.clone(),
            RefreshOptions {
                force: true,
                adopted: adopted(true, true, false),
            },
        )
        .await
        .unwrap();
    assert_eq!(outer.requests().len(), 1);
    assert_eq!(siwc.requests().len(), 1);
    let CodexPersistIntent::Refresh(patch) = &after.codex_persist_intent else {
        panic!("missing refresh patch");
    };
    let (_, which) = patch.apply(&auth.metadata);
    assert_eq!(which.outer, Some(true));
    assert_eq!(which.siwc, Some(true));
}
