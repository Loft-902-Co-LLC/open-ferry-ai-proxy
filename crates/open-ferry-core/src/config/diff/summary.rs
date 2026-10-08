// Ported from CLIProxyAPI internal/watcher/diff/models_summary.go
// (SummarizeGeminiModels, SummarizeClaudeModels, SummarizeCodexModels,
// SummarizeVertexModels), oauth_excluded.go (SummarizeExcludedModels) and
// model_hash.go (ComputeExcludedModelsHash, thinkingHashSuffix,
// normalizeModelPairs, hashJoined) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Hashes of a key's model lists, so a change line can say a list changed
//! and how many entries it has without showing them.
//!
//! Deviations from upstream: none.

use std::collections::BTreeSet;

use open_ferry_translate::go::{json_string, to_lower};

use crate::auth::synthesizer::sha256_hex;
use crate::config::{ClaudeModel, CodexModel, GeminiModel, ThinkingSupport, VertexCompatModel};

/// A list's hash, empty for an empty list, and how many entries it counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Summary {
    pub(super) hash: String,
    pub(super) count: usize,
}

impl Summary {
    /// The summary of `keys`, already normalized: their hash joined by
    /// `separator`.
    pub(super) fn of(keys: &[String], separator: &str) -> Self {
        if keys.is_empty() {
            return Self::default();
        }
        Self {
            hash: sha256_hex(keys.join(separator).as_bytes()),
            count: keys.len(),
        }
    }
}

/// Upstream's `SummarizeGeminiModels`.
pub(super) fn gemini_models(models: &[GeminiModel]) -> Summary {
    model_pairs(models.iter().map(|model| {
        model_key(
            &model.name,
            &model.alias,
            &model.display_name,
            None,
            model.is_compat,
            model.thinking.as_ref(),
        )
    }))
}

/// Upstream's `SummarizeClaudeModels`.
pub(super) fn claude_models(models: &[ClaudeModel]) -> Summary {
    model_pairs(models.iter().map(|model| {
        model_key(
            &model.name,
            &model.alias,
            &model.display_name,
            None,
            model.is_compat,
            model.thinking.as_ref(),
        )
    }))
}

/// Upstream's `SummarizeCodexModels`.
pub(super) fn codex_models(models: &[CodexModel]) -> Summary {
    model_pairs(models.iter().map(|model| {
        model_key(
            &model.name,
            &model.alias,
            &model.display_name,
            Some(model.force_mapping),
            model.is_compat,
            model.thinking.as_ref(),
        )
    }))
}

/// Upstream's `SummarizeVertexModels`: the alias, or the name without
/// one, with the display name and thinking support. Unlike the others,
/// duplicates count and the keys are joined by `|`.
pub(super) fn vertex_models(models: &[VertexCompatModel]) -> Summary {
    let mut names: Vec<String> = models
        .iter()
        .filter_map(|model| {
            let name = model.name.trim();
            let alias = model.alias.trim();
            if name.is_empty() && alias.is_empty() {
                return None;
            }
            let shown = if alias.is_empty() { name } else { alias };
            Some(format!(
                "{shown}|{}{}",
                model.display_name.trim(),
                thinking_suffix(model.thinking.as_ref())
            ))
        })
        .collect();
    names.sort();
    Summary::of(&names, "|")
}

/// Upstream's `SummarizeExcludedModels`: the names trimmed, in lower case,
/// without duplicates.
pub(super) fn excluded_models(list: &[String]) -> Summary {
    let normalized: BTreeSet<String> = list
        .iter()
        .map(|entry| to_lower(entry.trim()))
        .filter(|entry| !entry.is_empty())
        .collect();
    if normalized.is_empty() {
        return Summary::default();
    }
    // ComputeExcludedModelsHash hashes the sorted list as Go writes it in
    // JSON.
    let json = format!(
        "[{}]",
        normalized
            .iter()
            .map(|entry| json_string(entry))
            .collect::<Vec<_>>()
            .join(",")
    );
    Summary {
        hash: sha256_hex(json.as_bytes()),
        count: normalized.len(),
    }
}

/// The key of one Gemini, Claude or Codex model; `force_mapping` is only
/// Codex's.
fn model_key(
    name: &str,
    alias: &str,
    display_name: &str,
    force_mapping: Option<bool>,
    is_compat: bool,
    thinking: Option<&ThinkingSupport>,
) -> Option<String> {
    let name = name.trim();
    let alias = alias.trim();
    if name.is_empty() && alias.is_empty() {
        return None;
    }
    let force_mapping = force_mapping
        .map(|force| format!("|force-mapping={force}"))
        .unwrap_or_default();
    Some(format!(
        "{}|{}|{}{force_mapping}|is-compat={is_compat}{}",
        to_lower(name),
        to_lower(alias),
        display_name.trim(),
        thinking_suffix(thinking)
    ))
}

/// Upstream's `normalizeModelPairs` and `hashJoined`: the keys without
/// duplicates, sorted, hashed joined by newlines.
fn model_pairs(keys: impl Iterator<Item = Option<String>>) -> Summary {
    let keys: BTreeSet<String> = keys.flatten().collect();
    Summary::of(&keys.into_iter().collect::<Vec<_>>(), "\n")
}

/// Upstream's `thinkingHashSuffix`: the thinking support as Go writes
/// `registry.ThinkingSupport` in JSON, `null` without it.
fn thinking_suffix(thinking: Option<&ThinkingSupport>) -> String {
    let Some(thinking) = thinking else {
        return "|thinking=null".to_owned();
    };
    let mut fields = Vec::new();
    if thinking.min != 0 {
        fields.push(format!("\"min\":{}", thinking.min));
    }
    if thinking.max != 0 {
        fields.push(format!("\"max\":{}", thinking.max));
    }
    if thinking.zero_allowed {
        fields.push("\"zero_allowed\":true".to_owned());
    }
    if thinking.dynamic_allowed {
        fields.push("\"dynamic_allowed\":true".to_owned());
    }
    if !thinking.levels.is_empty() {
        let levels: Vec<String> = thinking.levels.iter().map(|l| json_string(l)).collect();
        fields.push(format!("\"levels\":[{}]", levels.join(",")));
    }
    format!("|thinking={{{}}}", fields.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the thinking support as Go's encoding/json writes it.
    #[test]
    fn thinking_suffix_is_go_json() {
        assert_eq!(thinking_suffix(None), "|thinking=null");
        assert_eq!(
            thinking_suffix(Some(&ThinkingSupport::default())),
            "|thinking={}"
        );
        let thinking = ThinkingSupport {
            min: -1,
            max: 2048,
            zero_allowed: true,
            dynamic_allowed: true,
            levels: vec!["low".to_owned(), "<high>".to_owned()],
        };
        assert_eq!(
            thinking_suffix(Some(&thinking)),
            concat!(
                r#"|thinking={"min":-1,"max":2048,"zero_allowed":true,"#,
                r#""dynamic_allowed":true,"levels":["low","\u003chigh\u003e"]}"#
            )
        );
    }

    // Not upstream's: the hashes are upstream's, so equal lists in either
    // port hash alike.
    #[test]
    fn hashes_are_upstreams() {
        let excluded = excluded_models(&["B".to_owned(), " a ".to_owned(), "b".to_owned()]);
        assert_eq!(excluded.count, 2);
        assert_eq!(excluded.hash, sha256_hex(br#"["a","b"]"#));
        let models = gemini_models(&[GeminiModel {
            name: "M".to_owned(),
            ..GeminiModel::default()
        }]);
        assert_eq!(
            models.hash,
            sha256_hex(b"m|||is-compat=false|thinking=null")
        );
    }
}
