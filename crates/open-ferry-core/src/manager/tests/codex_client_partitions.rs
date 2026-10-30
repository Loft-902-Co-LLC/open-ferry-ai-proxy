// Ports the native credential-selection and fencing cases of upstream d946c4fb.
use super::support::*;
use crate::auth::{Auth, CodexCredentialPartition, is_codex_client_user_agent};
use crate::exec::{Dispatcher, ExecError};
use crate::manager::{CallResult, cooldown::is_stale_result};
use http::{HeaderMap, HeaderValue};
use serde_json::json;

fn fixture() -> Auth {
    auth_with_metadata(
        "codex",
        "codex",
        json!({
            "type":"codex", "use_siwc":true,
            "access_token":"outer-old", "refresh_token":"outer-refresh",
            "expired":"2099-01-01T00:00:00Z",
            "siwc":{"access_token":"nested-old", "refresh_token":"nested-refresh",
                "client_id":"dummy", "expired":"2099-01-01T00:00:00Z"}
        }),
    )
}

fn headers(ua: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", HeaderValue::from_str(ua).unwrap());
    headers
}

#[test]
fn codex_client_selection_is_request_local_and_has_no_bearer_fallback() {
    let mut auth = fixture();
    for ua in [
        "codex-desktop/1",
        "Codex Desktop",
        "codex-rs/1",
        "CODEX_CLI_RS/1",
        " codex-tui/1 ",
        "client (codex_exec/1)",
    ] {
        let h = headers(ua);
        assert!(is_codex_client_user_agent(ua));
        let mut opts = options();
        opts.headers = h.clone();
        assert!(opts.is_codex_client_request());
        assert!(!auth.use_siwc_for_request(&h));
        assert_eq!(auth.access_token_for_request(&h), "outer-old");
        assert_eq!(
            auth.credential_partition_for_request(&h),
            CodexCredentialPartition::Outer
        );
        assert!(auth.use_siwc());
    }
    for ua in ["", "curl/1", "codex", "ordinary"] {
        assert!(!is_codex_client_user_agent(ua));
        assert!(auth.use_siwc_for_request(&headers(ua)));
        assert_eq!(auth.access_token_for_request(&headers(ua)), "nested-old");
    }
    auth.metadata.remove("access_token");
    assert_eq!(auth.access_token_for_request(&headers("codex-tui/1")), "");
    assert_eq!(
        auth.access_token_for_request(&HeaderMap::new()),
        "nested-old"
    );
    auth.attributes.insert("api_key".into(), "dummy-key".into());
    assert_eq!(
        auth.result_access_token_for_request(&headers("codex-rs")),
        ""
    );
    assert_eq!(
        auth.result_access_token_for_request(&HeaderMap::new()),
        "nested-old"
    );
}

#[tokio::test(start_paused = true)]
async fn codex_client_401_refreshes_and_rejects_only_outer_for_every_call_kind() {
    for kind in [Kind::Execute, Kind::Count, Kind::Stream] {
        let h = Harness::new(Default::default());
        let executor = FakeExecutor::with("codex", |call| {
            assert!(
                call.auth.use_siwc(),
                "request must not rewrite the stored switch"
            );
            if call.auth.access_token_for_request(&call.options.headers) == "outer-old" {
                Reply::status(401, "unauthorized")
            } else if call.kind == Kind::Stream {
                Reply::chunks(vec![Ok("ok".into())])
            } else {
                Reply::ok("ok")
            }
        });
        executor.set_refresh(|before| {
            assert_eq!(before.rejected_access_token, "outer-old");
            assert!(before.rejected_siwc_access_token.is_empty());
            let mut after = before.clone();
            after.metadata["access_token"] = json!("outer-new");
            Ok(after)
        });
        h.executor(&executor);
        h.add(fixture(), &["m"]);
        let mut opts = options();
        opts.headers = headers("codex-tui/1");
        match kind {
            Kind::Execute => {
                h.manager
                    .execute(&providers(&["codex"]), request("m"), opts)
                    .await
                    .unwrap();
            }
            Kind::Count => {
                h.manager
                    .count_tokens(&providers(&["codex"]), request("m"), opts)
                    .await
                    .unwrap();
            }
            Kind::Stream => {
                let stream = h
                    .manager
                    .execute_stream(&providers(&["codex"]), request("m"), opts)
                    .await
                    .unwrap();
                assert!(collect(stream).await.1.is_none());
            }
        }
        assert_eq!(executor.refresh_count(), 1);
        let live = h.get("codex");
        assert!(live.use_siwc());
        assert_eq!(live.access_token(), "nested-old");
        assert!(live.rejected_siwc_access_token.is_empty());
    }
}

#[test]
fn outer_result_survives_sibling_rotation_but_never_registration_or_api_key_change() {
    let mut auth = fixture();
    auth.credential_version = 3;
    auth.registration_epoch = 4;
    let result = CallResult {
        credential_version: 2,
        registration_epoch: 4,
        access_token: "outer-old".into(),
        ..Default::default()
    };
    assert!(!is_stale_result(&result, &auth));
    auth.registration_epoch = 5;
    assert!(is_stale_result(&result, &auth));
    auth.registration_epoch = 4;
    auth.metadata["access_token"] = json!("outer-new");
    assert!(is_stale_result(&result, &auth));
    auth.metadata["access_token"] = json!("outer-old");
    auth.attributes.insert("api_key".into(), "dummy".into());
    assert!(is_stale_result(&result, &auth));
}

#[tokio::test(start_paused = true)]
async fn committed_codex_client_stream_rejects_outer_without_rejecting_nested() {
    let h = Harness::new(Default::default());
    let executor = FakeExecutor::with("codex", |_| {
        Reply::chunks(vec![
            Ok("first".into()),
            Err(ExecError::upstream(401, "unauthorized")),
        ])
    });
    h.executor(&executor);
    h.add(fixture(), &["m"]);
    let mut opts = options();
    opts.headers = headers("codex-rs/1");
    let stream = h
        .manager
        .execute_stream(&providers(&["codex"]), request("m"), opts)
        .await
        .unwrap();
    assert!(collect(stream).await.1.is_some());
    let live = h.get("codex");
    assert_eq!(live.rejected_access_token, "outer-old");
    assert!(live.rejected_siwc_access_token.is_empty());
    assert!(live.use_siwc());
}

#[tokio::test(start_paused = true)]
async fn round2_missing_outer_never_falls_back_during_execute_refresh_or_mark() {
    use crate::auth::AuthError;
    let h = Harness::new(Default::default());
    let executor = FakeExecutor::with("codex", |call| {
        assert!(!call.auth.use_siwc_for_request(&call.options.headers));
        assert!(
            call.auth
                .access_token_for_request(&call.options.headers)
                .is_empty()
        );
        Reply::status(401, "missing outer bearer")
    });
    executor.set_refresh(|before| {
        assert!(before.rejected_access_token.is_empty());
        assert!(before.rejected_siwc_access_token.is_empty());
        assert!(!before.siwc_access_token().is_empty());
        Ok(before.clone())
    });
    h.executor(&executor);
    let mut auth = fixture();
    auth.metadata.remove("access_token");
    h.add(auth, &["m"]);
    let mut opts = options();
    opts.headers = headers("codex-tui/1");
    assert!(
        h.manager
            .execute(&providers(&["codex"]), request("m"), opts)
            .await
            .is_err()
    );
    assert_eq!(executor.refresh_count(), 1);
    assert_eq!(executor.calls().len(), 2);
    for partition in [
        None,
        Some(CodexCredentialPartition::Outer),
        Some(CodexCredentialPartition::Siwc),
    ] {
        let live = h.get("codex");
        assert!(live.rejected_access_token.is_empty());
        assert!(live.rejected_siwc_access_token.is_empty());
        assert!(live.has_valid_access_token(h.now()));
        h.manager.mark_result(&CallResult {
            auth_id: live.id.clone(),
            provider: live.provider.clone(),
            credential_version: live.credential_version,
            registration_epoch: live.registration_epoch,
            credential_partition: partition,
            error: Some(AuthError {
                http_status: 401,
                message: "empty captured bearer".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    assert!(h.get("codex").rejected_siwc_access_token.is_empty());
}

#[test]
fn round2_codex_user_agents_match_non_ascii_header_bytes() {
    for ua in [
        "CoDeX-DeSkToP/1",
        "CoDeX DeSkToP",
        "CoDeX-Rs/1",
        "CoDeX_ClI_Rs/1",
        "CoDeX-TuI/1",
        "CoDeX_ExEc/1",
    ] {
        let mut raw = b"\xff client(\xc3\xa9 ".to_vec();
        raw.extend_from_slice(ua.as_bytes());
        raw.extend_from_slice(b") ");
        let mut opts = options();
        opts.headers
            .insert("user-agent", http::HeaderValue::from_bytes(&raw).unwrap());
        assert!(opts.is_codex_client_request());
        let auth = fixture();
        assert!(!auth.use_siwc_for_request(&opts.headers));
        assert_eq!(
            auth.access_token_for_request(&opts.headers),
            auth.outer_access_token()
        );
    }
    let mut opts = options();
    opts.headers.insert(
        "user-agent",
        http::HeaderValue::from_bytes(b"\xff ordinary").unwrap(),
    );
    assert!(!opts.is_codex_client_request());
    assert!(fixture().use_siwc_for_request(&opts.headers));
}
