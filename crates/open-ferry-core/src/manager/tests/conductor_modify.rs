// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_patch_fields_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::support::*;
use crate::manager::{EditError, Settings};
use serde_json::json;

#[test]
fn an_epoch_fenced_edit_rejects_a_replacement_before_calling_the_editor() {
    let h = Harness::with_store(Settings::default());
    let selected = h.add(auth("a", "test"), &[]);
    let replacement = h.add(auth("a", "test"), &[]);
    assert!(replacement.registration_epoch > selected.registration_epoch);
    let result =
        h.manager
            .edit_at_epoch("a", selected.registration_epoch, |_| -> Result<bool, ()> {
                panic!("an edit must not run on a replacement account")
            });
    assert!(matches!(result, Err(EditError::Manager(_))));
    assert_eq!(h.get("a").generation, replacement.generation);
}

#[test]
fn an_epoch_fenced_edit_does_not_recreate_a_removed_account() {
    let h = Harness::with_store(Settings::default());
    let selected = h.add(auth("a", "test"), &[]);
    h.manager.remove("a");
    let result = h
        .manager
        .edit_at_epoch("a", selected.registration_epoch, |_| -> Result<bool, ()> {
            panic!("an edit must not run on a removed account")
        })
        .unwrap();
    assert!(result.is_none());
    assert!(h.manager.get("a").is_none());
}

#[test]
fn epoch_fenced_edits_merge_fields_and_keep_a_concurrent_refresh() {
    let h = Harness::with_store(Settings::default());
    let selected = h.add(
        auth_with_metadata("a", "test", json!({"access_token":"dummy-old"})),
        &[],
    );
    let mut refreshed = (*selected).clone();
    refreshed
        .metadata
        .insert("access_token".into(), json!("dummy-new"));
    h.manager
        .update_refreshed(&selected, selected.registration_epoch, refreshed)
        .unwrap();
    std::thread::scope(|scope| {
        for key in ["note", "priority"] {
            let manager = &h.manager;
            let epoch = selected.registration_epoch;
            scope.spawn(move || {
                manager
                    .edit_at_epoch("a", epoch, |auth| {
                        auth.metadata.insert(key.into(), json!(key));
                        Ok::<_, ()>(true)
                    })
                    .unwrap()
                    .unwrap();
            });
        }
    });
    let current = h.get("a");
    assert_eq!(current.metadata["access_token"], "dummy-new");
    assert_eq!(current.metadata["note"], "note");
    assert_eq!(current.metadata["priority"], "priority");
    assert_eq!(current.registration_epoch, selected.registration_epoch);
}
