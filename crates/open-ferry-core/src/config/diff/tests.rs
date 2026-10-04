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
