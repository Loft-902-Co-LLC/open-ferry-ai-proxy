// Ported from CLIProxyAPI sdk/cliproxy/auth/types_cooling_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential's own `disable_cooling` setting, including an explicit
//! false.
//!
//! Deviations from upstream:
//! - Upstream's (value, present) pair is an `Option<bool>`.

use serde_json::json;

use super::support::*;
use crate::manager::credential::disable_cooling_override;

#[test]
fn disable_cooling_override_supports_explicit_false() {
    let cases = [
        ("unset", json!({}), None),
        (
            "canonical true",
            json!({"disable_cooling": true}),
            Some(true),
        ),
        (
            "canonical false",
            json!({"disable_cooling": false}),
            Some(false),
        ),
        (
            "legacy false",
            json!({"disable-cooling": false}),
            Some(false),
        ),
        (
            "string false",
            json!({"disable_cooling": "false"}),
            Some(false),
        ),
        ("invalid", json!({"disable_cooling": "invalid"}), None),
    ];
    for (name, metadata, want) in cases {
        let auth = auth_with_metadata("a", "", metadata);
        assert_eq!(disable_cooling_override(&auth), want, "{name}");
    }
}
