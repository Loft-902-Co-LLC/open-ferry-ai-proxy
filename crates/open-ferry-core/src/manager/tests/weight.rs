// Ported from CLIProxyAPI sdk/cliproxy/auth/weight_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which explicit credential weights are valid.
//!
//! Deviations from upstream:
//! - Upstream's `json.Number` metadata values are plain JSON numbers here.

use serde_json::json;

use super::support::*;
use crate::manager::credential::validate_weight;

#[test]
fn validate_auth_weight() {
    let with_attr = |value: &str| {
        let mut auth = auth("", "");
        auth.attributes.insert("weight".into(), value.into());
        auth
    };
    let with_meta = |value: serde_json::Value| auth_with_metadata("", "", json!({"weight": value}));
    let mut mixed = with_attr("2");
    mixed.metadata.insert("weight".into(), json!(1.5));

    let cases = [
        ("omitted", auth("", ""), false),
        ("positive attribute", with_attr("7"), false),
        ("zero metadata", with_meta(json!(0)), false),
        ("negative attribute", with_attr("-2"), false),
        ("fraction metadata", with_meta(json!(1.5)), true),
        ("above maximum attribute", with_attr("1000001"), true),
        (
            "overflow metadata",
            with_meta(json!(9_223_372_036_854_775_808_u64)),
            true,
        ),
        ("nonnumeric attribute", with_attr("invalid"), true),
        (
            "valid attribute does not hide invalid metadata",
            mixed,
            true,
        ),
    ];
    for (name, auth, want_err) in cases {
        let got = validate_weight(&auth);
        assert_eq!(
            got.is_err(),
            want_err,
            "{name}: validate_weight() = {got:?}"
        );
    }
}
