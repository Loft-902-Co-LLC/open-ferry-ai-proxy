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
fn ingress_limits_each_have_a_reload_change_line() {
    let old = crate::config::Config::default();
    let mut new = old.clone();
    new.ingress_limits.max_connections = 7;
    new.ingress_limits.header_timeout_seconds = 7;
    new.ingress_limits.tls_handshake_timeout_seconds = 7;
    new.ingress_limits.idle_timeout_seconds = 7;
    new.ingress_limits.body_idle_timeout_seconds = 7;
    new.ingress_limits.body_total_timeout_seconds = 7;
    new.ingress_limits.buffered_body_bytes = 7;
    new.ingress_limits.write_stall_timeout_seconds = 7;
    let changes = super::build_change_details(&old, &new);
    expect_contains(&changes, "ingress-limits.max-connections: 0 -> 7");
    expect_contains(&changes, "ingress-limits.header-timeout-seconds: 0 -> 7");
    expect_contains(&changes, "ingress-limits.tls-handshake-timeout-seconds: 0 -> 7");
    expect_contains(&changes, "ingress-limits.idle-timeout-seconds: 0 -> 7");
    expect_contains(&changes, "ingress-limits.body-idle-timeout-seconds: 0 -> 7");
    expect_contains(&changes, "ingress-limits.body-total-timeout-seconds: 0 -> 7");
    expect_contains(&changes, "ingress-limits.buffered-body-bytes: 0 -> 7");
    expect_contains(&changes, "ingress-limits.write-stall-timeout-seconds: 0 -> 7");
}
