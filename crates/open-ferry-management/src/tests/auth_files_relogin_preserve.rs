// Ported from CLIProxyAPI
// internal/api/handlers/management/auth_files_relogin_preserve_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Saving a login's credential over the file of a past login, as
//! `save_token_record` does.
//!
//! Deviations from upstream:
//! - Tokens are in the record's metadata, as the providers' token storage
//!   writes them, where upstream's records carry a `Storage`.
//! - What upstream's post-auth persist hook receives is checked as the
//!   credential the [`FakeSync`](super::FakeSync) registered from the saved
//!   file.
//! - `TestPatchAuthFileFields_DeletesPluginFields` is dropped: plugins
//!   aren't ported.

use std::path::Path;
use std::sync::Arc;

use http::StatusCode;
use open_ferry_core::auth::{Auth, AuthKind, AuthStore, Status};
use open_ferry_providers::claude::token::{
    TokenStorage as ClaudeTokenStorage, credential_file_name,
};
use open_ferry_providers::codex::token::TokenStorage as CodexTokenStorage;
use serde_json::{Map, Value, json};

use super::{Api, AuthDir, CommandRefresh, CommandSync, SyncCall};
use crate::token_record::{SaveError, save_token_record};

fn map(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

/// A record named `file_name` for `provider`, with `metadata`.
fn record(provider: &str, file_name: &str, metadata: Map<String, Value>) -> Auth {
    Auth {
        id: file_name.into(),
        provider: provider.into(),
        file_name: file_name.into(),
        metadata,
        ..Auth::default()
    }
}

// TestSaveTokenRecord_PostPersistHookReceivesCanonicalClaudeOAuth
#[tokio::test]
async fn save_token_record_post_persist_hook_receives_canonical_claude_oauth() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let file_name = "claude-user@example.com.json";
    let access_token = "sk-ant-oat01-hot-reload";
    let storage = ClaudeTokenStorage {
        access_token: access_token.into(),
        refresh_token: "refresh-token".into(),
        last_refresh: "2026-03-09T00:00:00Z".into(),
        email: "user@example.com".into(),
        expire: "2026-12-31T23:59:59Z".into(),
        metadata: map(json!({"email": "user@example.com"})),
        ..ClaudeTokenStorage::default()
    };
    let record = record("claude", file_name, storage.to_json());

    let path = save_token_record(&api.state, record).await.unwrap();
    let calls = api.sync.calls();
    let [SyncCall::FileWritten(file)] = calls.as_slice() else {
        panic!("expected one written file, got {calls:?}");
    };
    assert_eq!(file.path, Path::new(&path));

    let persisted = api.manager.get(file_name).expect("registered");
    assert_eq!(persisted.metadata["access_token"], access_token);
    assert_eq!(persisted.auth_kind(), Some(AuthKind::OAuth));
    assert_eq!(persisted.status, Status::Active);
}

// TestSaveTokenRecord_PreservesExistingAuthFileSettings
#[tokio::test]
async fn save_token_record_preserves_existing_auth_file_settings() {
    let auth_dir = AuthDir::new();
    let file_name = "codex-user@example.com.json";
    // User configured fields on existing OAuth account
    let initial = json!({
        "type": "codex",
        "email": "user@example.com",
        "access_token": "old-access",
        "refresh_token": "old-refresh",
        "prefix": "custom-prefix",
        "websockets": false,
        "note": "my important account",
        "proxy_url": "http://127.0.0.1:8080",
        "weight": 5.0,
        "headers": {"User-Agent": "Custom"},
        "models": ["o3-mini"],
        "thinking": {"enabled": true},
        "priority": 2.0,
    });
    let file_path = auth_dir.write(file_name, &initial.to_string());
    let api = Api::over(&auth_dir);

    // Re-login arrives with new OAuth tokens
    let storage = CodexTokenStorage {
        email: "user@example.com".into(),
        access_token: "new-access-token".into(),
        refresh_token: "new-refresh-token".into(),
        id_token: "new-id-token".into(),
        account_id: "act-123".into(),
        expire: "2026-12-31T23:59:59Z".into(),
        metadata: map(json!({"email": "user@example.com", "account_id": "act-123"})),
        ..CodexTokenStorage::default()
    };
    let record = record("codex", file_name, storage.to_json());

    let saved_path = save_token_record(&api.state, record).await.unwrap();
    assert_eq!(Path::new(&saved_path), file_path);

    let saved = auth_dir.read_json(file_name);
    // Verify new OAuth token data was updated
    assert_eq!(saved["access_token"], "new-access-token");
    assert_eq!(saved["refresh_token"], "new-refresh-token");
    // Verify user-configured fields were preserved
    assert_eq!(saved["prefix"], "custom-prefix");
    assert_eq!(saved["websockets"], false);
    assert_eq!(saved["note"], "my important account");
    assert_eq!(saved["proxy_url"], "http://127.0.0.1:8080");
    assert_eq!(saved["weight"].as_f64(), Some(5.0));
    assert_eq!(saved["headers"], json!({"User-Agent": "Custom"}));
    assert_eq!(saved["models"], json!(["o3-mini"]));
    assert_eq!(saved["thinking"], json!({"enabled": true}));
    assert_eq!(saved["priority"].as_f64(), Some(2.0));
}

// TestSaveTokenRecord_MigratesMatchingLegacyClaudeCredential
#[tokio::test]
async fn save_token_record_migrates_matching_legacy_claude_credential() {
    let auth_dir = AuthDir::new();
    let legacy_file_name = "claude-user@example.com.json";
    let target_file_name = credential_file_name("user@example.com", "organization-a", "account-a");
    let existing = json!({
        "type": "claude",
        "email": "user@example.com",
        "organization_uuid": "organization-a",
        "account_uuid": "account-a",
        "access_token": "old-token",
        "refresh_token": "old-refresh",
        "prefix": "team",
        "proxy_url": "http://127.0.0.1:8080",
        "disabled": true,
        "weight": 5.0,
    });
    let legacy_path = auth_dir.write(legacy_file_name, &existing.to_string());

    let storage = ClaudeTokenStorage {
        access_token: "new-token".into(),
        refresh_token: "new-refresh".into(),
        email: "user@example.com".into(),
        organization_uuid: "organization-a".into(),
        account_uuid: "account-a".into(),
        expire: "2026-12-31T23:59:59Z".into(),
        metadata: map(json!({
            "email": "user@example.com",
            "organization_uuid": "organization-a",
            "account_uuid": "account-a",
        })),
        ..ClaudeTokenStorage::default()
    };
    let record = record("claude", &target_file_name, storage.to_json());

    let api = Api::over(&auth_dir);
    let saved_path = save_token_record(&api.state, record).await.unwrap();
    assert_eq!(
        Path::new(&saved_path),
        auth_dir.path().join(&target_file_name)
    );
    assert!(!legacy_path.exists(), "legacy credential still exists");

    let saved = auth_dir.read_json(&target_file_name);
    for (key, want) in [
        ("access_token", json!("new-token")),
        ("refresh_token", json!("new-refresh")),
        ("prefix", json!("team")),
        ("proxy_url", json!("http://127.0.0.1:8080")),
        ("disabled", json!(true)),
    ] {
        assert_eq!(saved[key], want, "{key}");
    }
    assert_eq!(saved["weight"].as_f64(), Some(5.0));
}

// Not upstream's: mergeExistingAuthFileMetadata takes a registered
// credential's settings when there is no file to merge.
#[tokio::test]
async fn save_token_record_merges_a_registered_credential_without_a_file() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let file_name = "codex-user@example.com.json";
    let mut registered = record(
        "codex",
        file_name,
        map(json!({"type": "codex", "access_token": "old", "prefix": "team"})),
    );
    registered
        .attributes
        .insert("runtime_only".into(), "true".into());
    api.manager.register_unsaved(registered).unwrap();

    let fresh = record(
        "codex",
        file_name,
        map(json!({"type": "codex", "access_token": "new"})),
    );
    save_token_record(&api.state, fresh).await.unwrap();
    let saved = auth_dir.read_json(file_name);
    assert_eq!(saved["access_token"], "new");
    assert_eq!(saved["prefix"], "team");
}

// Not upstream's: without a store or a sync nothing is saved, and a stopped
// service fails the save after the file is written.
#[tokio::test]
async fn save_token_record_needs_the_store_and_the_service() {
    let file_name = "codex-user@example.com.json";
    let fresh = || {
        record(
            "codex",
            file_name,
            map(json!({"type": "codex", "access_token": "new"})),
        )
    };

    let api = Api::new();
    let error = save_token_record(&api.state, fresh()).await.unwrap_err();
    assert!(matches!(error, SaveError::Unavailable), "{error}");
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.to_string(), "credential store unavailable");

    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    api.sync.stop();
    let error = save_token_record(&api.state, fresh()).await.unwrap_err();
    let SaveError::Sync { path, .. } = &error else {
        panic!("expected a sync error, got {error}");
    };
    assert_eq!(Path::new(path), auth_dir.path().join(file_name));
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        error.to_string(),
        "post-auth persist hook failed: credential sync unavailable: the service has stopped"
    );
    assert_eq!(auth_dir.read_json(file_name)["access_token"], "new");
    assert!(api.manager.list().is_empty());
}

#[tokio::test]
async fn login_dispatches_completed_commands_only_after_success() {
    use std::sync::atomic::Ordering;
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let original = record(
        "codex",
        "command.json",
        map(
            json!({"type":"codex", "access_token":"A", "refresh_token":"A-refresh", "siwc":{"access_token":"S","refresh_token":"S-refresh"}}),
        ),
    );
    dir.store.save_new(&original).unwrap();
    api.manager.register_unsaved(original.clone()).unwrap();
    let sync = std::sync::Arc::new(CommandSync {
        manager: api.manager.clone(),
        commands: Default::default(),
        observations: Default::default(),
    });
    let state = api.state.clone().with_sync(sync.clone());
    let mut login = original.clone();
    login.id.clear(); // The store assigns the canonical id only after capture.
    login.metadata["access_token"] = json!("L");
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        save_token_record(&state, login),
    )
    .await
    .expect("management login command writes within 5s")
    .unwrap();
    assert_eq!(
        sync.commands.load(Ordering::SeqCst),
        1,
        "successful login dispatches command-aware sync"
    );
    assert_eq!(
        api.manager.get("command.json").unwrap().metadata["access_token"],
        "L"
    );
    // A runtime-only rotation is discarded without reaching disk, establishing
    // an outer marker which a failed login must not clear.
    api.manager
        .register_executor(Arc::new(CommandRefresh { siwc: false }));
    api.manager
        .edit_unsaved("command.json", |auth| {
            auth.attributes.insert("runtime_only".into(), "true".into());
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        api.manager.force_refresh("command.json"),
    )
    .await
    .expect("management fixture rotation finishes within 5s")
    .unwrap();
    api.manager
        .edit_unsaved("command.json", |auth| {
            auth.attributes
                .insert("runtime_only".into(), "false".into());
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    // A directory at the destination makes the next login write fail.
    let path = dir.store.resolve_auth_path(&original).unwrap();
    let disk_before = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let before = api.manager.get("command.json").unwrap().metadata.clone();
    let failed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        save_token_record(&state, original.clone()),
    )
    .await
    .expect("failed login answers within 5s");
    assert!(
        matches!(failed, Err(SaveError::Save(_))),
        "failed login writes nothing"
    );
    assert_eq!(
        sync.commands.load(Ordering::SeqCst),
        1,
        "failed login drops command without dispatch"
    );
    assert_eq!(sync.observations.load(Ordering::SeqCst), 0);
    assert_eq!(
        api.manager.get("command.json").unwrap().metadata,
        before,
        "failed login adopts no credentials"
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, disk_before).unwrap();
    api.manager
        .adopt_observed(&original, None, |auth| {
            dir.store
                .read_for_adoption(auth)
                .map_err(open_ferry_core::manager::ManagerError::Store)
        })
        .unwrap();
    assert_eq!(
        api.manager.get("command.json").unwrap().metadata["access_token"],
        "discarded-live",
        "failed login leaves outer marker intact"
    );
}

#[tokio::test]
async fn ambiguous_login_destination_dispatches_plain_sync() {
    use std::sync::atomic::Ordering;
    let dir = AuthDir::new();
    let api = Api::over(&dir);
    let original = record(
        "codex",
        "ambiguous.json",
        map(json!({"type":"codex", "access_token":"A", "refresh_token":"R"})),
    );
    dir.store.save_new(&original).unwrap();
    api.manager.register_unsaved(original.clone()).unwrap();
    let mut alias = original.clone();
    alias.id = "alias".into();
    api.manager.register_unsaved(alias).unwrap();
    let sync = std::sync::Arc::new(CommandSync {
        manager: api.manager.clone(),
        commands: Default::default(),
        observations: Default::default(),
    });
    let state = api.state.clone().with_sync(sync.clone());
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        save_token_record(&state, original),
    )
    .await
    .expect("ambiguous login writes within 5s")
    .unwrap();
    assert_eq!(
        sync.commands.load(Ordering::SeqCst),
        0,
        "ambiguous destination captures no command"
    );
    assert_eq!(
        sync.observations.load(Ordering::SeqCst),
        1,
        "ambiguous destination dispatches plain sync"
    );
}
