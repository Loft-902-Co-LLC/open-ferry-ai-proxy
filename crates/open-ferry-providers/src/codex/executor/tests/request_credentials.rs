//! d946c4fb transport cases, with mock credentials and loopback destinations.
use super::*;
use open_ferry_core::auth::{CodexCredentialPartition, Status};
use open_ferry_core::exec::{Dispatcher, HttpCall, HttpTarget};
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;

const COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"done\",\"output\":[{\"type\":\"image_generation_call\",\"result\":\"QUJD\",\"output_format\":\"png\"}]}}\n\n";
const AGENTS: &[&str] = &[
    "codex-desktop/1",
    "Codex Desktop/1",
    "codex-rs/1",
    "codex_cli_rs/1",
    "codex-tui/1",
    "codex_exec/1",
    "  CoDeX_CLI_RS/1  ",
    "wrapper (CODEX-DESKTOP/1)",
];
fn dual(base: &str) -> Auth {
    let mut auth = (*api_key_auth(base)).clone();
    auth.attributes.remove("api_key");
    auth.metadata = json!({"type":"codex", "use_siwc":true,
        "access_token":"outer-transport-dummy", "expired":"2099-01-01T00:00:00Z",
        "siwc":{"access_token":"nested-transport-dummy", "expired":"2099-01-01T00:00:00Z"}})
    .as_object()
    .unwrap()
    .clone();
    auth
}
fn opts(path: &str, agent: &str) -> Options {
    let mut opts = if path == "compact" {
        compact_options("openai-response")
    } else {
        options(if path == "image" {
            "openai-image"
        } else {
            "openai-response"
        })
    };
    if path == "image" {
        opts.metadata.request_path = "/v1/images/generations".into();
    }
    opts.headers
        .insert("user-agent", HeaderValue::from_str(agent).unwrap());
    opts
}

#[tokio::test]
async fn r3_dispatched_user_agent_selects_http_compact_and_image_credentials() {
    for agent in AGENTS.iter().copied().chain(["ordinary-client/1"]) {
        for path in ["execute", "stream", "compact", "image"] {
            let native = agent != "ordinary-client/1";
            let mock = Mock::start(if path == "compact" {
                Reply::json(r#"{"id":"done","output":[]}"#)
            } else if path == "image" && native {
                Reply::json(r#"{"created":1,"data":[{"b64_json":"QUJD"}]}"#)
            } else {
                Reply::sse(COMPLETED)
            })
            .await;
            let auth = Arc::new(dual(&mock.url));
            let mut options = opts(path, "before-dispatch");
            options
                .headers
                .insert("user-agent", HeaderValue::from_str(agent).unwrap());
            let req = request(
                if path == "image" {
                    "gpt-image-2"
                } else {
                    "gpt-5"
                },
                r#"{"input":[],"prompt":"draw a fox","temperature":0.7}"#,
            );
            if path == "stream" {
                let (_, error) = collect(
                    executor()
                        .execute_stream(auth.clone(), req, options)
                        .await
                        .unwrap(),
                )
                .await;
                assert!(error.is_none());
            } else {
                executor()
                    .execute(auth.clone(), req, options)
                    .await
                    .unwrap();
            }
            let seen = mock.last();
            assert_eq!(
                seen.header("authorization"),
                Some(if native {
                    "Bearer outer-transport-dummy"
                } else {
                    "Bearer nested-transport-dummy"
                })
            );
            assert_eq!(
                seen.path,
                if path == "compact" {
                    "/responses/compact"
                } else if path == "image" && native {
                    "/images/generations"
                } else {
                    "/responses"
                }
            );
        }
    }
}

#[tokio::test]
async fn r2_native_missing_outer_is_401_before_any_http_egress_and_api_key_keeps_precedence() {
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    let mut auth = dual(&mock.url);
    auth.metadata.remove("access_token");
    for path in ["execute", "stream", "compact", "image"] {
        let options = opts(path, "codex-rs/1");
        let req = request("gpt-5", r#"{"input":[],"prompt":"draw"}"#);
        let error = if path == "stream" {
            match executor()
                .execute_stream(Arc::new(auth.clone()), req, options)
                .await
            {
                Err(e) => e,
                Ok(_) => panic!("missing outer bearer was allowed"),
            }
        } else {
            executor()
                .execute(Arc::new(auth.clone()), req, options)
                .await
                .unwrap_err()
        };
        assert_eq!(error.status, 401);
    }
    assert!(mock.requests().is_empty());
    auth.attributes
        .insert("api_key".into(), "api-key-transport-dummy".into());
    executor()
        .execute(
            Arc::new(auth),
            request("gpt-5", r#"{"input":[]}"#),
            opts("execute", "codex-rs/1"),
        )
        .await
        .unwrap();
    assert_eq!(
        mock.last().header("authorization"),
        Some("Bearer api-key-transport-dummy")
    );
}

#[test]
fn r2_native_and_ordinary_default_destinations_use_the_selected_mode() {
    let mut auth = dual("");
    assert_eq!(
        endpoint(
            &auth,
            &opts("execute", "codex-rs/1").headers,
            "https://chatgpt.com/backend-api/codex",
            false
        ),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        endpoint(
            &auth,
            &HeaderMap::new(),
            "https://chatgpt.com/backend-api/codex",
            false
        ),
        "https://api.openai.com/v1/responses"
    );
    auth.attributes
        .insert("api_key".into(), "api-key-transport-dummy".into());
    assert_eq!(
        crate::codex::request::credentials(&auth, &opts("execute", "codex-rs/1").headers).0,
        "api-key-transport-dummy"
    );
}

#[tokio::test]
async fn r2_manager_rejects_only_the_partition_whose_bearer_was_sent() {
    for native in [true, false] {
        let mock = Mock::start(Reply::error(
            401,
            r#"{"error":{"message":"invalid credential"}}"#,
        ))
        .await;
        let registry = Arc::new(ModelRegistry::new());
        let manager = Manager::new(Settings::default(), registry.clone(), None);
        manager.register_executor(Arc::new(executor()));
        let mut auth = dual(&mock.url);
        auth.id = "transport-partition".into();
        auth.status = Status::Active;
        registry.register_client(
            &auth.id,
            "codex",
            &[ModelInfo {
                id: "gpt-5".into(),
                ..ModelInfo::default()
            }],
        );
        manager.register(auth).unwrap();
        let options = opts(
            "execute",
            if native {
                "codex-rs/1"
            } else {
                "ordinary-client/1"
            },
        );
        let result = manager
            .execute(
                &["codex".into()],
                request("gpt-5", r#"{"input":[]}"#),
                options,
            )
            .await;
        assert!(result.is_err());
        let live = manager.get("transport-partition").unwrap();
        assert_eq!(
            mock.last().header("authorization"),
            Some(if native {
                "Bearer outer-transport-dummy"
            } else {
                "Bearer nested-transport-dummy"
            })
        );
        let partition = if native {
            CodexCredentialPartition::Outer
        } else {
            CodexCredentialPartition::Siwc
        };
        assert_eq!(
            live.credential_partition_for_request(
                &opts(
                    "execute",
                    if native {
                        "codex-rs/1"
                    } else {
                        "ordinary-client/1"
                    }
                )
                .headers
            ),
            partition
        );
        assert_eq!(
            (live.rejected_siwc_access_token == live.siwc_access_token()),
            !native
        );
        assert_eq!(
            (live.rejected_access_token == live.outer_access_token()),
            native
        );
    }
}

#[tokio::test]
async fn r2_native_passthrough_keeps_exact_destination_guard_and_outer_bearer() {
    let mock = Mock::start(Reply::json("{}")).await;
    let mut exec = executor();
    exec.http_mock_url = Some(mock.url.clone());
    let mut auth = dual("");
    for agent in AGENTS {
        let call = |target: &str| HttpCall {
            method: http::Method::POST,
            target: HttpTarget::Url(target.into()),
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"{}"),
            client_headers: opts("execute", agent).headers,
            response_limit: 1024,
            observation: None,
        };
        let reply = exec
            .http_request_inner(
                &auth,
                call("https://chatgpt.com/backend-api/codex/alpha/search"),
            )
            .await
            .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(
            mock.last().header("authorization"),
            Some("Bearer outer-transport-dummy")
        );
        assert_eq!(
            exec.http_request_inner(&auth, call("https://example.invalid/responses"))
                .await
                .unwrap_err()
                .status,
            400
        );
    }
    auth.metadata.remove("access_token");
    let call = HttpCall {
        method: http::Method::POST,
        target: HttpTarget::Url("https://chatgpt.com/backend-api/codex/alpha/search".into()),
        headers: HeaderMap::new(),
        body: Bytes::from_static(b"{}"),
        client_headers: opts("execute", "codex-rs/1").headers,
        response_limit: 1024,
        observation: None,
    };
    assert_eq!(
        exec.http_request_inner(&auth, call)
            .await
            .unwrap_err()
            .status,
        401
    );
}

#[tokio::test]
async fn r3_passthrough_selects_request_mode_and_preserves_api_key_precedence() {
    let mock = Mock::start(Reply::json("{}")).await;
    let mut exec = executor();
    exec.http_mock_url = Some(mock.url.clone());
    for native in [true, false] {
        for enabled in [true, false] {
            for keyed in [true, false] {
                let mut auth = dual("");
                auth.metadata.insert("use_siwc".into(), json!(enabled));
                if keyed {
                    auth.attributes
                        .insert("api_key".into(), "passthrough-api-dummy".into());
                }
                let call = HttpCall {
                    method: http::Method::POST,
                    target: HttpTarget::Url(
                        "https://chatgpt.com/backend-api/codex/alpha/search".into(),
                    ),
                    headers: HeaderMap::new(),
                    body: Bytes::from_static(b"{}"),
                    client_headers: opts(
                        "execute",
                        if native {
                            "codex-rs/1"
                        } else {
                            "ordinary-client/1"
                        },
                    )
                    .headers,
                    response_limit: 1024,
                    observation: None,
                };
                exec.http_request_inner(&auth, call).await.unwrap();
                let expected = if keyed && (native || !enabled) {
                    "Bearer passthrough-api-dummy"
                } else {
                    "Bearer outer-transport-dummy"
                };
                assert_eq!(mock.last().header("authorization"), Some(expected));
            }
        }
    }
    let mut auth = dual("");
    auth.metadata.remove("access_token");
    auth.attributes
        .insert("api_key".into(), "passthrough-api-dummy".into());
    let call = HttpCall {
        method: http::Method::POST,
        target: HttpTarget::Url("https://chatgpt.com/backend-api/codex/alpha/search".into()),
        headers: HeaderMap::new(),
        body: Bytes::from_static(b"{}"),
        client_headers: opts("execute", "codex-rs/1").headers,
        response_limit: 1024,
        observation: None,
    };
    exec.http_request_inner(&auth, call).await.unwrap();
    assert_eq!(
        mock.last().header("authorization"),
        Some("Bearer passthrough-api-dummy")
    );
}

#[tokio::test]
async fn r3_missing_native_outer_does_not_reject_or_cool_the_usable_siwc_credential() {
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    let registry = Arc::new(ModelRegistry::new());
    let manager = Manager::new(
        Settings {
            disable_cooling: false,
            ..Settings::default()
        },
        registry.clone(),
        None,
    );
    manager.register_executor(Arc::new(executor()));
    let mut auth = dual(&mock.url);
    auth.id = "missing-native-outer".into();
    auth.status = Status::Active;
    auth.metadata.remove("access_token");
    auth.metadata.insert("disable_cooling".into(), json!(false));
    let stored_metadata = auth.metadata.clone();
    registry.register_client(
        &auth.id,
        "codex",
        &[ModelInfo {
            id: "gpt-5".into(),
            ..ModelInfo::default()
        }],
    );
    manager.register(auth).unwrap();
    for streaming in [false, true] {
        let req = request("gpt-5", r#"{"input":[]}"#);
        let error = if streaming {
            match manager
                .execute_stream(&["codex".into()], req, opts("stream", "codex-rs/1"))
                .await
            {
                Err(e) => e,
                Ok(_) => panic!("missing outer was allowed"),
            }
        } else {
            manager
                .execute(&["codex".into()], req, opts("execute", "codex-rs/1"))
                .await
                .unwrap_err()
        };
        assert_eq!(error.status, 401);
        let current = manager.get("missing-native-outer").unwrap();
        assert!(!current.unavailable);
        assert!(
            current
                .model_states
                .values()
                .all(|model| !model.unavailable && model.next_retry_after.is_none())
        );
        assert!(current.rejected_siwc_access_token.is_empty());
        assert!(current.rejected_access_token.is_empty());
        assert!(error.request_scoped);
    }
    assert!(mock.requests().is_empty());
    manager
        .execute(
            &["codex".into()],
            request("gpt-5", r#"{"input":[]}"#),
            opts("execute", "ordinary-client/1"),
        )
        .await
        .unwrap();
    assert_eq!(
        mock.last().header("authorization"),
        Some("Bearer nested-transport-dummy")
    );
    let recovered = manager.get("missing-native-outer").unwrap();
    assert_eq!(recovered.metadata, stored_metadata);
    assert!(!recovered.unavailable);
    assert!(recovered.rejected_access_token.is_empty());
    assert!(recovered.rejected_siwc_access_token.is_empty());
    assert!(recovered.model_states.values().all(|state| {
        !state.unavailable && state.next_retry_after.is_none() && state.last_error.is_none()
    }));
}

#[tokio::test]
async fn pin_non_ascii_codex_agents_select_outer_across_http_and_stream() {
    for agent in AGENTS {
        for streaming in [false, true] {
            let mock = Mock::start(Reply::sse(COMPLETED)).await;
            let auth = Arc::new(dual(&mock.url));
            let metadata = auth.metadata.clone();
            let mut options = opts("execute", "ordinary-client/1");
            let mut bytes = b"\xff client(\xc3\xa9 ".to_vec();
            bytes.extend_from_slice(agent.as_bytes());
            bytes.extend_from_slice(b")");
            options
                .headers
                .insert("user-agent", HeaderValue::from_bytes(&bytes).unwrap());
            let req = request("gpt-5", r#"{"input":[]}"#);
            if streaming {
                let mut response = executor()
                    .execute_stream(auth.clone(), req, options)
                    .await
                    .unwrap();
                while let Some(chunk) = response.chunks.next().await {
                    chunk.unwrap();
                }
            } else {
                executor()
                    .execute(auth.clone(), req, options)
                    .await
                    .unwrap();
            }
            assert_eq!(
                mock.last().header("authorization"),
                Some("Bearer outer-transport-dummy")
            );
            assert_eq!(auth.metadata, metadata);
        }
    }
}

#[tokio::test]
async fn pin_configured_401_keeps_bearers_and_cools_only_when_requested() {
    use open_ferry_core::manager::RequestScopedErrorRule;
    use std::collections::BTreeMap;

    for action in [
        "stop",
        "continue",
        "stop-and-cooldown",
        "continue-and-cooldown",
    ] {
        for native in [false, true] {
            for streaming in [false, true] {
                let mock = Mock::start(Reply::error(
                    401,
                    r#"{"error":{"message":"scoped-marker"}}"#,
                ))
                .await;
                let registry = Arc::new(ModelRegistry::new());
                let manager = Manager::new(
                    Settings {
                        disable_cooling: false,
                        oauth_request_scoped_errors: BTreeMap::from([(
                            "codex".into(),
                            vec![RequestScopedErrorRule {
                                status: 401,
                                matches: vec!["scoped-marker".into()],
                                action: action.into(),
                                ..Default::default()
                            }],
                        )]),
                        ..Settings::default()
                    },
                    registry.clone(),
                    None,
                );
                manager.register_executor(Arc::new(executor()));
                let mut auth = dual(&mock.url);
                auth.id = "configured-transport".into();
                auth.status = Status::Active;
                auth.metadata.insert("disable_cooling".into(), json!(false));
                let metadata = auth.metadata.clone();
                registry.register_client(
                    &auth.id,
                    "codex",
                    &[ModelInfo {
                        id: "gpt-5".into(),
                        ..ModelInfo::default()
                    }],
                );
                manager.register(auth).unwrap();
                let options = opts(
                    "execute",
                    if native {
                        "codex-rs/1"
                    } else {
                        "ordinary-client/1"
                    },
                );
                let req = request("gpt-5", r#"{"input":[]}"#);
                if streaming {
                    assert!(
                        manager
                            .execute_stream(&["codex".into()], req, options)
                            .await
                            .is_err()
                    );
                } else {
                    assert!(
                        manager
                            .execute(&["codex".into()], req, options)
                            .await
                            .is_err()
                    );
                }
                assert_eq!(mock.requests().len(), 1);
                assert_eq!(
                    mock.last().header("authorization"),
                    Some(if native {
                        "Bearer outer-transport-dummy"
                    } else {
                        "Bearer nested-transport-dummy"
                    })
                );
                let live = manager.get("configured-transport").unwrap();
                assert_eq!(live.metadata, metadata);
                assert!(
                    live.rejected_access_token.is_empty(),
                    "{action}, native={native}, streaming={streaming}"
                );
                assert!(live.rejected_siwc_access_token.is_empty());
                assert_eq!(
                    live.model_states
                        .values()
                        .any(|state| state.unavailable && state.next_retry_after.is_some()),
                    action.ends_with("-cooldown")
                );
            }
        }
    }
}

#[tokio::test]
async fn r3_direct_images_remove_user_agent_before_custom_header_substitution() {
    let mock = Mock::start(Reply::json(r#"{"created":1,"data":[{"b64_json":"QUJD"}]}"#)).await;
    let mut auth = dual(&mock.url);
    auth.attributes
        .insert("api_key".into(), "image-api-dummy".into());
    auth.attributes
        .insert("header:X-From-UA".into(), "$User-Agent".into());
    auth.attributes
        .insert("header:X-From-Tenant".into(), "$X-Tenant".into());
    let mut options = opts("image", "codex-rs/1 caller-private-agent");
    options
        .headers
        .insert("x-tenant", HeaderValue::from_static("caller-team"));
    executor()
        .execute(
            Arc::new(auth),
            request("gpt-image-2", r#"{"prompt":"draw a fox"}"#),
            options,
        )
        .await
        .unwrap();
    let seen = mock.last();
    assert_eq!(seen.path, "/images/generations");
    assert_eq!(seen.header("authorization"), Some("Bearer image-api-dummy"));
    assert!(seen.header("x-from-ua").is_none());
    assert_eq!(seen.header("x-from-tenant"), Some("caller-team"));
    assert_eq!(seen.header("user-agent"), Some(USER_AGENT));
}

#[tokio::test]
async fn r3_whitespace_api_key_cannot_bypass_missing_credential_validation() {
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    for key in [" ", "\t  "] {
        for has_outer in [false, true] {
            let mut auth = dual(&mock.url);
            if !has_outer {
                auth.metadata.remove("access_token");
            }
            auth.attributes.insert("api_key".into(), key.into());
            let error = executor()
                .execute(
                    Arc::new(auth),
                    request("gpt-5", r#"{"input":[]}"#),
                    opts("execute", "codex-rs/1"),
                )
                .await
                .unwrap_err();
            assert_eq!(error.status, 401);
            assert!(error.request_scoped);
        }
    }
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn r3_native_requests_do_not_persist_a_request_specific_siwc_switch() {
    use open_ferry_core::auth::{AuthStore, FileStore};
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(directory.path()));
    let mock = Mock::start(Reply::sse(COMPLETED)).await;
    // Seed an already-bound credential: a new outer login deliberately strips SIWC.
    std::fs::write(
        directory.path().join("native-persist.json"),
        serde_json::to_vec(&dual(&mock.url).metadata).unwrap(),
    )
    .unwrap();
    let mut auth = store.list().unwrap().pop().unwrap();
    auth.attributes.insert("base_url".into(), mock.url.clone());
    store.save_auth(&mut auth).unwrap();
    let before = store.list().unwrap().pop().unwrap().metadata;
    assert_eq!(before["use_siwc"], true);
    let registry = Arc::new(ModelRegistry::new());
    let manager = Manager::new(Settings::default(), registry.clone(), Some(store.clone()));
    manager.register_executor(Arc::new(executor()));
    registry.register_client(
        &auth.id,
        "codex",
        &[ModelInfo {
            id: "gpt-5".into(),
            ..ModelInfo::default()
        }],
    );
    manager.register(auth).unwrap();
    manager
        .execute(
            &["codex".into()],
            request("gpt-5", r#"{"input":[]}"#),
            opts("execute", "codex-rs/1"),
        )
        .await
        .unwrap();
    assert_eq!(
        mock.last().header("authorization"),
        Some("Bearer outer-transport-dummy")
    );
    // Exercise a real manager save and a fresh disk reload, not the caller's immutable Arc.
    let live = (*manager.get("native-persist.json").unwrap()).clone();
    manager.update(live).unwrap();
    store
        .save(&manager.get("native-persist.json").unwrap())
        .unwrap();
    let mut saved = store.list().unwrap().pop().unwrap();
    assert_eq!(saved.metadata, before);
    assert!(saved.use_siwc());
    // FileStore lists raw credentials; synthesis normally attaches this route.
    // Inject only the loopback route, never a production SIWC default.
    saved.attributes.insert("base_url".into(), mock.url.clone());
    let reloaded_registry = Arc::new(ModelRegistry::new());
    reloaded_registry.register_client(
        &saved.id,
        "codex",
        &[ModelInfo {
            id: "gpt-5".into(),
            ..ModelInfo::default()
        }],
    );
    let reloaded = Manager::new(Settings::default(), reloaded_registry, Some(store));
    reloaded.register_executor(Arc::new(executor()));
    reloaded.register_unsaved(saved).unwrap();
    reloaded
        .execute(
            &["codex".into()],
            request("gpt-5", r#"{"input":[]}"#),
            opts("execute", "ordinary-client/1"),
        )
        .await
        .unwrap();
    assert_eq!(
        mock.last().header("authorization"),
        Some("Bearer nested-transport-dummy")
    );
}
