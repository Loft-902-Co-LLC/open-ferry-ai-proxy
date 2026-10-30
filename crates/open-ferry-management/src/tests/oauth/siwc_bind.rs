// Ported from CLIProxyAPI internal/cmd/siwc_bind_test.go and
// internal/api/handlers/management/oauth_siwc_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use open_ferry_core::auth::AuthStore as _;

mod admission;
mod mutation_identity;
mod round3;
mod round4;
mod round5;
mod round6;
mod round7;
mod route_parity;
mod status;

const START: &str =
    "/v8/management/oauth/auth-url?provider=codex&mode=siwc-bind&name=codex-bind.json";
const OUTER: &str = r#"{"type":"codex","access_token":"outer-bind-test","refresh_token":"outer-refresh-test","priority":9,"unknown":{"keep":true},"siwc":{"device_id":"123e4567-e89b-42d3-a456-426614174000"}}"#;

fn bind_api(dir: &AuthDir) -> Api {
    bind_api_with(dir, dir.config())
}
fn bind_api_with(dir: &AuthDir, config: Config) -> Api {
    let api = Api::over_with(dir, config, None);
    api.manager.load().unwrap();
    api
}

fn login_api(dir: &AuthDir, upstream: &str) -> Api {
    let api = super::login_api(dir, upstream);
    api.manager.load().unwrap();
    api
}

fn tokens() -> Vec<u8> {
    http_response("200 OK", &[("Content-Type", "application/json")],
        br#"{"access_token":"siwc-access-test","refresh_token":"siwc-refresh-test","id_token":"unsigned-test","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#)
}

fn issued() -> Callback {
    Callback {
        code: "bind-test".into(),
        client_id: "issued-bind-test".into(),
        error: String::new(),
    }
}

#[cfg(windows)]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/windows_junction.rs"
));

#[tokio::test]
async fn validates_targets_before_issuing_a_url() {
    let dir = AuthDir::new();
    let api = bind_api(&dir);
    for name in [
        "../escape.json",
        "missing.json",
        "sub/file.json",
        "C:stream.json",
        "directory.json",
    ] {
        if name == "directory.json" {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        let path =
            format!("/v8/management/oauth/auth-url?provider=codex&mode=siwc-bind&name={name}");
        assert_eq!(api.get(&path).await.status, StatusCode::BAD_REQUEST);
    }
    for payload in [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"type":"claude","access_token":"a","refresh_token":"r"}"#,
        r#"{"type":"codex","access_token":"a"}"#,
        r#"{"type":"codex","access_token":1,"refresh_token":"r"}"#,
        r#"{"type":"codex","access_token":"a","refresh_token":" "}"#,
    ] {
        let path = dir.write("codex-bind.json", payload);
        assert_eq!(api.get(START).await.status, StatusCode::BAD_REQUEST);
        assert!(std::fs::read_to_string(path).unwrap() == payload);
    }
    let outside = AuthDir::new();
    let _target = outside.write("outside.json", OUTER);
    std::fs::remove_file(dir.path().join("codex-bind.json")).unwrap();
    #[cfg(windows)]
    junction(&outside.path(), &dir.path().join("codex-bind.json"));
    #[cfg(unix)]
    std::os::unix::fs::symlink(_target, dir.path().join("codex-bind.json")).unwrap();
    assert_eq!(api.get(START).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(api.state.oauth_sessions().running(), 0);
}

#[tokio::test]
async fn callbacks_keep_issued_client_ids_and_sync_the_persisted_credential() {
    let (logs, _capture) = Logs::capture();
    let upstream = Upstream::answering(tokens()).await;
    for route in ["get", "post", "query", "forwarder"] {
        let dir = AuthDir::new();
        dir.write("codex-bind.json", OUTER);
        let mut config = dir.config();
        config.port = SERVER_PORT;
        let api = bind_api_with(&dir, config);
        api.state.oauth_sessions().overrides().endpoints = Some(upstream.url.clone());
        let start = if route == "forwarder" {
            format!("{START}&is_webui=1")
        } else {
            START.into()
        };
        let answer = api.get(&start).await.expect(StatusCode::OK);
        let state = answer["state"].as_str().unwrap();
        let url = answer["url"].as_str().unwrap();
        assert!(url.contains("agent_name_hint=open-ferry"));
        assert!(url.contains("ext_agent_host_id=urn%3Auuid%3A"));
        assert!(!answer.as_object().unwrap().contains_key("device_id"));
        match route {
            "get" => {
                api.get(&format!("/v8/management/oauth/callback?state={state}&code=siwc-code-test&client_id=issued-bind-test")).await.expect(StatusCode::OK);
            }
            "page" => {
                assert_eq!(open_page(&api, &format!("/codex/callback?state={state}&code=siwc-code-test&client_id=issued-bind-test")).await.status, StatusCode::OK);
            }
            "forwarder" => {
                let addr = forwarder(&api, Provider::Codex);
                let response = forward(addr, "GET", &format!("/auth/callback?state={state}&code=siwc-code-test&client_id=issued-bind-test")).await.unwrap();
                let location = response.header("location").unwrap();
                assert!(location.contains("client_id=issued-bind-test"));
                let path = location
                    .strip_prefix(&format!("http://127.0.0.1:{SERVER_PORT}"))
                    .unwrap();
                api.get(path).await.expect(StatusCode::OK);
            }
            _ => {
                let body = if route == "post" {
                    json!({"state":state,"code":"siwc-code-test","client_id":"issued-bind-test"})
                } else {
                    let redirect = reqwest::Url::parse(url)
                        .unwrap()
                        .query_pairs()
                        .find(|(key, _)| key == "redirect_uri")
                        .unwrap()
                        .1
                        .into_owned();
                    json!({"redirect_url":format!("{redirect}?state={state}&code=siwc-code-test&client_id=issued-bind-test")})
                };
                api.post("/v8/management/oauth/callback", &body.to_string())
                    .await
                    .expect(StatusCode::OK);
            }
        }
        completed(&api, state).await;
        let saved = dir.read_json("codex-bind.json");
        let mut stripped = saved.clone();
        stripped.as_object_mut().unwrap().remove("siwc");
        stripped.as_object_mut().unwrap().remove("use_siwc");
        let mut before: Value = serde_json::from_str(OUTER).unwrap();
        before.as_object_mut().unwrap().remove("siwc");
        assert!(stripped == before, "outer credential fields changed");
        assert_eq!(saved["use_siwc"], true);
        assert_eq!(saved["siwc"]["client_id"], "issued-bind-test");
        assert!(
            saved["siwc"]["device_id"]
                == serde_json::from_str::<Value>(OUTER).unwrap()["siwc"]["device_id"]
        );
        assert_eq!(api.sync.calls().len(), 1);
        let live = api.manager.list();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].metadata["siwc"]["client_id"], "issued-bind-test");
        let status = api
            .get(&format!("/v8/management/oauth/status?state={state}"))
            .await;
        assert!(!status.body.contains("123e4567"));
    }
    let requests = upstream.requests();
    assert_eq!(requests.len(), 4);
    for request in requests {
        let body = request_body(&request);
        assert!(body.contains("client_id=issued-bind-test"));
        assert!(
            !body.contains("device_id") && !body.contains("123e4567") && !body.contains("nonce")
        );
    }
    assert!(!logs.text().contains("123e4567"));
}

#[tokio::test]
async fn substituted_deleted_or_corrupt_targets_are_refused_at_save_time() {
    let upstream = Upstream::answering(tokens()).await;
    for replacement in [
        "replaced",
        "deleted",
        "corrupt",
        "non-codex",
        "no-tokens",
        "symlink",
    ] {
        let dir = AuthDir::new();
        let path = dir.write("codex-bind.json", OUTER);
        let api = login_api(&dir, &upstream.url);
        let state = start_login(&api, START).await;
        match replacement {
            "replaced" => {
                std::fs::rename(&path, dir.path().join("selected.json")).unwrap();
                dir.write("codex-bind.json", OUTER);
            }
            "deleted" => {
                std::fs::remove_file(&path).unwrap();
            }
            "corrupt" => {
                atomic_replace(&path, b"{");
            }
            "non-codex" => {
                atomic_replace(
                    &path,
                    br#"{"type":"claude","access_token":"a","refresh_token":"r"}"#,
                );
            }
            "no-tokens" => {
                atomic_replace(&path, br#"{"type":"codex"}"#);
            }
            "symlink" => {
                std::fs::remove_file(&path).unwrap();
                let _other = dir.write("other.json", OUTER);
                #[cfg(windows)]
                junction(&dir.path(), &path);
                #[cfg(unix)]
                std::os::unix::fs::symlink(_other, &path).unwrap();
            }
            _ => unreachable!(),
        }
        let before = std::fs::read(&path).ok();
        deliver(&api, &state, "codex", issued());
        assert!(!failure(&api, &state).await.is_empty());
        assert!(std::fs::read(&path).ok() == before);
        assert!(api.sync.calls().is_empty());
    }
}

#[tokio::test]
async fn cancel_timeout_shutdown_and_gate_refusal_save_nothing() {
    let upstream = Upstream::answering(tokens()).await;
    for stop in [
        "cancel",
        "timeout",
        "shutdown",
        "gate",
        "cancel-at-lock",
        "shutdown-at-lock",
    ] {
        let dir = AuthDir::new();
        let path = dir.write("codex-bind.json", OUTER);
        let before = std::fs::read(&path).unwrap();
        let api = login_api(&dir, &upstream.url);
        if stop == "timeout" {
            api.state.oauth_sessions().overrides().wait = Some(Duration::from_millis(20));
        }
        let state = start_login(&api, START).await;
        match stop {
            "cancel" => {
                assert!(api.state.oauth_sessions().store().cancel(&state));
            }
            "timeout" => {
                assert!(failure(&api, &state).await.contains("Timeout"));
            }
            "shutdown" => {
                api.state.shutdown().await;
            }
            "gate" => {
                let gate = api.state.gate();
                gate.close(gate.epoch()).await.unwrap();
                deliver(&api, &state, "codex", issued());
                assert!(!failure(&api, &state).await.is_empty());
            }
            _ => {
                let guard = open_ferry_core::auth::credential_lock::acquire_credential_lock(&path)
                    .await
                    .unwrap();
                let waiting = Arc::new(tokio::sync::Notify::new());
                api.state.oauth_sessions().overrides().bind_lock_wait = Some(waiting.clone());
                deliver(&api, &state, "codex", issued());
                tokio::time::timeout(Duration::from_secs(5), waiting.notified())
                    .await
                    .unwrap();
                if stop == "cancel-at-lock" {
                    assert!(api.state.oauth_sessions().store().cancel(&state));
                } else {
                    api.state.shutdown().await;
                }
                drop(guard);
                if stop == "cancel-at-lock" {
                    // Wait for the refused save, not gate closure masking it.
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while api.state.oauth_sessions().running() != 0 {
                        assert!(Instant::now() < deadline);
                        tokio::task::yield_now().await;
                    }
                }
                let gate = api.state.gate();
                if gate.state() == open_ferry_core::gate::GateState::Open {
                    gate.close(gate.epoch()).await.unwrap();
                }
            }
        }
        assert!(std::fs::read(&path).unwrap() == before);
        assert!(api.sync.calls().is_empty());
        api.state.shutdown().await;
    }
}

#[tokio::test]
async fn missing_client_and_exchange_errors_do_not_leak_or_save() {
    let (logs, _capture) = Logs::capture();
    for missing in [true, false] {
        let dir = AuthDir::new();
        let path = dir.write("codex-bind.json", OUTER);
        let before = std::fs::read(&path).unwrap();
        let upstream = Upstream::answering(http_response(
            "400 Bad Request",
            &[],
            b"123e4567-e89b-42d3-a456-426614174000",
        ))
        .await;
        let api = login_api(&dir, &upstream.url);
        let state = start_login(&api, START).await;
        let client_id = if missing { "" } else { "issued-bind-test" };
        api.post(
            "/v8/management/oauth/callback",
            &json!({"state":state,"code":"bind-test","client_id":client_id}).to_string(),
        )
        .await
        .expect(if missing {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::OK
        });
        if !missing {
            let status = failure(&api, &state).await;
            assert!(!status.contains("123e4567"));
        }
        assert!(std::fs::read(&path).unwrap() == before);
        assert!(api.sync.calls().is_empty());
        assert_eq!(upstream.requests().len(), usize::from(!missing));
    }
    assert!(!logs.text().contains("123e4567"));
}

fn atomic_replace(path: &std::path::Path, data: &[u8]) {
    use std::io::Write as _;
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    temp.write_all(data).unwrap();
    temp.persist(path).map_err(|error| error.error).unwrap();
}

#[tokio::test]
async fn typo_modes_and_pasted_oauth_errors_never_start_or_save_outer_login() {
    let dir = AuthDir::new();
    let path = dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    for mode in ["SIWC-bind", "siwc_bind"] {
        api.get(&START.replace("siwc-bind", mode))
            .await
            .expect(StatusCode::BAD_REQUEST);
    }
    assert_eq!(api.state.oauth_sessions().running(), 0);
    let before = std::fs::read(&path).unwrap();
    let answer = api.get(START).await.expect(StatusCode::OK);
    let state = answer["state"].as_str().unwrap();
    let redirect = reqwest::Url::parse(answer["url"].as_str().unwrap())
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    api.post(
        "/v8/management/oauth/callback",
        &json!({"state":state, "redirect_url":format!("http://127.0.0.1/cb#state={state}&error=denied")})
            .to_string(),
    )
    .await
    .expect(StatusCode::BAD_REQUEST);
    assert!(
        api.state
            .oauth_sessions()
            .store()
            .is_pending(state, "codex")
    );
    api.post(
        "/v8/management/oauth/callback",
        &json!({"redirect_url":format!("{redirect}?state={state}&error=denied")}).to_string(),
    )
    .await
    .expect(StatusCode::OK);
    assert!(!failure(&api, state).await.is_empty());
    assert!(upstream.requests().is_empty());
    assert!(std::fs::read(&path).unwrap() == before);
}

#[tokio::test]
async fn pending_bind_allows_atomic_refresh_but_refuses_another_user_in_same_workspace() {
    let upstream = Upstream::answering(tokens()).await;
    for different_user in [false, true] {
        let dir = AuthDir::new();
        let mut outer: Value = serde_json::from_str(OUTER).unwrap();
        outer["account_id"] = json!("shared-workspace-test");
        outer["email"] = json!("first@example.test");
        dir.write("codex-bind.json", &outer.to_string());
        let api = login_api(&dir, &upstream.url);
        let state = start_login(&api, START).await;
        let before_auth = dir.store.list().unwrap().remove(0);
        let mut auth = before_auth.clone();
        auth.metadata
            .insert("access_token".into(), json!("rotated-outer-test"));
        auth.metadata
            .insert("refresh_token".into(), json!("rotated-refresh-test"));
        if different_user {
            auth.metadata
                .insert("email".into(), json!("second@example.test"));
        }
        if different_user {
            atomic_replace(
                &dir.path().join("codex-bind.json"),
                &serde_json::to_vec(&auth.metadata).unwrap(),
            );
        } else {
            let patch = open_ferry_core::auth::codex_refresh::CredentialRefreshPatch::new(
                &before_auth.metadata,
                &auth.metadata,
                true,
                false,
            );
            dir.store.apply_codex_refresh(auth, patch).await.unwrap();
        }
        let before = std::fs::read(dir.path().join("codex-bind.json")).unwrap();
        deliver(&api, &state, "codex", issued());
        if different_user {
            assert!(!failure(&api, &state).await.is_empty());
            assert!(std::fs::read(dir.path().join("codex-bind.json")).unwrap() == before);
        } else {
            completed(&api, &state).await;
            let saved = dir.read_json("codex-bind.json");
            assert!(saved["access_token"] == "rotated-outer-test");
            assert!(saved["refresh_token"] == "rotated-refresh-test");
            assert!(saved["use_siwc"] == true);
        }
    }
}

#[tokio::test]
async fn admitted_bind_refuses_cancellation_and_reserves_sync_before_releasing_file_lock() {
    use open_ferry_core::auth::credential_lock::acquire_credential_lock;
    let dir = AuthDir::new();
    let path = dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    api.state.oauth_sessions().overrides().bind_after_sync = Some(pause.clone());
    let state = start_login(&api, START).await;
    let before = dir.store.list().unwrap().remove(0);
    deliver(&api, &state, "codex", issued());
    tokio::time::timeout(Duration::from_secs(5), pause.0.notified())
        .await
        .unwrap();
    let answer = api
        .call(
            Method::DELETE,
            &format!("/v8/management/oauth/session?state={state}"),
            "",
        )
        .await
        .expect(StatusCode::OK);
    assert_eq!(answer["cancelled"], false);
    let mut rotated = before.clone();
    rotated
        .metadata
        .insert("access_token".into(), json!("later-refresh-test"));
    rotated
        .metadata
        .insert("refresh_token".into(), json!("later-refresh-grant"));
    let patch = open_ferry_core::auth::codex_refresh::CredentialRefreshPatch::new(
        &before.metadata,
        &rotated.metadata,
        true,
        false,
    );
    // Starting this real refresh while the bind owns the guard must block.
    let mut refresh = Box::pin(dir.store.apply_codex_refresh(rotated, patch));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut refresh)
            .await
            .is_err(),
        "refresh passed the bind lock after revision allocation"
    );
    let mut acquire = Box::pin(acquire_credential_lock(&path));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut acquire)
            .await
            .is_err(),
        "bind released guard before the after-allocation barrier"
    );
    drop(acquire);
    pause.1.notify_one();
    let saved = tokio::time::timeout(Duration::from_secs(5), refresh)
        .await
        .unwrap()
        .unwrap();
    assert!(saved.auth.metadata["access_token"] == "later-refresh-test");
    assert!(saved.auth.metadata["refresh_token"] == "later-refresh-grant");
    assert!(saved.auth.metadata["use_siwc"] == true);
    assert!(saved.auth.metadata["siwc"]["access_token"] == "siwc-access-test");
    assert!(saved.auth.metadata["siwc"]["refresh_token"] == "siwc-refresh-test");
    let data = serde_json::to_vec(&saved.auth.metadata).unwrap();
    let store = api.state.credential_store().unwrap();
    store
        .sync
        .file_written(open_ferry_core::config::AuthFile {
            path,
            data: data.into(),
        })
        .await
        .unwrap();
    completed(&api, &state).await;
    assert!(api.manager.list()[0].metadata["access_token"] == "later-refresh-test");
}

#[tokio::test]
async fn bind_completes_a_siwc_command_over_a_discarded_rotation() {
    use super::super::{CommandRefresh, CommandSync};
    let dir = AuthDir::new();
    let mut outer: Value = serde_json::from_str(OUTER).unwrap();
    outer["siwc"]["access_token"] = json!("siwc-old-test");
    outer["siwc"]["refresh_token"] = json!("siwc-old-refresh-test");
    dir.write("codex-bind.json", &outer.to_string());
    let upstream = Upstream::answering(tokens()).await;
    let mut api = login_api(&dir, &upstream.url);
    let original = dir.store.list().unwrap().remove(0);
    let id = original.id.clone();
    api.manager.register_unsaved(original.clone()).unwrap();
    // A runtime-only SIWC rotation is discarded without reaching disk,
    // marking the partition: an observation of the file keeps it live.
    api.manager
        .register_executor(Arc::new(CommandRefresh { siwc: true }));
    let set_runtime_only = |value: &str| {
        api.manager
            .edit_unsaved(&id, |auth| {
                auth.attributes.insert("runtime_only".into(), value.into());
                Ok::<_, ()>(true)
            })
            .unwrap()
            .unwrap();
    };
    set_runtime_only("true");
    tokio::time::timeout(Duration::from_secs(5), api.manager.force_refresh(&id))
        .await
        .expect("fixture rotation finishes within 5s")
        .unwrap();
    set_runtime_only("false");
    let observed = api
        .manager
        .adopt_observed(&original, None, |auth| {
            dir.store
                .read_for_adoption(auth)
                .map_err(open_ferry_core::manager::ManagerError::Store)
        })
        .unwrap();
    assert!(
        matches!(
            observed,
            open_ferry_core::manager::AdoptionResult::Published { .. }
        ),
        "the observation publishes the file"
    );
    assert_eq!(
        api.manager.get(&id).unwrap().metadata["siwc"]["access_token"],
        "discarded-live",
        "an observation keeps the marked SIWC partition"
    );
    let sync = Arc::new(CommandSync {
        manager: api.manager.clone(),
        commands: Default::default(),
        observations: Default::default(),
    });
    let state = api.state.clone().with_sync(sync.clone());
    api.router = crate::router(state.clone());
    api.state = state;
    let session = start_login(&api, START).await;
    deliver(&api, &session, "codex", issued());
    completed(&api, &session).await;
    assert_eq!(
        sync.commands.load(Ordering::SeqCst),
        1,
        "bind dispatches command-aware sync"
    );
    assert_eq!(sync.observations.load(Ordering::SeqCst), 0);
    let live = api.manager.get(&id).unwrap();
    assert_eq!(
        live.metadata["siwc"]["access_token"], "siwc-access-test",
        "the bind's command adopts its SIWC tokens over the marker"
    );
    assert_eq!(live.metadata["use_siwc"], true);
    assert_eq!(live.metadata["access_token"], "outer-bind-test");
}

#[tokio::test]
async fn refused_bind_dispatches_no_command() {
    use super::super::CommandSync;
    let (logs, _capture) = Logs::capture();
    let dir = AuthDir::new();
    let mut outer: Value = serde_json::from_str(OUTER).unwrap();
    outer["account_id"] = json!("shared-workspace-test");
    outer["email"] = json!("first@example.test");
    dir.write("codex-bind.json", &outer.to_string());
    let upstream = Upstream::answering(tokens()).await;
    let mut api = login_api(&dir, &upstream.url);
    let original = dir.store.list().unwrap().remove(0);
    api.manager.register_unsaved(original.clone()).unwrap();
    let sync = Arc::new(CommandSync {
        manager: api.manager.clone(),
        commands: Default::default(),
        observations: Default::default(),
    });
    let state = api.state.clone().with_sync(sync.clone());
    api.router = crate::router(state.clone());
    api.state = state;
    let session = start_login(&api, START).await;
    // Another user's credential replaces the target after selection: the
    // bind captures its command, then its identity check refuses the write.
    let mut substituted = original.metadata.clone();
    substituted.insert("email".into(), json!("second@example.test"));
    atomic_replace(
        &dir.path().join("codex-bind.json"),
        &serde_json::to_vec(&substituted).unwrap(),
    );
    let before = std::fs::read(dir.path().join("codex-bind.json")).unwrap();
    // The registration has a command to capture, so none dispatched below
    // means the refusal dropped it.
    assert!(
        crate::token_record::capture_destination_command(
            &api.state,
            &dir.store,
            &original,
            open_ferry_core::auth::CodexCredentialPartition::Siwc,
        )
        .is_some()
    );
    deliver(&api, &session, "codex", issued());
    assert!(!failure(&api, &session).await.is_empty());
    // The log redacts the cause to its kind. With a store set, only the
    // bind's own checks, run after the capture, refuse it as invalid input.
    let text = logs.text();
    assert!(text.contains("SIWC binding failed (InvalidInput"), "{text}");
    assert!(std::fs::read(dir.path().join("codex-bind.json")).unwrap() == before);
    assert_eq!(
        sync.commands.load(Ordering::SeqCst),
        0,
        "a refused bind drops its captured command"
    );
    assert_eq!(sync.observations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn auth_directory_changed_during_bind_is_refused() {
    let dir = AuthDir::new();
    let other = AuthDir::new();
    let alias_parent = AuthDir::new();
    let mut outer: Value = serde_json::from_str(OUTER).unwrap();
    outer["account_id"] = json!("directory-test");
    outer["email"] = json!("directory@example.test");
    dir.write("codex-bind.json", &outer.to_string());
    other.write("codex-bind.json", &outer.to_string());
    let alias = alias_parent.path().join("alias");
    #[cfg(windows)]
    junction(&dir.path(), &alias);
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
    dir.store.set_base_dir(&alias);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let state = start_login(&api, START).await;
    #[cfg(windows)]
    {
        std::fs::remove_dir(&alias).unwrap();
        junction(&other.path(), &alias);
    }
    #[cfg(unix)]
    {
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(other.path(), &alias).unwrap();
    }
    deliver(&api, &state, "codex", issued());
    assert!(!failure(&api, &state).await.is_empty());
    assert!(api.sync.calls().is_empty());
    assert!(dir.read_json("codex-bind.json")["use_siwc"].is_null());
    assert!(other.read_json("codex-bind.json")["use_siwc"].is_null());
}

#[tokio::test]
async fn claude_siwc_bind_mode_is_rejected_without_starting_login() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let answer = api
        .get("/v8/management/oauth/auth-url?provider=claude&mode=siwc-bind&name=codex-bind.json")
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(api.sync.calls().is_empty());
    assert!(dir.read_json("codex-bind.json")["use_siwc"].is_null());
}

#[tokio::test]
async fn management_refuses_real_target_with_leaf_alias() {
    let dir = AuthDir::new();
    let path = dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let state = start_login(&api, START).await;
    let alias = dir.path().join("alias.json");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_file(&path, &alias) {
        if error.raw_os_error() == Some(1314) {
            eprintln!("UNVERIFIED: management file-symlink fixture unavailable (OS 1314)");
            return;
        }
        panic!("file-symlink fixture failed");
    }
    assert_eq!(
        api.get("/v8/management/oauth/auth-url?provider=codex&mode=siwc-bind&name=alias.json")
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    deliver(&api, &state, "codex", issued());
    assert!(!failure(&api, &state).await.is_empty());
    assert!(api.sync.calls().is_empty());
    assert!(std::fs::read(&path).unwrap() == OUTER.as_bytes());
}

// Not upstream's: a store whose guard refuses the credential refuses the bind
// before a URL is issued, and leaves the file as it was.
#[tokio::test]
async fn refuses_a_target_the_file_guard_refuses() {
    let dir = AuthDir::guarded(Some(open_ferry_core::auth::file_guard::FileGuard::new(
        |path| {
            if path
                .file_name()
                .is_some_and(|name| name == "codex-bind.json")
            {
                Err("untrusted owner".to_owned())
            } else {
                Ok(())
            }
        },
    )));
    let api = bind_api(&dir);
    let path = dir.write("codex-bind.json", OUTER);
    assert_eq!(api.get(START).await.status, StatusCode::BAD_REQUEST);
    assert!(std::fs::read_to_string(path).unwrap() == OUTER);
    assert_eq!(api.state.oauth_sessions().running(), 0);
}

// Not upstream's: a credential the guard comes to refuse during sign-in is
// neither saved nor synchronized.
#[tokio::test]
async fn a_target_the_file_guard_refuses_at_save_time_is_not_saved() {
    let trusted = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let check = Arc::clone(&trusted);
    let dir = AuthDir::guarded(Some(open_ferry_core::auth::file_guard::FileGuard::new(
        move |_| {
            if check.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("untrusted owner".to_owned())
            }
        },
    )));
    let path = dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let state = start_login(&api, START).await;
    trusted.store(false, Ordering::SeqCst);
    deliver(&api, &state, "codex", issued());
    assert!(!failure(&api, &state).await.is_empty());
    assert!(std::fs::read(&path).unwrap() == OUTER.as_bytes());
    assert!(api.sync.calls().is_empty());
}

#[tokio::test]
async fn v8_bind_route_has_expiry_one_pending_target_and_no_listener() {
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let port = occupied.local_addr().unwrap().port();
    api.state.oauth_sessions().overrides().ports = Some((port, port));
    let answer = api
        .post(
            "/v8/management/oauth/siwc/bind",
            r#"{"name":"codex-bind.json"}"#,
        )
        .await
        .expect(StatusCode::OK);
    assert!(
        api.state
            .oauth_sessions()
            .forwarder_addr(Provider::Codex)
            .is_none()
    );
    assert_eq!(answer["flow"], "siwc_bind");
    assert_eq!(answer["expires_in"], 300);
    api.post(
        "/v8/management/oauth/siwc/bind",
        r#"{"name":"codex-bind.json"}"#,
    )
    .await
    .expect(StatusCode::CONFLICT);
    let state = answer["state"].as_str().unwrap();
    api.call(
        Method::DELETE,
        &format!("/v8/management/oauth/session?state={state}"),
        "",
    )
    .await
    .expect(StatusCode::OK);
    api.post(
        "/v8/management/oauth/siwc/bind",
        r#"{"name":"codex-bind.json"}"#,
    )
    .await
    .expect(StatusCode::OK);
    for body in ["null", "{", r#"{"name":4}"#, r#"{"name":""}"#] {
        api.post("/v8/management/oauth/siwc/bind", body)
            .await
            .expect(StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn v8_bind_callbacks_reject_conflicts_without_spending_the_code() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let answer = api.get(START).await.expect(StatusCode::OK);
    let state = answer["state"].as_str().unwrap();
    let redirect = reqwest::Url::parse(answer["url"].as_str().unwrap())
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    for suffix in [
        "&state=other",
        "&code=other",
        "&client_id=other",
        "&provider=codex",
    ] {
        api.get(&format!("/v8/management/oauth/callback?state={state}&code=dummy&client_id=dummy&provider=codex{suffix}")).await.expect(StatusCode::BAD_REQUEST);
    }
    for callback in [
        format!("{redirect}?state={state}&code=other&client_id=dummy"),
        format!("{redirect}?state={state}&code=dummy&code=other&client_id=dummy"),
        format!("{redirect}#state={state}&code=dummy&client_id=dummy"),
        format!("http://example.test/auth/callback?state={state}&code=dummy&client_id=dummy"),
        format!(
            "{}?state={state}&code=dummy&client_id=dummy",
            redirect.replacen("://", "://@", 1)
        ),
    ] {
        api.post(
            "/v8/management/oauth/callback",
            &json!({"state": state, "code":"dummy", "client_id":"dummy", "redirect_url":callback})
                .to_string(),
        )
        .await
        .expect(StatusCode::BAD_REQUEST);
    }
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
    assert!(
        api.state
            .oauth_sessions()
            .store()
            .is_pending(state, "codex")
    );
}

#[tokio::test]
async fn auth_file_upload_invalidates_even_an_identical_pending_bind() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let answer = api.get(START).await.expect(StatusCode::OK);
    let state = answer["state"].as_str().unwrap();
    api.post("/v8/management/credentials?name=codex-bind.json", OUTER)
        .await
        .expect(StatusCode::OK);
    assert!(
        !api.state
            .oauth_sessions()
            .store()
            .is_pending(state, "codex")
    );
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
}

#[tokio::test]
async fn v8_credentials_advertise_siwc_without_token_values() {
    let dir = AuthDir::new();
    let mut outer: Value = serde_json::from_str(OUTER).unwrap();
    outer["siwc"] = json!({"access_token":"dummy-access", "refresh_token":"dummy-refresh", "client_id":"dummy-client", "expired":"2030-01-02T03:04:05Z"});
    outer["use_siwc"] = json!(true);
    dir.write("codex-bind.json", &outer.to_string());
    let api = bind_api(&dir);
    let response = api.get("/v8/management/credentials").await;
    assert!(!response.body.contains("dummy-access"));
    assert!(!response.body.contains("dummy-refresh"));
    let response = response.expect(StatusCode::OK);
    let file = &response["files"][0];
    assert_eq!(file["siwc_bind_supported"], true);
    assert_eq!(file["siwc_bound"], true);
    assert_eq!(file["use_siwc"], true);
    assert_eq!(file["siwc_expires_at"], "2030-01-02T03:04:05Z");
    let legacy = api
        .get("/v0/management/auth-files")
        .await
        .expect(StatusCode::OK);
    assert!(legacy["files"][0].get("siwc_bind_supported").is_none());
}

#[tokio::test]
async fn legacy_sinks_refuse_bind_states_and_leave_the_target_untouched() {
    let dir = AuthDir::new();
    let path = dir.write("codex-bind.json", OUTER);
    let api = bind_api(&dir);
    let answer = api.get(START).await.expect(StatusCode::OK);
    let state = answer["state"].as_str().unwrap();
    api.get(&format!(
        "/v0/management/oauth-callback?state={state}&code=dummy&client_id=dummy"
    ))
    .await
    .expect(StatusCode::CONFLICT);
    api.post(
        "/v0/management/oauth-callback",
        &json!({"state":state, "code":"dummy", "client_id":"dummy"}).to_string(),
    )
    .await
    .expect(StatusCode::CONFLICT);
    let page = open_page(
        &api,
        &format!("/codex/callback?state={state}&code=dummy&client_id=dummy"),
    )
    .await;
    assert_eq!(page.status, StatusCode::CONFLICT);
    assert!(
        api.state
            .oauth_sessions()
            .store()
            .is_pending(state, "codex")
    );
    assert_eq!(std::fs::read(path).unwrap(), OUTER.as_bytes());
    assert!(api.sync.calls().is_empty());
}

#[tokio::test]
async fn start_refuses_manager_disk_account_disagreement_including_empty_account() {
    for initial in [None, Some("first-account")] {
        let dir = AuthDir::new();
        let mut outer: Value = serde_json::from_str(OUTER).unwrap();
        outer["email"] = json!("dummy@example.test");
        if let Some(account) = initial {
            outer["account_id"] = json!(account);
        }
        dir.write("codex-bind.json", &outer.to_string());
        let api = bind_api(&dir);
        let id = api.manager.list()[0].id.clone();
        api.manager
            .edit_unsaved(&id, |auth| {
                auth.metadata
                    .insert("account_id".into(), json!("different-account"));
                Ok::<_, ()>(true)
            })
            .unwrap();
        api.post(
            "/v8/management/oauth/siwc/bind",
            r#"{"name":"codex-bind.json"}"#,
        )
        .await
        .expect(StatusCode::BAD_REQUEST);
        assert_eq!(api.state.oauth_sessions().running(), 0);
        assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
    }
}

#[tokio::test]
async fn pending_bind_fences_disk_watcher_and_legacy_upload_account_replacements() {
    for replacement in ["disk", "watcher", "legacy-upload"] {
        for initial in [None, Some("first-account")] {
            let dir = AuthDir::new();
            let mut outer: Value = serde_json::from_str(OUTER).unwrap();
            outer["email"] = json!("dummy@example.test");
            if let Some(account) = initial {
                outer["account_id"] = json!(account);
            }
            let path = dir.write("codex-bind.json", &outer.to_string());
            let upstream = Upstream::answering(tokens()).await;
            let api = login_api(&dir, &upstream.url);
            let state = start_login(&api, START).await;
            outer["account_id"] = json!("different-account");
            match replacement {
                "disk" => atomic_replace(&path, outer.to_string().as_bytes()),
                "watcher" => {
                    let expected = api.manager.list()[0].clone();
                    api.manager
                        .edit_unsaved(&expected.id, |auth| {
                            auth.metadata
                                .insert("account_id".into(), json!("different-account"));
                            Ok::<_, ()>(true)
                        })
                        .unwrap();
                    assert_eq!(
                        api.manager.get(&expected.id).unwrap().registration_epoch,
                        expected.registration_epoch
                    );
                }
                _ => {
                    api.post(
                        "/v0/management/auth-files?name=codex-bind.json",
                        &outer.to_string(),
                    )
                    .await
                    .expect(StatusCode::OK);
                }
            }
            let before = std::fs::read(&path).unwrap();
            if replacement != "legacy-upload" {
                deliver(&api, &state, "codex", issued());
            }
            assert!(!failure(&api, &state).await.is_empty());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
        }
    }
}

#[tokio::test]
async fn legacy_and_v8_uploads_wait_for_codex_lock_even_when_replacing_provider() {
    use open_ferry_core::auth::credential_lock::acquire_credential_lock;
    for route in [
        "/v0/management/auth-files?name=codex-bind.json",
        "/v8/management/credentials?name=codex-bind.json",
    ] {
        for provider in ["codex", "claude"] {
            let dir = AuthDir::new();
            let path = dir.write("codex-bind.json", OUTER);
            let api = Api::over(&dir);
            let guard = acquire_credential_lock(&path).await.unwrap();
            let payload = json!({"type":provider, "access_token":"dummy-upload", "refresh_token":"dummy-upload-refresh"}).to_string();
            let mut upload = Box::pin(api.post(route, &payload));
            assert!(
                tokio::time::timeout(Duration::from_millis(100), &mut upload)
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), OUTER.as_bytes());
            drop(guard);
            upload.await.expect(StatusCode::OK);
            assert_eq!(dir.read_json("codex-bind.json")["type"], provider);
        }
    }
}

#[tokio::test]
async fn incomplete_token_responses_never_enable_siwc() {
    for body in [
        r#"{"access_token":"dummy-access","id_token":"unsigned-test","scope":"openid chatgpt.tokens.use.direct"}"#,
        r#"{"refresh_token":"dummy-refresh","id_token":"unsigned-test","scope":"openid chatgpt.tokens.use.direct"}"#,
    ] {
        let dir = AuthDir::new();
        let path = dir.write("codex-bind.json", OUTER);
        let upstream = Upstream::answering(http_response(
            "200 OK",
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        ))
        .await;
        let api = login_api(&dir, &upstream.url);
        let state = start_login(&api, START).await;
        deliver(&api, &state, "codex", issued());
        assert!(!failure(&api, &state).await.is_empty());
        assert_eq!(std::fs::read(path).unwrap(), OUTER.as_bytes());
        assert!(api.sync.calls().is_empty());
    }
}

#[tokio::test]
async fn bind_callback_rejects_normalized_alternatives_and_accepts_exact_url() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    api.state.oauth_sessions().overrides().ports = Some((1455, 1455));
    let state = start_login(&api, START).await;
    for url in [
        "http://127.0.0.1:1455/other/../auth/callback",
        "http://127.0.0.1:1455/./auth/callback",
        "http://2130706433:1455/auth/callback",
        "http://127.1:1455/auth/callback",
        "http://0x7f.0.0.1:1455/auth/callback",
    ] {
        api.post("/v8/management/oauth/callback", &json!({
            "state":state,"redirect_url":format!("{url}?state={state}&code=dummy&client_id=dummy")
        }).to_string()).await.expect(StatusCode::BAD_REQUEST);
        assert!(upstream.requests().is_empty());
        assert!(
            api.state
                .oauth_sessions()
                .store()
                .is_pending(&state, "codex")
        );
    }
    api.post("/v8/management/oauth/callback", &json!({
        "redirect_url":format!("http://127.0.0.1:1455/auth/callback?state={state}&code=dummy&client_id=dummy")
    }).to_string()).await.expect(StatusCode::OK);
    completed(&api, &state).await;
    assert_eq!(upstream.requests().len(), 1);
}

#[cfg(windows)]
#[tokio::test]
async fn differently_cased_upload_invalidates_the_same_bind_file() {
    let dir = AuthDir::new();
    dir.write("account.json", OUTER);
    let api = bind_api(&dir);
    let answer = api
        .get(&START.replace("codex-bind.json", "account.json"))
        .await
        .expect(StatusCode::OK);
    let state = answer["state"].as_str().unwrap();
    api.post("/v8/management/credentials?name=ACCOUNT.json", OUTER)
        .await
        .expect(StatusCode::OK);
    assert!(
        !api.state
            .oauth_sessions()
            .store()
            .is_pending(state, "codex")
    );
    assert!(
        api.state
            .oauth_sessions()
            .store()
            .get(state)
            .unwrap()
            .status
            .contains("target changed")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn differently_cased_distinct_upload_preserves_the_other_bind_file() {
    let dir = AuthDir::new();
    dir.write("account.json", OUTER);
    let api = bind_api(&dir);
    let answer = api
        .get(&START.replace("codex-bind.json", "account.json"))
        .await
        .expect(StatusCode::OK);
    let state = answer["state"].as_str().unwrap();
    let upper = dir.path().join("ACCOUNT.json");
    if upper.exists() {
        eprintln!("UNVERIFIED: this filesystem is not case-sensitive");
        return;
    }
    api.post("/v8/management/credentials?name=ACCOUNT.json", OUTER)
        .await
        .expect(StatusCode::OK);
    assert!(upper.is_file());
    assert!(
        api.state
            .oauth_sessions()
            .store()
            .is_pending(state, "codex")
    );
    assert!(dir.read_json("account.json").get("use_siwc").is_none());
}

/// Holds a real shared mutation permit on another credential until released.
struct ActiveMutation {
    release: std::sync::mpsc::Sender<()>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl ActiveMutation {
    fn start(api: &Api) -> Self {
        let manager = api.manager.clone();
        let id = manager
            .list()
            .into_iter()
            .find(|auth| auth.file_name == "other.json")
            .unwrap()
            .id
            .clone();
        let (started, ready) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            manager
                .edit_unsaved(&id, |_| {
                    started.send(()).unwrap();
                    // Drop releases this permit, including when the test fails.
                    released.recv().unwrap();
                    Ok::<_, ()>(false)
                })
                .unwrap();
        });
        ready.recv_timeout(Duration::from_secs(30)).unwrap();
        Self {
            release,
            task: Some(task),
        }
    }
}

impl Drop for ActiveMutation {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(task) = self.task.take() {
            task.join().unwrap();
        }
    }
}

fn watch_admission(api: &Api) -> Arc<tokio::sync::Notify> {
    let waiting = Arc::new(tokio::sync::Notify::new());
    api.state.oauth_sessions().overrides().bind_admission_wait = Some(waiting.clone());
    waiting
}

async fn wait_for_bind_task(api: &Api) {
    tokio::time::timeout(Duration::from_secs(12), async {
        while api.state.oauth_sessions().running() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bind task stopped while admission was busy");
}

#[tokio::test]
async fn busy_bind_commits_despite_overlapping_mutations_on_another_credential() {
    let dir = AuthDir::new();
    dir.write("codex-bind.json", OUTER);
    dir.write("other.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let session = start_login(&api, START).await;
    let waiting = watch_admission(&api);
    let other = api
        .manager
        .list()
        .into_iter()
        .find(|auth| auth.file_name == "other.json")
        .unwrap()
        .id
        .clone();
    let active = ActiveMutation::start(&api);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mutations = Arc::new(AtomicUsize::new(0));
    let traffic: Vec<_> = (0..2)
        .map(|_| {
            let manager = api.manager.clone();
            let id = other.clone();
            let stop = stop.clone();
            let mutations = mutations.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    manager
                        .edit_unsaved(&id, |_| {
                            mutations.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(20));
                            Ok::<_, ()>(false)
                        })
                        .unwrap();
                }
            })
        })
        .collect();
    deliver(&api, &session, "codex", issued());
    tokio::time::timeout(Duration::from_secs(2), waiting.notified())
        .await
        .unwrap();
    // Both traffic threads can hold shared permits while one waits on the
    // state mutex. The global reservation must stop their next iterations.
    drop(active);
    let result = tokio::time::timeout(Duration::from_secs(2), completed(&api, &session)).await;
    stop.store(true, Ordering::SeqCst);
    if result.is_err() {
        api.state.oauth_sessions().store().cancel(&session);
    }
    tokio::task::spawn_blocking(move || {
        for task in traffic {
            task.join().unwrap();
        }
    })
    .await
    .unwrap();
    result.expect("continuous unrelated traffic starved the bind");
    assert!(mutations.load(Ordering::SeqCst) >= 2);
    assert_eq!(dir.read_json("codex-bind.json")["use_siwc"], true);
}

#[tokio::test]
async fn busy_bind_expiry_cancellation_and_owner_disconnect_release_admission() {
    for stop in ["expiry", "cancel", "disconnect"] {
        let (logs, _capture) = Logs::capture();
        let dir = AuthDir::new();
        let path = dir.write("codex-bind.json", OUTER);
        dir.write("other.json", OUTER);
        let upstream = Upstream::answering(tokens()).await;
        let api = login_api(&dir, &upstream.url);
        let session = start_login(&api, START).await;
        let waiting = watch_admission(&api);
        let active = ActiveMutation::start(&api);
        deliver(&api, &session, "codex", issued());
        tokio::time::timeout(Duration::from_secs(2), waiting.notified())
            .await
            .unwrap();
        match stop {
            "expiry" => api
                .state
                .oauth_sessions()
                .store()
                .expire_bind_for_test(&session),
            "cancel" => assert!(api.state.oauth_sessions().store().cancel(&session)),
            // Aborting the owning login also drops its detached completion.
            _ => api.state.oauth_sessions().shutdown().await,
        }
        wait_for_bind_task(&api).await;
        if stop == "expiry" {
            assert!(logs.text().contains("SIWC binding failed (Other, OS None)"));
            assert!(logs.text().contains("Failed to save authentication tokens"));
        }
        assert!(std::fs::read(&path).unwrap() == OUTER.as_bytes());
        assert!(api.sync.calls().is_empty());
        drop(active);
        // A leaked writer reservation would prevent this shared mutation.
        let manager = api.manager.clone();
        let (finished, done) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            let id = manager.list()[0].id.clone();
            manager.edit_unsaved(&id, |_| Ok::<_, ()>(false)).unwrap();
            finished.send(()).unwrap();
        });
        done.recv_timeout(Duration::from_secs(2))
            .expect("cancelled bind leaked its reservation");
        task.join().unwrap();
    }
}

#[tokio::test]
async fn busy_bind_releases_both_locks_for_an_upload() {
    use open_ferry_core::auth::credential_lock::acquire_credential_lock;
    let dir = AuthDir::new();
    let path = dir.write("codex-bind.json", OUTER);
    dir.write("other.json", OUTER);
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    api.sync.watch_lock(api.state.clone());
    let session = start_login(&api, START).await;
    let waiting = watch_admission(&api);
    let active = ActiveMutation::start(&api);
    deliver(&api, &session, "codex", issued());
    tokio::time::timeout(Duration::from_secs(30), waiting.notified())
        .await
        .unwrap();
    let mut body: Value = serde_json::from_str(OUTER).unwrap();
    body["upload_marker"] = json!(true);
    let body = body.to_string();
    let mut upload = Box::pin(api.post("/v8/management/credentials?name=codex-bind.json", &body));
    // The sync spy records when file_written is awaited, after the atomic write
    // and management lock release. Reading the file before that rendezvous can
    // itself prevent replacement on Windows. The active mutation keeps the
    // response pending until we explicitly release it.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                answer = &mut upload => panic!(
                    "upload finished before the active mutation was released: {} {}",
                    answer.status, answer.body,
                ),
                () = tokio::time::sleep(Duration::from_millis(10)) => {
                    if !api.sync.lock_held().is_empty() {
                        break;
                    }
                }
            }
        }
    })
    .await
    .expect("upload writes the file and releases the management lock before synchronization");
    assert_eq!(api.sync.lock_held(), [(true, false)]);
    assert_eq!(dir.read_json("codex-bind.json")["upload_marker"], true);
    assert!(api.state.credential_lock().try_lock().is_ok());
    let lock = tokio::time::timeout(Duration::from_secs(30), acquire_credential_lock(&path))
        .await
        .unwrap()
        .unwrap();
    drop(lock);
    wait_for_bind_task(&api).await;
    drop(active);
    tokio::time::timeout(Duration::from_secs(30), upload)
        .await
        .unwrap()
        .expect(StatusCode::OK);
    assert!(dir.read_json("codex-bind.json").get("use_siwc").is_none());
    assert!(
        !api.state
            .oauth_sessions()
            .store()
            .is_pending(&session, "codex")
    );
}
