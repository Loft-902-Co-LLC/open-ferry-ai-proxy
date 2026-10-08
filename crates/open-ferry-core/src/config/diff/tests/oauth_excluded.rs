// Ported from CLIProxyAPI internal/watcher/diff/oauth_excluded_test.go
// (TestSummarizeExcludedModels_NormalizesAndDedupes,
// TestDiffOAuthExcludedModelChanges,
// TestSummarizeOAuthExcludedModels_NormalizesKeys,
// TestSummarizeVertexModels) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The excluded-model summaries and their change lines.
//!
//! Deviations from upstream:
//! - Upstream summarizes a nil map as a nil map; here an empty map gives an
//!   empty map.

use std::collections::BTreeMap;

use super::{expect_contains, strings};
use crate::config::VertexCompatModel;
use crate::config::diff::oauth::{diff_excluded, summarize_excluded};
use crate::config::diff::summary::{excluded_models, vertex_models};

// Ports TestSummarizeExcludedModels_NormalizesAndDedupes.
#[test]
fn summarize_excluded_models_normalizes_and_dedupes() {
    let summary = excluded_models(&strings(&["A", " a ", "B", "b"]));
    assert_eq!(summary.count, 2);
    assert!(!summary.hash.is_empty());
    let empty = excluded_models(&[]);
    assert_eq!((empty.count, empty.hash.as_str()), (0, ""));
}

// Ports TestDiffOAuthExcludedModelChanges.
#[test]
fn diff_oauth_excluded_model_changes() {
    let old = BTreeMap::from([
        ("ProviderA".to_owned(), strings(&["model-1", "model-2"])),
        ("providerB".to_owned(), strings(&["x"])),
    ]);
    let new = BTreeMap::from([
        ("providerA".to_owned(), strings(&["model-1", "model-3"])),
        ("providerC".to_owned(), strings(&["y"])),
    ]);

    let (changes, affected) = diff_excluded(&old, &new);
    expect_contains(
        &changes,
        "oauth-excluded-models[providera]: updated (2 -> 2 entries)",
    );
    expect_contains(&changes, "oauth-excluded-models[providerb]: removed");
    expect_contains(
        &changes,
        "oauth-excluded-models[providerc]: added (1 entries)",
    );
    assert_eq!(affected.len(), 3, "{affected:?}");
}

// Ports TestSummarizeOAuthExcludedModels_NormalizesKeys.
#[test]
fn summarize_oauth_excluded_models_normalizes_keys() {
    let out = summarize_excluded(&BTreeMap::from([
        ("ProvA".to_owned(), strings(&["X"])),
        (String::new(), strings(&["ignored"])),
    ]));
    assert_eq!(out.len(), 1, "{out:?}");
    let summary = out.get("prova").expect("normalized key");
    assert_eq!(summary.count, 1);
    assert!(!summary.hash.is_empty());
    assert!(summarize_excluded(&BTreeMap::new()).is_empty());
}

// Ports TestSummarizeVertexModels.
#[test]
fn summarize_vertex_models() {
    let model = |name: &str, alias: &str| VertexCompatModel {
        name: name.to_owned(),
        alias: alias.to_owned(),
        ..VertexCompatModel::default()
    };
    let summary = vertex_models(&[model("m1", ""), model(" ", "alias"), model("", "")]);
    assert_eq!(summary.count, 2);
    assert!(!summary.hash.is_empty());
    let empty = vertex_models(&[]);
    assert_eq!((empty.count, empty.hash.as_str()), (0, ""));
    let blank = vertex_models(&[model(" ", "")]);
    assert_eq!((blank.count, blank.hash.as_str()), (0, ""));
}
