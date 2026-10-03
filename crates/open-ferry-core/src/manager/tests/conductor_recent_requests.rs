// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_recent_requests_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Call counts: an outcome is counted in the credential's totals and its
//! recent windows, and an update keeps both. Registering and updating also
//! give the credential its index.
//!
//! Deviations from upstream:
//! - Snapshots are read at the test clock's now, where upstream reads the
//!   wall clock.
//! - Added: `register_and_update_keep_the_index`.

use serde_json::json;

use super::support::*;
use crate::auth::Auth;
use crate::manager::{CallResult, Settings};

fn result(success: bool) -> CallResult {
    CallResult {
        auth_id: "auth-1".into(),
        provider: "antigravity".into(),
        model: "gpt-5".into(),
        success,
        ..CallResult::default()
    }
}

/// The successes and failures over every recent window of `auth`.
fn window_totals(h: &Harness, auth: &Auth) -> (i64, i64) {
    auth.recent_requests_snapshot(h.now())
        .iter()
        .fold((0, 0), |(s, f), b| (s + b.success, f + b.failed))
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_records_recent_requests() {
    let h = Harness::new(Settings::default());
    let mut auth = auth_with_metadata("auth-1", "antigravity", json!({"type": "antigravity"}));
    auth.attributes.insert("runtime_only".into(), "true".into());
    h.manager.register_unsaved(auth).unwrap();

    h.manager.mark_result(&result(true));
    h.manager.mark_result(&result(false));

    let got = h.get("auth-1");
    assert_eq!((got.success, got.failed), (1, 1));
    assert_eq!(window_totals(&h, &got), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn manager_update_preserves_recent_requests_and_totals() {
    let h = Harness::new(Settings::default());
    h.manager
        .register_unsaved(auth_with_metadata(
            "auth-1",
            "antigravity",
            json!({"type": "antigravity"}),
        ))
        .unwrap();

    h.manager.mark_result(&result(true));

    h.manager
        .update_unsaved(auth_with_metadata(
            "auth-1",
            "antigravity",
            json!({"type": "antigravity", "note": "updated"}),
        ))
        .unwrap();

    let got = h.get("auth-1");
    assert_eq!((got.success, got.failed), (1, 0));
    assert_eq!(window_totals(&h, &got), (1, 0));
}

#[tokio::test(start_paused = true)]
async fn register_and_update_keep_the_index() {
    let h = Harness::new(Settings::default());
    let registered = h.add(auth("auth-1", "codex"), &[]);
    let index = registered.index.clone();
    assert_eq!(index.len(), 16);

    // An update with no index keeps the one the credential has.
    let mut changed = auth("auth-1", "codex");
    changed.attributes.insert("note".into(), "x".into());
    let updated = h.manager.update_unsaved(changed).unwrap().unwrap();
    assert_eq!(updated.index, index);

    // One with its own index keeps that, trimmed.
    let mut own = auth("auth-1", "codex");
    own.index = " own ".into();
    let updated = h.manager.update_unsaved(own).unwrap().unwrap();
    assert_eq!(updated.index, "own");
}
