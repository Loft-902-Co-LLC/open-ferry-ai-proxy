// Ported from CLIProxyAPI internal/watcher/diff/oauth_settings_test.go
// (TestDiffOAuthSettingsChanges, TestDiffOAuthSettingsChanges_Reordering)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `oauth-settings` change lines.
//!
//! Deviations from upstream: none.

use std::collections::BTreeMap;

use crate::config::OAuthModelSetting;
use crate::config::diff::oauth::diff_settings;

fn settings(entries: &[(&str, &[(&str, i64)])]) -> BTreeMap<String, Vec<OAuthModelSetting>> {
    entries
        .iter()
        .map(|(channel, models)| {
            let models = models
                .iter()
                .map(|(name, max_context_length)| OAuthModelSetting {
                    name: (*name).to_owned(),
                    max_context_length: *max_context_length,
                    ..OAuthModelSetting::default()
                })
                .collect();
            ((*channel).to_owned(), models)
        })
        .collect()
}

/// Upstream's local `expectContains`: an entry holds `expected`.
#[track_caller]
fn expect_substring(slice: &[String], expected: &str) {
    assert!(
        slice.iter().any(|entry| entry.contains(expected)),
        "expected slice to contain {expected:?}, but got {slice:#?}"
    );
}

// Ports TestDiffOAuthSettingsChanges.
#[test]
fn diff_changes() {
    let old = settings(&[
        ("codex", &[("gpt-6-sol", 272_000)]),
        ("vertex", &[("gemini-2.5-pro", 1_048_576)]),
    ]);
    let new = settings(&[
        ("codex", &[("gpt-6-sol", 524_288)]),
        ("claude", &[("claude-sonnet-4-5-20250929", 200_000)]),
    ]);

    let (changes, affected) = diff_settings(&old, &new);
    expect_substring(&changes, "oauth-settings[vertex]: removed");
    expect_substring(&changes, "oauth-settings[claude]: added (1 entries)");
    expect_substring(&changes, "oauth-settings[codex]: updated (1 -> 1 entries)");
    for channel in ["vertex", "claude", "codex"] {
        expect_substring(&affected, channel);
    }
}

// Ports TestDiffOAuthSettingsChanges_Reordering.
#[test]
fn reordering_is_a_change() {
    let old = settings(&[(
        "codex",
        &[("gpt-6-sol", 524_288), ("deepseek-v4-flash", 1_048_576)],
    )]);
    let new = settings(&[(
        "codex",
        &[("deepseek-v4-flash", 1_048_576), ("gpt-6-sol", 524_288)],
    )]);

    let (changes, affected) = diff_settings(&old, &new);
    assert!(
        !changes.is_empty(),
        "expected changes when rules are reordered"
    );
    assert_eq!(affected.first().map(String::as_str), Some("codex"));
}
