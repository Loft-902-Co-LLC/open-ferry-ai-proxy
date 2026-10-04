// Ported from CLIProxyAPI internal/watcher/diff/openai_compat.go
// (DiffOpenAICompatibility, uniqueOpenAICompatKey,
// describeOpenAICompatibilityUpdate, countAPIKeys, countOpenAIModels,
// openAICompatKey, openAICompatSignature) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The change lines of `openai-compatibility`: providers added, removed or
//! updated, matched by name, else base URL, else first model, else a hash
//! of what they hold. Key material is never shown, only counted.
//!
//! Deviations from upstream: none.

use std::collections::{BTreeMap, BTreeSet};

use open_ferry_translate::go::to_lower;

use super::{equal_string_map, format_optional_bool, format_optional_int, format_url};
use crate::auth::synthesizer::sha256_hex;
use crate::config::{OpenAiCompatibility, OpenAiCompatibilityModel};

/// Upstream's `DiffOpenAICompatibility`: one line per provider added,
/// removed or updated, in the order of their keys.
pub(super) fn diff(
    old_list: &[OpenAiCompatibility],
    new_list: &[OpenAiCompatibility],
) -> Vec<String> {
    let (old_map, old_labels) = keyed(old_list);
    let (new_map, new_labels) = keyed(new_list);
    let keys: BTreeSet<&String> = old_map.keys().chain(new_map.keys()).collect();
    let mut changes = Vec::new();
    for key in keys {
        let label = old_labels
            .get(key)
            .filter(|label| !label.is_empty())
            .or_else(|| new_labels.get(key))
            .map(String::as_str)
            .unwrap_or_default();
        match (old_map.get(key), new_map.get(key)) {
            (None, Some(new)) => changes.push(format!(
                "provider added: {label} (api-keys={}, models={})",
                count_api_keys(new),
                count_models(&new.models)
            )),
            (Some(old), None) => changes.push(format!(
                "provider removed: {label} (api-keys={}, models={})",
                count_api_keys(old),
                count_models(&old.models)
            )),
            (Some(old), Some(new)) => {
                if let Some(detail) = describe_update(old, new) {
                    changes.push(format!("provider updated: {label} {detail}"));
                }
            }
            (None, None) => {}
        }
    }
    changes
}

/// Each provider by its unique key, and its label.
fn keyed(
    list: &[OpenAiCompatibility],
) -> (
    BTreeMap<String, &OpenAiCompatibility>,
    BTreeMap<String, String>,
) {
    let mut map = BTreeMap::new();
    let mut labels = BTreeMap::new();
    for (index, entry) in list.iter().enumerate() {
        let (key, label) = unique_key(&map, entry, index);
        map.insert(key.clone(), entry);
        labels.insert(key, label);
    }
    (map, labels)
}

/// Upstream's `uniqueOpenAICompatKey`: the key, numbered when it is taken.
fn unique_key(
    existing: &BTreeMap<String, &OpenAiCompatibility>,
    entry: &OpenAiCompatibility,
    index: usize,
) -> (String, String) {
    let (base_key, label) = key(entry, index);
    let mut key = base_key.clone();
    let mut duplicate = 1;
    while existing.contains_key(&key) {
        key = format!("duplicate:{base_key}:{duplicate}");
        duplicate += 1;
    }
    (key, label)
}

/// Upstream's `describeOpenAICompatibilityUpdate`: what changed, in
/// parentheses, or `None`.
fn describe_update(old: &OpenAiCompatibility, new: &OpenAiCompatibility) -> Option<String> {
    let mut details = Vec::new();
    if old.disabled != new.disabled {
        details.push(format!("disabled {} -> {}", old.disabled, new.disabled));
    }
    if old.support_prompt_cache_key != new.support_prompt_cache_key {
        details.push(format!(
            "support-prompt-cache-key {} -> {}",
            old.support_prompt_cache_key, new.support_prompt_cache_key
        ));
    }
    if old.disable_cooling != new.disable_cooling {
        details.push(format!(
            "disable-cooling {} -> {}",
            format_optional_bool(old.disable_cooling),
            format_optional_bool(new.disable_cooling)
        ));
    }
    if old.request_retry != new.request_retry {
        details.push(format!(
            "request-retry {} -> {}",
            format_optional_int(old.request_retry),
            format_optional_int(new.request_retry)
        ));
    }
    let (old_keys, new_keys) = (count_api_keys(old), count_api_keys(new));
    if old_keys != new_keys {
        details.push(format!("api-keys {old_keys} -> {new_keys}"));
    }
    let (old_models, new_models) = (count_models(&old.models), count_models(&new.models));
    if old_models != new_models {
        details.push(format!("models {old_models} -> {new_models}"));
    }
    if !equal_string_map(&old.headers, &new.headers) {
        details.push("headers updated".to_owned());
    }
    (!details.is_empty()).then(|| format!("({})", details.join(", ")))
}

/// Upstream's `countAPIKeys`: the entries with a key.
pub(super) fn count_api_keys(entry: &OpenAiCompatibility) -> usize {
    entry
        .api_key_entries
        .iter()
        .filter(|key| !key.api_key.trim().is_empty())
        .count()
}

/// Upstream's `countOpenAIModels`: the models with a name or an alias.
pub(super) fn count_models(models: &[OpenAiCompatibilityModel]) -> usize {
    models
        .iter()
        .filter(|model| !model.name.trim().is_empty() || !model.alias.trim().is_empty())
        .count()
}

/// Upstream's `openAICompatKey`: the key a provider is matched by and the
/// label its lines show.
pub(super) fn key(entry: &OpenAiCompatibility, index: usize) -> (String, String) {
    let name = entry.name.trim();
    if !name.is_empty() {
        return (format!("name:{name}"), name.to_owned());
    }
    let base = entry.base_url.trim();
    if !base.is_empty() {
        return (format!("base:{base}"), format_url(base));
    }
    for model in &entry.models {
        let mut alias = model.alias.trim();
        if alias.is_empty() {
            alias = model.name.trim();
        }
        if !alias.is_empty() {
            return (format!("alias:{alias}"), alias.to_owned());
        }
    }
    let signature = signature(entry);
    if signature.is_empty() {
        return (format!("index:{index}"), format!("entry-{}", index + 1));
    }
    let short = signature.get(..8).unwrap_or(&signature);
    (format!("sig:{signature}"), format!("compat-{short}"))
}

/// Upstream's `openAICompatSignature`: a hash of the provider's name, base
/// URL, models, header names and key count, or empty when it has none.
pub(super) fn signature(entry: &OpenAiCompatibility) -> String {
    let mut parts = Vec::new();
    let name = entry.name.trim();
    if !name.is_empty() {
        parts.push(format!("name={}", to_lower(name)));
    }
    let base = entry.base_url.trim();
    if !base.is_empty() {
        parts.push(format!("base={base}"));
    }

    let mut models: Vec<String> = entry
        .models
        .iter()
        .filter_map(|model| {
            let name = model.name.trim();
            let alias = model.alias.trim();
            if name.is_empty() && alias.is_empty() {
                return None;
            }
            Some(format!(
                "{}|{}|{}|image={}",
                to_lower(name),
                to_lower(alias),
                model.display_name.trim(),
                model.image
            ))
        })
        .collect();
    if !models.is_empty() {
        models.sort();
        parts.push(format!("models={}", models.join(",")));
    }

    let mut headers: Vec<String> = entry
        .headers
        .keys()
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
        .map(to_lower)
        .collect();
    if !headers.is_empty() {
        headers.sort();
        parts.push(format!("headers={}", headers.join(",")));
    }

    // Key material is left out; only the keys are counted.
    let count = count_api_keys(entry);
    if count > 0 {
        parts.push(format!("api_keys={count}"));
    }

    if parts.is_empty() {
        return String::new();
    }
    sha256_hex(parts.join("|").as_bytes())
}
