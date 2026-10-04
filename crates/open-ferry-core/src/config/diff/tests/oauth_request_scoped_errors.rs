// Ported from CLIProxyAPI
// internal/watcher/diff/oauth_request_scoped_errors_test.go
// (TestSummarizeOAuthRequestScopedErrors_NormalizesKeys,
// TestDiffOAuthRequestScopedErrorsChanges) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `oauth-request-scoped-errors` summaries and change lines.
//!
//! Deviations from upstream:
//! - Upstream summarizes a nil map as a nil map; here an empty map gives an
//!   empty map.

use std::collections::BTreeMap;

use super::{expect_contains, strings};
use crate::config::RequestScopedErrorRule;
use crate::config::diff::oauth::{diff_request_scoped_errors, summarize_request_scoped_errors};

fn rule(status: i64, matches: &str, action: &str) -> RequestScopedErrorRule {
    RequestScopedErrorRule {
        status,
        matches: strings(&[matches]),
        action: action.to_owned(),
        ..RequestScopedErrorRule::default()
    }
}

// Ports TestSummarizeOAuthRequestScopedErrors_NormalizesKeys.
#[test]
fn summarize_normalizes_keys() {
    let out = summarize_request_scoped_errors(&BTreeMap::from([
        (" Vertex ".to_owned(), vec![rule(400, "error", "stop")]),
        (String::new(), vec![rule(500, "err", "continue")]),
    ]));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out.get("vertex").map(|summary| summary.count), Some(1));
    assert!(summarize_request_scoped_errors(&BTreeMap::new()).is_empty());
}

// Ports TestDiffOAuthRequestScopedErrorsChanges.
#[test]
fn diff_changes() {
    let old = BTreeMap::from([
        (
            "vertex".to_owned(),
            vec![rule(400, "context_length", "stop")],
        ),
        (
            "claude".to_owned(),
            vec![rule(429, "rate_limit", "continue")],
        ),
    ]);
    let new = BTreeMap::from([
        (
            "vertex".to_owned(),
            vec![rule(400, "context_length_updated", "stop")],
        ),
        (
            "codex".to_owned(),
            vec![rule(400, "window_exceeded", "stop")],
        ),
    ]);

    let (changes, affected) = diff_request_scoped_errors(&old, &new);
    expect_contains(&changes, "oauth-request-scoped-errors[claude]: removed");
    expect_contains(
        &changes,
        "oauth-request-scoped-errors[codex]: added (1 entries)",
    );
    expect_contains(
        &changes,
        "oauth-request-scoped-errors[vertex]: updated (1 -> 1 entries)",
    );
    for channel in ["claude", "codex", "vertex"] {
        expect_contains(&affected, channel);
    }
}
