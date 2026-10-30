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
fn round6_result_save_uses_captured_tokens_after_an_outside_write() {
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
        std::fs::write(
            dir.path().join("runtime.json"),
            serde_json::to_vec(&serde_json::json!({
                "type":"claude", "access_token":"dummy-outside-r2"
            }))
            .unwrap(),
        )
        .unwrap();
        let result = manager
            .adopt_observed(&initial, None, |live| {
                Ok(AdoptionRead::Record(
                    store.read_back(live).unwrap().unwrap(),
                ))
            })
            .unwrap();
        let observed = manager.get("runtime.json").unwrap();
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
        assert_eq!(observed.metadata["access_token"], "dummy-outside-r2");
        assert_eq!(live.metadata["access_token"], "dummy-outside-r2");
        // Upstream serializes file publication, but its pending result still
        // writes the captured record after an outside file change.
        assert_eq!(
            store.read_back(&initial).unwrap().unwrap().metadata["access_token"],
            "dummy-runtime",
            "upstream saves its captured result snapshot, not the current manager object"
        );
    }
}

#[test]
fn round6_failed_policy_save_does_not_block_observation_during_counter_saves() {
    use crate::auth::file_guard::FileGuard;
    use crate::manager::CallResult;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let failing = fail.clone();
    let store = Arc::new(
        FileStore::new(dir.path()).with_file_guard(Some(FileGuard::new(move |_| {
            if failing.load(Ordering::SeqCst) {
                Err("test save refusal".into())
            } else {
                Ok(())
            }
        }))),
    );
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    let mut auth = auth_with_metadata(
        "failed-policy.json",
        "claude",
        serde_json::json!({
            "type":"claude", "access_token":"dummy-r1", "note":"before"
        }),
    );
    auth.file_name = auth.id.clone();
    let initial = manager.register(auth).unwrap();
    fail.store(true, Ordering::SeqCst);
    let edited = manager
        .edit(&initial.id, |auth| {
            auth.metadata["note"] = serde_json::json!("failed edit");
            Ok::<_, ()>(true)
        })
        .unwrap()
        .unwrap();
    assert!(edited.policy_generation > initial.policy_generation);
    assert_eq!(edited.metadata["note"], "failed edit");
    fail.store(false, Ordering::SeqCst);
    assert_eq!(
        store.read_back(&initial).unwrap().unwrap().metadata["note"],
        "before"
    );
    std::fs::write(
        dir.path().join(&initial.id),
        serde_json::to_vec(&serde_json::json!({
            "type":"claude", "access_token":"dummy-r2", "note":"outside"
        }))
        .unwrap(),
    )
    .unwrap();

    let (published_tx, published_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if auth.metadata["note"] == "failed edit" {
            published_tx.send(()).unwrap();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
        }
    }));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let recorder = manager.clone();
        workers.push(std::thread::spawn(move || {
            recorder.mark_result(&CallResult {
                auth_id: "failed-policy.json".into(),
                provider: "claude".into(),
                success: true,
                ..Default::default()
            });
        }));
        published_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    }
    let mut read = false;
    let result = manager
        .adopt_observed(&initial, None, |live| {
            read = true;
            Ok(AdoptionRead::Record(
                store.read_back(live).unwrap().unwrap(),
            ))
        })
        .unwrap();
    let observed = manager.get(&initial.id).unwrap();
    // Fail both paused writes after allowing the genuine file read.
    fail.store(true, Ordering::SeqCst);
    // Release both result saves even when the observation was incorrectly refused.
    release_tx.send(()).unwrap();
    release_tx.send(()).unwrap();
    for worker in workers {
        worker.join().unwrap();
    }
    *lock(&manager.shared.persist_hook) = None;
    fail.store(false, Ordering::SeqCst);
    assert!(read, "only outstanding policy saves may postpone the read");
    assert!(matches!(result, AdoptionResult::Published { .. }));
    assert_eq!(observed.metadata["note"], "outside");
    assert_eq!(observed.metadata["access_token"], "dummy-r2");
    assert_eq!(observed.success, 2);
    assert_eq!(
        manager.get(&initial.id).unwrap().metadata["note"],
        "outside"
    );
    assert_eq!(
        store.read_back(&initial).unwrap().unwrap().metadata["note"],
        "outside"
    );
}

#[test]
fn round6_repeating_a_failed_policy_edit_waits_for_its_actual_save() {
    use crate::auth::file_guard::FileGuard;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let failing = fail.clone();
    let store = Arc::new(
        FileStore::new(dir.path()).with_file_guard(Some(FileGuard::new(move |_| {
            if failing.load(Ordering::SeqCst) {
                Err("test save refusal".into())
            } else {
                Ok(())
            }
        }))),
    );
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    let mut auth = auth_with_metadata(
        "repeat-policy.json",
        "claude",
        serde_json::json!({"type":"claude", "access_token":"dummy", "note":"before"}),
    );
    auth.file_name = auth.id.clone();
    let initial = manager.register(auth).unwrap();
    fail.store(true, Ordering::SeqCst);
    let edit_note = |auth: &mut crate::auth::Auth| {
        auth.metadata["note"] = serde_json::json!("operator");
        Ok::<_, ()>(true)
    };
    let edited = manager.edit(&initial.id, edit_note).unwrap().unwrap();
    fail.store(false, Ordering::SeqCst);
    let (published_tx, published_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let once = AtomicBool::new(true);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |_| {
        if once.swap(false, Ordering::SeqCst) {
            published_tx.send(()).unwrap();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
        }
    }));
    let editor = manager.clone();
    let worker = std::thread::spawn(move || {
        editor
            .edit("repeat-policy.json", edit_note)
            .unwrap()
            .unwrap()
    });
    published_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let pending = manager.get(&initial.id).unwrap();
    assert_eq!(pending.policy_generation, edited.policy_generation);
    let mut read = false;
    let result = manager
        .adopt_observed(&initial, None, |live| {
            read = true;
            Ok(AdoptionRead::Record(
                store.read_back(live).unwrap().unwrap(),
            ))
        })
        .unwrap();
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    *lock(&manager.shared.persist_hook) = None;
    assert!(
        !read,
        "the explicit repeated edit still has a save in flight"
    );
    assert!(matches!(result, AdoptionResult::Retry));
    assert_eq!(
        manager.get(&initial.id).unwrap().metadata["note"],
        "operator"
    );
    assert_eq!(
        store.read_back(&initial).unwrap().unwrap().metadata["note"],
        "operator"
    );
}

#[test]
fn round7_observation_waits_for_a_refresh_that_changes_tokens() {
    use crate::manager::CallResult;
    use std::sync::atomic::{AtomicBool, Ordering};

    for changes_tokens in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileStore::new(dir.path()));
        let manager = Manager::new(
            Settings::default(),
            Arc::new(FakeModels::default()),
            Some(store.clone()),
        );
        let mut auth = auth_with_metadata(
            "refresh.json",
            "claude",
            serde_json::json!({
                "type":"claude", "access_token":"dummy-access-r1",
                "refresh_token":"dummy-refresh-r1"
            }),
        );
        auth.file_name = auth.id.clone();
        let initial = manager.register(auth).unwrap();
        let (published_tx, published_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let once = AtomicBool::new(true);
        *lock(&manager.shared.persist_hook) = Some(Arc::new(move |_| {
            if once.swap(false, Ordering::SeqCst) {
                published_tx.send(()).unwrap();
                lock(&release_rx)
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap();
            }
        }));
        let refresher = manager.clone();
        let base = initial.clone();
        let refresh = std::thread::spawn(move || {
            let mut updated = base.as_ref().clone();
            if changes_tokens {
                updated.metadata["access_token"] = serde_json::json!("dummy-access-r2");
                updated.metadata["refresh_token"] = serde_json::json!("dummy-refresh-r2");
            }
            updated.last_refreshed_at = Some(refresher.now());
            refresher
                .update_refreshed(&base, base.registration_epoch, updated)
                .unwrap()
                .unwrap()
        });
        published_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let mut read = false;
        let observed = manager
            .adopt_observed(&initial, None, |live| {
                read = true;
                Ok(AdoptionRead::Record(
                    store.read_back(live).unwrap().unwrap(),
                ))
            })
            .unwrap();
        release_tx.send(()).unwrap();
        refresh.join().unwrap();
        *lock(&manager.shared.persist_hook) = None;
        let live_after_save = manager.get(&initial.id).unwrap();
        let disk_after_save = store.read_back(&initial).unwrap().unwrap();
        manager.mark_result(&CallResult {
            auth_id: initial.id.clone(),
            provider: "claude".into(),
            success: true,
            ..Default::default()
        });
        let disk_after_result = store.read_back(&initial).unwrap().unwrap();
        if changes_tokens {
            assert!(
                !read,
                "F2a: an outstanding token-changing refresh must postpone the read"
            );
            assert!(matches!(observed, AdoptionResult::Retry));
        } else {
            assert!(
                read,
                "a refresh without token changes need not postpone the read"
            );
            assert!(matches!(observed, AdoptionResult::Published { .. }));
        }
        let expected = if changes_tokens {
            "dummy-access-r2"
        } else {
            "dummy-access-r1"
        };
        for record in [
            live_after_save.as_ref(),
            &disk_after_save,
            &disk_after_result,
        ] {
            assert_eq!(record.metadata["access_token"], expected);
            assert_eq!(
                record.metadata["refresh_token"],
                if changes_tokens {
                    "dummy-refresh-r2"
                } else {
                    "dummy-refresh-r1"
                }
            );
        }
    }
}

#[test]
fn round7_repeated_edit_published_during_read_retries_the_observation() {
    use crate::auth::file_guard::FileGuard;
    use crate::manager::CallResult;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let failing = fail.clone();
    let store = Arc::new(
        FileStore::new(dir.path()).with_file_guard(Some(FileGuard::new(move |_| {
            if failing.load(Ordering::SeqCst) {
                Err("test save refusal".into())
            } else {
                Ok(())
            }
        }))),
    );
    let manager = Manager::new(
        Settings::default(),
        Arc::new(FakeModels::default()),
        Some(store.clone()),
    );
    let mut auth = auth_with_metadata(
        "during-read.json",
        "claude",
        serde_json::json!({"type":"claude", "access_token":"dummy", "note":"before"}),
    );
    auth.file_name = auth.id.clone();
    let initial = manager.register(auth).unwrap();
    fail.store(true, Ordering::SeqCst);
    let edit_note = |auth: &mut crate::auth::Auth| {
        auth.metadata["note"] = serde_json::json!("operator");
        Ok::<_, ()>(true)
    };
    let edited = manager.edit(&initial.id, edit_note).unwrap().unwrap();
    fail.store(false, Ordering::SeqCst);
    let (read_tx, read_rx) = mpsc::channel();
    let (published_tx, published_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let once = AtomicBool::new(true);
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if auth.metadata["note"] == "operator" && once.swap(false, Ordering::SeqCst) {
            published_tx.send(()).unwrap();
            lock(&release_rx)
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
        }
    }));
    let editor = manager.clone();
    let repeat = std::thread::spawn(move || {
        read_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        editor.edit("during-read.json", edit_note).unwrap().unwrap()
    });
    let mut during_read = None;
    let observed = manager
        .adopt_observed(&edited, None, |live| {
            let record = store.read_back(live).unwrap().unwrap();
            read_tx.send(()).unwrap();
            published_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            during_read = manager.get(&initial.id);
            Ok(AdoptionRead::Record(record))
        })
        .unwrap();
    release_tx.send(()).unwrap();
    repeat.join().unwrap();
    *lock(&manager.shared.persist_hook) = None;
    let during_read = during_read.unwrap();
    let after_save = manager.get(&initial.id).unwrap();
    manager.mark_result(&CallResult {
        auth_id: initial.id.clone(),
        provider: "claude".into(),
        success: true,
        ..Default::default()
    });
    let final_disk = store.read_back(&initial).unwrap().unwrap();
    assert_eq!(during_read.policy_generation, edited.policy_generation);
    assert_eq!(during_read.credential_version, edited.credential_version);
    assert_eq!(during_read.metadata, edited.metadata);
    assert!(
        matches!(observed, AdoptionResult::Retry),
        "F2b: a repeated edit published during the read must invalidate that read"
    );
    assert_eq!(after_save.metadata["note"], "operator");
    assert_eq!(final_disk.metadata["note"], "operator");
}
