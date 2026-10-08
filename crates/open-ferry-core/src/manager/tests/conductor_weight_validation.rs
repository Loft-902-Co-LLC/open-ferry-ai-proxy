// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_weight_validation_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Loading, registering and updating credentials whose explicit weight is
//! invalid.
//!
//! Deviations from upstream:
//! - The store is the harness's [`FakeStore`]; upstream's
//!   `json.Number` metadata values are plain JSON numbers.

use serde_json::json;

use super::support::*;
use crate::manager::{ManagerError, Settings};

#[tokio::test(start_paused = true)]
async fn manager_load_skips_invalid_explicit_weights() {
    let h = Harness::with_store(Settings::default());
    let mut overflow = auth("overflow", "test");
    overflow
        .attributes
        .insert("weight".into(), "9223372036854775808".into());
    h.store.put(auth("omitted", "test"));
    h.store
        .put(auth_with_metadata("zero", "test", json!({"weight": 0})));
    h.store.put(auth_with_metadata(
        "fraction",
        "test",
        json!({"weight": 1.5}),
    ));
    h.store.put(overflow);

    h.manager.load().expect("load");
    assert!(
        h.manager.get("omitted").is_some(),
        "omitted weight auth was not loaded"
    );
    assert!(
        h.manager.get("zero").is_some(),
        "zero weight auth was not loaded"
    );
    for id in ["fraction", "overflow"] {
        assert!(
            h.manager.get(id).is_none(),
            "invalid auth {id:?} remained active after load"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_register_and_update_reject_invalid_explicit_weights() {
    let h = Harness::with_store(Settings::default());

    let invalid = auth_with_metadata("invalid", "test", json!({"weight": "nonnumeric"}));
    let err = h
        .manager
        .register(invalid)
        .expect_err("register accepted an invalid weight");
    assert!(matches!(err, ManagerError::InvalidWeight(_)), "{err:?}");
    assert!(
        h.manager.get("invalid").is_none(),
        "invalid registered auth became active"
    );
    assert_eq!(h.store.save_count(), 0, "invalid register save count");

    let mut valid = auth_with_metadata("valid", "test", json!({"type": "test"}));
    valid.attributes.insert("weight".into(), "2".into());
    h.manager.register(valid.clone()).expect("register valid");

    let mut invalid_update = valid;
    invalid_update
        .attributes
        .insert("weight".into(), "1000001".into());
    let err = h
        .manager
        .update(invalid_update)
        .expect_err("update accepted an invalid weight");
    assert!(matches!(err, ManagerError::InvalidWeight(_)), "{err:?}");
    let current = h.get("valid");
    assert_eq!(
        current.attribute("weight"),
        Some("2"),
        "invalid update changed the active auth"
    );
    assert_eq!(
        h.store.save_count(),
        1,
        "save count, want only the valid register's save"
    );
}
