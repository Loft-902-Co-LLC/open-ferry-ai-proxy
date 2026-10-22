//! Local regressions against upstream's fresh Stat/SameFile comparison.

use super::*;

pub(super) async fn refreshed_alias_invalidation(obsolete: bool, delete: bool) -> bool {
    let dir = AuthDir::new();
    let mut outer: Value = serde_json::from_str(OUTER).unwrap();
    outer["account_id"] = json!("same-account-test");
    outer["email"] = json!("same-user@example.test");
    let path = dir.write("account.json", &outer.to_string());
    let upstream = Upstream::answering(tokens()).await;
    let api = login_api(&dir, &upstream.url);
    let session = start_login(&api, &START.replace("codex-bind.json", "account.json")).await;
    let alias = dir.path().join("alias.json");
    if obsolete {
        std::fs::hard_link(&path, &alias).unwrap();
    }
    let before = api.manager.list()[0].clone();
    let (_, old_id) = dir.store.read_bind_target(&path).unwrap();
    let mut rotated = open_ferry_core::auth::Auth::clone(&before);
    rotated
        .metadata
        .insert("access_token".into(), json!("rotated-outer-test"));
    rotated
        .metadata
        .insert("refresh_token".into(), json!("rotated-refresh-test"));
    let patch = open_ferry_core::auth::codex_refresh::CredentialRefreshPatch::new(
        &before.metadata,
        &rotated.metadata,
        true,
        false,
    );
    dir.store.apply_codex_refresh(rotated, patch).await.unwrap();
    let (_, new_id) = dir.store.read_bind_target(&path).unwrap();
    assert!(
        old_id != new_id,
        "refresh must atomically replace the target"
    );
    if !obsolete {
        std::fs::hard_link(&path, &alias).unwrap();
    }
    if delete {
        api.call(
            Method::DELETE,
            "/v8/management/credentials?name=alias.json",
            "",
        )
        .await
        .expect(StatusCode::OK);
    } else {
        api.post("/v8/management/credentials?name=alias.json", OUTER)
            .await
            .expect(StatusCode::OK);
    }
    let pending = api
        .state
        .oauth_sessions()
        .store()
        .is_pending(&session, "codex");
    assert_eq!(
        pending, obsolete,
        "invalidation compared the start-time identity instead of the bound path's current identity"
    );
    if obsolete {
        deliver(&api, &session, "codex", issued());
        completed(&api, &session).await;
        assert_eq!(
            dir.read_json("account.json")["access_token"],
            "rotated-outer-test"
        );
        assert_eq!(dir.read_json("account.json")["use_siwc"], true);
    }
    !pending
}

#[tokio::test]
async fn refreshed_file_hard_link_upload_invalidates_the_bind() {
    refreshed_alias_invalidation(false, false).await;
}
#[tokio::test]
async fn refreshed_file_hard_link_delete_invalidates_the_bind() {
    refreshed_alias_invalidation(false, true).await;
}
#[tokio::test]
async fn obsolete_file_hard_link_upload_preserves_the_bind() {
    refreshed_alias_invalidation(true, false).await;
}
#[tokio::test]
async fn obsolete_file_hard_link_delete_preserves_the_bind() {
    refreshed_alias_invalidation(true, true).await;
}
