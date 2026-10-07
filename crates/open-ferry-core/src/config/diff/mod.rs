// Ported from CLIProxyAPI internal/watcher/diff/config_diff.go
// (BuildConfigChangeDetails, trimStrings, appendPayloadConfigChanges,
// appendPayloadRuleChanges, appendPayloadFilterRuleChanges,
// appendOptionalIntChange, appendOptionalBoolChange, formatOptionalBool,
// formatOptionalInt, equalStringMap, formatProxyURL, formatURL) and the
// change logging of internal/watcher/config_reload.go (reloadConfig)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What a reload changed, as the lines upstream logs after
//! `config changes detected:`.
//!
//! A line names a setting and shows its old and new value, or only says it
//! changed. Secrets never show: API keys and the management key are only
//! said to be created, updated or deleted, key lists are counted, header
//! values are left out, and a base or proxy URL shows only its scheme and
//! host (`format_url`), so no user information, path or query.
//!
//! Deviations from upstream:
//! - Only the settings open-ferry types have lines. The sections it reads
//!   and ignores (pprof, cloaking and `claude-code`, fingerprints,
//!   Antigravity, Devin, Codex live media relay and
//!   `disable-codex-cloaking`) have none.
//! - open-ferry's `claude-cli` list, which upstream doesn't have, gets
//!   lines in the style of the key lists: its count, or each entry's
//!   changed settings as `claude-cli[0].timeout: 5m -> 10m`.
//! - Go tells a list or map that is missing from one that is empty, and
//!   reports `payload.default: []` against no `payload.default` as an
//!   update (0 -> 0 rules); the typed config can't tell them apart, so no
//!   line is given. A payload value in a mapping with a key that isn't a
//!   string is equal to any other such value.

pub mod go_url;
mod oauth;
mod openai_compat;
mod summary;

use std::collections::BTreeMap;

use super::{AnyValue, Config, PayloadRule};

#[cfg(test)]
mod tests;

/// The changes from `old` to `new`, one readable line each, with secrets
/// left out (upstream's `BuildConfigChangeDetails`).
pub fn build_change_details(old: &Config, new: &Config) -> Vec<String> {
    let mut changes = Changes::default();

    // Simple scalars.
    changes.flag(
        "client.codex.enable-apply-patch",
        old.client.codex.enable_apply_patch,
        new.client.codex.enable_apply_patch,
    );
    changes.int("port", old.port, new.port);
    changes.text("auth-dir", &old.auth_dir, &new.auth_dir);
    changes.flag("debug", old.debug, new.debug);
    changes.flag("logging-to-file", old.logging_to_file, new.logging_to_file);
    changes.flag(
        "usage-statistics-enabled",
        old.usage_statistics_enabled,
        new.usage_statistics_enabled,
    );
    changes.int(
        "redis-usage-queue-retention-seconds",
        old.redis_usage_queue_retention_seconds,
        new.redis_usage_queue_retention_seconds,
    );
    changes.flag("disable-cooling", old.disable_cooling, new.disable_cooling);
    changes.flag(
        "save-cooldown-status",
        old.save_cooldown_status,
        new.save_cooldown_status,
    );
    changes.int(
        "transient-error-cooldown-seconds",
        old.transient_error_cooldown_seconds,
        new.transient_error_cooldown_seconds,
    );
    changes.text(
        "disable-image-generation",
        old.disable_image_generation.as_str(),
        new.disable_image_generation.as_str(),
    );
    changes.trimmed(
        "gpt-image-2-base-model",
        &old.gpt_image_2_base_model,
        &new.gpt_image_2_base_model,
    );
    changes.flag("request-log", old.request_log, new.request_log);
    changes.int(
        "logs-max-total-size-mb",
        old.logs_max_total_size_mb,
        new.logs_max_total_size_mb,
    );
    changes.int(
        "error-logs-max-files",
        old.error_logs_max_files,
        new.error_logs_max_files,
    );
    changes.int("request-retry", old.request_retry, new.request_retry);
    changes.int(
        "max-retry-credentials",
        old.max_retry_credentials,
        new.max_retry_credentials,
    );
    changes.int(
        "max-retry-interval",
        old.max_retry_interval,
        new.max_retry_interval,
    );
    if old.proxy_url != new.proxy_url {
        changes.push(format!(
            "proxy-url: {} -> {}",
            format_url(&old.proxy_url),
            format_url(&new.proxy_url)
        ));
    }
    changes.flag("ws-auth", old.ws_auth, new.ws_auth);
    changes.flag(
        "force-model-prefix",
        old.force_model_prefix,
        new.force_model_prefix,
    );
    changes.int(
        "nonstream-keepalive-interval",
        old.nonstream_keepalive_interval,
        new.nonstream_keepalive_interval,
    );

    // Quota-exceeded behavior.
    let (old_quota, new_quota) = (&old.quota_exceeded, &new.quota_exceeded);
    changes.flag(
        "quota-exceeded.switch-project",
        old_quota.switch_project,
        new_quota.switch_project,
    );
    changes.flag(
        "quota-exceeded.switch-preview-model",
        old_quota.switch_preview_model,
        new_quota.switch_preview_model,
    );
    changes.flag(
        "quota-exceeded.antigravity-credits",
        old_quota.antigravity_credits,
        new_quota.antigravity_credits,
    );

    changes.flag(
        "codex.stream-bootstrap-buffering",
        old.codex.stream_bootstrap_buffering,
        new.codex.stream_bootstrap_buffering,
    );
    changes.trimmed(
        "codex.stream-bootstrap-timeout",
        &old.codex.stream_bootstrap_timeout,
        &new.codex.stream_bootstrap_timeout,
    );
    changes.flag(
        "client.codex.optimize-multi-agent-v2",
        old.client.codex.optimize_multi_agent_v2,
        new.client.codex.optimize_multi_agent_v2,
    );
    changes.flag(
        "codex.orphan-delegation-compatibility",
        old.codex.orphan_delegation_compatibility,
        new.codex.orphan_delegation_compatibility,
    );
    changes.flag(
        "xai.inject-x-search",
        old.xai.inject_x_search,
        new.xai.inject_x_search,
    );

    changes.text(
        "routing.strategy",
        &old.routing.strategy,
        &new.routing.strategy,
    );
    let (old_payload, new_payload) = (&old.payload, &new.payload);
    changes.payload("default", &old_payload.default, &new_payload.default);
    changes.payload(
        "default-raw",
        &old_payload.default_raw,
        &new_payload.default_raw,
    );
    changes.payload("override", &old_payload.r#override, &new_payload.r#override);
    changes.payload(
        "override-raw",
        &old_payload.override_raw,
        &new_payload.override_raw,
    );
    if old_payload.filter != new_payload.filter {
        changes.push(format!(
            "payload.filter: updated ({} -> {} rules)",
            old_payload.filter.len(),
            new_payload.filter.len()
        ));
    }

    // API keys (redacted) and counts.
    if old.api_keys.len() != new.api_keys.len() {
        changes.push(format!(
            "api-keys count: {} -> {}",
            old.api_keys.len(),
            new.api_keys.len()
        ));
    } else if !old
        .api_keys
        .iter()
        .zip(&new.api_keys)
        .all(|(old, new)| old.trim() == new.trim())
    {
        changes.push("api-keys: values updated (count unchanged, redacted)".to_owned());
    }

    if old.gemini_api_key.len() != new.gemini_api_key.len() {
        changes.count(
            "gemini-api-key",
            old.gemini_api_key.len(),
            new.gemini_api_key.len(),
        );
    } else {
        for (i, (o, n)) in old
            .gemini_api_key
            .iter()
            .zip(&new.gemini_api_key)
            .enumerate()
        {
            let field = |name: &str| format!("gemini[{i}].{name}");
            changes.url(&field("base-url"), &o.base_url, &n.base_url);
            changes.url(&field("proxy-url"), &o.proxy_url, &n.proxy_url);
            changes.trimmed(&field("prefix"), &o.prefix, &n.prefix);
            changes.optional_bool(
                &field("disable-cooling"),
                o.disable_cooling,
                n.disable_cooling,
            );
            changes.secret(&field("api-key"), &o.api_key, &n.api_key);
            changes.headers(&field("headers"), &o.headers, &n.headers);
            changes.summary(
                &field("models"),
                &summary::gemini_models(&o.models),
                &summary::gemini_models(&n.models),
            );
            changes.excluded(
                &field("excluded-models"),
                &o.excluded_models,
                &n.excluded_models,
            );
            changes.optional_int(&field("request-retry"), o.request_retry, n.request_retry);
        }
    }

    if old.interactions_api_key.len() != new.interactions_api_key.len() {
        changes.count(
            "interactions-api-key",
            old.interactions_api_key.len(),
            new.interactions_api_key.len(),
        );
    } else {
        for (i, (o, n)) in old
            .interactions_api_key
            .iter()
            .zip(&new.interactions_api_key)
            .enumerate()
        {
            let field = |name: &str| format!("interactions[{i}].{name}");
            changes.url(&field("base-url"), &o.base_url, &n.base_url);
            changes.url(&field("proxy-url"), &o.proxy_url, &n.proxy_url);
            changes.trimmed(&field("prefix"), &o.prefix, &n.prefix);
            changes.optional_bool(
                &field("disable-cooling"),
                o.disable_cooling,
                n.disable_cooling,
            );
            changes.secret(&field("api-key"), &o.api_key, &n.api_key);
            changes.headers(&field("headers"), &o.headers, &n.headers);
            changes.summary(
                &field("models"),
                &summary::gemini_models(&o.models),
                &summary::gemini_models(&n.models),
            );
            changes.excluded(
                &field("excluded-models"),
                &o.excluded_models,
                &n.excluded_models,
            );
            changes.optional_int(&field("request-retry"), o.request_retry, n.request_retry);
        }
    }

    // Claude keys (no key material).
    if old.claude_api_key.len() != new.claude_api_key.len() {
        changes.count(
            "claude-api-key",
            old.claude_api_key.len(),
            new.claude_api_key.len(),
        );
    } else {
        for (i, (o, n)) in old
            .claude_api_key
            .iter()
            .zip(&new.claude_api_key)
            .enumerate()
        {
            let field = |name: &str| format!("claude[{i}].{name}");
            changes.url(&field("base-url"), &o.base_url, &n.base_url);
            changes.url(&field("proxy-url"), &o.proxy_url, &n.proxy_url);
            changes.trimmed(&field("prefix"), &o.prefix, &n.prefix);
            changes.optional_bool(
                &field("disable-cooling"),
                o.disable_cooling,
                n.disable_cooling,
            );
            changes.secret(&field("api-key"), &o.api_key, &n.api_key);
            changes.headers(&field("headers"), &o.headers, &n.headers);
            changes.summary(
                &field("models"),
                &summary::claude_models(&o.models),
                &summary::claude_models(&n.models),
            );
            changes.excluded(
                &field("excluded-models"),
                &o.excluded_models,
                &n.excluded_models,
            );
            changes.flag(
                &field("rebuild-mid-system-message"),
                o.rebuild_mid_system_message,
                n.rebuild_mid_system_message,
            );
            changes.optional_int(&field("request-retry"), o.request_retry, n.request_retry);
        }
    }

    // Codex keys (no key material).
    if old.codex_api_key.len() != new.codex_api_key.len() {
        changes.count(
            "codex-api-key",
            old.codex_api_key.len(),
            new.codex_api_key.len(),
        );
    } else {
        for (i, (o, n)) in old.codex_api_key.iter().zip(&new.codex_api_key).enumerate() {
            let field = |name: &str| format!("codex[{i}].{name}");
            changes.url(&field("base-url"), &o.base_url, &n.base_url);
            changes.url(&field("proxy-url"), &o.proxy_url, &n.proxy_url);
            changes.trimmed(&field("prefix"), &o.prefix, &n.prefix);
            changes.flag(&field("websockets"), o.websockets, n.websockets);
            changes.flag(&field("alpha-search"), o.alpha_search, n.alpha_search);
            changes.optional_bool(
                &field("disable-cooling"),
                o.disable_cooling,
                n.disable_cooling,
            );
            changes.secret(&field("api-key"), &o.api_key, &n.api_key);
            changes.headers(&field("headers"), &o.headers, &n.headers);
            changes.summary(
                &field("models"),
                &summary::codex_models(&o.models),
                &summary::codex_models(&n.models),
            );
            changes.excluded(
                &field("excluded-models"),
                &o.excluded_models,
                &n.excluded_models,
            );
            changes.optional_int(&field("request-retry"), o.request_retry, n.request_retry);
        }
    }

    // xAI keys (no key material).
    if old.xai_api_key.len() != new.xai_api_key.len() {
        changes.count("xai-api-key", old.xai_api_key.len(), new.xai_api_key.len());
    } else {
        for (i, (o, n)) in old.xai_api_key.iter().zip(&new.xai_api_key).enumerate() {
            let field = |name: &str| format!("xai[{i}].{name}");
            changes.url(&field("base-url"), &o.base_url, &n.base_url);
            changes.url(&field("proxy-url"), &o.proxy_url, &n.proxy_url);
            changes.trimmed(&field("prefix"), &o.prefix, &n.prefix);
            changes.int(&field("priority"), o.priority, n.priority);
            changes.flag(&field("websockets"), o.websockets, n.websockets);
            changes.optional_bool(
                &field("disable-cooling"),
                o.disable_cooling,
                n.disable_cooling,
            );
            changes.optional_int(&field("request-retry"), o.request_retry, n.request_retry);
            changes.secret(&field("api-key"), &o.api_key, &n.api_key);
            changes.headers(&field("headers"), &o.headers, &n.headers);
            changes.summary(
                &field("models"),
                &summary::codex_models(&o.models),
                &summary::codex_models(&n.models),
            );
            changes.excluded(
                &field("excluded-models"),
                &o.excluded_models,
                &n.excluded_models,
            );
        }
    }

    // Meta keys (no key material). The prefix is compared trimmed but shown
    // as written, as upstream does.
    if old.meta_api_key.len() != new.meta_api_key.len() {
        changes.count(
            "meta-api-key",
            old.meta_api_key.len(),
            new.meta_api_key.len(),
        );
    } else {
        for (i, (o, n)) in old.meta_api_key.iter().zip(&new.meta_api_key).enumerate() {
            let field = |name: &str| format!("meta[{i}].{name}");
            changes.url(&field("base-url"), &o.base_url, &n.base_url);
            changes.url(&field("proxy-url"), &o.proxy_url, &n.proxy_url);
            if o.prefix.trim() != n.prefix.trim() {
                changes.push(format!("{}: {} -> {}", field("prefix"), o.prefix, n.prefix));
            }
            changes.int(&field("priority"), o.priority, n.priority);
            changes.optional_bool(
                &field("disable-cooling"),
                o.disable_cooling,
                n.disable_cooling,
            );
            changes.optional_int(&field("request-retry"), o.request_retry, n.request_retry);
            changes.secret(&field("api-key"), &o.api_key, &n.api_key);
            changes.headers(&field("headers"), &o.headers, &n.headers);
            changes.summary(
                &field("models"),
                &summary::codex_models(&o.models),
                &summary::codex_models(&n.models),
            );
            changes.excluded(
                &field("excluded-models"),
                &o.excluded_models,
                &n.excluded_models,
            );
        }
    }

    // open-ferry's claude-cli entries (they hold no secrets).
    if old.claude_cli.len() != new.claude_cli.len() {
        changes.count("claude-cli", old.claude_cli.len(), new.claude_cli.len());
    } else {
        for (i, (o, n)) in old.claude_cli.iter().zip(&new.claude_cli).enumerate() {
            let field = |name: &str| format!("claude-cli[{i}].{name}");
            changes.text(&field("name"), &o.name, &n.name);
            changes.text(&field("command"), &o.command, &n.command);
            changes.text(&field("config-dir"), &o.config_dir, &n.config_dir);
            changes.text(&field("system-prompt"), &o.system_prompt, &n.system_prompt);
            changes.int(
                &field("max-concurrency"),
                o.max_concurrency,
                n.max_concurrency,
            );
            changes.text(&field("timeout"), &o.timeout, &n.timeout);
            changes.trimmed(&field("prefix"), &o.prefix, &n.prefix);
            changes.int(&field("priority"), o.priority, n.priority);
            changes.optional_int(&field("weight"), o.weight, n.weight);
            changes.flag(&field("disabled"), o.disabled, n.disabled);
            changes.summary(
                &field("models"),
                &summary::claude_models(&o.models),
                &summary::claude_models(&n.models),
            );
            changes.excluded(
                &field("excluded-models"),
                &o.excluded_models,
                &n.excluded_models,
            );
        }
    }

    for (entries, _) in [
        oauth::diff_excluded(&old.oauth_excluded_models, &new.oauth_excluded_models),
        oauth::diff_model_alias(&old.oauth_model_alias, &new.oauth_model_alias),
        oauth::diff_request_scoped_errors(
            &old.oauth_request_scoped_errors,
            &new.oauth_request_scoped_errors,
        ),
        oauth::diff_settings(&old.oauth_settings, &new.oauth_settings),
    ] {
        changes.0.extend(entries);
    }

    // Remote management (never the key).
    let (old_remote, new_remote) = (&old.remote_management, &new.remote_management);
    changes.flag(
        "remote-management.allow-remote",
        old_remote.allow_remote,
        new_remote.allow_remote,
    );
    changes.flag(
        "remote-management.disable-control-panel",
        old_remote.disable_control_panel,
        new_remote.disable_control_panel,
    );
    changes.flag(
        "remote-management.disable-auto-update-panel",
        old_remote.disable_auto_update_panel,
        new_remote.disable_auto_update_panel,
    );
    changes.url(
        "remote-management.panel-github-repository",
        &old_remote.panel_github_repository,
        &new_remote.panel_github_repository,
    );
    changes.url(
        "remote-management.base-url",
        &old_remote.base_url,
        &new_remote.base_url,
    );
    if old_remote.secret_key != new_remote.secret_key {
        let what = match (
            old_remote.secret_key.is_empty(),
            new_remote.secret_key.is_empty(),
        ) {
            (true, false) => "created",
            (false, true) => "deleted",
            _ => "updated",
        };
        changes.push(format!("remote-management.secret-key: {what}"));
    }

    // OpenAI-compatible providers (summarized).
    let compat = openai_compat::diff(&old.openai_compatibility, &new.openai_compatibility);
    if !compat.is_empty() {
        changes.push("openai-compatibility:".to_owned());
        changes
            .0
            .extend(compat.into_iter().map(|line| format!("  {line}")));
    }

    // Vertex-compatible keys.
    if old.vertex_api_key.len() != new.vertex_api_key.len() {
        changes.count(
            "vertex-api-key",
            old.vertex_api_key.len(),
            new.vertex_api_key.len(),
        );
    } else {
        for (i, (o, n)) in old
            .vertex_api_key
            .iter()
            .zip(&new.vertex_api_key)
            .enumerate()
        {
            let field = |name: &str| format!("vertex[{i}].{name}");
            changes.url(&field("base-url"), &o.base_url, &n.base_url);
            changes.url(&field("proxy-url"), &o.proxy_url, &n.proxy_url);
            changes.trimmed(&field("prefix"), &o.prefix, &n.prefix);
            changes.optional_bool(
                &field("disable-cooling"),
                o.disable_cooling,
                n.disable_cooling,
            );
            changes.secret(&field("api-key"), &o.api_key, &n.api_key);
            changes.summary(
                &field("models"),
                &summary::vertex_models(&o.models),
                &summary::vertex_models(&n.models),
            );
            changes.excluded(
                &field("excluded-models"),
                &o.excluded_models,
                &n.excluded_models,
            );
            changes.headers(&field("headers"), &o.headers, &n.headers);
            changes.optional_int(&field("request-retry"), o.request_retry, n.request_retry);
        }
    }

    changes.0
}

/// Logs what a reload changed, as upstream's `reloadConfig` does once it
/// has the new config: `config changes detected:` and then each change,
/// indented, at info level, or a debug line when nothing changed.
pub fn log_changes(previous: &Config, config: &Config) {
    let details = build_change_details(previous, config);
    if details.is_empty() {
        tracing::debug!("no material config field changes detected");
        return;
    }
    tracing::info!("config changes detected:");
    for detail in details {
        tracing::info!("  {detail}");
    }
}

/// The change lines so far.
#[derive(Default)]
struct Changes(Vec<String>);

impl Changes {
    fn push(&mut self, line: String) {
        self.0.push(line);
    }

    /// A switch, with both values.
    fn flag(&mut self, name: &str, old: bool, new: bool) {
        if old != new {
            self.push(format!("{name}: {old} -> {new}"));
        }
    }

    /// A number, with both values.
    fn int(&mut self, name: &str, old: i64, new: i64) {
        if old != new {
            self.push(format!("{name}: {old} -> {new}"));
        }
    }

    /// A text, with both values as written.
    fn text(&mut self, name: &str, old: &str, new: &str) {
        if old != new {
            self.push(format!("{name}: {old} -> {new}"));
        }
    }

    /// A text, compared and shown trimmed.
    fn trimmed(&mut self, name: &str, old: &str, new: &str) {
        self.text(name, old.trim(), new.trim());
    }

    /// A URL, compared trimmed and shown as its scheme and host.
    fn url(&mut self, name: &str, old: &str, new: &str) {
        if old.trim() != new.trim() {
            self.push(format!(
                "{name}: {} -> {}",
                format_url(old),
                format_url(new)
            ));
        }
    }

    /// A secret, compared trimmed and never shown.
    fn secret(&mut self, name: &str, old: &str, new: &str) {
        if old.trim() != new.trim() {
            self.push(format!("{name}: updated"));
        }
    }

    /// The length of a key list.
    fn count(&mut self, name: &str, old: usize, new: usize) {
        self.push(format!("{name} count: {old} -> {new}"));
    }

    /// Header values are never shown.
    fn headers(
        &mut self,
        name: &str,
        old: &BTreeMap<String, String>,
        new: &BTreeMap<String, String>,
    ) {
        if !equal_string_map(old, new) {
            self.push(format!("{name}: updated"));
        }
    }

    /// A model list, by its summary.
    fn summary(&mut self, name: &str, old: &summary::Summary, new: &summary::Summary) {
        if old.hash != new.hash {
            self.push(format!(
                "{name}: updated ({} -> {} entries)",
                old.count, new.count
            ));
        }
    }

    /// An excluded-model list, by its summary.
    fn excluded(&mut self, name: &str, old: &[String], new: &[String]) {
        self.summary(
            name,
            &summary::excluded_models(old),
            &summary::excluded_models(new),
        );
    }

    /// Upstream's `appendOptionalBoolChange`.
    fn optional_bool(&mut self, name: &str, old: Option<bool>, new: Option<bool>) {
        if old != new {
            self.push(format!(
                "{name}: {} -> {}",
                format_optional_bool(old),
                format_optional_bool(new)
            ));
        }
    }

    /// Upstream's `appendOptionalIntChange`.
    fn optional_int(&mut self, name: &str, old: Option<i64>, new: Option<i64>) {
        if old != new {
            self.push(format!(
                "{name}: {} -> {}",
                format_optional_int(old),
                format_optional_int(new)
            ));
        }
    }

    /// Upstream's `appendPayloadRuleChanges`: a section's rule count when
    /// its rules changed.
    fn payload(&mut self, section: &str, old: &[PayloadRule], new: &[PayloadRule]) {
        let equal = old.len() == new.len()
            && old.iter().zip(new).all(|(old, new)| {
                old.models == new.models && params_equal(&old.params, &new.params)
            });
        if !equal {
            self.push(format!(
                "payload.{section}: updated ({} -> {} rules)",
                old.len(),
                new.len()
            ));
        }
    }
}

/// Whether two rules' params are equal as the Go maps they are upstream:
/// the same paths with the same values, in any order.
fn params_equal(old: &[(String, AnyValue)], new: &[(String, AnyValue)]) -> bool {
    old.len() == new.len()
        && old
            .iter()
            .all(|(path, value)| new.iter().any(|(p, v)| p == path && v == value))
}

/// Upstream's `formatOptionalBool`: `inherit` when unset.
fn format_optional_bool(value: Option<bool>) -> String {
    value.map_or_else(|| "inherit".to_owned(), |value| value.to_string())
}

/// Upstream's `formatOptionalInt`: `<unset>` when unset.
fn format_optional_int(value: Option<i64>) -> String {
    value.map_or_else(|| "<unset>".to_owned(), |value| value.to_string())
}

/// Upstream's `equalStringMap`: as Go reads a missing key as an empty
/// value, maps of one size whose other keys all hold empty values are
/// equal.
fn equal_string_map(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(key, value)| b.get(key).map_or("", String::as_str) == value)
}

/// Upstream's `formatURL` (and `formatProxyURL`): the scheme and host of a
/// URL, the host alone for `host:port`, `<none>` when blank and
/// `<redacted>` when it has no host or Go can't parse it.
fn format_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "<none>".to_owned();
    }
    let Some(parsed) = go_url::parse(trimmed.as_bytes()) else {
        return "<redacted>".to_owned();
    };
    let mut host = parsed.host;
    let mut scheme = parsed.scheme;
    if trim_bytes(&host).is_empty() {
        // Allow host:port without a scheme.
        host = go_url::parse(format!("http://{trimmed}").as_bytes())
            .map(|parsed| parsed.host)
            .unwrap_or_default();
        scheme = String::new();
    }
    let host = String::from_utf8_lossy(trim_bytes(&host)).into_owned();
    let scheme = scheme.trim();
    if host.is_empty() {
        "<redacted>".to_owned()
    } else if scheme.is_empty() {
        host
    } else {
        format!("{scheme}://{host}")
    }
}

/// Go's `strings.TrimSpace` of a host's bytes.
fn trim_bytes(bytes: &[u8]) -> &[u8] {
    open_ferry_translate::go::trim_space(bytes)
}
