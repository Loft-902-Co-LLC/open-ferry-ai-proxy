// Ported from CLIProxyAPI internal/watcher/diff/oauth_model_alias_test.go
// (TestDiffOAuthModelAliasChanges_IncludesDisplayName) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `oauth-model-alias` change lines.
//!
//! Deviations from upstream: none.

use std::collections::BTreeMap;

use super::expect_contains;
use crate::config::OAuthModelAlias;
use crate::config::diff::oauth::diff_model_alias;

fn aliases(display_name: &str) -> BTreeMap<String, Vec<OAuthModelAlias>> {
    BTreeMap::from([(
        "antigravity".to_owned(),
        vec![OAuthModelAlias {
            name: "claude-opus-4-6-thinking".to_owned(),
            alias: "claude-antigravity-opus-4-6-thinking".to_owned(),
            display_name: display_name.to_owned(),
            ..OAuthModelAlias::default()
        }],
    )])
}

// Ports TestDiffOAuthModelAliasChanges_IncludesDisplayName.
#[test]
fn includes_display_name() {
    let (changes, affected) = diff_model_alias(
        &aliases("Antigravity Opus 4.6"),
        &aliases("Antigravity Opus 4.6 (Thinking)"),
    );
    expect_contains(
        &changes,
        "oauth-model-alias[antigravity]: updated (1 -> 1 entries)",
    );
    assert_eq!(affected, ["antigravity"]);
}
