// Ported from CLIProxyAPI sdk/cliproxy/auth/auto_refresh_loop_test.go,
// metadata_merge_test.go and conductor_refresh.go (v8.0.27, MIT).
use super::support::*;
use crate::auth::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
use crate::auth::{Auth, AuthKind, AuthStore, CodexRefreshFailure, FileStore, Status};
use crate::exec::{ExecError, Options, Request, Response, StreamResponse};
use crate::executor::{ProviderExecutor, RefreshOptions};
use crate::manager::credential::credentials_changed;
use crate::manager::merge::merge_auth_content;
use crate::manager::refresh::{next_refresh_check_at, should_refresh};
use crate::manager::{Manager, Settings};
use chrono::{TimeDelta, Utc};
use futures_core::future::BoxFuture;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

fn fixture() -> Auth {
    auth_with_metadata(
        "codex.json",
        "codex",
        json!({
            "type":"codex", "access_token":"outer-old-token", "refresh_token":"outer-old-refresh",
            "last_refresh":"2026-10-13T12:00:00Z", "expired":"2026-10-20T12:00:00Z",
            "refresh_interval_seconds":3600, "use_siwc":true,
            "siwc":{"access_token":"siwc-old-token", "refresh_token":"siwc-old-refresh", "client_id":"client",
                "id_token":"siwc-id-token", "device_id":"a0d26f60-26b5-4b30-bf65-4d7d8719c146",
                "expired":"2026-10-13T12:20:00Z", "last_refresh":"2026-10-13T12:00:00Z", "note":"keep"}
        }),
    )
}

/// Creates the file as a login does, then seeds the fixture's SIWC keys with a
/// raw write: a login no longer writes them.
fn create_with_siwc(store: &FileStore, dir: &tempfile::TempDir, auth: &Auth) {
    store.save_new(auth).unwrap();
    let path = dir.path().join(&auth.id);
    let mut on_disk: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for key in ["siwc", "use_siwc"] {
        on_disk.insert(key.into(), auth.metadata[key].clone());
    }
    std::fs::write(&path, serde_json::to_vec(&on_disk).unwrap()).unwrap();
}

fn at(text: &str) -> crate::auth::Timestamp {
    crate::auth::parse_go_rfc3339(text).unwrap()
}

#[test]
fn codex_dual_refresh_scheduling_and_backoff() {
    let now = at("2026-10-13T12:00:00Z");
    let mut auth = fixture();
    assert_eq!(
        auth.outer_refresh_due_at(now, None),
        Some(now + TimeDelta::hours(1))
    );
    assert_eq!(
        auth.siwc_refresh_due_at(now),
        Some(now + TimeDelta::minutes(15))
    );
    assert_eq!(
        next_refresh_check_at(now, &auth, None),
        Some(now + TimeDelta::minutes(15))
    );
    assert!(!should_refresh(&auth, None, now));
    assert!(should_refresh(&auth, None, now + TimeDelta::minutes(15)));
    auth.next_refresh_after = Some(now + TimeDelta::hours(2));
    assert_eq!(
        next_refresh_check_at(now, &auth, None),
        auth.next_refresh_after
    );
    assert!(!should_refresh(&auth, None, now));
    auth.next_refresh_after = None;
    auth.metadata.insert("use_siwc".into(), json!(false));
    auth.attributes.insert("auth_kind".into(), "apikey".into());
    auth.attributes
        .insert("api_key".into(), "legacy-key".into());
    assert_eq!(auth.auth_kind(), Some(AuthKind::ApiKey));
    assert_eq!(
        next_refresh_check_at(now, &auth, None),
        Some(now + TimeDelta::minutes(15))
    );
    auth.metadata.insert("use_siwc".into(), json!(true));
    assert_eq!(auth.auth_kind(), Some(AuthKind::OAuth));
}

#[test]
fn failed_partition_retries_after_backoff_even_when_not_due() {
    let now = at("2026-10-13T12:00:00Z");
    for partition in [
        crate::auth::CodexCredentialPartition::Outer,
        crate::auth::CodexCredentialPartition::Siwc,
    ] {
        let mut auth = fixture();
        auth.next_refresh_after = Some(now + TimeDelta::minutes(5));
        auth.codex_refresh_failure = Some(CodexRefreshFailure {
            partition,
            error: ExecError::upstream(500, "retry"),
        });
        assert!(!should_refresh(&auth, None, now));
        assert_eq!(
            next_refresh_check_at(now, &auth, None),
            auth.next_refresh_after
        );
        let retry_at = auth.next_refresh_after.unwrap();
        assert!(should_refresh(&auth, None, retry_at));
        assert_eq!(next_refresh_check_at(retry_at, &auth, None), Some(retry_at));
        let active_before = partition.is_active(&auth);
        auth.metadata.insert("use_siwc".into(), json!(false));
        assert_ne!(partition.is_active(&auth), active_before);
    }
}

#[test]
fn active_access_token_validity_and_outer_interval_are_independent() {
    let now = at("2026-10-13T12:10:00Z");
    let mut auth = fixture();
    assert_eq!(auth.access_token(), "siwc-old-token");
    assert_eq!(auth.expiration_time(), Some(at("2026-10-13T12:20:00Z")));
    assert!(auth.has_valid_access_token(now));
    auth.rejected_access_token = "siwc-old-token".into();
    assert!(!auth.has_valid_access_token(now));
    assert_eq!(
        auth.outer_expiration_time(),
        Some(at("2026-10-20T12:00:00Z"))
    );
    auth.last_refreshed_at = Some(now);
    assert_eq!(
        auth.outer_refresh_due_at(now, None),
        Some(at("2026-10-13T13:00:00Z"))
    );
    auth.metadata["siwc"]["access_token"] = json!("");
    assert_eq!(auth.access_token(), "");
    assert!(!auth.has_valid_access_token(now));
    auth.metadata.insert("use_siwc".into(), json!(false));
    assert_eq!(auth.access_token(), "outer-old-token");
    assert!(auth.has_valid_access_token(now));
    let mut non_codex = auth.clone();
    non_codex.provider = "claude".into();
    non_codex.metadata.insert("type".into(), json!("claude"));
    non_codex.metadata.insert("use_siwc".into(), json!(true));
    assert!(!non_codex.use_siwc());
    assert!(non_codex.siwc_refresh_due_at(now).is_none());
}

#[test]
fn credential_versions_change_for_every_partition_identity() {
    let h = Harness::new(Settings::default());
    let mut before = fixture();
    h.add(before.clone(), &[]);
    for key in ["access_token", "refresh_token", "id_token", "client_id"] {
        let live = h.get(&before.id);
        let mut incoming = (*live).clone();
        incoming.metadata["siwc"][key] = json!(format!("new-{key}"));
        assert!(credentials_changed(&live, &incoming));
        let next = h.manager.update(incoming).unwrap().unwrap();
        assert!(next.credential_version > live.credential_version);
        before = (*next).clone();
    }
    // Upstream's CredentialsChanged has no device_id check.
    let live = h.get(&before.id);
    let mut incoming = (*live).clone();
    incoming.metadata["siwc"]["device_id"] = json!("another-device");
    assert!(!credentials_changed(&live, &incoming));
    for key in [
        "access_token",
        "refresh_token",
        "id_token",
        "account_id",
        "use_siwc",
    ] {
        let live = h.get(&before.id);
        let mut incoming = (*live).clone();
        incoming.metadata.insert(
            key.into(),
            if key == "use_siwc" {
                json!(false)
            } else {
                json!(format!("new-{key}"))
            },
        );
        let next = h.manager.update(incoming).unwrap().unwrap();
        assert!(next.credential_version > live.credential_version);
    }
}

#[test]
fn merge_refreshed_metadata_preserves_siwc_and_concurrent_bind() {
    let before = fixture();
    let mut updated = before.clone();
    updated.metadata.remove("siwc");
    updated.metadata.remove("use_siwc");
    updated
        .metadata
        .insert("access_token".into(), json!("outer-new-token"));
    let mut current = before.clone();
    current.metadata["siwc"]["refresh_token"] = json!("siwc-bound-refresh");
    current.metadata["siwc"]["custom"] = json!({"keep":true});
    let merged = merge_auth_content(&before, &current, &updated);
    assert_eq!(merged.metadata["siwc"], current.metadata["siwc"]);
    assert_eq!(merged.metadata["use_siwc"], true);
    assert_eq!(merged.metadata["access_token"], "outer-new-token");
    let mut login = before.clone();
    login.metadata.remove("siwc");
    login.metadata.remove("use_siwc");
    crate::auth::metadata::merge_existing_auth_metadata(&mut login, &before.metadata);
    assert_eq!(login.metadata["siwc"], before.metadata["siwc"]);
    assert_eq!(login.metadata["use_siwc"], true);
    login
        .metadata
        .insert("siwc".into(), json!({"access_token":"login-siwc"}));
    crate::auth::metadata::merge_existing_auth_metadata(&mut login, &before.metadata);
    assert_eq!(login.metadata["siwc"]["access_token"], "login-siwc");
    assert_eq!(
        login.metadata["siwc"]["device_id"],
        before.metadata["siwc"]["device_id"]
    );
    let mut nested = before.clone();
    nested.metadata["siwc"]
        .as_object_mut()
        .unwrap()
        .remove("note");
    nested.metadata["siwc"]["access_token"] = json!("new-token");
    let merged = merge_auth_content(&before, &before, &nested);
    assert_eq!(merged.metadata["siwc"]["note"], "keep");
    assert_eq!(merged.metadata["siwc"]["access_token"], "new-token");
}

pub(super) struct ControlledExecutor {
    fake: Arc<FakeExecutor>,
    entered: Notify,
    release: Semaphore,
    force: AtomicBool,
    outer: bool,
    siwc: bool,
    failure_active: Option<bool>,
    status: AtomicU16,
    note: std::sync::Mutex<String>,
}

impl ControlledExecutor {
    fn new(outer: bool, siwc: bool, failure_active: Option<bool>, held: bool) -> Arc<Self> {
        Arc::new(Self {
            fake: FakeExecutor::new("codex"),
            entered: Notify::new(),
            release: Semaphore::new(usize::from(!held)),
            force: AtomicBool::new(false),
            outer,
            siwc,
            failure_active,
            status: AtomicU16::new(500),
            note: std::sync::Mutex::new("refresh failed".into()),
        })
    }

    /// The status and message prefix of the partition failure it reports.
    fn set_failure(&self, status: u16, note: &str) {
        self.status.store(status, Ordering::SeqCst);
        *self.note.lock().unwrap() = note.into();
    }
}

impl ProviderExecutor for ControlledExecutor {
    fn id(&self) -> &str {
        "codex"
    }
    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.fake.execute(auth, request, options)
    }
    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        self.fake.execute_stream(auth, request, options)
    }
    fn count_tokens(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        self.fake.count_tokens(auth, request, options)
    }
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        self.refresh_with_options(auth, RefreshOptions::default())
    }
    fn refresh_with_options(
        &self,
        auth: Arc<Auth>,
        options: RefreshOptions,
    ) -> BoxFuture<'_, Result<Auth, ExecError>> {
        Box::pin(async move {
            self.force.store(options.force, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
            let mut after = (*auth).clone();
            after.codex_refresh_failure = None;
            if self.outer {
                after
                    .metadata
                    .insert("access_token".into(), json!("outer-rotated-token"));
                after
                    .metadata
                    .insert("refresh_token".into(), json!("outer-rotated-refresh"));
                after
                    .metadata
                    .insert("last_refresh".into(), json!(Utc::now().to_rfc3339()));
            }
            if self.siwc {
                after.metadata["siwc"]["access_token"] = json!("siwc-rotated-token");
                after.metadata["siwc"]["refresh_token"] = json!("siwc-rotated-refresh");
            }
            after.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
                &auth.metadata,
                &after.metadata,
                self.outer,
                self.siwc,
            ));
            if let Some(active) = self.failure_active {
                let note = self.note.lock().unwrap().clone();
                after.codex_refresh_failure = Some(CodexRefreshFailure {
                    partition: if auth.use_siwc() == active {
                        crate::auth::CodexCredentialPartition::Siwc
                    } else {
                        crate::auth::CodexCredentialPartition::Outer
                    },
                    error: ExecError::upstream(
                        self.status.load(Ordering::SeqCst),
                        format!(
                            "{note} {} {} {} {}",
                            auth.siwc_token_field("access_token"),
                            auth.outer_access_token(),
                            after.siwc_token_field("access_token"),
                            after.outer_access_token()
                        ),
                    ),
                });
            }
            Ok(after)
        })
    }
}

fn disk_manager() -> (tempfile::TempDir, Arc<FileStore>, Manager) {
    let root = std::env::var_os("OFP_TEST_SCRATCH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
        });
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let store = Arc::new(FileStore::new(dir.path()));
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    create_with_siwc(&store, &dir, &fixture());
    manager.register(fixture()).unwrap();
    (dir, store, manager)
}

/// [`disk_manager`] with the manager over a store that gives no lease: the
/// unleased refresh path these tests were written for. The returned store is
/// the same files, for the peer's own writes.
fn disk_manager_unleased() -> (tempfile::TempDir, Arc<FileStore>, Manager) {
    let (dir, store, _leased) = disk_manager();
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(super::siwc_persistence::observe(store.clone(), true)),
    );
    manager.register(fixture()).unwrap();
    (dir, store, manager)
}

const NOON10: &str = "2026-10-13T12:10:00Z";
const SECRETS: [&str; 4] = [
    "siwc-old-token",
    "outer-old-token",
    "siwc-rotated-token",
    "outer-rotated-token",
];

#[tokio::test]
async fn force_reaches_executor_and_partial_success_persists_both_directions() {
    for (outer, siwc, use_siwc, active) in [
        (true, false, true, true),
        (false, true, false, true),
        (true, false, false, false),
        (false, true, true, false),
    ] {
        let (_dir, store, manager, executor) =
            partial_setup(NOON10, use_siwc, outer, siwc, active, 500, "refresh failed");
        let before = manager.now();
        let result = manager.force_refresh("codex.json").await;
        let after = manager.now();
        assert_eq!(result.is_err(), active);
        let live = manager.get("codex.json").unwrap();
        // Neither the old nor the rotated tokens reach the caller's error or
        // the credential's recorded error.
        let mut messages = vec![live.last_error.as_ref().unwrap().message.clone()];
        if let Err(error) = &result {
            messages.push(error.to_string());
        }
        for message in messages {
            assert!(message.contains("refresh failed"), "{message}");
            for secret in SECRETS {
                assert!(!message.contains(secret), "{secret} in {message}");
            }
        }
        assert!(executor.force.load(Ordering::SeqCst));
        let disk = store.list().unwrap().remove(0);
        assert_eq!(
            disk.metadata["refresh_token"],
            if outer {
                "outer-rotated-refresh"
            } else {
                "outer-old-refresh"
            }
        );
        assert_eq!(
            disk.metadata["siwc"]["refresh_token"],
            if siwc {
                "siwc-rotated-refresh"
            } else {
                "siwc-old-refresh"
            }
        );
        // The active token is still valid, so the credential stays in
        // rotation either way and only the retry is set.
        assert_retry_in(&live, before, after, TimeDelta::minutes(5));
        assert_ne!(live.status, Status::Error);
    }
}

fn error_os_code(error: &std::io::Error) -> Option<i32> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = source {
        if let Some(code) = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
        {
            return Some(code);
        }
        source = error.source();
    }
    None
}

// Observe completion without repeatedly opening the credential during rename.
struct RefreshSaveEvent {
    files: Arc<FileStore>,
    completed: Notify,
    last_error: std::sync::Mutex<Option<(std::io::ErrorKind, Option<i32>)>>,
}

impl AuthStore for RefreshSaveEvent {
    fn list(&self) -> std::io::Result<Vec<Auth>> {
        self.files.list()
    }
    fn delete(&self, id: &str) -> std::io::Result<()> {
        self.files.delete(id)
    }
    fn read_back(&self, auth: &Auth) -> std::io::Result<Option<Auth>> {
        self.files.read_back(auth)
    }
    fn supports_codex_refresh_patches(&self) -> bool {
        true
    }
    fn same_record(&self, a: &Auth, b: &Auth) -> std::io::Result<bool> {
        self.files.same_record(a, b)
    }
    fn binding_generation(&self) -> u64 {
        self.files.binding_generation()
    }
    fn save(&self, auth: &Auth) -> std::io::Result<String> {
        let result = self.files.save(auth);
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_)) {
            *self.last_error.lock().unwrap() = result
                .as_ref()
                .err()
                .map(|error| (error.kind(), error_os_code(error)));
            self.completed.notify_one();
        }
        result
    }
}

#[tokio::test]
async fn caller_cancelled_refresh_still_persists() {
    let (_dir, files, _initial_manager) = disk_manager();
    let store = Arc::new(RefreshSaveEvent {
        files,
        completed: Notify::new(),
        last_error: std::sync::Mutex::new(None),
    });
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    manager.register_unsaved(fixture()).unwrap();
    let executor = ControlledExecutor::new(true, true, None, true);
    manager.register_executor(executor.clone());
    let worker = manager.clone();
    let caller = tokio::spawn(async move { worker.force_refresh("codex.json").await });
    executor.entered.notified().await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    executor.release.add_permits(1);
    let completion =
        tokio::time::timeout(Duration::from_secs(60), store.completed.notified()).await;
    assert!(
        completion.is_ok(),
        "save completion timed out: live pending non-empty={}, last persist error (kind, OS code)={:?}",
        !manager
            .get("codex.json")
            .unwrap()
            .codex_pending_refresh
            .is_empty(),
        *store.last_error.lock().unwrap()
    );
    let last_error = *store.last_error.lock().unwrap();
    assert!(
        last_error.is_none(),
        "last persist error (kind, OS code)={last_error:?}"
    );
    assert_eq!(
        manager.get("codex.json").unwrap().metadata["refresh_token"],
        "outer-rotated-refresh"
    );
    let saved = store.list().unwrap().remove(0);
    assert_eq!(saved.metadata["refresh_token"], "outer-rotated-refresh");
    assert_eq!(
        saved.metadata["siwc"]["refresh_token"],
        "siwc-rotated-refresh"
    );
}

#[tokio::test]
async fn manager_outer_refresh_preserves_concurrent_siwc_bind() {
    // The peer writes through the files while the pass would hold the lease;
    // this test pins the unleased path.
    let (_dir, store, manager) = disk_manager_unleased();
    let executor = ControlledExecutor::new(true, false, None, true);
    manager.register_executor(executor.clone());
    let worker = manager.clone();
    let refresh = tokio::spawn(async move { worker.force_refresh("codex.json").await });
    executor.entered.notified().await;
    let mut siwc = fixture().metadata["siwc"].as_object().unwrap().clone();
    siwc.insert("refresh_token".into(), json!("siwc-bound-refresh"));
    siwc.insert("access_token".into(), json!("siwc-bound-token"));
    let bound = store.bind_siwc(fixture(), siwc).await.unwrap();
    manager.update_unsaved(bound).unwrap();
    executor.release.add_permits(1);
    let result = refresh.await.unwrap().unwrap();
    assert_eq!(result.metadata["refresh_token"], "outer-rotated-refresh");
    assert_eq!(
        result.metadata["siwc"]["refresh_token"],
        "siwc-bound-refresh"
    );
    assert_eq!(result.metadata, store.list().unwrap()[0].metadata);
}

#[tokio::test]
async fn partial_success_superseded_on_disk_does_not_expose_rotated_token() {
    // The peer writes through the files while the pass would hold the lease;
    // this test pins the unleased path.
    let (_dir, store, manager) = disk_manager_unleased();
    let executor = ControlledExecutor::new(true, false, Some(true), true);
    manager.register_executor(executor.clone());
    let worker = manager.clone();
    let refresh = tokio::spawn(async move { worker.force_refresh("codex.json").await });
    executor.entered.notified().await;
    let mut newer = fixture();
    newer
        .metadata
        .insert("access_token".into(), json!("newer-outer-token"));
    newer
        .metadata
        .insert("refresh_token".into(), json!("newer-outer-refresh"));
    store.save_new(&newer).unwrap();
    executor.release.add_permits(1);
    let error = refresh.await.unwrap().unwrap_err().to_string();
    assert!(!error.contains("outer-rotated-token"));
    assert!(!error.contains("siwc-old-token"));
    assert_eq!(
        store.list().unwrap()[0].metadata["refresh_token"],
        "newer-outer-refresh"
    );
}

#[tokio::test]
async fn manager_adopts_newer_disk_partition_and_keeps_epoch_protection() {
    // The peer writes through the files while the pass would hold the lease;
    // this test pins the unleased path.
    let (_dir, store, manager) = disk_manager_unleased();
    let executor = ControlledExecutor::new(true, true, None, true);
    manager.register_executor(executor.clone());
    let worker = manager.clone();
    let refresh = tokio::spawn(async move { worker.force_refresh("codex.json").await });
    executor.entered.notified().await;
    let mut newer = fixture();
    newer
        .metadata
        .insert("refresh_token".into(), json!("newer-outer-refresh"));
    newer
        .metadata
        .insert("access_token".into(), json!("newer-outer-token"));
    store.save_new(&newer).unwrap();
    executor.release.add_permits(1);
    let result = refresh.await.unwrap().unwrap();
    assert_eq!(result.metadata["refresh_token"], "newer-outer-refresh");
    assert_eq!(
        result.metadata["siwc"]["refresh_token"],
        "siwc-rotated-refresh"
    );
    assert_eq!(result.metadata, store.list().unwrap()[0].metadata);

    let executor = ControlledExecutor::new(true, true, None, true);
    manager.register_executor(executor.clone());
    let worker = manager.clone();
    let refresh = tokio::spawn(async move { worker.force_refresh("codex.json").await });
    executor.entered.notified().await;
    manager.remove("codex.json");
    manager.register(newer.clone()).unwrap();
    executor.release.add_permits(1);
    assert!(refresh.await.unwrap().is_err());
    assert_eq!(
        manager.get("codex.json").unwrap().metadata["refresh_token"],
        "newer-outer-refresh"
    );
}

#[test]
fn nested_siwc_tokens_and_device_id_are_redacted() {
    let auth = fixture();
    let mut secrets = crate::observe::redact::Secrets::new();
    secrets.add_auth(&auth);
    let output = secrets.text(
        format!(
            "{} {} {} {} {}",
            auth.siwc_token_field("access_token"),
            auth.siwc_token_field("refresh_token"),
            auth.siwc_token_field("id_token"),
            auth.siwc_token_field("device_id"),
            auth.outer_access_token()
        ),
        crate::observe::redact::Policy::Disk,
    );
    assert!(!output.contains("old-token"));
    assert!(!output.contains("old-refresh"));
    assert!(!output.contains("siwc-id-token"));
    assert!(!output.contains(auth.siwc_token_field("device_id")));
    let debug = format!("{auth:?}");
    assert!(!debug.contains(auth.siwc_token_field("device_id")));
    assert!(!debug.contains("old-token"));
}

#[tokio::test]
async fn concurrent_settings_edit_does_not_inherit_refresh_intent() {
    let (_dir, store, manager) = disk_manager();
    let executor = ControlledExecutor::new(true, false, None, false);
    manager.register_executor(executor);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let once = AtomicBool::new(true);
    *crate::manager::lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
            && once.swap(false, Ordering::SeqCst)
        {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
    }));
    let worker = manager.clone();
    let refresh = tokio::spawn(async move { worker.force_refresh("codex.json").await });
    tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
        .await
        .unwrap();
    manager
        .edit("codex.json", |auth| {
            auth.metadata.insert("note".into(), json!("edited"));
            Ok::<_, ()>(true)
        })
        .unwrap();
    // A watcher can add the canonical path while retaining the same file.
    let path = store
        .resolve_auth_path(&manager.get("codex.json").unwrap())
        .unwrap();
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.attributes
                .insert("path".into(), path.to_string_lossy().into_owned());
            Ok::<_, ()>(true)
        })
        .unwrap();
    release_tx.send(()).unwrap();
    refresh.await.unwrap().unwrap();
    assert_eq!(store.list().unwrap()[0].metadata["note"], "edited");
    assert_eq!(
        store.list().unwrap()[0].metadata["refresh_token"],
        "outer-rotated-refresh"
    );
    assert!(matches!(
        manager.get("codex.json").unwrap().codex_persist_intent,
        CodexPersistIntent::Configuration
    ));
    manager
        .edit("codex.json", |auth| {
            auth.metadata.remove("note");
            Ok::<_, ()>(true)
        })
        .unwrap();
    assert!(!store.list().unwrap()[0].metadata.contains_key("note"));
}

#[tokio::test]
async fn bind_during_refresh_save_reconciles_memory_and_obsoletes_old_failure() {
    let (_dir, store, manager) = disk_manager();
    let executor = ControlledExecutor::new(true, false, Some(true), false);
    manager.register_executor(executor);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let once = AtomicBool::new(true);
    *crate::manager::lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if matches!(auth.codex_persist_intent, CodexPersistIntent::Refresh(_))
            && once.swap(false, Ordering::SeqCst)
        {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
    }));
    let worker = manager.clone();
    let refresh = tokio::spawn(async move { worker.force_refresh("codex.json").await });
    tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
        .await
        .unwrap();
    let mut siwc = fixture().metadata["siwc"].as_object().unwrap().clone();
    siwc.insert("access_token".into(), json!("bound-token"));
    siwc.insert("refresh_token".into(), json!("bound-refresh"));
    let bound = store.bind_siwc(fixture(), siwc).await.unwrap();
    manager.update_unsaved(bound).unwrap();
    release_tx.send(()).unwrap();
    let returned = refresh.await.unwrap().unwrap();
    let live = manager.get("codex.json").unwrap();
    assert_eq!(live.metadata["refresh_token"], "outer-rotated-refresh");
    assert_eq!(live.metadata["siwc"]["refresh_token"], "bound-refresh");
    assert_eq!(returned.metadata, live.metadata);
    assert_eq!(live.metadata, store.list().unwrap()[0].metadata);
    assert!(live.codex_refresh_failure.is_none());
}

fn fixture_named(id: &str) -> Auth {
    let mut auth = fixture();
    auth.id = id.into();
    auth
}

/// A store on disk and a manager whose clock reads `now_text`, with one
/// registered Codex credential per id.
fn disk_manager_at(now_text: &str, ids: &[&str]) -> (tempfile::TempDir, Arc<FileStore>, Manager) {
    let root = std::env::var_os("OFP_TEST_SCRATCH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
        });
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let store = Arc::new(FileStore::new(dir.path()));
    let clock = TestClock::new();
    clock.advance((at(now_text) - clock.now()).to_std().unwrap());
    let manager = Manager::with_clock(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
        clock.clock(),
    );
    for id in ids {
        create_with_siwc(&store, &dir, &fixture_named(id));
        manager.register(fixture_named(id)).unwrap();
    }
    (dir, store, manager)
}

/// A clocked disk manager whose executor rotates the `outer` and `siwc`
/// partitions and reports a failure of the partition that is active (or not)
/// when `use_siwc` is as given.
fn partial_setup(
    now_text: &str,
    use_siwc: bool,
    outer: bool,
    siwc: bool,
    active: bool,
    status: u16,
    note: &str,
) -> (
    tempfile::TempDir,
    Arc<FileStore>,
    Manager,
    Arc<ControlledExecutor>,
) {
    let (dir, store, manager) = disk_manager_at(now_text, &["codex.json"]);
    manager
        .edit("codex.json", |auth| {
            auth.metadata.insert("use_siwc".into(), json!(use_siwc));
            Ok::<_, ()>(true)
        })
        .unwrap();
    let executor = ControlledExecutor::new(outer, siwc, Some(active), false);
    executor.set_failure(status, note);
    manager.register_executor(executor.clone());
    (dir, store, manager, executor)
}

/// Runs `hook` after a refresh was saved and before its partial failure is
/// recorded, until the guard is dropped.
fn race_hook<F, Fut>(
    manager: &Manager,
    hook: F,
) -> crate::manager::refresh::partial_failure_seam::Guard
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    crate::manager::refresh::partial_failure_seam::install(
        &manager.shared,
        Arc::new(move || -> BoxFuture<'static, ()> { Box::pin(hook()) }),
    )
}

fn set_use_siwc(manager: &Manager, value: bool) {
    manager
        .edit_unsaved("codex.json", |auth| {
            auth.metadata.insert("use_siwc".into(), json!(value));
            Ok::<_, ()>(true)
        })
        .unwrap();
}

fn refresh_failures(manager: &Manager, id: &str) -> u32 {
    manager.lock().auths[id].refresh_failures
}

fn assert_retry_in(
    auth: &Auth,
    before: crate::auth::Timestamp,
    after: crate::auth::Timestamp,
    delay: TimeDelta,
) {
    let next = auth.next_refresh_after.expect("a retry is scheduled");
    assert!(
        next >= before + delay && next <= after + delay,
        "{next} is not {delay} after {before}"
    );
}

fn failed_partition(auth: &Auth) -> Option<crate::auth::CodexCredentialPartition> {
    auth.codex_refresh_failure
        .as_ref()
        .map(|failure| failure.partition)
}

#[tokio::test]
async fn partial_failure_does_not_reject_a_token_bound_after_the_save() {
    let (_dir, store, manager, _executor) =
        partial_setup(NOON10, true, true, false, true, 401, "unauthorized");
    let _hook = race_hook(&manager, {
        let (store, manager) = (store.clone(), manager.clone());
        move || {
            let (store, manager) = (store.clone(), manager.clone());
            async move {
                let mut siwc = fixture().metadata["siwc"].as_object().unwrap().clone();
                siwc.insert("access_token".into(), json!("siwc-bound-token"));
                siwc.insert("refresh_token".into(), json!("siwc-bound-refresh"));
                let bound = store.bind_siwc(fixture(), siwc).await.unwrap();
                manager.update_unsaved(bound).unwrap();
            }
        }
    });
    let returned = manager.force_refresh("codex.json").await.unwrap();
    let live = manager.get("codex.json").unwrap();
    assert!(Arc::ptr_eq(&returned, &live));
    assert_eq!(live.metadata["refresh_token"], "outer-rotated-refresh");
    assert_eq!(live.metadata["siwc"]["access_token"], "siwc-bound-token");
    // The old token's 401 isn't charged to the one bound since.
    assert!(live.rejected_access_token.is_empty());
    assert!(live.codex_refresh_failure.is_none());
    assert!(live.last_error.is_none());
    assert!(!live.unavailable);
    assert_ne!(live.status, Status::Error);
    assert_eq!(refresh_failures(&manager, "codex.json"), 0);
}

#[tokio::test]
async fn partial_failure_follows_the_live_switch_in_both_directions() {
    // The failed partition stopped being the active one.
    let (_dir, _store, manager, _executor) =
        partial_setup(NOON10, true, true, false, true, 500, "refresh failed");
    let hook = race_hook(&manager, {
        let manager = manager.clone();
        move || {
            let manager = manager.clone();
            async move { set_use_siwc(&manager, false) }
        }
    });
    let (before, result, after) = (
        manager.now(),
        manager.force_refresh("codex.json").await,
        manager.now(),
    );
    let live = result.expect("the inactive partition's failure isn't an error");
    assert!(Arc::ptr_eq(&live, &manager.get("codex.json").unwrap()));
    assert_eq!(
        failed_partition(&live),
        Some(crate::auth::CodexCredentialPartition::Siwc)
    );
    assert!(live.last_error.is_some());
    assert_ne!(live.status, Status::Error);
    assert_retry_in(&live, before, after, TimeDelta::minutes(5));
    drop(hook);

    // The failed partition became the active one.
    let (_dir, _store, manager, _executor) =
        partial_setup(NOON10, false, true, false, false, 500, "refresh failed");
    let _hook = race_hook(&manager, {
        let manager = manager.clone();
        move || {
            let manager = manager.clone();
            async move { set_use_siwc(&manager, true) }
        }
    });
    let (before, result, after) = (
        manager.now(),
        manager.force_refresh("codex.json").await,
        manager.now(),
    );
    assert!(result.is_err());
    let live = manager.get("codex.json").unwrap();
    assert_eq!(
        failed_partition(&live),
        Some(crate::auth::CodexCredentialPartition::Siwc)
    );
    assert_retry_in(&live, before, after, TimeDelta::minutes(5));
    assert_ne!(live.status, Status::Error);
}

#[tokio::test]
async fn partial_failure_of_a_replaced_registration_is_discarded() {
    let (_dir, _store, manager, _executor) =
        partial_setup(NOON10, true, true, false, true, 500, "refresh failed");
    let _hook = race_hook(&manager, {
        let manager = manager.clone();
        move || {
            let manager = manager.clone();
            async move {
                manager.remove("codex.json");
                manager.register(fixture()).unwrap();
            }
        }
    });
    let returned = manager.force_refresh("codex.json").await.unwrap();
    let live = manager.get("codex.json").unwrap();
    assert!(Arc::ptr_eq(&returned, &live));
    assert_eq!(live.metadata["refresh_token"], "outer-old-refresh");
    assert!(live.codex_refresh_failure.is_none());
    assert!(live.last_error.is_none());
    assert!(live.next_refresh_after.is_none());
    assert_eq!(refresh_failures(&manager, "codex.json"), 0);
}

#[tokio::test]
async fn active_partial_failure_backs_off_like_a_full_refresh_failure() {
    // A valid access token keeps the credential in rotation.
    let (_dir, _store, manager, _executor) =
        partial_setup(NOON10, true, true, false, true, 500, "refresh failed");
    let before = manager.now();
    assert!(manager.force_refresh("codex.json").await.is_err());
    let after = manager.now();
    let live = manager.get("codex.json").unwrap();
    assert_eq!(live.status, Status::Active);
    assert!(!live.unavailable);
    assert_eq!(
        failed_partition(&live),
        Some(crate::auth::CodexCredentialPartition::Siwc)
    );
    assert_retry_in(&live, before, after, TimeDelta::minutes(5));
    assert_eq!(refresh_failures(&manager, "codex.json"), 0);

    // An expired one doesn't.
    let (_dir, _store, manager, _executor) = partial_setup(
        "2026-10-13T12:30:00Z",
        true,
        true,
        false,
        true,
        500,
        "refresh failed",
    );
    let before = manager.now();
    assert!(manager.force_refresh("codex.json").await.is_err());
    let after = manager.now();
    let live = manager.get("codex.json").unwrap();
    assert_eq!(live.status, Status::Error);
    assert!(live.unavailable);
    assert_eq!(live.status_message, "token expired");
    assert_retry_in(&live, before, after, TimeDelta::minutes(5));
}

#[tokio::test]
async fn active_partial_invalid_grant_failures_back_off_longer_each_time() {
    let (_dir, _store, manager, executor) =
        partial_setup(NOON10, true, true, false, true, 400, "invalid_grant");
    for (failures, delay) in [(1, TimeDelta::seconds(60)), (2, TimeDelta::seconds(120))] {
        if failures > 1 {
            // The executor's one permit went to the first refresh.
            executor.release.add_permits(1);
        }
        let before = manager.now();
        assert!(manager.force_refresh("codex.json").await.is_err());
        let after = manager.now();
        let live = manager.get("codex.json").unwrap();
        assert_eq!(refresh_failures(&manager, "codex.json"), failures);
        assert_retry_in(&live, before, after, delay);
        assert_eq!(live.status, Status::Active);
    }
}

#[tokio::test]
async fn active_partial_invalid_grant_after_a_rejected_token_is_terminal() {
    let (_dir, _store, manager, _executor) =
        partial_setup(NOON10, true, true, false, true, 400, "invalid_grant");
    let result = manager
        .refresh_at_epoch("codex.json", "siwc-old-token", 0)
        .await;
    assert!(result.is_err());
    let live = manager.get("codex.json").unwrap();
    assert!(live.unavailable);
    assert_eq!(live.status, Status::Error);
    assert!(live.next_refresh_after.is_none());
    assert!(crate::manager::classify::has_unauthorized_auth_failure(
        &live
    ));
    assert_eq!(refresh_failures(&manager, "codex.json"), 0);
}

#[tokio::test]
async fn partial_failure_keeps_a_credential_that_became_terminal() {
    let (_dir, _store, manager, _executor) =
        partial_setup(NOON10, true, true, false, true, 500, "refresh failed");
    let _hook = race_hook(&manager, {
        let manager = manager.clone();
        move || {
            let manager = manager.clone();
            async move {
                manager
                    .edit_unsaved("codex.json", |auth| {
                        auth.unavailable = true;
                        auth.status = Status::Error;
                        auth.next_refresh_after = None;
                        auth.next_retry_after = None;
                        auth.last_error = Some(crate::auth::AuthError {
                            code: "unauthorized".into(),
                            message: "terminal".into(),
                            http_status: 401,
                            ..crate::auth::AuthError::default()
                        });
                        Ok::<_, ()>(true)
                    })
                    .unwrap();
            }
        }
    });
    // Not forced: a credential that is out of rotation for a 401 stays as it
    // is.
    assert!(manager.refresh_at_epoch("codex.json", "", 0).await.is_err());
    let live = manager.get("codex.json").unwrap();
    assert!(crate::manager::classify::has_unauthorized_auth_failure(
        &live
    ));
    assert_eq!(live.last_error.as_ref().unwrap().message, "terminal");
    assert!(live.codex_refresh_failure.is_none());
    assert!(live.next_refresh_after.is_none());
    assert_eq!(refresh_failures(&manager, "codex.json"), 0);
}

/// Holds a credential's lock the way another process of the store would, on
/// a thread of its own, until released or dropped.
struct HeldLock {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HeldLock {
    fn hold(path: std::path::PathBuf) -> Self {
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let guard = runtime
                .block_on(crate::auth::credential_lock::acquire_credential_lock(&path))
                .unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv();
            drop(guard);
        });
        held_rx.recv().unwrap();
        Self {
            release: Some(release_tx),
            thread: Some(thread),
        }
    }

    fn release(&mut self) {
        drop(self.release.take());
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Notifies once a save starts waiting for the credential's lock.
fn lock_wait_signal(path: &std::path::Path) -> Arc<Notify> {
    let waiting = Arc::new(Notify::new());
    let signal = waiting.clone();
    crate::auth::credential_lock::set_test_hook(
        path,
        Some(Arc::new(move |event: &str| {
            if event == "waiting" {
                signal.notify_one();
            }
        })),
    );
    waiting
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_save_waiting_for_a_credential_lock_does_not_stop_the_runtime() {
    let (_dir, store, manager) = disk_manager_at(NOON10, &["codex.json"]);
    manager.register_executor(ControlledExecutor::new(true, true, None, false));
    let path = store
        .resolve_auth_path(&manager.get("codex.json").unwrap())
        .unwrap();
    let waiting = lock_wait_signal(&path);
    let mut held = HeldLock::hold(path.clone());
    let worker = manager.clone();
    let refresh = tokio::spawn(async move { worker.force_refresh("codex.json").await });
    tokio::time::timeout(Duration::from_secs(10), waiting.notified())
        .await
        .expect("the save never waited for the lock");
    // This runtime has one thread: it only gets here, and only finishes the
    // sleeps, if the save's wait is off it.
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the runtime made no progress while the save waited");
    assert!(!refresh.is_finished());
    held.release();
    let saved = tokio::time::timeout(Duration::from_secs(10), refresh)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(saved.metadata["refresh_token"], "outer-rotated-refresh");
    let disk = store.list().unwrap().remove(0);
    assert_eq!(disk.metadata["refresh_token"], "outer-rotated-refresh");
    assert_eq!(
        disk.metadata["siwc"]["refresh_token"],
        "siwc-rotated-refresh"
    );
    crate::auth::credential_lock::set_test_hook(&path, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_save_waiting_for_one_credential_does_not_delay_another() {
    let (_dir, store, manager) = disk_manager_at(NOON10, &["codex-a.json", "codex-b.json"]);
    let executor = ControlledExecutor::new(true, false, None, true);
    executor.release.add_permits(2);
    manager.register_executor(executor);
    let path_a = store
        .resolve_auth_path(&manager.get("codex-a.json").unwrap())
        .unwrap();
    let waiting = lock_wait_signal(&path_a);
    let mut held = HeldLock::hold(path_a.clone());
    let worker = manager.clone();
    let refresh_a = tokio::spawn(async move { worker.force_refresh("codex-a.json").await });
    tokio::time::timeout(Duration::from_secs(10), waiting.notified())
        .await
        .expect("the save of A never waited for its lock");
    let saved_b = tokio::time::timeout(
        Duration::from_secs(5),
        manager.force_refresh("codex-b.json"),
    )
    .await
    .expect("the save of B waited behind A's lock")
    .unwrap();
    assert_eq!(saved_b.metadata["refresh_token"], "outer-rotated-refresh");
    assert!(!refresh_a.is_finished());
    held.release();
    let saved_a = tokio::time::timeout(Duration::from_secs(10), refresh_a)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(saved_a.metadata["refresh_token"], "outer-rotated-refresh");
    crate::auth::credential_lock::set_test_hook(&path_a, None);
}

/// Polls `future` on this thread, parking between polls: no runtime.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::park();
    }
}

#[test]
fn refresh_with_a_store_and_no_runtime_does_not_panic() {
    assert!(tokio::runtime::Handle::try_current().is_err());
    let (_dir, store, manager) = disk_manager_at(NOON10, &["codex.json"]);
    manager.register_executor(ControlledExecutor::new(true, true, None, false));
    let saved = block_on(manager.force_refresh("codex.json")).unwrap();
    assert_eq!(saved.metadata["refresh_token"], "outer-rotated-refresh");
    let disk = store.list().unwrap().remove(0);
    assert_eq!(disk.metadata["refresh_token"], "outer-rotated-refresh");
    assert_eq!(
        disk.metadata["siwc"]["refresh_token"],
        "siwc-rotated-refresh"
    );
}
