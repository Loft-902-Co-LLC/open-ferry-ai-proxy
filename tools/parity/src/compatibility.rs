//! Machine-readable compatibility metadata from the actual suite registry.
//!
//! Counts are maintained hand-written cases (including signature-relay
//! cases), with seed 1 and no random cases. Export never runs translators or
//! Go and excludes configuration fixtures that read the upstream checkout.

use serde_json::{Value, json};

use crate::{Args, cases, suite_registry};

/// Every intentional omission is named; an unclassified new key is an error.
const EXCLUDED: &[(&str, &str)] = &[
    (
        "signature/inspect",
        "Signature inspection, not protocol translation.",
    ),
    (
        "signature/claude-messages",
        "Claude signature validation helper.",
    ),
    ("signature/gemini", "Gemini signature validation helper."),
    (
        "registry/request",
        "Mixed-format dispatch, covered by individual translation suites.",
    ),
    (
        "registry/response",
        "Mixed-format stream dispatch, covered by individual suites.",
    ),
    (
        "registry/response-non-stream",
        "Mixed-format whole-response dispatch.",
    ),
    (
        "registry/lookup",
        "Registry lookup rather than a single translation.",
    ),
    (
        "multi-agent/prepare",
        "Opt-in collaboration schema preparation.",
    ),
    (
        "multi-agent/optimize",
        "Opt-in multi-agent namespace optimization.",
    ),
    ("multi-agent/input", "Opt-in multi-agent input repair."),
    ("multi-agent/orphan", "Orphan delegation repair helper."),
    (
        "multi-agent/restore",
        "Multi-agent namespace restoration helper.",
    ),
    ("payload/apply", "Executor payload configuration rules."),
    ("usage/parse", "Usage accounting helper."),
    (
        "ttft/token-event",
        "First-token timing classification helper.",
    ),
    ("session/info", "Session metadata helper."),
    ("session/derive", "Session key derivation helper."),
    ("config-diff/details", "Configuration comparison fixtures."),
    (
        "config-save/steps",
        "Configuration save fixtures requiring an upstream example.",
    ),
    ("quota-signals/observe", "Credential quota signal parsing."),
];

/// Keys already encode upstream/client/direction. The few two-part keys
/// describe protocol helpers rather than request/response translations.
pub(crate) fn dimensions(key: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<_> = key.split('/').collect();
    match parts.as_slice() {
        [upstream, client, "request"] => Some((client, upstream, "request")),
        [upstream, client, "request-compat"] => Some((client, upstream, "request-compat")),
        [upstream, client, "response"] => Some((client, upstream, "streamed-response")),
        [upstream, client, "response-non-stream"] => Some((client, upstream, "whole-response")),
        ["interactions", "openai-responses", "tool-input-error"] => {
            Some(("openai-responses", "interactions", "response-error"))
        }
        ["completions", "request"] => Some(("completions", "openai", "request")),
        ["completions", "response"] => Some(("completions", "openai", "whole-response")),
        ["completions", "stream-chunk"] => Some(("completions", "openai", "streamed-response")),
        ["thinking", upstream] => Some(("reasoning settings", upstream, "reasoning")),
        ["codex-models", "list"] => {
            Some(("Codex model discovery", "registry models", "model-list"))
        }
        _ => None,
    }
}

pub(crate) fn catalog() -> Result<Value, String> {
    let args = Args::parse(
        ["--upstream", "unused-no-upstream-needed", "--random", "0"]
            .map(str::to_owned)
            .into_iter(),
    )?;
    let mut catalog = Vec::new();
    for (translator, handwritten, generated) in suite_registry(&args, true)? {
        if EXCLUDED.iter().any(|(key, _)| *key == translator.key()) {
            continue;
        }
        let (client, upstream, direction) = dimensions(translator.key())
            .ok_or_else(|| format!("Unclassified parity registry key: {}", translator.key()))?;
        debug_assert!(generated.is_empty());
        catalog.push(json!({
            "slug": translator.slug(),
            "label": translator.key(),
            "title": translator.title(),
            "client_format": client,
            "upstream_format": upstream,
            "direction": direction,
            "cases": handwritten.len() + cases::signature_relay::for_suite(translator.key()).len(),
        }));
    }
    catalog.sort_by(|left, right| left["label"].as_str().cmp(&right["label"].as_str()));
    Ok(json!(catalog))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Metadata transport only: this is not compatibility evidence.
    #[test]
    fn write_catalog_for_generator() {
        let catalog = catalog().unwrap();
        if let Some(path) = std::env::var_os("OPEN_FERRY_COMPATIBILITY_CATALOG") {
            std::fs::write(path, catalog.to_string()).unwrap();
        }
    }

    #[test]
    fn every_registered_suite_is_classified_or_explicitly_excluded() {
        let args = Args::parse(
            ["--upstream", "unused", "--random", "0"]
                .map(str::to_owned)
                .into_iter(),
        )
        .unwrap();
        let registered = suite_registry(&args, true).unwrap();
        let keys: BTreeSet<_> = registered
            .iter()
            .map(|(translator, _, _)| translator.key())
            .collect();
        for key in &keys {
            let excluded = EXCLUDED
                .iter()
                .any(|(entry, reason)| entry == key && !reason.is_empty());
            assert!(
                dimensions(key).is_some() ^ excluded,
                "Unclassified or conflicting registry key: {key}"
            );
        }
        assert_eq!(
            EXCLUDED.len(),
            EXCLUDED
                .iter()
                .map(|(key, _)| key)
                .collect::<BTreeSet<_>>()
                .len()
        );
        for (key, _) in EXCLUDED {
            assert!(keys.contains(key), "Stale parity exclusion: {key}");
        }
    }

    #[test]
    fn export_uses_registered_suites_without_an_upstream_checkout() {
        let catalog = catalog().unwrap();
        let entries = catalog.as_array().unwrap();
        assert!(!entries.is_empty());
        let mut slugs = BTreeSet::new();
        let mut directions = BTreeSet::new();
        for entry in entries {
            assert!(slugs.insert(entry["slug"].as_str().unwrap()));
            assert!(entry["cases"].as_u64().unwrap() > 0, "{entry}");
            directions.insert(entry["direction"].as_str().unwrap());
        }
        for direction in [
            "request",
            "streamed-response",
            "whole-response",
            "reasoning",
            "model-list",
        ] {
            assert!(directions.contains(direction));
        }
        assert_eq!(catalog, super::catalog().unwrap());
    }
}
