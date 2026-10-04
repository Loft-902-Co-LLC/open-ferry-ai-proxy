// Ported from CLIProxyAPI sdk/cliproxy/service_cooldown_store_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The cooldown state store as the binary drives it: where [`reconfigure`]
//! puts it, what [`restore`] reads back, and the `.cds` files on disk.
//!
//! Deviations from upstream:
//! - Dropped: `TestResolveCooldownStateStoreUsesCapturedBackendProvider`.
//!   Only the file store is ported; no token store backend provides one.
//! - The tests here are not upstream's; upstream's service has no tests of
//!   its file store.

use std::path::Path;
use std::time::Duration;

use chrono::{TimeDelta, TimeZone, Utc};

use super::cooldown_state_store::{failure, files, success, temp_dir};
use super::support::*;
use crate::auth::path::absolute;
use crate::auth::{Auth, AuthStore, Status};
use crate::config::Config;
use crate::manager::Settings;
use crate::manager::cooldown_store::{
    FileStore, Record, StateStore, flush, installed_dir, is_pending, reconfigure, restore,
    set_debounce,
};

/// A config with its auth directory at `dir`, saving cooldowns if `on`.
fn config(dir: &Path, on: bool) -> Config {
    Config {
        auth_dir: dir.to_string_lossy().into_owned(),
        save_cooldown_status: on,
        ..Config::default()
    }
}

/// A manager whose store saves only when flushed.
fn harness() -> Harness {
    let h = Harness::new(Settings::default());
    set_debounce(&h.manager, Duration::from_secs(3600));
    h
}

/// `auth-1`, loaded from `xai.json` in `dir`.
fn credential(dir: &Path) -> Auth {
    let mut auth = auth("auth-1", "xai");
    auth.status = Status::Active;
    auth.attributes.insert(
        "path".into(),
        dir.join("xai.json").to_string_lossy().into_owned(),
    );
    auth
}

/// Turns the store to `config`, as the binary does at start and on reload.
fn apply(h: &Harness, config: &Config) {
    reconfigure(&h.manager, None, config);
    restore(&h.manager, config);
}

/// The models of the records saved in `dir`, sorted; a credential's own
/// record (which a cooldown of its only models brings) is left out.
fn saved_models(dir: &Path) -> Vec<String> {
    let mut models: Vec<String> = FileStore::new(dir.to_path_buf())
        .load()
        .expect("load")
        .into_iter()
        .map(|record| record.model)
        .filter(|model| !model.is_empty())
        .collect();
    models.sort();
    models
}

/// A `.cds` file holding one cooldown of `auth_id` on `model` until
/// 01:00, an hour after the test clock's start.
fn cds_file(auth_id: &str, model: &str) -> String {
    format!(
        r#"{{"version":1,"auth_id":"{auth_id}","provider":"xai","updated_at":"2026-06-01T00:00:00Z","records":[{{"provider":"xai","auth_id":"{auth_id}","model":"{model}","status":"cooling","next_retry_after":"2026-06-01T01:00:00Z","reason":"quota","quota":{{"exceeded":true,"reason":"quota","next_recover_at":"2026-06-01T01:00:00Z","backoff_level":1,"observed_at":"0001-01-01T00:00:00Z"}},"updated_at":"2026-06-01T00:00:00Z"}}]}}"#
    )
}

/// Not upstream's: the store is in the auth directory, made absolute, while
/// `save-cooldown-status` is on; while it is off nothing is read or
/// written, and turning it off leaves the files as they are.
#[tokio::test(start_paused = true)]
async fn the_store_is_in_the_auth_directory_while_saving_is_on() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("ghost.cds"), cds_file("ghost", "m1")).expect("write");
    let h = harness();
    h.manager
        .register_unsaved(credential(dir.path()))
        .expect("register");

    let off = config(dir.path(), false);
    apply(&h, &off);
    assert_eq!(installed_dir(&h.manager), None);
    h.manager
        .mark_result(&failure("auth-1", "xai", "grok-4", 429, "rate limited"));
    flush(&h.manager);
    assert_eq!(files(dir.path()), ["ghost.cds"], "saved while off");

    let on = config(dir.path(), true);
    reconfigure(&h.manager, Some(&off), &on);
    assert_eq!(installed_dir(&h.manager), Some(absolute(dir.path())));
    assert!(is_pending(&h.manager));
    restore(&h.manager, &on);
    assert!(!is_pending(&h.manager));
    // The restore saved the cooldown the credential holds, and dropped the
    // one of a credential that isn't loaded.
    assert_eq!(files(dir.path()), ["xai.cds"]);
    assert_eq!(saved_models(dir.path()), ["grok-4"]);

    // The same directory again restores nothing more.
    reconfigure(&h.manager, Some(&on), &on);
    assert!(!is_pending(&h.manager));

    reconfigure(&h.manager, Some(&on), &off);
    assert_eq!(installed_dir(&h.manager), None);
    h.manager.mark_result(&success("auth-1", "xai", "grok-4"));
    flush(&h.manager);
    assert_eq!(
        saved_models(dir.path()),
        ["grok-4"],
        "saved after turning off"
    );
}

/// Not upstream's: a cooldown saved by one run is put back by the next,
/// and its file goes once it is cleared.
#[tokio::test(start_paused = true)]
async fn a_restart_restores_the_saved_cooldowns() {
    let dir = temp_dir();
    let on = config(dir.path(), true);
    let until = {
        let h = harness();
        h.manager
            .register_unsaved(credential(dir.path()))
            .expect("register");
        apply(&h, &on);
        assert!(files(dir.path()).is_empty(), "nothing to save yet");
        h.manager
            .mark_result(&failure("auth-1", "xai", "grok-4", 429, "rate limited"));
        flush(&h.manager);
        assert_eq!(files(dir.path()), ["xai.cds"]);
        let state = h.get("auth-1").model_states.get("grok-4").cloned();
        state
            .and_then(|state| state.next_retry_after)
            .expect("cooling")
    };

    let h = harness();
    h.manager
        .register_unsaved(credential(dir.path()))
        .expect("register");
    assert!(h.get("auth-1").model_states.is_empty());
    apply(&h, &on);
    let auth = h.get("auth-1");
    let state = auth.model_states.get("grok-4").expect("restored");
    assert!(state.unavailable);
    assert_eq!(state.next_retry_after, Some(until));
    assert_eq!(
        state.last_error.as_ref().map(|err| err.http_status),
        Some(429)
    );

    h.manager.mark_result(&success("auth-1", "xai", "grok-4"));
    flush(&h.manager);
    assert!(files(dir.path()).is_empty(), "{:?}", files(dir.path()));
}

/// Not upstream's: moving the auth directory saves to the old one first,
/// then restores from the new one and saves there, removing its stale
/// files; the old one is left as it was.
#[tokio::test(start_paused = true)]
async fn moving_the_auth_directory_saves_to_the_old_one_then_restores_from_the_new_one() {
    let old_dir = temp_dir();
    let new_dir = temp_dir();
    std::fs::write(new_dir.path().join("xai.cds"), cds_file("auth-1", "m3")).expect("write");
    std::fs::write(new_dir.path().join("stale.cds"), cds_file("gone", "m1")).expect("write");
    let h = harness();
    h.manager
        .register_unsaved(credential(old_dir.path()))
        .expect("register");
    let old = config(old_dir.path(), true);
    apply(&h, &old);
    h.manager
        .mark_result(&failure("auth-1", "xai", "m1", 429, "rate limited"));
    flush(&h.manager);
    h.manager
        .mark_result(&failure("auth-1", "xai", "m2", 429, "rate limited"));
    assert_eq!(saved_models(old_dir.path()), ["m1"]);

    let new = config(new_dir.path(), true);
    reconfigure(&h.manager, Some(&old), &new);
    assert_eq!(saved_models(old_dir.path()), ["m1", "m2"], "the last save");
    assert!(is_pending(&h.manager));
    restore(&h.manager, &new);

    let until = Utc
        .with_ymd_and_hms(2026, 6, 1, 1, 0, 0)
        .single()
        .expect("time");
    let auth = h.get("auth-1");
    let m3 = auth.model_states.get("m3").expect("restored");
    assert!(m3.unavailable);
    assert_eq!(m3.next_retry_after, Some(until));
    // The credential's file is outside the new directory, so its file is
    // at the top level, by the auth file's name.
    assert_eq!(files(new_dir.path()), ["xai.cds"]);
    assert_eq!(saved_models(new_dir.path()), ["m1", "m2", "m3"]);

    h.manager.mark_result(&success("auth-1", "xai", "m3"));
    flush(&h.manager);
    assert_eq!(saved_models(new_dir.path()), ["m1", "m2"]);
    assert_eq!(
        saved_models(old_dir.path()),
        ["m1", "m2"],
        "the old one is left"
    );
}

/// Not upstream's: the auth store lists `.json` files only, so a `.cds`
/// file is never taken for a credential.
#[test]
fn the_auth_store_lists_no_cds_files() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("a.json"), r#"{"type":"xai"}"#).expect("write");
    std::fs::write(dir.path().join("a.cds"), r#"{"type":"xai"}"#).expect("write");
    let auths = crate::auth::FileStore::new(dir.path())
        .list()
        .expect("list");
    assert_eq!(auths.len(), 1, "{auths:?}");
    assert_eq!(auths[0].id, "a.json");
}

/// Not upstream's: a record of a credential the store can't name a file
/// for (no ID) is dropped, and a time Go can't write is skipped.
#[test]
fn unwritable_records_are_skipped() {
    let dir = temp_dir();
    let store = FileStore::new(dir.path().to_path_buf());
    let far = Utc
        .with_ymd_and_hms(9999, 12, 31, 23, 59, 59)
        .single()
        .expect("time")
        + TimeDelta::seconds(2);
    let now = Utc
        .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("time");
    store
        .save(
            &[
                Record {
                    auth_id: "  ".into(),
                    next_retry_after: Some(now),
                    ..Record::default()
                },
                Record {
                    auth_id: "far".into(),
                    next_retry_after: Some(far),
                    ..Record::default()
                },
            ],
            now,
        )
        .expect("save");
    assert!(files(dir.path()).is_empty());
}
