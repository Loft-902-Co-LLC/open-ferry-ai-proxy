// Round 2: a fresh read must not supersede a policy edit published during it.
use super::support::*;
use crate::auth::{AdoptionRead, AuthStore, FileStore, Status};
use crate::manager::{AdoptionResult, Manager, Settings, lock};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

#[test]
fn round2_observation_racing_disable_preserves_memory_and_disk() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStore::new(dir.path()));
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    let mut auth = auth_with_metadata(
        "policy.json",
        "claude",
        serde_json::json!({
            "type":"claude", "access_token":"dummy-policy", "disabled":false
        }),
    );
    auth.file_name = auth.id.clone();
    let initial = manager.register(auth).unwrap();
    let (read_tx, read_rx) = mpsc::channel();
    let (published_tx, published_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let once = std::sync::atomic::AtomicBool::new(true);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if auth.disabled && once.swap(false, std::sync::atomic::Ordering::SeqCst) {
            published_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
        }
    }));
    let editor = manager.clone();
    let epoch = initial.registration_epoch;
    let edit = std::thread::spawn(move || {
        read_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        editor
            .edit_at_epoch("policy.json", epoch, |auth| {
                auth.disabled = true;
                auth.status = Status::Disabled;
                auth.metadata
                    .insert("disabled".into(), serde_json::json!(true));
                Ok::<_, ()>(true)
            })
            .unwrap();
    });
    let result = manager
        .adopt_observed(&initial, None, |live| {
            let record = store.read_back(live).unwrap().unwrap();
            read_tx.send(()).unwrap();
            published_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            assert!(
                manager.get("policy.json").unwrap().disabled,
                "disable has published"
            );
            Ok(AdoptionRead::Record(record))
        })
        .unwrap();
    let mut reread = false;
    let retry = manager
        .adopt_observed(&initial, None, |live| {
            reread = true;
            Ok(AdoptionRead::Record(
                store.read_back(live).unwrap().unwrap(),
            ))
        })
        .unwrap();
    release_tx.send(()).unwrap();
    edit.join().unwrap();
    *lock(&manager.shared.persist_hook) = None;
    let memory_disabled = manager.get("policy.json").unwrap().disabled;
    let disk_disabled = store.read_back(&initial).unwrap().unwrap().disabled;
    assert_eq!(
        (memory_disabled, disk_disabled),
        (true, true),
        "the disable and its save must win"
    );
    assert!(matches!(result, AdoptionResult::Retry));
    assert!(matches!(retry, AdoptionResult::Retry));
    assert!(!reread, "an in-flight policy save defers the next read");
}

#[test]
fn round2_observation_policy_fence_covers_edits_and_round_trips() {
    for field in [
        "disabled",
        "label",
        "prefix",
        "proxy",
        "metadata",
        "attributes",
        "round-trip",
    ] {
        let h = Harness::new(Settings::default());
        let initial = h.add(auth("policy", "claude"), &["m"]);
        let result = h
            .manager
            .adopt_observed(&initial, None, |_| {
                h.manager
                    .edit_at_epoch("policy", initial.registration_epoch, |auth| {
                        match field {
                            "disabled" => {
                                auth.disabled = true;
                                auth.status = Status::Disabled;
                            }
                            "label" | "round-trip" => auth.label = "operator edit".into(),
                            "prefix" => auth.prefix = "operator-prefix".into(),
                            "proxy" => auth.proxy_url = "http://example.invalid".into(),
                            "metadata" => {
                                auth.metadata
                                    .insert("operator-setting".into(), serde_json::json!(true));
                            }
                            "attributes" => {
                                auth.attributes
                                    .insert("operator-setting".into(), "true".into());
                            }
                            _ => unreachable!(),
                        }
                        Ok::<_, ()>(true)
                    })
                    .unwrap();
                if field == "round-trip" {
                    h.manager
                        .edit_at_epoch("policy", initial.registration_epoch, |auth| {
                            auth.label = initial.label.clone();
                            Ok::<_, ()>(true)
                        })
                        .unwrap();
                }
                Ok(AdoptionRead::Record(initial.as_ref().clone()))
            })
            .unwrap();
        assert!(matches!(result, AdoptionResult::Retry), "{field}");
        assert!(
            h.get("policy").policy_generation > initial.policy_generation,
            "{field}"
        );
    }
}

#[test]
fn round2_observation_allows_pending_runtime_counter_and_cooldown_saves() {
    use crate::auth::AuthError;
    use crate::manager::CallResult;
    use std::sync::atomic::{AtomicBool, Ordering};

    for success in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileStore::new(dir.path()));
        let manager = Manager::new(
            Settings::default(),
            Arc::new(FakeModels::default()),
            Some(store.clone()),
        );
        let mut auth = auth_with_metadata(
            "runtime.json",
            "claude",
            serde_json::json!({
                "type":"claude", "access_token":"dummy-runtime"
            }),
        );
        auth.file_name = auth.id.clone();
        let initial = manager.register(auth).unwrap();
        let (published_tx, published_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let once = AtomicBool::new(false);
        *lock(&manager.shared.persist_hook) = Some(Arc::new(move |_| {
            if !once.swap(true, Ordering::SeqCst) {
                published_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap();
            }
        }));
        let recorder = manager.clone();
        let record = std::thread::spawn(move || {
            recorder.mark_result(&CallResult {
                auth_id: "runtime.json".into(),
                provider: "claude".into(),
                success,
                error: (!success).then(|| AuthError {
                    http_status: 503,
                    message: "dummy unavailable".into(),
                    ..Default::default()
                }),
                execution_seq: recorder.next_execution_seq(),
                ..Default::default()
            })
        });
        published_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let pending = manager.get("runtime.json").unwrap();
        assert_eq!(pending.policy_generation, initial.policy_generation);
        let result = manager
            .adopt_observed(&initial, None, |live| {
                Ok(AdoptionRead::Record(
                    store.read_back(live).unwrap().unwrap(),
                ))
            })
            .unwrap();
        release_tx.send(()).unwrap();
        record.join().unwrap();
        *lock(&manager.shared.persist_hook) = None;
        assert!(matches!(result, AdoptionResult::Published { .. }));
        let live = manager.get("runtime.json").unwrap();
        assert_eq!(
            (live.success, live.failed),
            (i64::from(success), i64::from(!success))
        );
        assert_eq!(live.next_retry_after, pending.next_retry_after);
        assert_eq!(live.status, pending.status);
    }
}
