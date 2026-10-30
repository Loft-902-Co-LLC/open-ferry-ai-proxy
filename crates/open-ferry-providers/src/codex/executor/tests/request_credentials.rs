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
async fn r2_native_agents_select_outer_on_http_compact_and_images_without_persisting_mode() {
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
            let original = auth.metadata.clone();
            let mut options = opts(path, agent);
            let sent = options.clone();
            options.headers.insert(
                "user-agent",
                HeaderValue::from_static("changed-after-snapshot"),
            );
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
                        .execute_stream(auth.clone(), req, sent)
                        .await
                        .unwrap(),
                )
                .await;
                assert!(error.is_none());
            } else {
                executor().execute(auth.clone(), req, sent).await.unwrap();
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
            assert_eq!(auth.metadata, original);
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
