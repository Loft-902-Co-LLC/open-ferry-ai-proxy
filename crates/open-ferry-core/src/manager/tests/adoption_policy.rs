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
    *lock(&manager.shared.persist_hook) = Some(Arc::new(move |auth| {
        if auth.disabled {
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
    let retry = manager
        .adopt_observed(&initial, None, |_| {
            panic!("a pending policy save must defer the next read")
        })
        .unwrap();
    release_tx.send(()).unwrap();
    edit.join().unwrap();
    assert!(matches!(retry, AdoptionResult::Retry));
    *lock(&manager.shared.persist_hook) = None;
    let memory_disabled = manager.get("policy.json").unwrap().disabled;
    let disk_disabled = store.read_back(&initial).unwrap().unwrap().disabled;
    assert_eq!(
        (memory_disabled, disk_disabled),
        (true, true),
        "the disable and its save must win"
    );
    assert!(matches!(result, AdoptionResult::Retry));
}
