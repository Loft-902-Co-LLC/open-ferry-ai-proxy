// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_persist_failure_logging_test.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A failed save doesn't fail a register, an update or a refresh: the
//! change still lands in memory.
//!
//! Deviations from upstream:
//! - The original call outcomes are retained. Round 5 also captures the
//!   common save warning, file-name masking and secret scrubbing.
//! - `TestMetaRefreshPersistFailureLogsWarn` is dropped: the Meta key mint
//!   save inside the lock isn't ported (lifecycle.rs).

use serde_json::{Value, json};

use super::support::*;
use crate::manager::Settings;

fn failing_store() -> Harness {
    let h = Harness::with_store(Settings::default());
    h.store.set_fail_saves(true);
    h
}

#[tokio::test(start_paused = true)]
async fn register_persist_failure_logs_warn() {
    let h = failing_store();
    let auth = auth_with_metadata(
        "auth-register-persist-1",
        "claude",
        json!({"access_token": "at", "refresh_token": "rt"}),
    );

    let registered = h
        .manager
        .register(auth)
        .expect("Register must stay non-fatal on persist failure");
    assert_eq!(registered.id, "auth-register-persist-1");
    assert!(h.manager.get("auth-register-persist-1").is_some());
    assert!(h.store.stored("auth-register-persist-1").is_none());
}

#[tokio::test(start_paused = true)]
async fn update_persist_failure_logs_warn() {
    let h = failing_store();
    let auth = auth_with_metadata(
        "auth-update-persist-1",
        "claude",
        json!({"access_token": "at", "refresh_token": "rt"}),
    );
    h.manager.register(auth.clone()).expect("register");

    let updated = h
        .manager
        .update(auth)
        .expect("Update must stay non-fatal on persist failure");
    assert!(
        updated.is_some(),
        "Update returned no auth on persist failure"
    );
    assert!(h.store.stored("auth-update-persist-1").is_none());
}

#[tokio::test(start_paused = true)]
async fn refresh_persist_failure_logs_warn() {
    let h = failing_store();
    let auth = auth_with_metadata(
        "auth-refresh-persist-1",
        "claude",
        json!({"access_token": "stale-access-token", "refresh_token": "rt"}),
    );
    h.manager.register(auth).expect("register");
    let executor = FakeExecutor::new("claude");
    executor.set_refresh(|auth| {
        let mut updated = auth.clone();
        updated
            .metadata
            .insert("access_token".into(), json!("refreshed-access-token"));
        Ok(updated)
    });
    h.executor(&executor);

    let refreshed = h
        .manager
        .force_refresh("auth-refresh-persist-1")
        .await
        .expect("ForceRefreshAuth must stay non-fatal on persist failure");
    assert_eq!(
        refreshed.metadata.get("access_token"),
        Some(&Value::from("refreshed-access-token"))
    );
    assert_eq!(
        h.get("auth-refresh-persist-1").metadata.get("access_token"),
        Some(&Value::from("refreshed-access-token"))
    );
    assert!(h.store.stored("auth-refresh-persist-1").is_none());
}

#[test]
fn round5_every_failed_save_logs_warn_with_cause_and_masked_file() {
    use crate::auth::{Auth, AuthStore};
    use crate::manager::{CallResult, Manager};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Store(AtomicBool);
    impl AuthStore for Store {
        fn list(&self) -> std::io::Result<Vec<Auth>> {
            Ok(Vec::new())
        }
        fn delete(&self, _: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn save(&self, auth: &Auth) -> std::io::Result<String> {
            if !self.0.load(Ordering::SeqCst) {
                return Ok(auth.file_name.clone());
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "permission denied saving {}: {} {} {} {}",
                    auth.file_name,
                    auth.metadata["access_token"],
                    auth.metadata["refresh_token"],
                    auth.metadata["siwc"]["access_token"],
                    auth.metadata["siwc"]["refresh_token"],
                ),
            ))
        }
    }
    for action in [
        "register",
        "status",
        "fields",
        "refresh",
        "result",
        "direct-save",
    ] {
        let store = Arc::new(Store(AtomicBool::new(false)));
        let manager = Manager::new(
            Settings::default(),
            Arc::new(FakeModels::default()),
            Some(store.clone()),
        );
        let mut auth = auth_with_metadata(
            "runtime-credential",
            "claude",
            json!({
                "access_token":"dummy-outer-access", "refresh_token":"dummy-outer-refresh",
                "siwc":{"access_token":"dummy-siwc-access", "refresh_token":"dummy-siwc-refresh"}
            }),
        );
        auth.file_name = "claude-1a2b3c4d-john.doe@example.com-plus.json".into();
        let before = manager.register(auth.clone()).unwrap();
        store.0.store(true, Ordering::SeqCst);
        let events = captured_log_events(|| match action {
            "register" => {
                manager.register(auth).unwrap();
            }
            "status" => {
                manager
                    .edit(&before.id, |auth| {
                        auth.disabled = true;
                        auth.status = crate::auth::Status::Disabled;
                        auth.metadata.insert("disabled".into(), json!(true));
                        Ok::<_, ()>(true)
                    })
                    .unwrap();
            }
            "fields" => {
                manager
                    .edit(&before.id, |auth| {
                        auth.metadata.insert("note".into(), json!("api-edit"));
                        Ok::<_, ()>(true)
                    })
                    .unwrap();
            }
            "refresh" => {
                let mut refreshed = before.as_ref().clone();
                refreshed.metadata["access_token"] = json!("dummy-refreshed-access");
                manager
                    .update_refreshed(&before, before.registration_epoch, refreshed)
                    .unwrap();
            }
            "result" => manager.mark_result(&CallResult {
                auth_id: before.id.clone(),
                provider: "claude".into(),
                success: true,
                registration_epoch: before.registration_epoch,
                credential_version: before.credential_version,
                ..Default::default()
            }),
            _ => {
                assert!(
                    manager
                        .persist(
                            &before,
                            before.registration_epoch,
                            before.generation,
                            0,
                            crate::manager::lifecycle::Save::Yes
                        )
                        .is_err()
                );
            }
        });
        let failures: Vec<_> = events
            .iter()
            .filter(|(_, text)| text.contains("failed to persist credential"))
            .collect();
        assert_eq!(failures.len(), 1, "one warning for {action}");
        let (level, line) = failures[0];
        assert_eq!(*level, tracing::Level::WARN);
        assert!(
            line.contains("permission denied"),
            "the cause is visible for {action}"
        );
        assert!(
            line.contains("file=claude-1a2b3c4d-j***@e***.com-plus.json"),
            "the masked file is identified for {action}"
        );
        assert!(line.contains("auth_id=runtime-credential"));
        assert!(!line.contains("john.doe@example.com"));
        for secret in [
            "dummy-outer-access",
            "dummy-outer-refresh",
            "dummy-siwc-access",
            "dummy-siwc-refresh",
            "dummy-refreshed-access",
        ] {
            assert!(!line.contains(secret), "secret scrubbed for {action}");
        }
    }
}

fn captured_log_events(action: impl FnOnce()) -> Vec<(tracing::Level, String)> {
    use crate::manager::lock;
    use std::sync::{Arc, Mutex};

    struct Fields(String);
    impl tracing::field::Visit for Fields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!("{}={value:?} ", field.name()));
        }
    }
    struct Capture(Arc<Mutex<Vec<(tracing::Level, String)>>>);
    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            lock(&self.0).push((*event.metadata().level(), fields.0));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    crate::test_tracing::keep_every_callsite_open();
    let events = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::with_default(Capture(events.clone()), action);
    lock(&events).clone()
}

#[tokio::test]
async fn round7_refused_detached_save_logs_masked_file_and_cause() {
    use crate::gate::ActiveGate;
    use crate::manager::CallResult;

    let h = Harness::with_store(Settings::default());
    let mut auth = auth_with_metadata(
        "codex-1a2b3c4d-john.doe@example.com-plus.json",
        "codex",
        json!({"access_token":"dummy-access", "refresh_token":"dummy-refresh"}),
    );
    auth.file_name = auth.id.clone();
    let initial = h.manager.register_unsaved(auth).unwrap();
    let (gate, _activation) = ActiveGate::standby();
    h.manager.set_gate(gate);
    let events = captured_log_events(|| {
        h.manager.mark_result(&CallResult {
            auth_id: initial.id.clone(),
            provider: "codex".into(),
            success: true,
            ..Default::default()
        });
    });
    let refused: Vec<_> = events
        .iter()
        .filter(|(_, line)| line.contains("credential state save refused"))
        .collect();
    assert_eq!(refused.len(), 1);
    let (level, line) = refused[0];
    assert_eq!(*level, tracing::Level::WARN);
    assert!(line.contains("file=codex-1a2b3c4d-j***@e***.com-plus.json"));
    assert!(line.contains("open-ferry is not the active version (closed)"));
    assert!(!line.contains("john.doe@example.com"));
    assert!(!line.contains("dummy-access"));
    assert!(!line.contains("dummy-refresh"));
    assert!(h.store.saved().is_empty());
    assert!(h.manager.lock().pending_saves.is_empty());
}

#[test]
fn round8_save_deferral_scrubs_tokens_before_masking_email() {
    use crate::auth::{
        Auth, AuthStore, CodexOrigin, GuardedSave, SaveDeferral, WithheldPartitions,
    };
    use crate::manager::Manager;
    use std::sync::Arc;

    struct DeferredStore;
    impl AuthStore for DeferredStore {
        fn list(&self) -> std::io::Result<Vec<Auth>> {
            Ok(Vec::new())
        }
        fn save(&self, auth: &Auth) -> std::io::Result<String> {
            Ok(auth.file_name.clone())
        }
        fn delete(&self, _: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn save_guarded(
            &self,
            _auth: &Auth,
            guard: &mut dyn FnMut(Option<&CodexOrigin>) -> Result<WithheldPartitions, SaveDeferral>,
        ) -> std::io::Result<GuardedSave> {
            let reason = guard(None).err().unwrap_or(SaveDeferral::RefreshInFlight);
            Ok(GuardedSave::Deferred(reason))
        }
    }
    for id in [
        "codex-1a2b3c4d-john.doe@example.com-plus.json",
        "codex-1a2b3c4d-john.doe@example.com-plus.json/dummy-access/dummy-refresh",
    ] {
        let manager = Manager::new(
            Settings::default(),
            Arc::new(FakeModels::default()),
            Some(Arc::new(DeferredStore)),
        );
        let mut auth = auth_with_metadata(
            id,
            "codex",
            json!({"access_token":"dummy-access", "refresh_token":"dummy-refresh"}),
        );
        auth.file_name = auth.id.clone();
        let initial = manager.register_unsaved(auth).unwrap();
        let events = captured_log_events(|| {
            manager
                .edit(&initial.id, |auth| {
                    auth.metadata.insert("note".into(), json!("operator"));
                    Ok::<_, ()>(true)
                })
                .unwrap()
                .unwrap();
        });
        let deferred: Vec<_> = events
            .iter()
            .filter(|(_, line)| line.contains("credential save deferred for "))
            .collect();
        assert!(!deferred.is_empty());
        for (level, line) in deferred {
            assert_eq!(*level, tracing::Level::WARN);
            assert!(
                !line.contains("dummy-access"),
                "Round 8: deferred-save ID must not expose the saving snapshot's access token"
            );
            assert!(!line.contains("dummy-refresh"));
            assert!(line.contains("j***@e***.com-plus.json"));
            if !id.contains("dummy-access") {
                assert!(line.contains("codex-1a2b3c4d-j***@e***.com-plus.json"));
            }
            assert!(line.contains("RefreshInFlight"));
            assert!(!line.contains("john.doe@example.com"));
        }
    }
}
