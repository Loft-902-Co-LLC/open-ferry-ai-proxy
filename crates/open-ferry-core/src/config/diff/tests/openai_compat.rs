// Ported from CLIProxyAPI internal/watcher/diff/openai_compat_test.go
// (TestDiffOpenAICompatibility, TestDiffOpenAICompatibilityPromptCacheKey,
// TestDiffOpenAICompatibilityDuplicateNames,
// TestDiffOpenAICompatibilityDuplicateKeyDoesNotCollide,
// TestDiffOpenAICompatibility_RemovedAndUnchanged,
// TestOpenAICompatKeyFallbacks, TestOpenAICompatKey_UsesName,
// TestOpenAICompatKey_SignatureFallbackWhenOnlyAPIKeys,
// TestOpenAICompatSignature_EmptyReturnsEmpty,
// TestOpenAICompatSignature_StableAndNormalized,
// TestCountOpenAIModelsSkipsBlanks,
// TestOpenAICompatKeyUsesModelNameWhenAliasEmpty) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `openai-compatibility` change lines and how providers are matched.
//!
//! Deviations from upstream: none.

use std::collections::BTreeMap;

use super::expect_contains;
use crate::config::diff::openai_compat::{count_models, diff, key, signature};
use crate::config::{OpenAiCompatibility, OpenAiCompatibilityApiKey, OpenAiCompatibilityModel};

fn api_keys(keys: &[&str]) -> Vec<OpenAiCompatibilityApiKey> {
    keys.iter()
        .map(|key| OpenAiCompatibilityApiKey {
            api_key: (*key).to_owned(),
            ..OpenAiCompatibilityApiKey::default()
        })
        .collect()
}

fn model(name: &str, alias: &str) -> OpenAiCompatibilityModel {
    OpenAiCompatibilityModel {
        name: name.to_owned(),
        alias: alias.to_owned(),
        ..OpenAiCompatibilityModel::default()
    }
}

fn named(name: &str) -> OpenAiCompatibility {
    OpenAiCompatibility {
        name: name.to_owned(),
        ..OpenAiCompatibility::default()
    }
}

fn with_cache_key(name: &str, support_prompt_cache_key: bool) -> OpenAiCompatibility {
    OpenAiCompatibility {
        support_prompt_cache_key,
        ..named(name)
    }
}

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

// Ports TestDiffOpenAICompatibility.
#[test]
fn diff_open_ai_compatibility() {
    let old = [OpenAiCompatibility {
        api_key_entries: api_keys(&["key-a"]),
        models: vec![model("m1", "")],
        ..named("provider-a")
    }];
    let new = [
        OpenAiCompatibility {
            api_key_entries: api_keys(&["key-a", "key-b"]),
            models: vec![model("m1", ""), model("m2", "")],
            headers: headers(&[("X-Test", "1")]),
            ..named("provider-a")
        },
        OpenAiCompatibility {
            api_key_entries: api_keys(&["key-b"]),
            ..named("provider-b")
        },
    ];

    let changes = diff(&old, &new);
    expect_contains(
        &changes,
        "provider added: provider-b (api-keys=1, models=0)",
    );
    expect_contains(
        &changes,
        "provider updated: provider-a (api-keys 1 -> 2, models 1 -> 2, headers updated)",
    );
}

// Ports TestDiffOpenAICompatibilityPromptCacheKey.
#[test]
fn prompt_cache_key() {
    let changes = diff(
        &[with_cache_key("provider-a", false)],
        &[with_cache_key("provider-a", true)],
    );
    expect_contains(
        &changes,
        "provider updated: provider-a (support-prompt-cache-key false -> true)",
    );
}

// Ports TestDiffOpenAICompatibilityDuplicateNames.
#[test]
fn duplicate_names() {
    let old = [
        with_cache_key("duplicate", false),
        with_cache_key("duplicate", false),
    ];
    let new = [
        with_cache_key("duplicate", true),
        with_cache_key("duplicate", false),
    ];
    let changes = diff(&old, &new);
    expect_contains(
        &changes,
        "provider updated: duplicate (support-prompt-cache-key false -> true)",
    );
}

// Ports TestDiffOpenAICompatibilityDuplicateKeyDoesNotCollide.
#[test]
fn duplicate_key_does_not_collide() {
    let old = [named("foo"), named("foo#1")];
    let new = [named("foo"), named("foo"), named("foo#1")];
    let changes = diff(&old, &new);
    expect_contains(&changes, "provider added: foo (api-keys=0, models=0)");
}

// Ports TestDiffOpenAICompatibility_RemovedAndUnchanged.
#[test]
fn removed_and_unchanged() {
    let old = [OpenAiCompatibility {
        api_key_entries: api_keys(&["key-a"]),
        models: vec![model("m1", "")],
        ..named("provider-a")
    }];
    let changes = diff(&old, &old.clone());
    assert!(changes.is_empty(), "expected no changes, got {changes:?}");

    let changes = diff(&old, &[]);
    expect_contains(
        &changes,
        "provider removed: provider-a (api-keys=1, models=1)",
    );
}

// Ports TestOpenAICompatKeyFallbacks.
#[test]
fn key_fallbacks() {
    let mut entry = OpenAiCompatibility {
        base_url: "http://base".to_owned(),
        models: vec![model("", "alias-only")],
        ..OpenAiCompatibility::default()
    };
    assert_eq!(
        key(&entry, 0),
        ("base:http://base".to_owned(), "http://base".to_owned())
    );

    entry.base_url = String::new();
    assert_eq!(
        key(&entry, 1),
        ("alias:alias-only".to_owned(), "alias-only".to_owned())
    );

    entry.models = Vec::new();
    assert_eq!(key(&entry, 2), ("index:2".to_owned(), "entry-3".to_owned()));
}

// Ports TestOpenAICompatKey_UsesName.
#[test]
fn key_uses_name() {
    assert_eq!(
        key(&named("My-Provider"), 0),
        ("name:My-Provider".to_owned(), "My-Provider".to_owned())
    );
}

// Ports TestOpenAICompatKey_SignatureFallbackWhenOnlyAPIKeys.
#[test]
fn key_signature_fallback_when_only_api_keys() {
    let entry = OpenAiCompatibility {
        api_key_entries: api_keys(&["k1", "k2"]),
        ..OpenAiCompatibility::default()
    };
    let (key, label) = key(&entry, 0);
    assert!(key.starts_with("sig:"), "{key}");
    assert!(label.starts_with("compat-"), "{label}");
    // The label is the signature's first eight hex digits.
    assert_eq!(label.len(), "compat-".len() + 8, "{label}");
}

// Ports TestOpenAICompatSignature_EmptyReturnsEmpty.
#[test]
fn signature_empty_returns_empty() {
    assert_eq!(signature(&OpenAiCompatibility::default()), "");
}

// Ports TestOpenAICompatSignature_StableAndNormalized.
#[test]
fn signature_stable_and_normalized() {
    let a = OpenAiCompatibility {
        name: "  Provider  ".to_owned(),
        base_url: "http://base".to_owned(),
        models: vec![model("m1", ""), model("  ", ""), model("", "A1")],
        headers: headers(&[("X-Test", "1"), ("  ", "ignored")]),
        api_key_entries: api_keys(&["k1", " "]),
        ..OpenAiCompatibility::default()
    };
    let b = OpenAiCompatibility {
        name: "provider".to_owned(),
        base_url: "http://base".to_owned(),
        models: vec![model("", "a1"), model("m1", "")],
        headers: headers(&[("x-test", "2")]),
        api_key_entries: api_keys(&["k2"]),
        ..OpenAiCompatibility::default()
    };

    let (sig_a, sig_b) = (signature(&a), signature(&b));
    assert!(!sig_a.is_empty() && !sig_b.is_empty());
    assert_eq!(sig_a, sig_b);

    let mut c = b.clone();
    c.models.push(model("m2", ""));
    assert_ne!(signature(&c), sig_b);
}

// Ports TestCountOpenAIModelsSkipsBlanks.
#[test]
fn count_models_skips_blanks() {
    let models = [
        model("m1", ""),
        model("", ""),
        model("", ""),
        model(" ", ""),
        model("", "a1"),
    ];
    assert_eq!(count_models(&models), 2);
}

// Ports TestOpenAICompatKeyUsesModelNameWhenAliasEmpty.
#[test]
fn key_uses_model_name_when_alias_empty() {
    let entry = OpenAiCompatibility {
        models: vec![model("model-name", "")],
        ..OpenAiCompatibility::default()
    };
    assert_eq!(
        key(&entry, 5),
        ("alias:model-name".to_owned(), "model-name".to_owned())
    );
}
