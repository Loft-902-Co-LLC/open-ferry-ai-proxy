// Ported from CLIProxyAPI internal/watcher/diff/oauth_excluded.go
// (SummarizeOAuthExcludedModels, DiffOAuthExcludedModelChanges),
// oauth_model_alias.go (SummarizeOAuthModelAlias,
// DiffOAuthModelAliasChanges, summarizeOAuthModelAliasList),
// oauth_request_scoped_errors.go (SummarizeOAuthRequestScopedErrors,
// DiffOAuthRequestScopedErrorsChanges,
// summarizeOAuthRequestScopedErrorsList) and oauth_settings.go
// (SummarizeOAuthSettings, DiffOAuthSettingsChanges,
// summarizeOAuthSettingsList) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The change lines of the per-channel OAuth maps: `oauth-excluded-models`,
//! `oauth-model-alias`, `oauth-request-scoped-errors` and `oauth-settings`.
//! Each channel's list is summarized as a hash and a count, so a line says
//! a channel was added, removed or updated without showing its entries.
//!
//! Deviations from upstream:
//! - Where two channel names differ only in case or surrounding spaces,
//!   the one sorting last wins; upstream keeps whichever its map iteration
//!   gives last.

use std::collections::{BTreeMap, BTreeSet};

use open_ferry_translate::go::to_lower;

use super::summary::{self, Summary};
use crate::config::{OAuthModelAlias, OAuthModelSetting, RequestScopedErrorRule};

/// Upstream's `SummarizeOAuthExcludedModels`.
pub(super) fn summarize_excluded(
    entries: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Summary> {
    summarize(entries, summary::excluded_models)
}

/// Upstream's `DiffOAuthExcludedModelChanges`: the change lines and the
/// channels they name.
pub(super) fn diff_excluded(
    old: &BTreeMap<String, Vec<String>>,
    new: &BTreeMap<String, Vec<String>>,
) -> (Vec<String>, Vec<String>) {
    diff(
        "oauth-excluded-models",
        &summarize_excluded(old),
        &summarize_excluded(new),
    )
}

/// Upstream's `SummarizeOAuthModelAlias`.
pub(super) fn summarize_model_alias(
    entries: &BTreeMap<String, Vec<OAuthModelAlias>>,
) -> BTreeMap<String, Summary> {
    summarize(entries, |list| {
        let keys: BTreeSet<String> = list
            .iter()
            .filter_map(|alias| {
                let name = to_lower(alias.name.trim());
                let alias_value = to_lower(alias.alias.trim());
                if name.is_empty() || alias_value.is_empty() {
                    return None;
                }
                let mut key = format!("{name}->{alias_value}");
                if alias.fork {
                    key.push_str("|fork");
                }
                let display_name = alias.display_name.trim();
                if !display_name.is_empty() {
                    key.push_str("|display-name=");
                    key.push_str(display_name);
                }
                if alias.force_mapping {
                    key.push_str("|force-mapping");
                }
                Some(key)
            })
            .collect();
        Summary::of(&keys.into_iter().collect::<Vec<_>>(), "|")
    })
}

/// Upstream's `DiffOAuthModelAliasChanges`.
pub(super) fn diff_model_alias(
    old: &BTreeMap<String, Vec<OAuthModelAlias>>,
    new: &BTreeMap<String, Vec<OAuthModelAlias>>,
) -> (Vec<String>, Vec<String>) {
    diff(
        "oauth-model-alias",
        &summarize_model_alias(old),
        &summarize_model_alias(new),
    )
}

/// Upstream's `SummarizeOAuthRequestScopedErrors`: only rules with a
/// status, something to match and an action count.
pub(super) fn summarize_request_scoped_errors(
    entries: &BTreeMap<String, Vec<RequestScopedErrorRule>>,
) -> BTreeMap<String, Summary> {
    summarize(entries, |list| {
        let mut text = String::new();
        let mut valid = 0;
        for rule in list {
            if rule.status <= 0
                || (rule.matches.is_empty() && rule.match_regexr.is_empty())
                || rule.action.is_empty()
            {
                continue;
            }
            valid += 1;
            text.push_str(&format!(
                "{}|{}|{}|{}\n",
                rule.status,
                rule.matches.join(","),
                rule.match_regexr.join(","),
                rule.action
            ));
        }
        if valid == 0 {
            return Summary::default();
        }
        Summary {
            hash: crate::auth::synthesizer::sha256_hex(text.as_bytes()),
            count: valid,
        }
    })
}

/// Upstream's `DiffOAuthRequestScopedErrorsChanges`.
pub(super) fn diff_request_scoped_errors(
    old: &BTreeMap<String, Vec<RequestScopedErrorRule>>,
    new: &BTreeMap<String, Vec<RequestScopedErrorRule>>,
) -> (Vec<String>, Vec<String>) {
    diff(
        "oauth-request-scoped-errors",
        &summarize_request_scoped_errors(old),
        &summarize_request_scoped_errors(new),
    )
}

/// Upstream's `SummarizeOAuthSettings`. The settings keep their order, so
/// reordering them is a change.
pub(super) fn summarize_settings(
    entries: &BTreeMap<String, Vec<OAuthModelSetting>>,
) -> BTreeMap<String, Summary> {
    summarize(entries, |list| {
        let mut seen = BTreeSet::new();
        let mut keys = Vec::new();
        for setting in list {
            let name = to_lower(setting.name.trim());
            if name.is_empty() {
                continue;
            }
            let mut key = format!("{name}->{}", to_lower(setting.alias.trim()));
            if setting.max_context_length > 0 {
                key.push_str(&format!(
                    "|max-context-length={}",
                    setting.max_context_length
                ));
            }
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }
        Summary::of(&keys, "|")
    })
}

/// Upstream's `DiffOAuthSettingsChanges`.
pub(super) fn diff_settings(
    old: &BTreeMap<String, Vec<OAuthModelSetting>>,
    new: &BTreeMap<String, Vec<OAuthModelSetting>>,
) -> (Vec<String>, Vec<String>) {
    diff(
        "oauth-settings",
        &summarize_settings(old),
        &summarize_settings(new),
    )
}

/// Each channel's summary, keyed by the channel trimmed and in lower case;
/// a blank channel is left out.
fn summarize<T>(
    entries: &BTreeMap<String, Vec<T>>,
    summarize_list: impl Fn(&[T]) -> Summary,
) -> BTreeMap<String, Summary> {
    entries
        .iter()
        .filter_map(|(channel, list)| {
            let key = to_lower(channel.trim());
            (!key.is_empty()).then(|| (key, summarize_list(list)))
        })
        .collect()
}

/// The lines for the channels added, removed or updated, sorted, and the
/// channels, sorted.
fn diff(
    section: &str,
    old: &BTreeMap<String, Summary>,
    new: &BTreeMap<String, Summary>,
) -> (Vec<String>, Vec<String>) {
    let mut changes = Vec::new();
    let mut affected = Vec::new();
    for key in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        let line = match (old.get(key), new.get(key)) {
            (Some(_), None) => format!("{section}[{key}]: removed"),
            (None, Some(new)) => format!("{section}[{key}]: added ({} entries)", new.count),
            (Some(old), Some(new)) if old.hash != new.hash => format!(
                "{section}[{key}]: updated ({} -> {} entries)",
                old.count, new.count
            ),
            _ => continue,
        };
        changes.push(line);
        affected.push(key.clone());
    }
    changes.sort();
    affected.sort();
    (changes, affected)
}
