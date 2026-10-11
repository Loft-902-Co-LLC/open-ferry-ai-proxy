//! Not upstream's: immutable staging and journal-aware updater paths.

use std::fs;
use std::sync::atomic::Ordering;

use open_ferry_core::config::SelfUpdateMode;

use super::support::*;
use crate::pins::PinStore;
use crate::release::sha256_hex;
use crate::staging::{StageExclusiveError, stage_exclusive};
use crate::updater::UpdateError;

fn journal(f: &Fixture, version: &str) {
    let artifact = serde_json::json!({"version":"0.1.0", "sha256":sha256_hex(&good_binary("0.1.0")), "size":16});
    let json = serde_json::json!({
        "format":1, "seq":1, "active":artifact, "confirmed":artifact,
        "previous":null, "installed":{"version":"0.1.0", "sha256":sha256_hex(&good_binary("0.1.0"))},
        "install_pending":null, "switch":null, "outcomes":[],
        "workers":[{"pid":123,"started":1,"version":version,"role":"draining"}]
    });
    fs::write(
        f.data.root().join("switch-journal.json"),
        serde_json::to_vec(&json).unwrap(),
    )
    .unwrap();
}

// Not upstream's: test 19, a draining artifact cannot be replaced or pruned.
#[tokio::test]
async fn draining_version_is_immutable() {
    let f = Fixture::new().await;
    let version = "0.2.0";
    let dir = f.data.version_dir(version);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("open-ferry");
    fs::write(&path, b"older-build").unwrap();
    journal(&f, version);
    fs::create_dir_all(f.data.version_dir("0.3.0")).unwrap();
    f.release(version);
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(error, UpdateError::VersionInUse(_)), "{error}");
    assert_eq!(fs::read(&path).unwrap(), b"older-build");
    let _lock = f.updater.lock().unwrap();
    f.updater.prune(&f.updater.state()).unwrap();
    assert!(path.is_file());
}

// Not upstream's: the CLI's prune keeps every worker the journal lists (it
// cannot tell which are alive), exactly as before the supervisor's prune
// existed; the supervisor's prune, which can, drops the version of a worker
// that is gone and keeps it while the worker lives.
#[tokio::test]
async fn the_cli_prune_keeps_a_listed_worker_and_the_supervised_prune_asks_if_it_lives() {
    use crate::updater::supervised_prune;
    let f = Fixture::new().await;
    journal(&f, "0.2.0");
    for version in ["0.2.0", "0.3.0", "0.4.0"] {
        fs::create_dir_all(f.data.version_dir(version)).unwrap();
        fs::write(f.data.binary(version, "open-ferry"), good_binary(version)).unwrap();
    }
    {
        let _lock = f.updater.lock().unwrap();
        f.updater.prune(&f.updater.state()).unwrap();
    }
    assert!(
        f.data.binary("0.2.0", "open-ferry").is_file(),
        "the CLI keeps it"
    );
    assert!(
        f.data.binary("0.4.0", "open-ferry").is_file(),
        "the newest other"
    );
    assert!(!f.data.version_dir("0.3.0").exists());
    let now = chrono::Utc::now();
    let alive = supervised_prune(&f.data, "open-ferry", [], None, now, |pid, started| {
        pid == 123 && started == 1
    })
    .unwrap();
    assert!(alive.removed.is_empty(), "{alive:?}");
    assert!(f.data.binary("0.2.0", "open-ferry").is_file());
    let gone = supervised_prune(&f.data, "open-ferry", [], None, now, |_, _| false).unwrap();
    assert_eq!(gone.removed, ["0.2.0"]);
    assert!(!f.data.version_dir("0.2.0").exists());
    assert!(f.data.binary("0.4.0", "open-ferry").is_file());
}

// Not upstream's: test 19, a changed pinned stage is refused without deletion.
#[tokio::test]
async fn changed_pinned_stage_is_not_deleted() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let _lock = f.updater.lock().unwrap();
    let state = f.updater.state();
    let pin = f
        .updater
        .pin_staged("0.2.0", state.staged_sha256.as_deref().unwrap())
        .unwrap();
    let path = f.data.binary("0.2.0", "open-ferry");
    fs::write(&path, b"changed").unwrap();
    let error = f
        .updater
        .switch_to_staged(&mut f.updater.state())
        .unwrap_err();
    assert!(matches!(error, UpdateError::StagedChanged { .. }));
    assert_eq!(fs::read(path).unwrap(), b"changed");
    assert_eq!(
        PinStore::new(&f.data.versions()).consume(&pin.id).unwrap(),
        Some(pin)
    );
}

// Not upstream's: test 20, newer formats permit checks but no disk mutation.
#[tokio::test]
async fn newer_format_refuses_mutations_but_check_works() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    fs::write(
        f.data.root().join("switch-journal.json"),
        br#"{"format":2,"seq":1}"#,
    )
    .unwrap();
    let report = f.updater.check(SelfUpdateMode::Notify).await.unwrap();
    assert!(matches!(report.report, crate::Report::Available { .. }));
    // Existing matching stage reuse is read-only; a different release needs mutation.
    f.release("0.3.0");
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(error.to_string().contains("newer"));
    let _lock = f.updater.lock().unwrap();
    for error in [
        f.updater
            .switch_to_staged(&mut f.updater.state())
            .unwrap_err(),
        f.updater
            .rollback(&mut f.updater.state())
            .await
            .unwrap_err(),
        f.updater.prune(&f.updater.state()).unwrap_err(),
    ] {
        assert!(matches!(error, UpdateError::Journal(_)));
        assert!(error.to_string().contains("newer"));
    }
    assert!(matches!(
        stage_exclusive(
            &f.data,
            &f.installed,
            "open-ferry",
            f.runner.as_ref(),
            f.updater.limits.version_timeout
        )
        .await,
        Err(StageExclusiveError::Journal(_))
    ));
    assert!(f.data.binary("0.2.0", "open-ferry").is_file());
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
}

// Not upstream's: O4 applies prune's full retention policy to destructive
// staging, including newest-other and state.previous from freshly loaded state.
#[tokio::test]
async fn stage_refuses_to_replace_newest_other_or_state_previous() {
    for supervised in [false, true] {
        let f = Fixture::new().await;
        let version = "0.2.0";
        let path = f.data.binary(version, "open-ferry");
        fs::create_dir_all(f.data.version_dir(version)).unwrap();
        fs::write(&path, b"existing-build").unwrap();
        if supervised {
            journal(&f, "0.1.0");
        } else {
            f.updater
                .mutate_state(|state| state.previous = Some(version.into()))
                .unwrap();
        }
        f.release(version);
        let _lock = f.updater.lock().unwrap();
        // Deliberately stale: the fresh state, not this snapshot, protects previous.
        let mut state = crate::State::default();
        let found = f.updater.latest(&state).await.unwrap();
        assert!(matches!(f.updater.stage(&found, &mut state).await,
            Err(UpdateError::VersionInUse(v)) if v == version));
        assert_eq!(fs::read(&path).unwrap(), b"existing-build");
        f.updater.prune(&state).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"existing-build");
    }
}

// Not upstream's: the staged pointer cannot wedge its own same-version
// replacement, but independent previous and live-pin protections still can.
#[tokio::test]
async fn restaging_changed_bytes_replaces_only_an_unprotected_stage() {
    for protection in ["none", "previous", "pin"] {
        let f = Fixture::new().await;
        f.release("0.2.0");
        f.updater.check(SelfUpdateMode::Auto).await.unwrap();
        let old = good_binary("0.2.0");
        let path = f.data.binary("0.2.0", "open-ferry");
        if protection == "previous" {
            f.updater
                .mutate_state(|state| state.previous = Some("0.2.0".into()))
                .unwrap();
        }
        let _lock = f.updater.lock().unwrap();
        if protection == "pin" {
            f.updater.pin_staged("0.2.0", &sha256_hex(&old)).unwrap();
        }
        fs::write(&path, b"changed staged bytes").unwrap();
        let mut replacement = old.clone();
        replacement.push(b'\n');
        f.release_binary("0.2.0", &replacement);
        let mut state = f.updater.state();
        let found = f.updater.latest(&state).await.unwrap();
        let result = f.updater.stage(&found, &mut state).await;
        if protection == "none" {
            result.unwrap();
            assert_eq!(fs::read(&path).unwrap(), replacement);
            assert_eq!(
                f.updater.state().staged_sha256,
                Some(sha256_hex(&replacement))
            );
        } else {
            assert!(matches!(result, Err(UpdateError::VersionInUse(_))));
            assert_eq!(fs::read(&path).unwrap(), b"changed staged bytes");
            assert!(f.updater.state().staged.is_none());
        }
    }
}

// Not upstream's: read-only staged reuse neither downloads an archive nor
// consults malformed pins; fail-closed retention applies only to mutations.
#[tokio::test]
async fn matching_staged_bytes_are_reused_despite_malformed_pins() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let pins = f.data.versions().join(".pins");
    fs::create_dir_all(&pins).unwrap();
    fs::write(pins.join("malformed.json"), b"not json").unwrap();
    let _lock = f.updater.lock().unwrap();
    let mut state = f.updater.state();
    let found = f.updater.latest(&state).await.unwrap();
    // Make any download fail, so success proves the early return.
    f.server.reply(&f.archive_path("0.2.0"), Reply::Status(500));
    let path = f.updater.stage(&found, &mut state).await.unwrap();
    assert_eq!(fs::read(path).unwrap(), good_binary("0.2.0"));
    assert!(matches!(
        f.updater.prune(&state),
        Err(UpdateError::Journal(_))
    ));
}

// Not upstream's: U2-P5 preflight refusal preserves every leftover, even a
// removable staging directory or a prev file matching the installed bytes.
#[tokio::test]
async fn refused_preflight_leaves_stale_artifacts_untouched() {
    for newer in [false, true] {
        let f = Fixture::new().await;
        f.release("0.2.0");
        let stale = f.data.versions().join(".staging.1");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("open-ferry"), b"stale binary").unwrap();
        let prev = f.installed.with_file_name("open-ferry.prev.1");
        fs::write(&prev, good_binary("0.1.0")).unwrap();
        if newer {
            fs::write(
                f.data.root().join("switch-journal.json"),
                br#"{"format":2,"seq":1}"#,
            )
            .unwrap();
        } else {
            let pins = f.data.versions().join(".pins");
            fs::create_dir_all(&pins).unwrap();
            fs::write(pins.join("bad.json"), b"bad pin").unwrap();
        }
        let _lock = f.updater.lock().unwrap();
        let mut state = f.updater.state();
        let found = f.updater.latest(&state).await.unwrap();
        assert!(matches!(
            f.updater.stage(&found, &mut state).await,
            Err(UpdateError::Journal(_))
        ));
        assert!(matches!(
            f.updater.prune(&state),
            Err(UpdateError::Journal(_))
        ));
        assert_eq!(fs::read(stale.join("open-ferry")).unwrap(), b"stale binary");
        assert_eq!(fs::read(prev).unwrap(), good_binary("0.1.0"));
    }
}

// Not upstream's: a stale directory that cannot be removed never blocks a
// new stage or prune; each run has its own staging name.
#[cfg(feature = "blue-green-test")]
#[tokio::test]
async fn a_failed_stale_directory_removal_still_allows_staging_and_pruning() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let stale = f.data.versions().join(".staging.1");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("open-ferry"), b"stale binary").unwrap();
    let seams = f.data.root().join(".blue-green-test");
    fs::create_dir_all(&seams).unwrap();
    fs::write(seams.join("staging.cleanup.fail"), b"1").unwrap();
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(
        fs::read(f.data.binary("0.2.0", "open-ferry")).unwrap(),
        good_binary("0.2.0")
    );
    assert_eq!(fs::read(stale.join("open-ferry")).unwrap(), b"stale binary");
    let _lock = f.updater.lock().unwrap();
    f.updater.prune(&f.updater.state()).unwrap();
    assert_eq!(fs::read(stale.join("open-ferry")).unwrap(), b"stale binary");
}

// Not upstream's: U2-P5 integration verifies a kept copy against a recorded
// state hash before deleting matching prev bytes, while unknown bytes survive.
#[tokio::test]
async fn prune_removes_only_stale_artifacts_with_safe_proofs() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let stale = f.data.versions().join(".staging.1");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("open-ferry"), b"stale binary").unwrap();
    let current = crate::staging::StagingDir::create(&f.data.versions()).unwrap();
    let matching = f.installed.with_file_name("open-ferry.prev.1");
    let unknown = f.installed.with_file_name("open-ferry.prev.2");
    fs::write(&matching, good_binary("0.2.0")).unwrap();
    fs::write(&unknown, b"only copy of unknown binary").unwrap();
    let _lock = f.updater.lock().unwrap();
    f.updater.prune(&f.updater.state()).unwrap();
    assert!(!stale.exists());
    assert!(current.0.is_dir());
    assert!(!matching.exists());
    assert_eq!(fs::read(unknown).unwrap(), b"only copy of unknown binary");
}

// Not upstream's: U3-2 keeps the last prev copy when its only matching
// recorded history version is outside the same prune's retention set.
#[tokio::test]
async fn prune_keeps_a_prev_matching_only_a_disposable_history_version() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let version = "0.0.5";
    let bytes = good_binary(version);
    let binary = f.data.binary(version, "open-ferry");
    fs::create_dir_all(f.data.version_dir(version)).unwrap();
    fs::write(&binary, &bytes).unwrap();
    f.updater
        .mutate_state(|state| {
            state.history.push(crate::state::SwitchRecord {
                to: version.into(),
                sha256: sha256_hex(&bytes),
                ..crate::state::SwitchRecord::default()
            });
        })
        .unwrap();
    let prev = f.installed.with_file_name("open-ferry.prev.1");
    fs::write(&prev, &bytes).unwrap();
    let _lock = f.updater.lock().unwrap();
    f.updater.prune(&f.updater.state()).unwrap();
    assert!(!binary.exists());
    assert_eq!(fs::read(prev).unwrap(), bytes);
}

// Not upstream's: the normal update staging path recovers a corrupt stage,
// rather than requiring a separate switch attempt to retire its pointer.
#[tokio::test]
async fn stage_replaces_its_corrupt_unprotected_stage() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let path = f.data.binary("0.2.0", "open-ferry");
    fs::write(&path, b"corrupt").unwrap();
    let _lock = f.updater.lock().unwrap();
    let mut state = f.updater.state();
    let found = f.updater.latest(&state).await.unwrap();
    f.updater.stage(&found, &mut state).await.unwrap();
    assert_eq!(fs::read(path).unwrap(), good_binary("0.2.0"));
    assert_eq!(f.updater.state().staged.as_deref(), Some("0.2.0"));
}

// Not upstream's: newest-other is an artifact, never a positively empty
// publication claim. Both staging paths must repair a journal-era empty claim.
#[tokio::test]
async fn a_journal_does_not_retain_an_empty_publication_claim() {
    for exclusive in [false, true] {
        let f = Fixture::new().await;
        journal(&f, "0.1.0");
        let version = if exclusive { "0.1.0" } else { "0.2.0" };
        // The installed fallback must not be named by the journal for this
        // test of newest-other alone.
        if exclusive {
            let file = f.data.root().join("switch-journal.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
            for field in ["active", "confirmed", "installed"] {
                value[field]["version"] = "0.0.1".into();
            }
            value["workers"] = serde_json::json!([]);
            fs::write(file, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        fs::create_dir_all(f.data.version_dir(version)).unwrap();
        let _lock = f.updater.lock().unwrap();
        if exclusive {
            stage_exclusive(
                &f.data,
                &f.installed,
                "open-ferry",
                f.runner.as_ref(),
                f.updater.limits.version_timeout,
            )
            .await
            .unwrap();
        } else {
            f.release(version);
            let mut state = f.updater.state();
            let found = f.updater.latest(&state).await.unwrap();
            f.updater.stage(&found, &mut state).await.unwrap();
        }
        assert_eq!(
            fs::read(f.data.binary(version, "open-ferry")).unwrap(),
            good_binary(version)
        );
    }
}

// Not upstream's: clearing a corrupt staging reference must not release
// the same version's independent rollback protection.
#[tokio::test]
async fn changed_staged_previous_is_not_deleted() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    f.updater
        .mutate_state(|state| state.previous = Some("0.2.0".into()))
        .unwrap();
    let path = f.data.binary("0.2.0", "open-ferry");
    fs::write(&path, b"changed").unwrap();
    let _lock = f.updater.lock().unwrap();
    assert!(matches!(
        f.updater.switch_to_staged(&mut f.updater.state()),
        Err(UpdateError::StagedChanged { .. })
    ));
    assert_eq!(fs::read(path).unwrap(), b"changed");
    assert!(f.updater.state().staged.is_none());
    assert_eq!(f.updater.state().previous.as_deref(), Some("0.2.0"));
}

// Not upstream's: Opus2 keeps legacy outside same-version reinstall behavior
// without a journal, replacing the mismatched kept copy atomically.
#[tokio::test]
async fn no_journal_switch_replaces_a_mismatched_kept_source() {
    let f = Fixture::new().await;
    let kept = f.data.binary("0.1.0", "open-ferry");
    fs::create_dir_all(f.data.version_dir("0.1.0")).unwrap();
    fs::write(&kept, b"different same-version build").unwrap();
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let _lock = f.updater.lock().unwrap();
    f.updater.switch_to_staged(&mut f.updater.state()).unwrap();
    assert_eq!(fs::read(kept).unwrap(), good_binary("0.1.0"));
    assert_eq!(f.installed_text(), "open-ferry 0.2.0");
}

// Not upstream's: U3-6 this rollback exercises both from == to and the
// previous == from protection after metadata falls back to running. The
// separate mismatched-source test below isolates previous with from != to.
#[tokio::test]
async fn a_same_version_rollback_with_a_protected_previous_is_refused() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let _lock = f.updater.lock().unwrap();
    f.updater.switch_to_staged(&mut f.updater.state()).unwrap();
    fs::write(&f.installed, b"open-ferry 0.2.0\n").unwrap();
    let kept = f.data.binary("0.1.0", "open-ferry");
    let before = fs::read(&kept).unwrap();
    let mut state = f.updater.state();
    assert_eq!(f.updater.installed_version(&state), "0.1.0");
    assert!(matches!(f.updater.rollback(&mut state).await,
        Err(UpdateError::VersionInUse(version)) if version == "0.1.0"));
    assert_eq!(fs::read(kept).unwrap(), before);
    assert_eq!(f.installed_text(), "open-ferry 0.2.0\n");
}

// Not upstream's: U2-P1 previous and live pins independently prohibit the
// legacy mismatched-copy replacement before any binary is written.
#[tokio::test]
async fn a_protected_mismatched_kept_source_is_refused_without_a_journal() {
    for pin in [false, true] {
        let f = Fixture::new().await;
        let kept = f.data.binary("0.1.0", "open-ferry");
        fs::create_dir_all(f.data.version_dir("0.1.0")).unwrap();
        fs::write(&kept, b"only good protected copy").unwrap();
        if !pin {
            f.updater
                .mutate_state(|state| state.previous = Some("0.1.0".into()))
                .unwrap();
        }
        f.release("0.2.0");
        f.updater.check(SelfUpdateMode::Auto).await.unwrap();
        let _lock = f.updater.lock().unwrap();
        if pin {
            PinStore::new(&f.data.versions())
                .create(
                    "0.1.0",
                    &sha256_hex(b"only good protected copy"),
                    chrono::Utc::now(),
                )
                .unwrap();
        }
        assert!(matches!(f.updater.switch_to_staged(&mut f.updater.state()),
            Err(UpdateError::VersionInUse(version)) if version == "0.1.0"));
        assert_eq!(fs::read(kept).unwrap(), b"only good protected copy");
        assert_eq!(f.installed_text(), "open-ferry 0.1.0");
    }
}

// Not upstream's: U2-P4 only post-commit state reporting accepts a landed
// rename whose final directory acknowledgement failed.
#[cfg(feature = "blue-green-test")]
#[tokio::test]
async fn a_landed_state_save_after_a_switch_reports_success() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let seam = f.data.root().join(".blue-green-test");
    fs::create_dir_all(&seam).unwrap();
    fs::write(seam.join("atomic.landed_update-state.json.fail"), b"").unwrap();
    let _lock = f.updater.lock().unwrap();
    let record = f.updater.switch_to_staged(&mut f.updater.state()).unwrap();
    let durable = f.updater.state();
    assert_eq!(durable.last_switch.as_ref(), Some(&record));
    assert_eq!(durable.previous.as_deref(), Some("0.1.0"));
    assert_eq!(f.installed_text(), "open-ferry 0.2.0");
    drop(_lock);
    assert!(
        f.updater
            .mutate_state_named("ordinary", |state| state.latest = Some("0.3.0".into()))
            .is_err()
    );
}

// Not upstream's: losing the installed pathname after assessment must reach
// the aside restore, not fail prematurely in the updater's hash precheck.
#[tokio::test]
async fn a_missing_installed_file_reaches_the_aside_restore() {
    use crate::install::System;
    use crate::switch::{
        RealFileOps, Switch, SwitchError, SwitchOutcome, SwitchPlan, commit_with, prepare_with,
    };
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, atomic::AtomicUsize};
    struct Disappear {
        installed: PathBuf,
        calls: AtomicUsize,
    }
    impl System for Disappear {
        fn current_exe(&self) -> std::io::Result<PathBuf> {
            Ok(self.installed.clone())
        }
        fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
            let canonical = fs::canonicalize(path)?;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                fs::rename(
                    &self.installed,
                    self.installed.with_file_name("open-ferry.old"),
                )?;
            }
            Ok(canonical)
        }
        fn in_container(&self) -> bool {
            false
        }
        fn dir_writable(&self, _: &Path) -> bool {
            true
        }
    }
    struct Aside;
    impl Switch for Aside {
        fn switch(&self, plan: &SwitchPlan) -> Result<SwitchOutcome, SwitchError> {
            assert!(plan.expected_installed_sha256.is_none());
            let prepared = prepare_with(&RealFileOps, plan, true)?;
            commit_with(&RealFileOps, &prepared)
        }
    }
    let mut f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    f.updater.system = Arc::new(Disappear {
        installed: f.installed.clone(),
        calls: AtomicUsize::new(0),
    });
    f.updater.switch = Arc::new(Aside);
    let _lock = f.updater.lock().unwrap();
    f.updater.switch_to_staged(&mut f.updater.state()).unwrap();
    assert_eq!(f.installed_text(), "open-ferry 0.2.0");
    assert_eq!(
        fs::read(f.data.binary("0.1.0", "open-ferry")).unwrap(),
        good_binary("0.1.0")
    );
}

// Not upstream's: P4 reporting follows a landed replacement even when the
// final directory-sync acknowledgement fails.
#[tokio::test]
async fn a_landed_switch_with_failed_sync_records_previous_and_version() {
    use crate::switch::{
        FileOps, RealFileOps, ReplaceOnDisk, Switch, SwitchError, SwitchOutcome, SwitchPlan,
        commit_with,
    };
    struct FailSync;
    impl FileOps for FailSync {
        fn rename(&self, from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
            RealFileOps.rename(from, to)
        }
        fn exists(&self, path: &std::path::Path) -> bool {
            RealFileOps.exists(path)
        }
        fn sync_parent(&self, _: &std::path::Path) -> std::io::Result<()> {
            Err(std::io::Error::other("injected final sync failure"))
        }
    }
    struct Landed;
    impl Switch for Landed {
        fn switch(&self, plan: &SwitchPlan) -> Result<SwitchOutcome, SwitchError> {
            let prepared = ReplaceOnDisk.prepare(plan)?;
            commit_with(&FailSync, &prepared)
        }
    }
    let mut f = Fixture::new().await;
    f.updater.switch = std::sync::Arc::new(Landed);
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let _lock = f.updater.lock().unwrap();
    f.updater.switch_to_staged(&mut f.updater.state()).unwrap();
    let state = f.updater.state();
    assert_eq!(state.previous.as_deref(), Some("0.1.0"));
    assert_eq!(state.last_switch.as_ref().unwrap().to, "0.2.0");
    assert_eq!(f.updater.installed_version(&state), "0.2.0");
    assert_eq!(f.installed_text(), "open-ferry 0.2.0");
}

// Not upstream's: malformed pins make pruning fail closed.
#[tokio::test]
async fn malformed_pin_refuses_prune() {
    let f = Fixture::new().await;
    let pins = f.data.versions().join(".pins");
    fs::create_dir_all(&pins).unwrap();
    fs::write(pins.join("bad.json"), b"not-json").unwrap();
    let dir = f.data.version_dir("0.0.9");
    fs::create_dir_all(&dir).unwrap();
    let _lock = f.updater.lock().unwrap();
    assert!(f.updater.prune(&f.updater.state()).is_err());
    assert!(dir.exists());
}

// Not upstream's: first-start adoption hashes the copy and refuses conflicts.
#[tokio::test]
async fn adoption_reuses_only_identical_bytes() {
    let f = Fixture::new().await;
    let _lock = f.updater.lock().unwrap();
    let artifact = stage_exclusive(
        &f.data,
        &f.installed,
        "open-ferry",
        f.runner.as_ref(),
        f.updater.limits.version_timeout,
    )
    .await
    .unwrap();
    assert_eq!(artifact.sha256, sha256_hex(&good_binary("0.1.0")));
    let again = stage_exclusive(
        &f.data,
        &f.installed,
        "open-ferry",
        f.runner.as_ref(),
        f.updater.limits.version_timeout,
    )
    .await
    .unwrap();
    assert_eq!(artifact, again);
    assert_eq!(f.runner.runs.load(Ordering::SeqCst), 2);
    fs::write(f.data.binary("0.1.0", "open-ferry"), b"another build").unwrap();
    assert!(matches!(
        stage_exclusive(
            &f.data,
            &f.installed,
            "open-ferry",
            f.runner.as_ref(),
            f.updater.limits.version_timeout
        )
        .await,
        Err(StageExclusiveError::SwitchingDisabled { .. })
    ));
}

// Not upstream's: O2's real same-version staging conflict executes the typed
// installed-file fallback, never the rebuilt version-directory journal.
#[tokio::test]
async fn recovery_stage_conflict_runs_installed_without_publishing_a_journal() {
    use crate::journal::{Artifact, Installed, Journal, JournalStore};
    use crate::recovery::{Facts, InstallAction, PlanStatus, RunChoice, plan_recovery};
    let f = Fixture::new().await;
    let artifact = Artifact {
        version: "0.1.0".into(),
        sha256: sha256_hex(&good_binary("0.1.0")),
        size: good_binary("0.1.0").len() as u64,
    };
    let kept = f.data.binary("0.1.0", "open-ferry");
    fs::create_dir_all(f.data.version_dir("0.1.0")).unwrap();
    fs::write(&kept, b"conflicting same-version build").unwrap();
    let mut journal = Journal::first_start(
        Artifact {
            version: "0.2.0".into(),
            sha256: "b".repeat(64),
            size: 16,
        },
        Some(Installed::from(&artifact)),
    );
    let _lock = f.updater.lock().unwrap();
    let store = JournalStore::new(f.data.root());
    store.write(&mut journal, "initial").unwrap();
    let before = fs::read(store.path()).unwrap();
    let facts = Facts {
        now: chrono::Utc::now(),
        self_sha256: Some(artifact.sha256.clone()),
        self_version: artifact.version.clone(),
        installed_sha256: Some(artifact.sha256.clone()),
        installed_path: f.installed.clone(),
        installed_size: artifact.size,
        versions: std::collections::BTreeMap::from([(
            "0.1.0".into(),
            sha256_hex(b"conflicting same-version build"),
        )]),
    };
    let plan = plan_recovery(&journal, &facts);
    assert_eq!(plan.run, RunChoice::VerifiedInstalled(artifact.clone()));
    assert!(plan.journal_write.is_some());
    assert!(matches!(
        stage_exclusive(
            &f.data,
            &f.installed,
            "open-ferry",
            f.runner.as_ref(),
            f.updater.limits.version_timeout
        )
        .await,
        Err(StageExclusiveError::SwitchingDisabled { .. })
    ));
    let fallback = plan
        .on_stage_conflict(&f.data.version_dir("0.1.0"))
        .unwrap();
    assert!(
        matches!(&fallback.run, RunChoice::InstalledSwitchingDisabled { artifact: a, reason }
        if a == &artifact && reason.contains(&f.data.version_dir("0.1.0").display().to_string()))
    );
    assert!(matches!(fallback.status, PlanStatus::SwitchingDisabled(_)));
    assert!(fallback.journal_write.is_none());
    assert_eq!(fallback.install, InstallAction::None);
    assert_eq!(fs::read(store.path()).unwrap(), before);
    assert_eq!(fs::read(kept).unwrap(), b"conflicting same-version build");
}

// Not upstream's: a fresh mutation does not drop independently written fields.
#[tokio::test]
async fn mutate_state_loads_under_its_lock() {
    let f = Fixture::new().await;
    f.updater
        .mutate_state(|state| state.latest = Some("0.2.0".into()))
        .unwrap();
    f.updater
        .mutate_state(|state| state.supervisor_version = Some("0.1.0".into()))
        .unwrap();
    let state = f.updater.state();
    assert_eq!(state.latest.as_deref(), Some("0.2.0"));
    assert_eq!(state.supervisor_version.as_deref(), Some("0.1.0"));
}

// Not upstream's: each phase-2 journal write can fail without publishing its change.
#[cfg(feature = "blue-green-test")]
#[test]
fn each_journal_write_failure_retains_the_last_durable_snapshot() {
    use crate::journal::{Artifact, Journal, JournalLoad, JournalStore};
    for write in [
        "J1", "J2", "J3", "J4", "J5", "J6", "J7", "J8", "J-R1", "J-R2",
    ] {
        let root = tempfile::tempdir().unwrap();
        let store = JournalStore::new(root.path()).with_retry_wait(std::time::Duration::ZERO);
        let artifact = Artifact {
            version: "0.1.0".into(),
            sha256: "a".repeat(64),
            size: 1,
        };
        let mut journal = Journal::first_start(artifact, None);
        store.write(&mut journal, "initial").unwrap();
        let original = journal.clone();
        let bytes = fs::read(store.path()).unwrap();
        let seams = root.path().join(".blue-green-test");
        fs::create_dir(&seams).unwrap();
        let failure = seams.join(format!("journal.fail_{write}.fail"));
        fs::write(&failure, []).unwrap();
        journal.active.version = "0.2.0".into();
        assert!(store.write(&mut journal, write).is_err(), "{write}");
        assert_eq!(journal.seq, original.seq, "{write}");
        assert_eq!(fs::read(store.path()).unwrap(), bytes, "{write}");
        assert_eq!(store.load().unwrap(), JournalLoad::Loaded(original));
        fs::remove_file(failure).unwrap();
        store.write(&mut journal, write).unwrap();
        assert_eq!(store.load().unwrap(), JournalLoad::Loaded(journal));
    }
}

// Not upstream's: failed S1/S2/S3 writes preserve reporting and journal outcomes.
#[cfg(feature = "blue-green-test")]
#[test]
fn each_state_write_failure_preserves_report_and_pending_outcome() {
    use crate::journal::{Artifact, Journal, JournalLoad, JournalStore, Outcome};
    use crate::state::{State, SwitchRecord};
    for write in ["S1", "S2", "S3"] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("update-state.json");
        let mut state = State::default();
        state.save_named(&path, "initial").unwrap();
        let bytes = fs::read(&path).unwrap();
        let mut journal = Journal::first_start(
            Artifact {
                version: "0.1.0".into(),
                sha256: "a".repeat(64),
                size: 1,
            },
            None,
        );
        let record = SwitchRecord::started(
            "switch-1",
            "update",
            "0.1.0",
            "0.2.0",
            "2026-10-09T00:00:00Z",
        );
        journal.push_outcome(Outcome {
            record: record.clone(),
            mark_failed: None,
        });
        let store = JournalStore::new(root.path());
        store.write(&mut journal, "held-outcome").unwrap();
        let durable_journal = journal.clone();
        state.apply_outcome(&record, None);
        let seams = root.path().join(".blue-green-test");
        fs::create_dir(&seams).unwrap();
        let failure = seams.join(format!("state.fail_{write}.fail"));
        fs::write(&failure, []).unwrap();
        assert!(state.save_named(&path, write).is_err(), "{write}");
        assert_eq!(fs::read(&path).unwrap(), bytes, "{write}");
        assert!(State::load(&path).history.is_empty(), "{write}");
        assert_eq!(
            store.load().unwrap(),
            JournalLoad::Loaded(durable_journal.clone()),
            "{write}"
        );
        fs::remove_file(failure).unwrap();
        state.save_named(&path, write).unwrap();
        let saved = State::load(&path);
        assert_eq!(saved.history, vec![record.clone()], "{write}");
        assert_eq!(saved.last_switch, Some(record), "{write}");
        assert_eq!(
            store.load().unwrap(),
            JournalLoad::Loaded(durable_journal),
            "{write}"
        );
        assert!(journal.remove_outcome("switch-1"));
        store.write(&mut journal, "acknowledged-outcome").unwrap();
        assert_eq!(store.load().unwrap(), JournalLoad::Loaded(journal));
    }
}

// Not upstream's: a pin is the sole reason this older, no-longer-staged version survives.
#[tokio::test]
async fn prune_keeps_a_pinned_version_until_consumed() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let _lock = f.updater.lock().unwrap();
    let mut state = f.updater.state();
    let pin = f
        .updater
        .pin_staged("0.2.0", state.staged_sha256.as_deref().unwrap())
        .unwrap();
    state.staged = None;
    state.staged_sha256 = None;
    state.save(&f.data.state_file()).unwrap();
    fs::create_dir_all(f.data.version_dir("0.3.0")).unwrap();
    f.updater.prune(&state).unwrap();
    assert!(f.data.binary("0.2.0", "open-ferry").is_file());
    PinStore::new(&f.data.versions()).consume(&pin.id).unwrap();
    f.updater.prune(&state).unwrap();
    assert!(!f.data.version_dir("0.2.0").exists());
}

// Not upstream's: a journal-present unsupervised switch never overwrites its kept source.
#[tokio::test]
async fn journal_switch_reuses_identical_kept_source_and_refuses_conflicts() {
    for conflict in [false, true] {
        let f = Fixture::new().await;
        f.release("0.2.0");
        f.updater.check(SelfUpdateMode::Auto).await.unwrap();
        journal(&f, "0.1.0");
        let dir = f.data.version_dir("0.1.0");
        fs::create_dir_all(&dir).unwrap();
        let bytes = if conflict {
            b"other build".to_vec()
        } else {
            good_binary("0.1.0")
        };
        let kept = f.data.binary("0.1.0", "open-ferry");
        fs::write(&kept, &bytes).unwrap();
        let _lock = f.updater.lock().unwrap();
        let switched = f.updater.switch_to_staged(&mut f.updater.state());
        if conflict {
            assert!(matches!(switched, Err(UpdateError::VersionInUse(_))));
            assert_eq!(f.installed_text(), "open-ferry 0.1.0");
        } else {
            switched.unwrap();
            assert_eq!(f.installed_text(), "open-ferry 0.2.0");
        }
        assert_eq!(fs::read(kept).unwrap(), bytes);
    }
}

// Not upstream's: unreadable journals fail closed across mutation paths, not checks.
#[tokio::test]
async fn unreadable_journal_refuses_updater_mutations() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    fs::write(f.data.root().join("switch-journal.json"), b"not json").unwrap();
    f.updater.check(SelfUpdateMode::Notify).await.unwrap();
    f.release("0.3.0");
    assert!(matches!(
        f.updater.check(SelfUpdateMode::Auto).await,
        Err(UpdateError::Journal(_))
    ));
    let _lock = f.updater.lock().unwrap();
    assert!(matches!(
        f.updater.switch_to_staged(&mut f.updater.state()),
        Err(UpdateError::Journal(_))
    ));
    assert!(matches!(
        f.updater.prune(&f.updater.state()),
        Err(UpdateError::Journal(_))
    ));
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
}

// Not upstream's: a failed fresh state mutation neither commits nor leaks its lock.
#[cfg(feature = "blue-green-test")]
#[tokio::test]
async fn mutate_state_failure_releases_lock_without_losing_saved_fields() {
    let f = Fixture::new().await;
    f.updater
        .mutate_state(|state| state.latest = Some("0.2.0".into()))
        .unwrap();
    let seams = f.data.root().join(".blue-green-test");
    fs::create_dir(&seams).unwrap();
    let failure = seams.join("state.fail_S2.fail");
    fs::write(&failure, []).unwrap();
    assert!(
        f.updater
            .mutate_state_named("S2", |state| state.supervisor_version =
                Some("0.1.0".into()))
            .is_err()
    );
    assert_eq!(f.updater.state().latest.as_deref(), Some("0.2.0"));
    assert!(f.updater.state().supervisor_version.is_none());
    drop(f.updater.lock().unwrap());
    fs::remove_file(failure).unwrap();
    f.updater
        .mutate_state_named("S2", |state| {
            state.supervisor_version = Some("0.1.0".into())
        })
        .unwrap();
    assert_eq!(f.updater.state().latest.as_deref(), Some("0.2.0"));
    assert_eq!(
        f.updater.state().supervisor_version.as_deref(),
        Some("0.1.0")
    );
}

// Not upstream's: a failed --version check removes only its own staging, not a kept copy.
#[tokio::test]
async fn failed_check_leaves_an_in_use_version_directory_intact() {
    let f = Fixture::new().await;
    let version = "0.2.0";
    let bytes = b"#fail";
    let dir = f.data.version_dir(version);
    fs::create_dir_all(&dir).unwrap();
    let path = f.data.binary(version, "open-ferry");
    fs::write(&path, bytes).unwrap();
    journal(&f, version);
    f.release_binary(version, bytes);
    assert!(matches!(
        f.updater.check(SelfUpdateMode::Auto).await,
        Err(UpdateError::StagedBinaryFailed { .. })
    ));
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
    assert!(fs::read_dir(f.data.versions()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging.")
    }));
}

// Not upstream's: a refused probe must never reach the runner, and owns only
// the staging directory it removes on the error path.
#[tokio::test]
async fn checked_staging_refuses_before_the_version_probe() {
    let f = Fixture::new().await;
    let error = crate::staging::stage_exclusive_checked(
        &f.data,
        &f.installed,
        "open-ferry",
        f.runner.as_ref(),
        std::time::Duration::from_secs(2),
        &|path| Err(format!("{}: refused staging chain", path.display())),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, StageExclusiveError::Untrusted(_)));
    assert_eq!(f.runner.runs.load(Ordering::SeqCst), 0);
    assert!(fs::read_dir(f.data.versions()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging.")
    }));
}

// Not upstream's: 5b H3. The supervisor's fail-closed list of every version
// treats any listing error as an error, where `DataDir::kept_versions` (the
// CLI's, unchanged) treats an unreadable `versions/` as empty.
#[tokio::test]
async fn the_supervised_listing_fails_closed_where_the_cli_listing_stays_lenient() {
    use crate::updater::{all_versions, supervised_keep_set, supervised_prune};
    let f = Fixture::new().await;
    let now = chrono::Utc::now();
    // Present: directories that parse as versions, and nothing else.
    for version in ["0.2.0", "0.4.0", "not-a-version"] {
        fs::create_dir_all(f.data.version_dir(version)).unwrap();
    }
    fs::write(f.data.versions().join("0.9.0"), b"a file, not a version").unwrap();
    assert_eq!(
        all_versions(&f.data)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        ["0.2.0", "0.4.0"]
    );
    // No `versions/` at all is an empty list.
    let _ = fs::remove_dir_all(f.data.versions());
    assert!(all_versions(&f.data).unwrap().is_empty());
    // A `versions/` that is a file cannot be listed.
    fs::write(f.data.versions(), b"not a directory").unwrap();
    assert!(all_versions(&f.data).is_err());
    assert!(
        f.data.kept_versions().is_empty(),
        "the CLI's listing is unchanged: an unreadable directory is empty"
    );
    assert!(supervised_keep_set(&f.data, [], None, now, |_, _| false).is_err());
    assert!(supervised_prune(&f.data, "open-ferry", [], None, now, |_, _| false).is_err());
}

/// Makes removing the pin file `file` (in `dir`) fail until the guard is
/// dropped. `None` when this process cannot make a removal fail (a Unix
/// superuser, to whom a directory's mode means nothing).
struct Unremovable {
    #[cfg(unix)]
    dir: std::path::PathBuf,
    #[cfg(windows)]
    _handle: fs::File,
}

impl Unremovable {
    #[cfg(windows)]
    fn hold(_dir: &std::path::Path, file: &std::path::Path) -> Option<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        // Open without FILE_SHARE_DELETE: a removal is a sharing violation.
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(file)
            .unwrap();
        Some(Self { _handle: handle })
    }

    #[cfg(unix)]
    fn hold(dir: &std::path::Path, _file: &std::path::Path) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).unwrap();
        let guard = Self {
            dir: dir.to_owned(),
        };
        let probe = dir.join(".probe");
        if fs::File::create(&probe).is_ok() {
            let _ = fs::remove_file(&probe);
            return None;
        }
        Some(guard)
    }
}

#[cfg(unix)]
impl Drop for Unremovable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700));
    }
}

// Not upstream's: 5b L8. An expired pin that cannot be removed comes after the
// version directories were deleted, so it is reported beside them, not as a
// refusal that says nothing was deleted.
#[tokio::test]
async fn a_pin_that_cannot_be_removed_is_reported_beside_the_versions_that_were() {
    use crate::updater::supervised_prune;
    let f = Fixture::new().await;
    for version in ["0.2.0", "0.3.0", "0.4.0"] {
        fs::create_dir_all(f.data.version_dir(version)).unwrap();
        fs::write(f.data.binary(version, "open-ferry"), good_binary(version)).unwrap();
    }
    let now = chrono::Utc::now();
    let store = PinStore::new(&f.data.versions());
    let pin = store
        .create("0.9.0", &sha256_hex(b"x"), now - chrono::Duration::days(30))
        .unwrap();
    let file = store.dir().join(format!("{}.json", pin.id));
    let Some(guard) = Unremovable::hold(store.dir(), &file) else {
        eprintln!("skipped: this process can remove files from a read-only directory");
        return;
    };
    let done = supervised_prune(&f.data, "open-ferry", [], None, now, |_, _| false).unwrap();
    assert!(
        !done.removed.is_empty() && !f.data.version_dir("0.2.0").exists(),
        "the versions were deleted: {done:?}"
    );
    assert!(done.pins_error.is_some(), "{done:?}");
    assert_eq!(done.pins_removed, 0);
    assert!(file.exists(), "the pin is still there");
    drop(guard);
    let again = supervised_prune(&f.data, "open-ferry", [], None, now, |_, _| false).unwrap();
    assert_eq!(again.pins_removed, 1, "{again:?}");
    assert_eq!(again.pins_error, None);
    assert!(!file.exists());
}

/// A version directory that is not empty (an empty one may be a crashed
/// publication claim, which the newest-other rule skips).
fn occupied(f: &Fixture, version: &str) {
    fs::create_dir_all(f.data.version_dir(version)).unwrap();
    fs::write(f.data.binary(version, "open-ferry"), b"x").unwrap();
}

// Not upstream's: the supervised keep set reads the state file strictly. A
// missing file is the empty state; a file it cannot read or parse is an error
// (the prune refuses and deletes nothing), where the CLI's `State::load` still
// turns either into an empty state.
#[tokio::test]
async fn the_supervised_keep_set_refuses_a_state_file_it_cannot_read() {
    use crate::state::State;
    use crate::updater::{supervised_keep_set, supervised_prune};
    let f = Fixture::new().await;
    let now = chrono::Utc::now();
    journal(&f, "0.1.0");
    for version in ["0.1.0", "0.2.0", "0.4.0"] {
        occupied(&f, version);
    }
    // 0.2.0 is protected by the state's `staged` alone.
    let state = State {
        staged: Some("0.2.0".to_owned()),
        ..State::default()
    };
    state.save(&f.data.state_file()).unwrap();
    let keep = supervised_keep_set(&f.data, [], None, now, |_, _| false).unwrap();
    assert!(keep.contains("0.2.0"), "{keep:?}");
    assert!(keep.contains("0.4.0"), "{keep:?}");

    // A missing file is the empty state: not an error.
    fs::remove_file(f.data.state_file()).unwrap();
    assert!(supervised_keep_set(&f.data, [], None, now, |_, _| false).is_ok());

    // A parse error, then a read error (a directory in place of the file).
    for damage in ["parse", "read"] {
        let _ = fs::remove_file(f.data.state_file());
        let _ = fs::remove_dir_all(f.data.state_file());
        if damage == "parse" {
            fs::write(f.data.state_file(), b"{ not json").unwrap();
        } else {
            fs::create_dir(f.data.state_file()).unwrap();
        }
        assert!(
            supervised_keep_set(&f.data, [], None, now, |_, _| false).is_err(),
            "{damage}"
        );
        assert!(
            supervised_prune(&f.data, "open-ferry", [], None, now, |_, _| false).is_err(),
            "{damage}"
        );
        for version in ["0.1.0", "0.2.0", "0.4.0"] {
            assert!(
                f.data.binary(version, "open-ferry").is_file(),
                "{damage} {version}"
            );
        }
        // The CLI's reader is unchanged: an empty state.
        assert_eq!(State::load(&f.data.state_file()), State::default());
    }
}

// Not upstream's: the supervised newest-other candidates come from the one
// strict listing, never a second lenient one. A version only the strict
// listing names (its directory cannot be read, as after a listing that fails
// the second time) stays kept; the CLI's path, which lists with
// `kept_versions`, is unchanged.
#[tokio::test]
async fn the_supervised_newest_other_comes_from_the_strict_listing() {
    use crate::updater::retention_keep_with;
    let f = Fixture::new().await;
    let now = chrono::Utc::now();
    journal(&f, "0.1.0");
    occupied(&f, "0.1.0");
    let strict: std::collections::BTreeSet<String> =
        ["0.1.0".to_owned(), "0.9.0".to_owned()].into();
    // 0.9.0 is not on disk: only the listing handed in names it.
    let keep = retention_keep_with(&f.data, [], now, |_, _| false, None, Some(&strict)).unwrap();
    assert!(keep.contains("0.9.0"), "{keep:?}");
    let lenient = retention_keep_with(&f.data, [], now, |_, _| false, None, None).unwrap();
    assert!(!lenient.contains("0.9.0"), "{lenient:?}");
}

// Through `supervised_keep_set` itself: the newest-other candidates come from
// the one strict listing it took first, not from a second one. `tracked` is
// read after that listing and before the candidates are chosen, so a version
// that vanishes while it is read is the seam: only the first listing has it.
#[tokio::test]
async fn the_supervised_keep_set_takes_its_newest_other_from_its_one_strict_listing() {
    use crate::updater::supervised_keep_set;
    let f = Fixture::new().await;
    let now = chrono::Utc::now();
    journal(&f, "0.1.0");
    occupied(&f, "0.1.0");
    occupied(&f, "0.9.0");
    let vanishing = f.data.version_dir("0.9.0");
    let mut taken = false;
    let tracked = std::iter::from_fn(move || {
        if !taken {
            taken = true;
            std::fs::remove_dir_all(&vanishing).unwrap();
        }
        None
    });
    let keep = supervised_keep_set(&f.data, tracked, None, now, |_, _| false).unwrap();
    assert!(
        keep.contains("0.9.0"),
        "the one listing named it; a second listing would not: {keep:?}"
    );
}
