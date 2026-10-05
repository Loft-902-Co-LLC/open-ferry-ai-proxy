// Ported from CLIProxyAPI sdk/cliproxy/auth/classification_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! How a credential's kind (API key or OAuth) and source (config, file,
//! memory, a store backend) are read from its attributes and metadata.
//!
//! Deviations from upstream:
//! - `TestAccountInfoUsesAuthKind` is dropped: `Auth.AccountInfo` isn't
//!   ported (upstream's management and API handlers use it, not the
//!   manager).

use serde_json::json;

use super::support::*;
use crate::auth::Auth;
use crate::manager::credential::{self, KIND_API_KEY, KIND_OAUTH};

fn with_attributes(attributes: &[(&str, &str)]) -> Auth {
    let mut credential = Auth::default();
    for (key, value) in attributes {
        credential
            .attributes
            .insert((*key).to_owned(), (*value).to_owned());
    }
    credential
}

#[test]
fn auth_kind() {
    let cases = [
        (
            "explicit api key attribute",
            with_attributes(&[("auth_kind", "api_key")]),
            KIND_API_KEY,
        ),
        (
            "explicit oauth attribute wins over api key fallback",
            with_attributes(&[("auth_kind", "oauth"), ("api_key", "k")]),
            KIND_OAUTH,
        ),
        (
            "explicit oauth metadata",
            auth_with_metadata("", "", json!({"auth_kind": "oauth"})),
            KIND_OAUTH,
        ),
        (
            "legacy api key attribute",
            with_attributes(&[("api_key", "k")]),
            KIND_API_KEY,
        ),
        (
            "legacy oauth metadata",
            auth_with_metadata("", "", json!({"access_token": "token"})),
            KIND_OAUTH,
        ),
        (
            "unknown metadata shape",
            auth_with_metadata("", "", json!({"type": "test"})),
            "",
        ),
    ];
    for (name, auth, want) in cases {
        assert_eq!(credential::auth_kind(&auth), want, "{name}: AuthKind()");
    }
}

#[test]
fn auth_source_kind() {
    let filename_fallback = Auth {
        file_name: "codex.json".into(),
        ..Auth::default()
    };
    let cases = [
        (
            "runtime only memory",
            with_attributes(&[("runtime_only", "true"), ("source_backend", "postgres")]),
            "memory",
        ),
        (
            "backend postgres",
            with_attributes(&[("source_backend", "postgresql"), ("path", "/tmp/auth.json")]),
            "postgres",
        ),
        (
            "backend object store",
            with_attributes(&[
                ("source_backend", "object-store"),
                ("path", "/tmp/auth.json"),
            ]),
            "objectstore",
        ),
        (
            "config source",
            with_attributes(&[("source", "config:codex[abc]")]),
            "config",
        ),
        (
            "path source",
            with_attributes(&[("source", "/tmp/auth.json")]),
            "file",
        ),
        (
            "path attribute",
            with_attributes(&[("path", "/tmp/auth.json")]),
            "file",
        ),
        ("filename fallback", filename_fallback, "file"),
    ];
    for (name, auth, want) in cases {
        assert_eq!(
            credential::auth_source_kind(&auth),
            want,
            "{name}: AuthSourceKind()"
        );
    }
}
