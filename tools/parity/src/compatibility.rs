//! Machine-readable compatibility metadata from the actual suite registry.
//!
//! Counts are maintained hand-written cases (including signature-relay
//! cases), with seed 1 and no random cases. Export never runs translators or
//! Go and excludes configuration fixtures that read the upstream checkout.

use serde_json::{Value, json};

use crate::{Args, cases, suite_registry};

/// Keys already encode upstream/client/direction. The few two-part keys
/// describe protocol helpers rather than request/response translations.
pub(crate) fn dimensions(key: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<_> = key.split('/').collect();
    match parts.as_slice() {
        [upstream, client, "request"] => Some((client, upstream, "request")),
        [upstream, client, "request-compat"] => Some((client, upstream, "request-compat")),
        [upstream, client, "response"] => Some((client, upstream, "streamed-response")),
        [upstream, client, "response-non-stream"] => Some((client, upstream, "whole-response")),
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
        let Some((client, upstream, direction)) = dimensions(translator.key()) else {
            continue;
        };
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
