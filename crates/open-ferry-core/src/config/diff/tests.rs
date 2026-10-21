//! Tests of the config diff, ported from upstream's
//! internal/watcher/diff tests. Each module names the tests it ports and
//! the ones it drops.

mod config_diff;
mod cooling_override;
mod oauth_excluded;
mod oauth_model_alias;
mod oauth_request_scoped_errors;
mod oauth_settings;
mod openai_compat;

/// Upstream's `expectContains`: `list` holds `target` as one entry.
#[track_caller]
fn expect_contains(list: &[String], target: &str) {
    assert!(
        list.iter().any(|entry| entry == target),
        "expected list to contain {target:?}, got {list:#?}"
    );
}

/// A default config with what `set` changes.
fn config_with(set: impl FnOnce(&mut crate::config::Config)) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    set(&mut config);
    config
}

/// `items` as owned strings.
fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn web_search_cache_reports_all_settings() {
    let old = crate::config::Config::default();
    let mut new = old.clone();
    new.client.codex.web_search_result_cache = crate::config::CodexWebSearchResultCacheConfig {
        enabled: true,
        ttl: "30m".into(),
        limit: 128,
    };
    let changes = super::build_change_details(&old, &new);
    for key in ["enabled", "ttl", "limit"] {
        assert!(
            changes
                .iter()
                .any(|line| line
                    .starts_with(&format!("client.codex.web-search-result-cache.{key}:"))),
            "{changes:?}"
        );
    }
}
