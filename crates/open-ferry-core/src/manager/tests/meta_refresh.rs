// Ported from CLIProxyAPI sdk/cliproxy/auth/meta_refresh_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Meta's device token counts as a refresh credential only for Meta.
//!
//! Deviations from upstream:
//! - The `nil` case is dropped: a credential here can't be nil.

use serde_json::json;

use super::support::*;
use crate::auth::Auth;
use crate::manager::credential::has_refresh_credential;

#[test]
fn meta_dca_refresh_credential_is_provider_scoped() {
    let mut meta_attributes = auth("meta-attributes", "meta");
    meta_attributes
        .attributes
        .insert("dca_token".into(), "dca:valid".into());

    let cases: [(&str, Auth, bool); 5] = [
        (
            "meta",
            auth_with_metadata("meta", "meta", json!({"dca_token": "dca:valid"})),
            true,
        ),
        ("meta attributes", meta_attributes, true),
        (
            "non-meta",
            auth_with_metadata("non-meta", "codex", json!({"dca_token": "dca:valid"})),
            false,
        ),
        (
            "empty",
            auth_with_metadata("empty", "meta", json!({"dca_token": " "})),
            false,
        ),
        (
            "existing oauth",
            auth_with_metadata(
                "existing-oauth",
                "codex",
                json!({"refresh_token": "refresh"}),
            ),
            true,
        ),
    ];
    for (name, auth, want) in cases {
        assert_eq!(
            has_refresh_credential(&auth),
            want,
            "{name}: refresh eligible"
        );
    }
}
