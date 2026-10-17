// Ported from CLIProxyAPI internal/auth/codex/refresh_patch_test.go,
// siwc_auth_test.go and sdk/auth/filestore_codex_refresh_test.go (v8.0.27, MIT).
use super::*;
use crate::auth::{AuthStore, FileStore};
use serde_json::json;
use std::sync::Arc;

fn scratch() -> tempfile::TempDir {
    let root = std::env::var_os("OFP_TEST_SCRATCH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
        });
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn fixture() -> Auth {
    Auth {
        id: "codex.json".into(),
        file_name: "codex.json".into(),
        provider: "codex".into(),
        metadata: json!({
            "type":"codex", "access_token":"outer-old", "refresh_token":"outer-ref-old",
            "last_refresh":"2026-10-12T12:00:00Z", "use_siwc":true, "disabled":false,
            "note":"original", "proxy_url":"http://original", "siwc": {
                "access_token":"siwc-old", "refresh_token":"siwc-ref-old", "client_id":"client",
                "last_refresh":"2026-10-12T12:00:00Z", "note":"siwc-original"
            }
        })
        .as_object()
        .unwrap()
        .clone(),
        ..Default::default()
    }
}

/// Creates the file as a login does, then seeds the fixture's SIWC keys with a
/// raw write: a login no longer writes them.
fn create(store: &FileStore, dir: &std::path::Path, auth: &Auth) {
    store.save_new_auth(&mut auth.clone()).unwrap();
    let path = dir.join(&auth.file_name);
    let mut on_disk: Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for key in ["siwc", "use_siwc"] {
        if let Some(value) = auth.metadata.get(key) {
            on_disk.insert(key.into(), value.clone());
        }
    }
    std::fs::write(&path, serde_json::to_vec(&on_disk).unwrap()).unwrap();
}

fn rotate(auth: &mut Auth, outer: bool, siwc: bool) {
    if outer {
        auth.metadata
            .insert("access_token".into(), json!("outer-new"));
        auth.metadata
            .insert("refresh_token".into(), json!("outer-ref-new"));
    }
    if siwc {
        let siwc = auth
            .metadata
            .get_mut("siwc")
            .unwrap()
            .as_object_mut()
            .unwrap();
        siwc.insert("access_token".into(), json!("siwc-new"));
        siwc.insert("refresh_token".into(), json!("siwc-ref-new"));
    }
}

#[test]
fn device_id_is_v4_and_rebind_validation_normalizes() {
    let id = generate_device_id();
    assert_eq!(validate_device_id(&id.to_uppercase()), Some(id.clone()));
    assert_eq!(id.len(), 36);
    assert_ne!(id, generate_device_id());
    for invalid in [
        "",
        "invalid",
        "00000000-0000-0000-0000-000000000000",
        "018f0000-0000-7000-8000-000000000000",
    ] {
        assert!(validate_device_id(invalid).is_none());
    }
    let intent = CodexPersistIntent::SiwcBind(json!({"device_id":id}).as_object().unwrap().clone());
    assert!(!format!("{intent:?}").contains(&id));
}

#[test]
fn credential_refresh_patch_checks_partitions_independently() {
    for rotated_outer in [true, false] {
        let mut before = fixture();
        let mut after = before.clone();
        rotate(&mut after, true, true);
        let patch = CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, true);
        let mut disk = before.clone();
        if rotated_outer {
            disk.metadata
                .insert("refresh_token".into(), json!("outer-bound"));
        } else {
            disk.metadata["siwc"]["refresh_token"] = json!("siwc-bound");
        }
        before
            .metadata
            .insert("refresh_token".into(), json!("mutated-before"));
        after
            .metadata
            .insert("refresh_token".into(), json!("mutated-after"));
        let (saved, applied) = patch.apply(&disk.metadata);
        assert_eq!(applied.outer, Some(!rotated_outer));
        assert_eq!(applied.siwc, Some(rotated_outer));
        assert_eq!(
            saved["refresh_token"],
            if rotated_outer {
                "outer-bound"
            } else {
                "outer-ref-new"
            }
        );
        assert_eq!(
            saved["siwc"]["refresh_token"],
            if rotated_outer {
                "siwc-ref-new"
            } else {
                "siwc-bound"
            }
        );
    }
}

#[test]
fn patch_preserves_unknown_fields_deletions_and_live_switch() {
    let before = fixture();
    let mut after = before.clone();
    rotate(&mut after, true, true);
    let patch = CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, true);
    let mut disk = before.metadata.clone();
    disk.remove("proxy_url");
    disk.remove("last_refresh");
    disk.insert("use_siwc".into(), json!(false));
    disk.insert("new_property".into(), json!({"keep":true}));
    disk["siwc"]["note"] = json!("disk-siwc-note");
    let (saved, _) = patch.apply(&disk);
    assert!(!saved.contains_key("proxy_url"));
    assert!(!saved.contains_key("last_refresh"));
    assert_eq!(saved["use_siwc"], false);
    assert_eq!(saved["siwc"]["note"], "disk-siwc-note");
    assert_eq!(saved["new_property"], disk["new_property"]);
    let (again, _) = patch.apply(&saved);
    assert_eq!(again, saved);
}

#[test]
fn disk_baseline_and_own_result_are_accepted() {
    let before = fixture();
    let mut disk = before.clone();
    disk.metadata
        .insert("access_token".into(), json!("disk-before"));
    let mut after = disk.clone();
    rotate(&mut after, true, false);
    let patch = CredentialRefreshPatch::with_disk_baseline(
        &before.metadata,
        Some(&disk.metadata),
        &after.metadata,
        true,
        false,
    );
    assert!(patch.can_apply_to(&disk.metadata));
    assert_eq!(patch.apply(&disk.metadata).0["access_token"], "outer-new");
    assert!(patch.can_apply_to(&after.metadata));
    assert_eq!(patch.apply(&after.metadata).0, after.metadata);
}

#[test]
fn empty_disk_partition_does_not_restore_deleted_credentials() {
    let before = fixture();
    let mut after = before.clone();
    rotate(&mut after, true, true);
    let disk = json!({"type":"codex","note":"keep"})
        .as_object()
        .unwrap()
        .clone();
    let patch = CredentialRefreshPatch::with_disk_baseline(
        &before.metadata,
        Some(&disk),
        &after.metadata,
        true,
        true,
    );
    let (saved, applied) = patch.apply(&disk);
    assert_eq!(saved, disk);
    assert_eq!(applied.outer, Some(false));
    assert_eq!(applied.siwc, Some(false));
}

#[test]
fn codex_refresh_preserves_disk_configuration() {
    for (outer, siwc) in [(true, false), (false, true), (true, true)] {
        let dir = scratch();
        let store = FileStore::new(dir.path());
        let before = fixture();
        let mut updated = before.clone();
        rotate(&mut updated, outer, siwc);
        updated.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
            &before.metadata,
            &updated.metadata,
            outer,
            siwc,
        ));
        let mut disk = before.metadata.clone();
        disk.insert("use_siwc".into(), json!(false));
        disk.insert("disabled".into(), json!(true));
        disk.insert("note".into(), json!("disk-note"));
        disk.remove("proxy_url");
        disk["siwc"]["note"] = json!("disk-siwc-note");
        std::fs::write(
            dir.path().join("codex.json"),
            serde_json::to_vec(&disk).unwrap(),
        )
        .unwrap();
        store.save_auth(&mut updated).unwrap();
        assert_eq!(updated.metadata["use_siwc"], false);
        assert!(updated.disabled);
        assert_eq!(updated.metadata["note"], "disk-note");
        assert_eq!(updated.metadata["siwc"]["note"], "disk-siwc-note");
        assert!(!updated.metadata.contains_key("proxy_url"));
        assert!(matches!(
            updated.codex_persist_intent,
            CodexPersistIntent::Configuration
        ));
        assert_eq!(
            updated.metadata["access_token"],
            if outer { "outer-new" } else { "outer-old" }
        );
        assert_eq!(
            updated.metadata["siwc"]["access_token"],
            if siwc { "siwc-new" } else { "siwc-old" }
        );
    }
}

#[test]
fn ordinary_save_preserves_siwc_and_allows_deleting_settings() {
    let dir = scratch();
    let store = FileStore::new(dir.path());
    let auth = fixture();
    create(&store, dir.path(), &auth);
    let mut incoming = fixture();
    incoming.metadata.remove("siwc");
    incoming.metadata.remove("use_siwc");
    incoming.metadata.remove("proxy_url");
    incoming.metadata.remove("note");
    incoming
        .metadata
        .insert("refresh_token".into(), json!("stale"));
    store.save_auth(&mut incoming).unwrap();
    assert_eq!(incoming.metadata["siwc"], auth.metadata["siwc"]);
    assert_eq!(incoming.metadata["use_siwc"], true);
    assert_eq!(incoming.metadata["refresh_token"], "outer-ref-old");
    assert!(!incoming.metadata.contains_key("proxy_url"));
    assert!(!incoming.metadata.contains_key("note"));
    incoming.metadata.insert("use_siwc".into(), json!(false));
    store.save_auth(&mut incoming).unwrap();
    assert_eq!(incoming.metadata["use_siwc"], false);
}

#[test]
fn outer_login_replacement_is_explicit() {
    let dir = scratch();
    let store = FileStore::new(dir.path());
    let mut before = fixture();
    create(&store, dir.path(), &before);
    let mut login = fixture();
    rotate(&mut login, true, true);
    login.metadata.remove("note");
    store.save_new_auth(&mut login).unwrap();
    assert_eq!(login.metadata["refresh_token"], "outer-ref-new");
    assert_eq!(login.metadata["siwc"], before.metadata["siwc"]);
    assert_eq!(login.metadata["note"], "original");
    store.save_auth(&mut before).unwrap();
    assert_eq!(before.metadata["refresh_token"], "outer-ref-new");
}

#[test]
fn concurrent_two_stores_same_timestamp_preserve_both_rotations() {
    let dir = scratch();
    let before = fixture();
    create(&FileStore::new(dir.path()), dir.path(), &before);
    let path = dir.path().join("codex.json");
    let (read, read_rx) = std::sync::mpsc::channel();
    let (waiting, waiting_rx) = std::sync::mpsc::channel();
    let finished = waiting.clone();
    let (release, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let read_count = reads.clone();
    crate::auth::credential_lock::set_test_hook(
        &path,
        Some(Arc::new(move |event| {
            if event == "read" && read_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
            {
                read.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            if event == "waiting" {
                let _ = waiting.send("waiting");
            }
        })),
    );
    let mut rendezvous = "";
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for (outer, siwc) in [(true, false), (false, true)] {
            let mut updated = before.clone();
            rotate(&mut updated, outer, siwc);
            updated.codex_persist_intent = CodexPersistIntent::Refresh(
                CredentialRefreshPatch::new(&before.metadata, &updated.metadata, outer, siwc),
            );
            let path = dir.path();
            let finished = finished.clone();
            workers.push(scope.spawn(move || {
                let result = FileStore::new(path).save_auth(&mut updated);
                let _ = finished.send("finished");
                result.unwrap();
            }));
            if outer {
                read_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
            }
        }
        // The first writer is parked after reading disk; the second must be
        // waiting outside that critical section, not reading the same baseline.
        rendezvous = waiting_rx.recv().unwrap();
        if rendezvous == "waiting" {
            assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        release.send(()).unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
    });
    crate::auth::credential_lock::set_test_hook(&path, None);
    assert_eq!(
        rendezvous, "waiting",
        "second persistence finished instead of waiting outside read/merge/write"
    );
    let store = FileStore::new(dir.path());
    let mut stale = before.clone();
    rotate(&mut stale, true, true);
    stale
        .metadata
        .insert("refresh_token".into(), json!("superseded-result"));
    stale.metadata["siwc"]["refresh_token"] = json!("superseded-siwc");
    stale.codex_persist_intent = CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
        &before.metadata,
        &stale.metadata,
        true,
        true,
    ));
    store.save_auth(&mut stale).unwrap();
    let saved = store.list().unwrap().remove(0);
    assert_eq!(saved.metadata["refresh_token"], "outer-ref-new");
    assert_eq!(saved.metadata["siwc"]["refresh_token"], "siwc-ref-new");
    let mut ordinary = before;
    store.save_auth(&mut ordinary).unwrap();
    assert_eq!(ordinary.metadata, saved.metadata);
}

#[tokio::test]
async fn bind_and_patch_api_return_persisted_record_and_reject_invalid_json() {
    let dir = scratch();
    let store = Arc::new(FileStore::new(dir.path()));
    let before = fixture();
    store.save_new(&before).unwrap();
    let mut after = before.clone();
    rotate(&mut after, true, true);
    let bound = store
        .bind_siwc(
            before.clone(),
            after.metadata["siwc"].as_object().unwrap().clone(),
        )
        .await
        .unwrap();
    assert_eq!(bound.metadata["refresh_token"], "outer-ref-old");
    assert_eq!(bound.metadata["siwc"]["refresh_token"], "siwc-ref-new");
    assert_eq!(bound.metadata["use_siwc"], true);
    let patch = CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, true);
    let result = store
        .apply_codex_refresh(before.clone(), patch)
        .await
        .unwrap();
    assert_eq!(result.applied.outer, Some(true));
    assert_eq!(result.applied.siwc, Some(true)); // Own result is safe to replay.
    assert_eq!(result.auth.metadata["refresh_token"], "outer-ref-new");
    let mut newer = after.metadata["siwc"].as_object().unwrap().clone();
    newer.insert("refresh_token".into(), json!("newer-bind"));
    store.bind_siwc(before.clone(), newer).await.unwrap();
    let replay = store
        .apply_codex_refresh(
            before.clone(),
            CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, true),
        )
        .await
        .unwrap();
    assert_eq!(replay.applied.siwc, Some(false));
    assert_eq!(replay.auth.metadata["siwc"]["refresh_token"], "newer-bind");
    let path = dir.path().join("codex.json");
    for bad in [b"not json".as_slice(), b"null", b"[]", b""] {
        std::fs::write(&path, bad).unwrap();
        assert!(store.save(&before).is_err());
        assert!(
            store
                .apply_codex_refresh(
                    before.clone(),
                    CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, true,)
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bad);
        assert!(
            store
                .bind_siwc(
                    before.clone(),
                    after.metadata["siwc"].as_object().unwrap().clone(),
                )
                .await
                .is_err()
        );
        assert!(std::fs::read(&path).unwrap() == bad);
    }
}

#[test]
fn synthesizer_and_store_ignore_lock_and_round_trip_siwc() {
    let dir = scratch();
    let store = FileStore::new(dir.path());
    let before = fixture();
    create(&store, dir.path(), &before);
    std::fs::write(
        dir.path().join("codex.json.lock"),
        serde_json::to_vec(&before.metadata).unwrap(),
    )
    .unwrap();
    let ctx = crate::auth::synthesizer::SynthesisContext::new(dir.path(), chrono::Utc::now());
    let records = crate::auth::synthesizer::file::synthesize_file_auths(&ctx);
    assert_eq!(records.len(), 1);
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(records[0].metadata["siwc"], before.metadata["siwc"]);
    assert_eq!(records[0].metadata["use_siwc"], true);
    assert!(!records[0].attributes.contains_key("use_siwc"));
}

#[tokio::test]
async fn raw_upload_and_delete_cooperate_with_credential_lock() {
    let dir = scratch();
    let store = Arc::new(FileStore::new(dir.path()));
    let before = fixture();
    store.save_new(&before).unwrap();
    let path = dir.path().join("codex.json");
    let held = crate::auth::credential_lock::acquire_credential_lock(&path)
        .await
        .unwrap();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let writer = store.clone();
    let mut write = tokio::task::spawn_blocking(move || {
        entered.send(()).unwrap();
        writer.write_file(
            "codex.json",
            br#"{"type":"codex","access_token":"uploaded"}"#,
        )
    });
    waiting.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut write)
            .await
            .is_err()
    );
    assert_eq!(
        store.list().unwrap()[0].metadata["access_token"],
        "outer-old"
    );
    drop(held);
    write.await.unwrap().unwrap();
    assert_eq!(
        store.list().unwrap()[0].metadata["access_token"],
        "uploaded"
    );

    let held = crate::auth::credential_lock::acquire_credential_lock(&path)
        .await
        .unwrap();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let writer = store.clone();
    let mut remove = tokio::task::spawn_blocking(move || {
        entered.send(()).unwrap();
        writer.remove_file("codex.json")
    });
    waiting.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut remove)
            .await
            .is_err()
    );
    assert!(path.exists());
    drop(held);
    remove.await.unwrap().unwrap();
    assert!(!path.exists());
}

#[test]
fn lock_files_are_reserved_and_other_provider_saves_are_unchanged() {
    let dir = scratch();
    let store = FileStore::new(dir.path());
    store.save_new(&fixture()).unwrap();
    let lock = dir.path().join("codex.json.lock");
    let bytes = std::fs::read(&lock).unwrap();
    assert!(store.write_file("codex.json.lock", b"replace").is_err());
    assert!(store.remove_file("codex.json.lock").is_err());
    assert!(store.delete("codex.json.lock").is_err());
    let mut target = fixture();
    target.file_name = "codex.json.lock".into();
    assert!(store.save_auth(&mut target).is_err());
    assert_eq!(std::fs::read(lock).unwrap(), bytes);
    let other = Auth {
        id: "claude.json".into(),
        provider: "claude".into(),
        metadata: json!({"type":"claude","access_token":"ordinary"})
            .as_object()
            .unwrap()
            .clone(),
        ..Default::default()
    };
    store.save_new(&other).unwrap();
    assert!(!dir.path().join("claude.json.lock").exists());
}

#[test]
fn null_identity_matches_missing_for_each_partition_and_baseline() {
    for siwc in [false, true] {
        for disk_baseline in [false, true] {
            let mut before = fixture();
            let baseline = if siwc {
                before.metadata["siwc"].as_object_mut().unwrap()
            } else {
                &mut before.metadata
            };
            baseline.insert("id_token".into(), Value::Null);
            let mut after = before.clone();
            rotate(&mut after, !siwc, siwc);
            let mut disk = before.metadata.clone();
            if disk_baseline {
                if siwc {
                    before.metadata["siwc"]["access_token"] = json!("other-memory");
                } else {
                    before
                        .metadata
                        .insert("access_token".into(), json!("other-memory"));
                }
            }
            let patch = CredentialRefreshPatch::with_disk_baseline(
                &before.metadata,
                disk_baseline.then_some(&disk),
                &after.metadata,
                !siwc,
                siwc,
            );
            if siwc {
                disk["siwc"].as_object_mut().unwrap().remove("id_token");
            } else {
                disk.remove("id_token");
            }
            let (saved, applied) = patch.apply(&disk);
            assert_eq!(if siwc { applied.siwc } else { applied.outer }, Some(true));
            assert_eq!(
                if siwc {
                    &saved["siwc"]["refresh_token"]
                } else {
                    &saved["refresh_token"]
                },
                if siwc {
                    "siwc-ref-new"
                } else {
                    "outer-ref-new"
                }
            );
        }
    }
}

#[test]
fn siwc_attributes_and_timestamp_alias_match_upstream() {
    let mut auth = fixture();
    auth.metadata.insert("use_siwc".into(), json!(false));
    auth.attributes.insert("use_siwc".into(), "true".into());
    assert!(auth.use_siwc());
    auth.attributes.insert("use_siwc".into(), "false".into());
    auth.metadata.insert("use_siwc".into(), json!("true"));
    assert!(auth.use_siwc());
    auth.metadata.insert("use_siwc".into(), json!(false));
    assert!(!auth.use_siwc());
    let siwc = auth.metadata["siwc"].as_object_mut().unwrap();
    let last = siwc.remove("last_refresh").unwrap();
    siwc.insert("lastRefresh".into(), last);
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-12T12:01:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert_eq!(
        auth.siwc_refresh_due_at(now),
        Some(now + chrono::TimeDelta::minutes(4))
    );
    auth.metadata["siwc"]["last_refresh"] = Value::Null;
    assert_eq!(
        auth.siwc_refresh_due_at(now),
        Some(now + chrono::TimeDelta::minutes(4))
    );
}

#[test]
fn login_skips_empty_settings_and_writes_no_switch() {
    let mut disk = fixture().metadata;
    disk.remove("use_siwc");
    disk.remove("siwc");
    let mut incoming = fixture().metadata;
    incoming.insert("note".into(), Value::Null);
    incoming.insert("proxy_url".into(), json!("  "));
    let (saved, _) = merge_persist(Some(&disk), &incoming, &CodexPersistIntent::OuterLogin);
    assert_eq!(saved["note"], "original");
    assert_eq!(saved["proxy_url"], "http://original");
    assert!(!saved.contains_key("use_siwc"));
    assert!(!saved.contains_key("siwc"));
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn incoming_b() -> Map<String, Value> {
    let mut incoming = fixture().metadata;
    incoming.insert("access_token".into(), json!("outer-B"));
    incoming.insert("refresh_token".into(), json!("outer-ref-B"));
    incoming.insert("siwc".into(), json!({"refresh_token": "siwc-ref-B"}));
    incoming
}

#[test]
fn login_over_no_disk_writes_neither_siwc_key() {
    let incoming = incoming_b();
    let (saved, _) = merge_persist(None, &incoming, &CodexPersistIntent::OuterLogin);
    assert_eq!(saved["refresh_token"], "outer-ref-B");
    assert_eq!(saved["note"], "original");
    assert!(!saved.contains_key("siwc"));
    assert!(!saved.contains_key("use_siwc"));
}

#[test]
fn login_over_siwc_disk_keeps_disks_siwc_and_switch() {
    let mut disk = fixture().metadata;
    disk.insert("use_siwc".into(), json!(false));
    let (saved, _) = merge_persist(Some(&disk), &incoming_b(), &CodexPersistIntent::OuterLogin);
    assert_eq!(saved["refresh_token"], "outer-ref-B");
    assert_eq!(saved["siwc"], disk["siwc"]);
    assert_eq!(saved["use_siwc"], false);
}

#[test]
fn only_a_login_loses_siwc_when_there_is_no_disk() {
    let incoming = incoming_b();
    let refresh = CredentialRefreshPatch::new(&fixture().metadata, &incoming, true, false);
    for intent in [
        CodexPersistIntent::Configuration,
        CodexPersistIntent::SiwcBind(object(json!({"refresh_token": "bound"}))),
        CodexPersistIntent::Refresh(refresh),
    ] {
        let (saved, _) = merge_persist(None, &incoming, &intent);
        assert_eq!(saved, incoming, "{intent:?}");
    }
}

#[test]
fn constrain_withheld_makes_each_withheld_partition_disks() {
    let disk = object(json!({
        "access_token": "disk-access", "refresh_token": "disk-ref", "plan_type": "pro",
        "use_siwc": false, "note": "disk-note",
        "siwc": {"refresh_token": "disk-siwc"}
    }));
    let merged = object(json!({
        "access_token": "live-access", "refresh_token": "live-ref", "email": "live@example.com",
        "use_siwc": true, "note": "live-note",
        "siwc": {"refresh_token": "live-siwc"}
    }));
    let run = |outer, siwc| {
        let mut merged = merged.clone();
        constrain_withheld(
            &mut merged,
            &disk,
            crate::auth::WithheldPartitions { outer, siwc },
        );
        merged
    };
    assert_eq!(run(false, false), merged);

    let both = run(true, true);
    assert_eq!(both["access_token"], "disk-access");
    assert_eq!(both["refresh_token"], "disk-ref");
    assert_eq!(both["plan_type"], "pro");
    // A key disk lacks is removed.
    assert!(!both.contains_key("email"));
    assert_eq!(both["siwc"], disk["siwc"]);
    // No other key changes, the switch included.
    assert_eq!(both["use_siwc"], true);
    assert_eq!(both["note"], "live-note");

    let outer = run(true, false);
    assert_eq!(outer["refresh_token"], "disk-ref");
    assert_eq!(outer["siwc"], merged["siwc"]);

    let siwc = run(false, true);
    assert_eq!(siwc["siwc"], disk["siwc"]);
    assert_eq!(siwc["refresh_token"], "live-ref");
    assert_eq!(siwc["email"], "live@example.com");

    // A disk without a siwc object removes the merged one.
    let mut bare = disk.clone();
    bare.remove("siwc");
    let mut merged = merged.clone();
    constrain_withheld(
        &mut merged,
        &bare,
        crate::auth::WithheldPartitions {
            outer: false,
            siwc: true,
        },
    );
    assert!(!merged.contains_key("siwc"));
}

#[test]
fn holds_partition_needs_a_non_empty_identity_string() {
    use crate::auth::CodexCredentialPartition::{Outer, Siwc};
    for key in [
        "access_token",
        "refresh_token",
        "id_token",
        "account_id",
        "client_id",
        "device_id",
    ] {
        let outer = object(json!({ key: "x" }));
        assert!(holds_partition(&outer, Outer), "{key}");
        assert!(!holds_partition(&outer, Siwc), "{key}");
        let nested = object(json!({"siwc": { key: "x" }}));
        assert!(holds_partition(&nested, Siwc), "{key}");
        assert!(!holds_partition(&nested, Outer), "{key}");
    }
    assert!(!holds_partition(
        &object(json!({"refresh_token": ""})),
        Outer
    ));
    assert!(!holds_partition(&object(json!({"note": "x"})), Outer));
    assert!(!holds_partition(
        &object(json!({"refresh_token": 5})),
        Outer
    ));
    assert!(!holds_partition(
        &object(json!({"refresh_token": null})),
        Outer
    ));
    assert!(!holds_partition(
        &object(json!({"refresh_token": "x"})),
        Siwc
    ));
    assert!(!holds_partition(
        &object(json!({"siwc": {"refresh_token": ""}})),
        Siwc
    ));
    assert!(!holds_partition(&object(json!({"siwc": "x"})), Siwc));
}

#[test]
fn refresh_normalizes_disk_settings_after_patch() {
    let mut disk = fixture().metadata;
    disk.insert("proxy-url".into(), json!("http://legacy"));
    disk.remove("proxy_url");
    let patch = CredentialRefreshPatch::new(&disk, &disk, true, false);
    let (saved, _) = merge_persist(Some(&disk), &disk, &CodexPersistIntent::Refresh(patch));
    assert_eq!(saved["proxy_url"], "http://legacy");
    assert!(!saved.contains_key("proxy-url"));
}

#[test]
fn pending_rotation_layers_over_configuration_without_serializing_intent() {
    let dir = scratch();
    let store = FileStore::new(dir.path());
    let mut before = fixture();
    before.metadata.insert("id_token".into(), json!("old-id"));
    store.save_new(&before).unwrap();
    let mut edited = before.clone();
    rotate(&mut edited, true, true);
    edited.metadata.remove("id_token");
    edited.codex_pending_refresh = vec![CredentialRefreshPatch::new(
        &before.metadata,
        &edited.metadata,
        true,
        true,
    )];
    assert_eq!(
        edited
            .codex_pending_refresh
            .first()
            .unwrap()
            .apply(&before.metadata)
            .0["id_token"],
        "old-id"
    );
    assert!(!format!("{edited:?}").contains("outer-ref-new"));
    edited.metadata.insert("note".into(), json!("edited"));
    store.save_auth(&mut edited).unwrap();
    assert_eq!(edited.metadata["refresh_token"], "outer-ref-new");
    assert_eq!(edited.metadata["siwc"]["refresh_token"], "siwc-ref-new");
    assert_eq!(edited.metadata["note"], "edited");
    assert_eq!(edited.metadata["id_token"], "old-id");
    assert!(edited.codex_pending_refresh.is_empty());
    let bytes = std::fs::read_to_string(dir.path().join("codex.json")).unwrap();
    assert!(!bytes.contains("codex_pending_refresh"));
    assert!(!bytes.contains("codex_persist_intent"));
    assert!(!format!("{edited:?}").contains("outer-ref-new"));
    let mut newer = before.clone();
    newer
        .metadata
        .insert("refresh_token".into(), json!("newer-writer"));
    store.save_new(&newer).unwrap();
    edited.codex_pending_refresh = vec![CredentialRefreshPatch::new(
        &before.metadata,
        &edited.metadata,
        true,
        false,
    )];
    store.save_auth(&mut edited).unwrap();
    assert_eq!(edited.metadata["refresh_token"], "newer-writer");
    assert!(edited.codex_pending_refresh.is_empty());
}

#[test]
fn corrupt_configuration_is_untouched_but_login_replaces_it() {
    let dir = scratch();
    let store = FileStore::new(dir.path());
    let path = dir.path().join("codex.json");
    std::fs::write(&path, b"broken-json-secret").unwrap();
    let mut auth = fixture();
    let error = store.save_auth(&mut auth).unwrap_err().to_string();
    assert!(error.contains("codex.json"));
    assert!(!error.contains("broken-json-secret"));
    assert_eq!(std::fs::read(&path).unwrap(), b"broken-json-secret");
    store.save_new_auth(&mut auth).unwrap();
    assert_eq!(
        store.list().unwrap()[0].metadata["refresh_token"],
        "outer-ref-old"
    );
}

#[test]
fn async_partition_apis_work_without_a_runtime() {
    use std::future::Future;
    let dir = scratch();
    let store = Arc::new(FileStore::new(dir.path()));
    let before = fixture();
    store.save_new(&before).unwrap();
    let patch = CredentialRefreshPatch::new(&before.metadata, &before.metadata, true, false);
    let mut future = std::pin::pin!(store.apply_codex_refresh(before, patch));
    let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
    assert!(matches!(
        future.as_mut().poll(&mut context),
        std::task::Poll::Ready(Ok(_))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn async_persistence_wait_does_not_stop_runtime_progress() {
    let dir = scratch();
    let store = Arc::new(FileStore::new(dir.path()));
    let before = fixture();
    store.save_new(&before).unwrap();
    let path = dir.path().join("codex.json");
    let guard = crate::auth::credential_lock::acquire_credential_lock(&path)
        .await
        .unwrap();
    let (waiting, mut waiting_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::auth::credential_lock::set_test_hook(
        &path,
        Some(Arc::new(move |event| {
            if event == "waiting" {
                let _ = waiting.send(());
            }
        })),
    );
    let worker = tokio::spawn(async move {
        let patch = CredentialRefreshPatch::new(&before.metadata, &before.metadata, true, false);
        store.apply_codex_refresh(before, patch).await.unwrap();
    });
    let (release, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(2));
        drop(guard);
    });
    let start = std::time::Instant::now();
    waiting_rx.recv().await.unwrap();
    tokio::task::yield_now().await;
    let progressed = start.elapsed() < std::time::Duration::from_secs(1);
    let _ = release.send(());
    worker.await.unwrap();
    holder.join().unwrap();
    crate::auth::credential_lock::set_test_hook(&path, None);
    assert!(progressed, "runtime could not progress during lock wait");
}

#[test]
fn contended_credential_does_not_hold_store_save_lock() {
    let dir = scratch();
    let store = Arc::new(FileStore::new(dir.path()));
    let before = fixture();
    store.save_new(&before).unwrap();
    let path = dir.path().join("codex.json");
    let guard = crate::auth::credential_lock::acquire_sync(&path).unwrap();
    let (waiting, waiting_rx) = std::sync::mpsc::channel();
    let finished = waiting.clone();
    crate::auth::credential_lock::set_test_hook(
        &path,
        Some(Arc::new(move |event| {
            if event == "waiting" {
                let _ = waiting.send("waiting");
            }
        })),
    );
    let saved = store.clone();
    let waiter = std::thread::spawn(move || {
        let result = saved.save(&before);
        let _ = finished.send("finished");
        result.unwrap();
    });
    assert_eq!(waiting_rx.recv().unwrap(), "waiting");
    let mut other = fixture();
    other.id = "other.json".into();
    other.file_name = other.id.clone();
    let saved = store.clone();
    let (finished, finished_rx) = std::sync::mpsc::channel();
    let second = std::thread::spawn(move || {
        saved.save_new(&other).unwrap();
        let _ = finished.send(());
    });
    let result = finished_rx.recv_timeout(std::time::Duration::from_secs(1));
    drop(guard);
    waiter.join().unwrap();
    second.join().unwrap();
    crate::auth::credential_lock::set_test_hook(&path, None);
    result.expect("credential B blocked behind credential A");
}

#[test]
fn held_guard_saves_without_reacquiring_and_rejects_wrong_guard() {
    let dir = scratch();
    let store = Arc::new(FileStore::new(dir.path()));
    let before = fixture();
    store.save_new(&before).unwrap();
    let path = dir.path().join("codex.json");
    let guard = crate::auth::credential_lock::acquire_sync(&path).unwrap();
    let mut after = before.clone();
    rotate(&mut after, true, false);
    let patch = CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, false);
    let start = std::time::Instant::now();
    let persisted = store
        .apply_codex_refresh_locked(&after, &patch, &guard)
        .unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(persisted.auth.metadata["refresh_token"], "outer-ref-new");
    let mut other = fixture();
    other.file_name = "other.json".into();
    store.save_new(&other).unwrap();
    let other_path = dir.path().join("other.json");
    let original = std::fs::read(&other_path).unwrap();
    assert!(
        store
            .apply_codex_refresh_locked(&other, &patch, &guard)
            .is_err()
    );
    assert_eq!(std::fs::read(other_path).unwrap(), original);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(crate::auth::credential_lock::acquire_with_timeout(
            path.clone(),
            std::time::Duration::from_millis(50),
        ))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    let (waiting, waiting_rx) = std::sync::mpsc::channel();
    crate::auth::credential_lock::set_test_hook(
        &path,
        Some(Arc::new(move |event| {
            if event == "waiting" {
                let _ = waiting.send(());
            }
        })),
    );
    let (saved, saved_rx) = std::sync::mpsc::channel();
    let saving = store.clone();
    let worker = std::thread::spawn(move || {
        let result = saving.save(&before);
        let _ = saved.send(result);
    });
    waiting_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the non-holder's save reaches the held lock");
    assert!(saved_rx.try_recv().is_err());
    drop(guard);
    saved_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    crate::auth::credential_lock::set_test_hook(&path, None);
    assert_eq!(
        store.list().unwrap()[0].metadata["refresh_token"],
        "outer-ref-new"
    );
}

#[test]
fn held_guard_binds_siwc_without_reacquiring_and_rejects_wrong_guard() {
    let dir = scratch();
    let store = Arc::new(FileStore::new(dir.path()));
    let before = fixture();
    store.save_new(&before).unwrap();
    let path = dir.path().join("codex.json");
    let guard = crate::auth::credential_lock::acquire_sync(&path).unwrap();
    let mut after = before.clone();
    rotate(&mut after, false, true);
    let siwc = after.metadata["siwc"].as_object().unwrap().clone();
    // Reacquiring the held lock would wait out its timeout and fail.
    let bound = store
        .bind_siwc_locked(&before, siwc.clone(), &guard, &|_, _| Ok(()))
        .unwrap();
    assert_eq!(bound.metadata["refresh_token"], "outer-ref-old");
    assert_eq!(bound.metadata["siwc"]["refresh_token"], "siwc-ref-new");
    assert_eq!(bound.metadata["use_siwc"], true);
    let mut other = fixture();
    other.file_name = "other.json".into();
    store.save_new(&other).unwrap();
    let other_path = dir.path().join("other.json");
    let original = std::fs::read(&other_path).unwrap();
    assert!(
        store
            .bind_siwc_locked(&other, siwc, &guard, &|_, _| Ok(()))
            .is_err()
    );
    assert_eq!(std::fs::read(other_path).unwrap(), original);
}

#[test]
fn pending_patch_requires_every_rotated_partition_to_match_after() {
    let before = fixture();
    let mut after = before.clone();
    rotate(&mut after, true, true);
    let patch = CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, true);
    assert!(patch.matches_after(&after.metadata));
    after.metadata.insert("note".into(), json!("edited"));
    assert!(patch.matches_after(&after.metadata));
    let mut explicit = after.metadata.clone();
    explicit.insert("refresh_token".into(), json!("user-login"));
    assert!(!patch.matches_after(&explicit));
    explicit = after.metadata.clone();
    explicit["siwc"]["refresh_token"] = json!("user-bind");
    assert!(!patch.matches_after(&explicit));
}

#[test]
fn later_refresh_preserves_pending_partition_and_chains_its_disk_baseline() {
    for intermediate_disk in [false, true] {
        for disk_baseline in [false, true] {
            for same_partition in [false, true] {
                let dir = scratch();
                let store = FileStore::new(dir.path());
                let before = fixture();
                create(&store, dir.path(), &before);
                let mut memory = before.clone();
                if disk_baseline {
                    memory
                        .metadata
                        .insert("access_token".into(), json!("memory-baseline"));
                }
                let mut first = memory.clone();
                rotate(&mut first, true, false);
                let pending = CredentialRefreshPatch::with_disk_baseline(
                    &memory.metadata,
                    disk_baseline.then_some(&before.metadata),
                    &first.metadata,
                    true,
                    false,
                );
                if intermediate_disk {
                    store.save_new(&first).unwrap();
                }
                let mut second = first.clone();
                if same_partition {
                    second
                        .metadata
                        .insert("access_token".into(), json!("outer-newest"));
                    second
                        .metadata
                        .insert("refresh_token".into(), json!("outer-ref-newest"));
                } else {
                    rotate(&mut second, false, true);
                }
                second.codex_persist_intent =
                    CodexPersistIntent::Refresh(CredentialRefreshPatch::new(
                        &first.metadata,
                        &second.metadata,
                        same_partition,
                        !same_partition,
                    ));
                second.codex_pending_refresh = vec![pending];
                store.save_auth(&mut second).unwrap();
                assert_eq!(
                    second.metadata["refresh_token"],
                    if same_partition {
                        "outer-ref-newest"
                    } else {
                        "outer-ref-new"
                    }
                );
                assert_eq!(
                    second.metadata["siwc"]["refresh_token"],
                    if same_partition {
                        "siwc-ref-old"
                    } else {
                        "siwc-ref-new"
                    }
                );
                assert!(second.codex_pending_refresh.is_empty());
            }
        }
    }
}

#[test]
fn sparse_effective_identity_is_applicable_and_idempotent() {
    for siwc in [false, true] {
        let mut before = fixture().metadata;
        let partition = if siwc {
            before["siwc"].as_object_mut().unwrap()
        } else {
            &mut before
        };
        partition.insert("id_token".into(), json!("retained-id"));
        let mut after = before.clone();
        let partition = if siwc {
            after["siwc"].as_object_mut().unwrap()
        } else {
            &mut after
        };
        partition.insert("access_token".into(), json!("rotated"));
        partition.remove("id_token");
        let patch = CredentialRefreshPatch::new(&before, &after, !siwc, siwc);
        let effective = patch.apply(&before).0;
        assert!(patch.can_apply_to(&effective));
        assert_eq!(patch.apply(&effective).0, effective);
    }
}

#[test]
fn sparse_effective_identity_keeps_pending_on_settings_edit() {
    let before = json!({"access_token":"before", "id_token":"retained-id"})
        .as_object()
        .unwrap()
        .clone();
    let after = json!({"access_token":"after"}).as_object().unwrap().clone();
    let patch = CredentialRefreshPatch::new(&before, &after, true, false);
    let mut effective = patch.apply(&before).0;
    effective.insert("note".into(), json!("edited"));
    assert!(patch.matches_after(&effective));
}

#[test]
fn sparse_chain_keeps_earlier_refresh_token_rotation() {
    for siwc in [false, true] {
        let before = json!({"access_token":"A0", "refresh_token":"R0"})
            .as_object()
            .unwrap()
            .clone();
        let first = json!({"access_token":"A1", "refresh_token":"R1"})
            .as_object()
            .unwrap()
            .clone();
        let second = json!({"access_token":"A2"}).as_object().unwrap().clone();
        let wrap = |partition: Map<String, Value>| {
            if siwc {
                json!({"siwc": partition}).as_object().unwrap().clone()
            } else {
                partition
            }
        };
        let before = wrap(before);
        let first = wrap(first);
        let second = wrap(second);
        // The first save failed: disk still holds A0/R0, while live holds A1/R1.
        let pending = CredentialRefreshPatch::new(&before, &first, !siwc, siwc);
        let chained =
            CredentialRefreshPatch::new(&first, &second, !siwc, siwc).with_pending(Some(&pending));
        let (saved, applied) = chained.apply(&before);
        assert_eq!(if siwc { applied.siwc } else { applied.outer }, Some(true));
        let partition = if siwc { nested(&saved) } else { saved };
        assert_eq!(partition["access_token"], "A2");
        assert_eq!(partition["refresh_token"], "R1");
    }
}

#[test]
fn sparse_chain_does_not_carry_older_metadata_changed_between_refreshes() {
    // Guards the retain condition; this is not the A2/R1 rotation regression.
    let before = json!({"access_token":"A0", "expired":"original"})
        .as_object()
        .unwrap()
        .clone();
    let first = json!({"access_token":"A1", "expired":"rotated"})
        .as_object()
        .unwrap()
        .clone();
    let pending = CredentialRefreshPatch::new(&before, &first, true, false);
    let mut live = first.clone();
    live.insert("expired".into(), json!("edited"));
    let second = json!({"access_token":"A2"}).as_object().unwrap().clone();
    let chained =
        CredentialRefreshPatch::new(&live, &second, true, false).with_pending(Some(&pending));
    assert!(
        !chained
            .outer
            .as_ref()
            .unwrap()
            .after
            .contains_key("expired")
    );
    let (saved, applied) = chained.apply(&before);
    assert_eq!(applied.outer, Some(true));
    assert_eq!(saved["access_token"], "A2");
    assert_eq!(saved["expired"], "original");
}

#[test]
fn unchanged_identity_updates_expired_from_real_baseline() {
    for siwc in [false, true] {
        let mut before = fixture().metadata;
        let partition = if siwc {
            before["siwc"].as_object_mut().unwrap()
        } else {
            &mut before
        };
        partition.insert("expired".into(), json!("2099-01-01T00:00:00Z"));
        let mut after = before.clone();
        let partition = if siwc {
            after["siwc"].as_object_mut().unwrap()
        } else {
            &mut after
        };
        partition.insert("expired".into(), json!("2099-02-01T00:00:00Z"));
        let patch = CredentialRefreshPatch::new(&before, &after, !siwc, siwc);
        let (saved, applied) = patch.apply(&before);
        assert_eq!(if siwc { applied.siwc } else { applied.outer }, Some(true));
        let partition = if siwc { nested(&saved) } else { saved };
        assert_eq!(partition["expired"], "2099-02-01T00:00:00Z");
    }
}

#[test]
fn sparse_effective_identity_chains_pending_rotation() {
    let before = json!({"access_token":"before", "id_token":"retained-id"})
        .as_object()
        .unwrap()
        .clone();
    let after = json!({"access_token":"after"}).as_object().unwrap().clone();
    let older = CredentialRefreshPatch::new(&before, &after, true, false);
    let effective = older.apply(&before).0;
    let mut newest = after.clone();
    newest.insert("access_token".into(), json!("newest"));
    let chained =
        CredentialRefreshPatch::new(&effective, &newest, true, false).with_pending(Some(&older));
    let (saved, applied) = chained.apply(&before);
    assert_eq!(applied.outer, Some(true));
    assert_eq!(saved["access_token"], "newest");
    assert_eq!(saved["id_token"], "retained-id");
}

#[test]
fn sparse_chain_keeps_explicit_null_identity_after_live_removal() {
    for siwc in [false, true] {
        let wrap = |partition: Value| {
            if siwc {
                json!({"siwc": partition}).as_object().unwrap().clone()
            } else {
                partition.as_object().unwrap().clone()
            }
        };
        let before = wrap(json!({"access_token":"A0", "id_token":"I0"}));
        let first = wrap(json!({"access_token":"A1", "id_token":null}));
        let pending = CredentialRefreshPatch::new(&before, &first, !siwc, siwc);
        // The first save failed, then an unsaved edit removed the explicit null.
        let live = wrap(json!({"access_token":"A1"}));
        assert!(pending.matches_after(&live));
        let second = wrap(json!({"access_token":"A2"}));
        let chained =
            CredentialRefreshPatch::new(&live, &second, !siwc, siwc).with_pending(Some(&pending));
        let expected = wrap(json!({"access_token":"A2", "id_token":null}));
        let (saved, applied) = chained.apply(&before);
        assert_eq!(if siwc { applied.siwc } else { applied.outer }, Some(true));
        assert_eq!(saved, expected);
    }
}

#[test]
fn chained_expiry_return_to_original_is_idempotent() {
    for siwc in [false, true] {
        let wrap = |partition: Value| {
            if siwc {
                json!({"siwc": partition}).as_object().unwrap().clone()
            } else {
                partition.as_object().unwrap().clone()
            }
        };
        let before = wrap(json!({"access_token":"A0", "id_token":"I", "expired":"E0"}));
        let first = wrap(json!({"access_token":"A1", "expired":"E1"}));
        let pending = CredentialRefreshPatch::new(&before, &first, !siwc, siwc);
        let intermediate = pending.apply(&before).0;
        let second = wrap(json!({"access_token":"A1", "expired":"E0"}));
        let chained = CredentialRefreshPatch::new(&intermediate, &second, !siwc, siwc)
            .with_pending(Some(&pending));
        let disk = wrap(json!({"access_token":"A0", "id_token":"I", "expired":"EX"}));
        let expected = wrap(json!({"access_token":"A1", "id_token":"I", "expired":"E0"}));
        let saved = chained.apply(&disk).0;
        let replayed = chained.apply(&saved).0;
        assert_eq!(
            replayed, saved,
            "replaying the chain must not change expiry"
        );
        assert_eq!(saved, expected);
    }
}

#[test]
fn chained_settings_return_to_original_overwrites_disk_edit() {
    for siwc in [false, true] {
        let key = if siwc { "scope" } else { "plan_type" };
        let wrap = |partition: Value| {
            if siwc {
                json!({"siwc": partition}).as_object().unwrap().clone()
            } else {
                partition.as_object().unwrap().clone()
            }
        };
        let before = wrap(json!({"access_token":"A0", key:"P0"}));
        let first = wrap(json!({"access_token":"A1", key:"P0"}));
        let pending = CredentialRefreshPatch::new(&before, &first, !siwc, siwc);
        let live = wrap(json!({"access_token":"A1", key:"P1"}));
        let second = wrap(json!({"access_token":"A2", key:"P0"}));
        let chained =
            CredentialRefreshPatch::new(&live, &second, !siwc, siwc).with_pending(Some(&pending));
        let disk = wrap(json!({"access_token":"A0", key:"PX"}));
        assert_eq!(chained.apply(&disk).0, second);
    }
}

#[test]
fn folded_baseline_histories_match_exact_applications() {
    // Fixed xorshift seed; the reference is fix3's exact-map recorder.
    fn exact_record(set: &mut Vec<Map<String, Value>>, map: &Map<String, Value>) {
        if !set.iter().any(|known| known == map) {
            set.push(map.clone());
        }
    }
    fn next(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }
    fn compare(
        before: &Map<String, Value>,
        after: &Map<String, Value>,
        variants: &[Map<String, Value>],
        targets: &[Map<String, Value>],
        keys: &[&str],
        disk: Option<&Map<String, Value>>,
        intermediate: &[Map<String, Value>],
    ) {
        let mut exact = PartitionPatch::new(before, disk, after, keys);
        let mut folded = exact.clone();
        for variant in variants {
            exact_record(&mut exact.accepted_baselines, variant);
            record_durable_baseline(&mut folded.accepted_baselines, variant);
        }
        exact.intermediate = intermediate.to_vec();
        folded.intermediate = intermediate.to_vec();
        for target in targets {
            let mut a = target.clone();
            let mut b = target.clone();
            let applied_a = exact.apply(&mut a);
            let applied_b = folded.apply(&mut b);
            assert!(
                a == b && applied_a == applied_b,
                "folded and exact histories produce identical applications"
            );
            assert!(
                exact.matches_after(target) == folded.matches_after(target),
                "folded and exact histories recognize identical after identities"
            );
        }
    }
    let mut seed = 0x6f70_662d_666f_6c64u64;
    for keys in [OUTER_KEYS, SIWC_KEYS] {
        // Guaranteed conflict: preserving either side instead of agreement is wrong.
        let before = json!({"access_token":"A", "refresh_token":"R", "expired":"base"})
            .as_object()
            .unwrap()
            .clone();
        let mut variant = before.clone();
        variant.insert("expired".into(), json!("other"));
        let mut after = before.clone();
        after.insert("access_token".into(), json!("B"));
        let mut target = before.clone();
        target.insert("expired".into(), json!("foreign"));
        compare(
            &before,
            &after,
            &[before.clone(), variant],
            &[target],
            keys,
            None,
            &[],
        );
        for round in 0usize..128 {
            let mut variants = Vec::new();
            for group in 0..4 {
                for _ in 0..6 {
                    let mut map = Map::new();
                    for key in keys {
                        if IDENTITY_KEYS.contains(key) {
                            if *key == "id_token" {
                                if next(&mut seed) & 1 == 0 {
                                    map.insert((*key).into(), Value::Null);
                                }
                            } else {
                                map.insert((*key).into(), json!(format!("{key}-{group}")));
                            }
                        } else {
                            match next(&mut seed) % 5 {
                                0 => {}
                                1 => {
                                    map.insert((*key).into(), Value::Null);
                                }
                                value => {
                                    map.insert((*key).into(), json!(value));
                                }
                            }
                        }
                    }
                    variants.push(map);
                }
            }
            let before = variants[(next(&mut seed) as usize) % variants.len()].clone();
            let mut after = variants[(next(&mut seed) as usize) % variants.len()].clone();
            after.retain(|_, _| !next(&mut seed).is_multiple_of(3));
            let mut targets = variants.clone();
            for map in &mut targets {
                for key in keys.iter().filter(|key| !IDENTITY_KEYS.contains(key)) {
                    if next(&mut seed) & 1 == 0 {
                        map.insert((*key).into(), json!("target-edit"));
                    }
                }
            }
            let mut foreign = before.clone();
            foreign.insert("access_token".into(), json!("unmatched"));
            targets.push(foreign);
            targets.push(after.clone());
            let disk = (round.is_multiple_of(2)).then_some(&variants[0]);
            let intermediate = if round.is_multiple_of(3) {
                &variants[6..9]
            } else {
                &[]
            };
            compare(
                &before,
                &after,
                &variants,
                &targets,
                keys,
                disk,
                intermediate,
            );
        }
    }
}

#[test]
fn pending_composition_excludes_its_own_operation() {
    for outer in [true, false] {
        let before = fixture();
        let mut middle = before.clone();
        rotate(&mut middle, outer, !outer);
        let older = CredentialRefreshPatch::new(&before.metadata, &middle.metadata, outer, !outer);
        let mut after = middle.clone();
        let map = if outer {
            &mut after.metadata
        } else {
            after
                .metadata
                .get_mut("siwc")
                .unwrap()
                .as_object_mut()
                .unwrap()
        };
        map.insert("access_token".into(), json!("self-final-access"));
        map.insert("refresh_token".into(), json!("self-final-refresh"));
        let newer = CredentialRefreshPatch::new(&middle.metadata, &after.metadata, outer, !outer);
        let expected = newer.with_pending_operations(std::slice::from_ref(&older));
        let duplicated = newer.with_pending_operations(&[older, newer.clone()]);
        assert!(
            duplicated == expected
                && duplicated.apply(&before.metadata) == expected.apply(&before.metadata),
            "pending composition with an older operation ignores its own operation"
        );
        assert!(
            newer.with_pending_operations(std::slice::from_ref(&newer)) == newer,
            "pending composition with only its own operation is unchanged"
        );
    }
}

#[tokio::test]
async fn strict_bind_check_and_missing_target_never_restore_a_snapshot() {
    use crate::auth::credential_lock::acquire_credential_lock;
    let dir = scratch();
    let store = FileStore::new(dir.path());
    let auth = fixture();
    store.save(&auth).unwrap();
    let path = dir.path().join("codex.json");
    let before = std::fs::read(&path).unwrap();
    let guard = acquire_credential_lock(&path).await.unwrap();
    let result = store.bind_siwc_locked(&auth, Map::new(), &guard, &|_, _| {
        Err(std::io::Error::other("refused test identity"))
    });
    assert!(result.is_err());
    assert!(std::fs::read(&path).unwrap() == before);
    std::fs::remove_file(&path).unwrap();
    assert!(
        store
            .bind_siwc_locked(&auth, Map::new(), &guard, &|_, _| Ok(()))
            .is_err()
    );
    assert!(!path.exists());
}

/// B-U1: the partition helpers of the leased refresh.
#[test]
fn partition_helpers_report_requested_rotated_and_landed() {
    let before = fixture();
    let mut after = before.clone();
    rotate(&mut after, true, true);

    // requested(): outer only, SIWC only, both.
    let outer_only = CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, false);
    let siwc_only = CredentialRefreshPatch::new(&before.metadata, &after.metadata, false, true);
    let both = CredentialRefreshPatch::new(&before.metadata, &after.metadata, true, true);
    let none = CredentialRefreshPatch::new(&before.metadata, &after.metadata, false, false);
    let outer = CodexPartitions {
        outer: true,
        siwc: false,
    };
    let siwc = CodexPartitions {
        outer: false,
        siwc: true,
    };
    let all = CodexPartitions {
        outer: true,
        siwc: true,
    };
    assert_eq!(outer_only.requested(), outer);
    assert_eq!(siwc_only.requested(), siwc);
    assert_eq!(both.requested(), all);
    assert!(outer.any() && siwc.any() && all.any());
    assert!(!none.requested().any());
    assert!(!CodexPartitions::default().any());

    // refresh_patch(): Some only for the Refresh intent.
    let bind = CodexPersistIntent::SiwcBind(Map::new());
    for intent in [
        CodexPersistIntent::Configuration,
        CodexPersistIntent::OuterLogin,
        bind,
    ] {
        assert!(intent.refresh_patch().is_none());
    }
    let refresh = CodexPersistIntent::Refresh(both.clone());
    assert!(
        refresh
            .refresh_patch()
            .is_some_and(|p| p.same_operation(&both))
    );

    // disk_rotated_partitions(): equal, outer differs, SIWC differs. A disk
    // equal to the live predecessor is reported rotated: this function is
    // identity inequality only, and the manager's overlays (4.2) decide
    // whether the predecessor is protected (B-2 proves that).
    let mut outer_rotated = before.clone();
    rotate(&mut outer_rotated, true, false);
    let mut siwc_rotated = before.clone();
    rotate(&mut siwc_rotated, false, true);
    assert_eq!(
        disk_rotated_partitions(&before.metadata, &before.metadata),
        CodexPartitions::default()
    );
    assert_eq!(
        disk_rotated_partitions(&before.metadata, &outer_rotated.metadata),
        outer
    );
    assert_eq!(
        disk_rotated_partitions(&before.metadata, &siwc_rotated.metadata),
        siwc
    );
    assert_eq!(
        disk_rotated_partitions(&after.metadata, &before.metadata),
        all
    );

    // landed_in(): per requested partition, the disk shows the patch's result.
    assert_eq!(both.landed_in(&after.metadata), all);
    assert_eq!(outer_only.landed_in(&after.metadata), outer);
    assert_eq!(siwc_only.landed_in(&after.metadata), siwc);
    assert_eq!(both.landed_in(&outer_rotated.metadata), outer);
    assert_eq!(both.landed_in(&siwc_rotated.metadata), siwc);
    assert!(!both.landed_in(&before.metadata).any());
    assert!(!none.landed_in(&after.metadata).any());
}
