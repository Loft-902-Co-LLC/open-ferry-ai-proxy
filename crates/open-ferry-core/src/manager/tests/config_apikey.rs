// Ported from CLIProxyAPI sdk/cliproxy/auth/config_apikey_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which credentials count as config API keys: an API key (by `auth_kind`
//! or an `api_key` attribute) whose source is the config.
//!
//! Deviations from upstream:
//! - The port has no `IsConfigAPIKeyAuth`; its one use is the save check,
//!   so each case is checked by whether registering the credential saves
//!   it (a config API key is never saved). Each case carries a metadata
//!   entry so it isn't skipped for empty metadata instead.
//! - The nil-auth case is dropped: there is no nil credential in Rust.

use serde_json::json;

use super::support::*;
use crate::manager::Settings;

/// Whether registering a credential with `attributes` skips the save, as
/// a config API key's does.
fn save_skipped(id: &str, attributes: &[(&str, &str)]) -> bool {
    let h = Harness::with_store(Settings::default());
    let mut auth = auth_with_metadata(id, "codex", json!({"type": "codex"}));
    for (key, value) in attributes {
        auth.attributes.insert((*key).into(), (*value).into());
    }
    let registered = h.manager.register(auth).expect("register");
    h.store.stored(&registered.id).is_none()
}

#[tokio::test(start_paused = true)]
async fn is_config_api_key_auth() {
    assert!(
        !save_skipped("", &[("source", "config:codex[x]")]),
        "expected missing auth_kind and api_key to be false"
    );
    assert!(
        !save_skipped(
            "codex:oauth:abc",
            &[
                ("auth_kind", "oauth"),
                ("api_key", "k"),
                ("source", "config:codex[abc]"),
            ],
        ),
        "expected explicit oauth auth to be false"
    );
    assert!(
        save_skipped(
            "codex:apikey:abc",
            &[("auth_kind", "apikey"), ("source", "config:codex[abc]")],
        ),
        "expected empty api_key with auth_kind=apikey and config source to be true"
    );
    assert!(
        save_skipped(
            "codex:apikey:abc",
            &[("api_key", "k"), ("source", "config:codex[abc]")],
        ),
        "expected config api key auth"
    );
}
