// Ported from CLIProxyAPI internal/config/config_v8.go (the v8 path tables,
// flattenV8, normalizeV8PrivateIPAlias, expandV8Groups), weight.go
// (validateCredentialWeightYAML and its helpers) and
// internal/credentialweight/weight.go (Normalize) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Reads the v8 config layout by moving it into the legacy one.
//!
//! Upstream's v8 layout groups settings (`server.port`, `routing.retry`,
//! `api-keys.codex[].keys[]`) where the legacy layout kept them at the top
//! level. Both stay readable, and a document may mix them: [`flatten_v8`]
//! rewrites the tree into the legacy layout, where a v8 setting that is
//! present wins over its legacy spelling, and the result is decoded.
//!
//! The shared `upstream.*` paths are read too, and expanding the `api-keys`
//! groups drops the read-only `auth_index` a management read adds to them.
//!
//! Deviations from upstream:
//! - [`V8_PATHS`] is a fixed table, checked against v8.0.15. Upstream builds
//!   it by reflecting over its `Config` struct; like upstream's, the table
//!   also moves sections this port ignores.
//! - Comments aren't carried along, as the loader only decodes the result.
//!   The config writer ([`super::save`]) flattens a tree that keeps them,
//!   as upstream does when it writes the file back.
//! - The deprecated `codex.live-media-relay.allow-private-remote-ips` is still
//!   converted and checked, though the section it converts into is ignored.

use std::collections::BTreeSet;

use super::yaml::{
    Kind, Node, Resolved, Scalar, Text, check_shape, delete_yaml_path, expand_merges, resolve_node,
    set_yaml_path, yaml_path,
};
use super::{ConfigError, ConfigErrorKind, decode::decode};

/// The largest positive credential weight.
pub(crate) const MAX_CREDENTIAL_WEIGHT: i64 = 1_000_000;

/// Every legacy setting with its v8 path, as `(legacy, v8)`, in upstream's
/// field order.
pub(crate) const V8_PATHS: &[(&str, &str)] = &[
    ("proxy-url", "requests.proxy-url"),
    (
        "disable-image-generation",
        "multimedia.disable-image-generation",
    ),
    (
        "gpt-image-2-base-model",
        "multimedia.gpt-image-2-base-model",
    ),
    (
        "video-result-auth-cache-ttl",
        "multimedia.video-result-auth-cache-ttl",
    ),
    ("force-model-prefix", "routing.force-model-prefix"),
    ("request-log", "observability.logs.request-log"),
    (
        "claude-code.disable-cloaking-model-list",
        "upstream.claude.disable-cloaking-model-list",
    ),
    ("api-keys", "access.api-keys"),
    ("passthrough-headers", "requests.passthrough-headers"),
    (
        "streaming.keepalive-seconds",
        "requests.streaming.keepalive-seconds",
    ),
    (
        "streaming.bootstrap-retries",
        "requests.streaming.bootstrap-retries",
    ),
    (
        "nonstream-keepalive-interval",
        "requests.nonstream-keepalive-interval",
    ),
    ("host", "server.host"),
    ("port", "server.port"),
    ("trusted-proxies", "server.trusted-proxies"),
    ("tls.enable", "server.tls.enable"),
    ("tls.cert", "server.tls.cert"),
    ("tls.key", "server.tls.key"),
    (
        "credential-concurrency.lifecycle-config-revision",
        "credentials.concurrency.lifecycle-config-revision",
    ),
    (
        "credential-concurrency.observation-barrier-revision",
        "credentials.concurrency.observation-barrier-revision",
    ),
    (
        "credential-concurrency.cpa-heartbeat-timeout",
        "credentials.concurrency.cpa-heartbeat-timeout",
    ),
    (
        "credential-concurrency.cpa-cancel-bound",
        "credentials.concurrency.cpa-cancel-bound",
    ),
    (
        "credential-concurrency.reclaim-grace",
        "credentials.concurrency.reclaim-grace",
    ),
    (
        "credential-concurrency.cleanup-interval",
        "credentials.concurrency.cleanup-interval",
    ),
    (
        "credential-concurrency.release-flush-interval",
        "credentials.concurrency.release-flush-interval",
    ),
    (
        "credential-concurrency.release-max-backoff",
        "credentials.concurrency.release-max-backoff",
    ),
    (
        "credential-concurrency.busy-retry-min",
        "credentials.concurrency.busy-retry-min",
    ),
    (
        "credential-concurrency.busy-retry-max",
        "credentials.concurrency.busy-retry-max",
    ),
    (
        "credential-concurrency.max-limit",
        "credentials.concurrency.max-limit",
    ),
    (
        "credential-in-flight.snapshot-interval",
        "credentials.in-flight.snapshot-interval",
    ),
    (
        "credential-in-flight.stale-after",
        "credentials.in-flight.stale-after",
    ),
    (
        "credential-in-flight.max-part-bytes",
        "credentials.in-flight.max-part-bytes",
    ),
    (
        "credential-in-flight.max-part-count",
        "credentials.in-flight.max-part-count",
    ),
    (
        "credential-in-flight.max-revision-bytes",
        "credentials.in-flight.max-revision-bytes",
    ),
    (
        "credential-in-flight.max-aggregate-groups",
        "credentials.in-flight.max-aggregate-groups",
    ),
    (
        "credential-in-flight.max-details",
        "credentials.in-flight.max-details",
    ),
    (
        "credential-in-flight.max-string-bytes",
        "credentials.in-flight.max-string-bytes",
    ),
    (
        "credential-in-flight.staging-retention",
        "credentials.in-flight.staging-retention",
    ),
    ("remote-management.allow-remote", "management.allow-remote"),
    ("remote-management.secret-key", "management.secret-key"),
    (
        "remote-management.disable-control-panel",
        "management.disable-control-panel",
    ),
    (
        "remote-management.disable-auto-update-panel",
        "management.disable-auto-update-panel",
    ),
    (
        "remote-management.panel-github-repository",
        "management.panel-github-repository",
    ),
    ("remote-management.base-url", "management.base-url"),
    ("auth-dir", "oauth.auth-dir"),
    ("debug", "observability.logs.debug"),
    ("pprof.enable", "observability.pprof.enable"),
    ("pprof.addr", "observability.pprof.addr"),
    ("discovery.enabled", "server.discovery.enabled"),
    ("discovery.service-name", "server.discovery.service-name"),
    ("discovery.service-type", "server.discovery.service-type"),
    ("discovery.subtypes", "server.discovery.subtypes"),
    (
        "discovery.interfaces.include",
        "server.discovery.interfaces.include",
    ),
    (
        "discovery.interfaces.exclude",
        "server.discovery.interfaces.exclude",
    ),
    ("discovery.auth-required", "server.discovery.auth-required"),
    (
        "discovery.advertise-management",
        "server.discovery.advertise-management",
    ),
    ("commercial-mode", "server.commercial-mode"),
    ("logging-to-file", "observability.logs.logging-to-file"),
    (
        "logs-max-total-size-mb",
        "observability.logs.logs-max-total-size-mb",
    ),
    (
        "error-logs-max-files",
        "observability.logs.error-logs-max-files",
    ),
    (
        "usage-statistics-enabled",
        "observability.usage.usage-statistics-enabled",
    ),
    (
        "redis-usage-queue-retention-seconds",
        "observability.usage.redis-usage-queue-retention-seconds",
    ),
    ("disable-cooling", "routing.cooldown.disable-cooling"),
    (
        "save-cooldown-status",
        "routing.cooldown.save-cooldown-status",
    ),
    (
        "transient-error-cooldown-seconds",
        "routing.cooldown.transient-error-cooldown-seconds",
    ),
    (
        "auth-auto-refresh-workers",
        "oauth.auth-auto-refresh-workers",
    ),
    ("request-retry", "routing.retry.request-retry"),
    (
        "max-retry-credentials",
        "routing.retry.max-retry-credentials",
    ),
    ("max-retry-interval", "routing.retry.max-retry-interval"),
    (
        "quota-exceeded.antigravity-credits",
        "oauth.providers.antigravity.antigravity-credits",
    ),
    ("ws-auth", "oauth.providers.aistudio.ws-auth"),
    (
        "antigravity-signature-cache-enabled",
        "oauth.providers.antigravity.signature-cache-enabled",
    ),
    (
        "antigravity-signature-bypass-strict",
        "oauth.providers.antigravity.signature-bypass-strict",
    ),
    (
        "antigravity.sensitive-words",
        "oauth.providers.antigravity.sensitive-words",
    ),
    (
        "antigravity.connection-pool.enabled",
        "oauth.providers.antigravity.connection-pool.enabled",
    ),
    (
        "antigravity.connection-pool.idle-conn-timeout",
        "oauth.providers.antigravity.connection-pool.idle-conn-timeout",
    ),
    (
        "antigravity.connection-pool.max-idle-conns-per-host",
        "oauth.providers.antigravity.connection-pool.max-idle-conns-per-host",
    ),
    (
        "devin.sensitive-words",
        "oauth.providers.devin.sensitive-words",
    ),
    ("xai.inject-x-search", "upstream.xai.inject-x-search"),
    (
        "codex.disable-codex-cloaking",
        "upstream.codex.disable-codex-cloaking",
    ),
    (
        "codex.stream-bootstrap-buffering",
        "upstream.codex.stream-bootstrap-buffering",
    ),
    (
        "codex.stream-bootstrap-timeout",
        "upstream.codex.stream-bootstrap-timeout",
    ),
    (
        "codex.orphan-delegation-compatibility",
        "upstream.codex.orphan-delegation-compatibility",
    ),
    (
        "codex.model-level-cooling",
        "upstream.codex.model-level-cooling",
    ),
    (
        "codex.live-media-relay.enabled",
        "oauth.providers.codex.live-media-relay.enabled",
    ),
    (
        "codex.live-media-relay.max-sessions",
        "oauth.providers.codex.live-media-relay.max-sessions",
    ),
    (
        "codex.live-media-relay.disable-private-remote-ips",
        "oauth.providers.codex.live-media-relay.disable-private-remote-ips",
    ),
    (
        "codex.live-media-relay.public-ip",
        "oauth.providers.codex.live-media-relay.public-ip",
    ),
    (
        "codex.live-media-relay.udp-port-min",
        "oauth.providers.codex.live-media-relay.udp-port-min",
    ),
    (
        "codex.live-media-relay.udp-port-max",
        "oauth.providers.codex.live-media-relay.udp-port-max",
    ),
    (
        "codex.live-media-relay.ice-servers",
        "oauth.providers.codex.live-media-relay.ice-servers",
    ),
    (
        "codex.response-steering",
        "upstream.codex.response-steering",
    ),
    (
        "codex-header-defaults.user-agent",
        "oauth.providers.codex.header-defaults.user-agent",
    ),
    (
        "codex-header-defaults.beta-features",
        "oauth.providers.codex.header-defaults.beta-features",
    ),
    (
        "claude.model-level-cooling",
        "upstream.claude.model-level-cooling",
    ),
    (
        "claude-header-defaults.user-agent",
        "upstream.claude.header-defaults.user-agent",
    ),
    (
        "claude-header-defaults.package-version",
        "upstream.claude.header-defaults.package-version",
    ),
    (
        "claude-header-defaults.runtime-version",
        "upstream.claude.header-defaults.runtime-version",
    ),
    (
        "claude-header-defaults.os",
        "upstream.claude.header-defaults.os",
    ),
    (
        "claude-header-defaults.arch",
        "upstream.claude.header-defaults.arch",
    ),
    (
        "claude-header-defaults.timeout",
        "upstream.claude.header-defaults.timeout",
    ),
    (
        "claude-header-defaults.timezone",
        "upstream.claude.header-defaults.timezone",
    ),
    (
        "claude-header-defaults.stabilize-device-profile",
        "upstream.claude.header-defaults.stabilize-device-profile",
    ),
    (
        "disable-claude-cloak-mode",
        "upstream.claude.disable-claude-cloak-mode",
    ),
    ("oauth-excluded-models", "oauth.excluded-models"),
    ("oauth-model-alias", "oauth.model-alias"),
    ("oauth-request-scoped-errors", "oauth.request-scoped-errors"),
    ("oauth-settings", "oauth.settings"),
    ("payload.default", "requests.payload.default"),
    ("payload.default-raw", "requests.payload.default-raw"),
    ("payload.override", "requests.payload.override"),
    ("payload.override-raw", "requests.payload.override-raw"),
    ("payload.filter", "requests.payload.filter"),
];

/// Earlier spellings of client settings, most preferred first.
pub(crate) const V8_CLIENT_PATHS: &[(&str, &str)] = &[
    (
        "oauth.providers.codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
    (
        "providers.codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
    (
        "codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
];

/// Settings an earlier v8 layout placed under `oauth.providers` that now
/// apply to every credential.
pub(crate) const V8_SHARED_PATHS: &[(&str, &str)] = &[
    (
        "oauth.providers.codex.disable-codex-cloaking",
        "upstream.codex.disable-codex-cloaking",
    ),
    (
        "oauth.providers.codex.stream-bootstrap-buffering",
        "upstream.codex.stream-bootstrap-buffering",
    ),
    (
        "oauth.providers.codex.stream-bootstrap-timeout",
        "upstream.codex.stream-bootstrap-timeout",
    ),
    (
        "oauth.providers.codex.orphan-delegation-compatibility",
        "upstream.codex.orphan-delegation-compatibility",
    ),
    (
        "oauth.providers.codex.model-level-cooling",
        "upstream.codex.model-level-cooling",
    ),
    (
        "oauth.providers.codex.response-steering",
        "upstream.codex.response-steering",
    ),
    (
        "oauth.providers.claude.model-level-cooling",
        "upstream.claude.model-level-cooling",
    ),
    (
        "oauth.providers.claude.claude-code.disable-cloaking-model-list",
        "upstream.claude.disable-cloaking-model-list",
    ),
    (
        "oauth.providers.claude.disable-claude-cloak-mode",
        "upstream.claude.disable-claude-cloak-mode",
    ),
    (
        "oauth.providers.claude.header-defaults.user-agent",
        "upstream.claude.header-defaults.user-agent",
    ),
    (
        "oauth.providers.claude.header-defaults.package-version",
        "upstream.claude.header-defaults.package-version",
    ),
    (
        "oauth.providers.claude.header-defaults.runtime-version",
        "upstream.claude.header-defaults.runtime-version",
    ),
    (
        "oauth.providers.claude.header-defaults.os",
        "upstream.claude.header-defaults.os",
    ),
    (
        "oauth.providers.claude.header-defaults.arch",
        "upstream.claude.header-defaults.arch",
    ),
    (
        "oauth.providers.claude.header-defaults.timeout",
        "upstream.claude.header-defaults.timeout",
    ),
    (
        "oauth.providers.claude.header-defaults.timezone",
        "upstream.claude.header-defaults.timezone",
    ),
    (
        "oauth.providers.claude.header-defaults.stabilize-device-profile",
        "upstream.claude.header-defaults.stabilize-device-profile",
    ),
    (
        "oauth.providers.xai.inject-x-search",
        "upstream.xai.inject-x-search",
    ),
];

/// Sections of the earlier layout that may be left behind empty.
pub(crate) const V8_SHARED_STRUCT_PATHS: &[(&str, &str)] = &[
    (
        "oauth.providers.claude.header-defaults",
        "upstream.claude.header-defaults",
    ),
    ("oauth.providers.claude.claude-code", "upstream.claude"),
    ("oauth.providers.claude", "upstream.claude"),
    ("oauth.providers.xai", "upstream.xai"),
];

/// API-key families: the legacy list and its group under `api-keys`.
pub(crate) const V8_KEY_FAMILIES: &[(&str, &str)] = &[
    ("gemini-api-key", "gemini"),
    ("interactions-api-key", "interactions"),
    ("vertex-api-key", "vertex"),
    ("codex-api-key", "codex"),
    ("claude-api-key", "claude"),
    ("xai-api-key", "xai"),
    ("meta-api-key", "meta"),
    ("openai-compatibility", "openai-compatibility"),
];

/// Group fields every key in a v8 group inherits.
pub(crate) const SHARED_KEY_FIELDS: &[&str] = &[
    "priority",
    "prefix",
    "proxy-url",
    "headers",
    "models",
    "excluded-models",
    "disable-cooling",
    "request-retry",
    "request-scoped-errors",
];

/// Legacy API-key lists whose entries carry a `weight`, and open-ferry's
/// `claude-cli` list, which upstream doesn't have.
const WEIGHTED_FAMILIES: &[&str] = &[
    "gemini-api-key",
    "interactions-api-key",
    "claude-api-key",
    "vertex-api-key",
    "codex-api-key",
    "xai-api-key",
    "meta-api-key",
    "claude-cli",
];

/// A document in the legacy layout.
#[derive(Debug)]
pub(crate) struct Flattened {
    pub(crate) root: Node,
    /// Legacy names of the settings found under `oauth.providers`.
    pub(crate) oauth_only_fields: BTreeSet<String>,
}

fn invalid(message: String) -> ConfigError {
    ConfigError::new(ConfigErrorKind::Invalid, message)
}

/// Legacy names of the settings a v8 document scopes to OAuth credentials.
#[cfg(test)]
pub(crate) fn oauth_scoped_paths() -> impl Iterator<Item = &'static str> {
    V8_PATHS
        .iter()
        .filter(|(_, current)| current.starts_with("oauth.providers."))
        .map(|(old, _)| *old)
}

/// Upstream's `flattenV8`: the document in the legacy layout. The root must
/// be a mapping.
pub(crate) fn flatten_v8(original: &Node) -> Result<Flattened, ConfigError> {
    if !original.is_mapping() {
        return Err(invalid("config must be a mapping".to_owned()));
    }
    // Reject duplicate keys even where a v8 value would hide the subtree.
    check_shape(original)
        .map_err(|error| ConfigError::new(ConfigErrorKind::Decode, error.message()))?;
    let mut node = expand_merges(original);
    let oauth_only_fields = V8_PATHS
        .iter()
        .filter(|(_, current)| current.starts_with("oauth.providers."))
        .filter(|(_, current)| yaml_path(&node, current).is_some())
        .map(|(old, _)| (*old).to_owned())
        .collect();
    normalize_private_ip_alias(&mut node)?;
    let aliases = V8_CLIENT_PATHS.iter().chain(V8_SHARED_PATHS);
    for (_, current) in V8_PATHS.iter().chain(aliases.clone()) {
        let parts: Vec<&str> = current.split('.').collect();
        for depth in 1..parts.len() {
            let prefix = parts.get(..depth).unwrap_or_default().join(".");
            let Some(parent) = yaml_path(&node, &prefix) else {
                break;
            };
            // Routing is shared with the legacy layout, where null means defaults.
            if depth == 1 && prefix == "routing" && parent.tag == "!!null" {
                break;
            }
            if !parent.is_mapping() {
                return Err(invalid(format!("{prefix} must be a mapping")));
            }
        }
    }
    let mut root = node.clone();
    for (old, current) in aliases {
        if let Some(value) = yaml_path(&root, old).cloned() {
            if yaml_path(&root, current).is_none() {
                set_yaml_path(&mut root, current, &value);
            }
            delete_yaml_path(&mut root, old);
        }
    }
    for (old, current) in V8_SHARED_STRUCT_PATHS {
        let Some(value) = yaml_path(&root, old) else {
            continue;
        };
        let null = value.tag == "!!null";
        if !null && !value.is_mapping() {
            return Err(invalid(format!("{old} must be a mapping")));
        }
        if !null && !value.content.is_empty() {
            continue;
        }
        if yaml_path(&root, current).is_none() {
            let mut copy = value.clone();
            copy.kind = Kind::Mapping;
            copy.tag = "!!map".into();
            copy.value = Text::default();
            set_yaml_path(&mut root, current, &copy);
        }
        delete_yaml_path(&mut root, old);
    }
    if let Some(version) = yaml_path(&root, "config-version")
        && (version.tag != "!!int" || version.value != "8")
    {
        return Err(invalid(
            "unsupported config-version (expected 8)".to_owned(),
        ));
    }
    // The v8 upstream map reuses the legacy client-key field name.
    if yaml_path(&root, "api-keys").is_some_and(Node::is_mapping) {
        delete_yaml_path(&mut root, "api-keys");
    }
    for (old, current) in V8_PATHS {
        if let Some(value) = yaml_path(&root, current).cloned() {
            delete_yaml_path(&mut root, current);
            set_yaml_path(&mut root, old, &value);
        }
    }
    for (old, current) in V8_KEY_FAMILIES {
        if let Some(groups) = yaml_path(&node, &format!("api-keys.{current}")) {
            let keys = expand_v8_groups(groups, current)?;
            set_yaml_path(&mut root, old, &keys);
        }
    }
    Ok(Flattened {
        root,
        oauth_only_fields,
    })
}

/// Upstream's `normalizeV8PrivateIPAlias` as loading runs it: the deprecated
/// allow flag becomes the inverted disable flag unless that is set.
pub(crate) fn normalize_private_ip_alias(root: &mut Node) -> Result<(), ConfigError> {
    const OLD: &str = "codex.live-media-relay.allow-private-remote-ips";
    const CANONICAL: &str = "codex.live-media-relay.disable-private-remote-ips";
    let Some(value) = yaml_path(root, OLD) else {
        return Ok(());
    };
    if yaml_path(root, &format!("oauth.providers.{CANONICAL}")).is_some() {
        delete_yaml_path(root, OLD);
        return Ok(());
    }
    if yaml_path(root, CANONICAL).is_some() {
        return Ok(());
    }
    let allow = decode::<bool>(value).map_err(|error| {
        ConfigError::new(
            ConfigErrorKind::Decode,
            format!("decode {OLD}: {}", error.message()),
        )
    })?;
    let disable = if allow { "false" } else { "true" };
    set_yaml_path(root, CANONICAL, &Node::scalar("!!bool", disable));
    delete_yaml_path(root, OLD);
    Ok(())
}

/// The read-only `auth_index` a management read adds to `api-keys` groups
/// and keys, in both spellings. Loading drops it, so a config saved from a
/// read loads as it did.
const AUTH_INDEX_FIELDS: [&str; 2] = ["auth_index", "auth-index"];

/// Upstream's `expandV8Groups`: one legacy entry per key, each inheriting
/// its group's endpoint and shared fields, without the `auth_index` a
/// management read shows.
fn expand_v8_groups(groups: &Node, provider: &str) -> Result<Node, ConfigError> {
    if groups.kind != Kind::Sequence {
        return Err(invalid(format!("api-keys.{provider} must be a list")));
    }
    let mut out = Node::sequence();
    for (index, group) in groups.content.iter().enumerate() {
        if !group.is_mapping() {
            return Err(invalid(format!(
                "api-keys.{provider}[{index}] must be a mapping"
            )));
        }
        let Some(keys) = yaml_path(group, "keys").filter(|keys| keys.kind == Kind::Sequence) else {
            return Err(invalid(format!(
                "api-keys.{provider}[{index}].keys must be a list"
            )));
        };
        validate_weight_sequence(keys, &format!("api-keys.{provider}.keys"))?;
        if provider == "openai-compatibility" {
            let mut item = group.clone();
            delete_yaml_path(&mut item, "keys");
            for field in AUTH_INDEX_FIELDS {
                delete_yaml_path(&mut item, field);
            }
            let mut clean_keys = keys.clone();
            for key in &mut clean_keys.content {
                for field in AUTH_INDEX_FIELDS {
                    delete_yaml_path(key, field);
                }
            }
            set_yaml_path(&mut item, "api-key-entries", &clean_keys);
            out.content.push(item);
            continue;
        }
        for (field, _) in group.pairs() {
            let field = field.value.as_str();
            if !matches!(field, "name" | "base-url" | "keys") && !SHARED_KEY_FIELDS.contains(&field)
            {
                return Err(invalid(format!(
                    "api-keys.{provider}: unsupported group field {field}"
                )));
            }
        }
        for key in &keys.content {
            if !key.is_mapping() {
                return Err(invalid(format!(
                    "api-keys.{provider} key must be a mapping"
                )));
            }
            if yaml_path(key, "base-url").is_some() {
                return Err(invalid(format!(
                    "api-keys.{provider}: base-url belongs to the group"
                )));
            }
            let mut item = Node::mapping();
            for (field, value) in group.pairs() {
                if field.value == "base-url" || SHARED_KEY_FIELDS.contains(&field.value.as_str()) {
                    set_yaml_path(&mut item, &field.value, value);
                }
            }
            for (field, value) in key.pairs() {
                if AUTH_INDEX_FIELDS.contains(&field.value.as_str()) {
                    continue;
                }
                if value.tag != "!!null" {
                    set_yaml_path(&mut item, &field.value, value);
                }
            }
            out.content.push(item);
        }
    }
    Ok(out)
}

/// `credentialweight.Normalize`'s check: a weight above the maximum is an
/// error.
pub(crate) fn check_weight(weight: i64) -> Result<(), String> {
    if weight > MAX_CREDENTIAL_WEIGHT {
        return Err(format!("weight must not exceed {MAX_CREDENTIAL_WEIGHT}"));
    }
    Ok(())
}

/// Upstream's `validateCredentialWeightYAML` after flattening: every
/// `weight` in the API-key lists must be a plain integer within range.
pub(crate) fn validate_weights(root: &Node) -> Result<(), ConfigError> {
    if !root.is_mapping() {
        return Ok(());
    }
    for (key, value) in root.pairs() {
        let name = key.value.as_str();
        if WEIGHTED_FAMILIES.contains(&name) {
            validate_weight_sequence(value, name)?;
            continue;
        }
        if name == "openai-compatibility" && value.kind == Kind::Sequence {
            for (index, provider) in value.content.iter().enumerate() {
                for (field, entries) in provider.pairs() {
                    if field.value == "api-key-entries" {
                        let path = format!("openai-compatibility[{index}].api-key-entries");
                        validate_weight_sequence(entries, &path)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_weight_sequence(sequence: &Node, path: &str) -> Result<(), ConfigError> {
    if sequence.kind != Kind::Sequence {
        return Ok(());
    }
    for (index, item) in sequence.content.iter().enumerate() {
        validate_weight_mapping(item, &format!("{path}[{index}]"))?;
    }
    Ok(())
}

fn validate_weight_mapping(mapping: &Node, path: &str) -> Result<(), ConfigError> {
    // A non-mapping has no pairs.
    for (key, value) in mapping.pairs().filter(|_| mapping.is_mapping()) {
        if key.value != "weight" {
            continue;
        }
        let not_integer = || invalid(format!("{path}.weight: weight must be an integer"));
        if value.kind != Kind::Scalar || value.tag != "!!int" {
            return Err(not_integer());
        }
        let weight = match resolve_node(value) {
            Ok(Resolved {
                value: Scalar::Int(weight),
                ..
            }) => weight,
            Ok(Resolved {
                value: Scalar::Uint(weight),
                ..
            }) => i64::try_from(weight).map_err(|_| not_integer())?,
            _ => return Err(not_integer()),
        };
        check_weight(weight).map_err(|message| invalid(format!("{path}.weight: {message}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::yaml::parse_document;

    fn flatten(text: &str) -> Result<Flattened, String> {
        let root = parse_document(text)
            .map_err(|error| error.message())?
            .unwrap_or_default();
        flatten_v8(&root).map_err(|error| error.to_string())
    }

    fn value_at(flattened: &Flattened, path: &str) -> Option<String> {
        yaml_path(&flattened.root, path).map(|node| node.value.to_string())
    }

    #[test]
    fn paths_table_matches_upstream_shape() {
        assert_eq!(V8_PATHS.len(), 113);
        let olds: BTreeSet<&str> = V8_PATHS.iter().map(|(old, _)| *old).collect();
        assert_eq!(olds.len(), V8_PATHS.len());
        let scoped: Vec<&str> = oauth_scoped_paths().collect();
        assert!(scoped.contains(&"ws-auth"));
        assert!(scoped.contains(&"quota-exceeded.antigravity-credits"));
        assert!(scoped.contains(&"codex-header-defaults.beta-features"));
        assert!(!scoped.contains(&"codex.stream-bootstrap-buffering"));
    }

    #[test]
    fn v8_values_move_to_legacy_paths() {
        let flattened = flatten(
            "server: {port: 9000, tls: {enable: true}}\nrouting: {strategy: ff, retry: {request-retry: 2}}\n\
             access: {api-keys: [a]}\nrequest-retry: 7\n",
        );
        let flattened = flattened.unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(value_at(&flattened, "port").as_deref(), Some("9000"));
        assert_eq!(value_at(&flattened, "tls.enable").as_deref(), Some("true"));
        assert_eq!(value_at(&flattened, "request-retry").as_deref(), Some("2"));
        assert_eq!(
            value_at(&flattened, "routing.strategy").as_deref(),
            Some("ff")
        );
        assert!(yaml_path(&flattened.root, "routing.retry").is_none());
        assert!(yaml_path(&flattened.root, "server").is_none());
        assert_eq!(
            yaml_path(&flattened.root, "api-keys").map(|keys| keys.content.len()),
            Some(1)
        );
    }

    #[test]
    fn oauth_scope_is_recorded() {
        let flattened = flatten(
            "oauth: {providers: {aistudio: {ws-auth: false}, codex: {header-defaults: {beta-features: x}}}}\n",
        );
        let flattened = flattened.unwrap_or_else(|error| panic!("{error}"));
        let fields: Vec<&str> = flattened
            .oauth_only_fields
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, ["codex-header-defaults.beta-features", "ws-auth"]);
        assert_eq!(value_at(&flattened, "ws-auth").as_deref(), Some("false"));
        let legacy = flatten("ws-auth: false\n").unwrap_or_else(|error| panic!("{error}"));
        assert!(legacy.oauth_only_fields.is_empty());
    }

    #[test]
    fn aliases_prefer_the_canonical_path() {
        let flattened = flatten(
            "codex: {optimize-multi-agent-v2: false}\nproviders: {codex: {optimize-multi-agent-v2: true}}\n",
        );
        let flattened = flattened.unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            value_at(&flattened, "client.codex.optimize-multi-agent-v2").as_deref(),
            Some("true")
        );
        assert!(yaml_path(&flattened.root, "providers").is_none());
        assert!(yaml_path(&flattened.root, "codex.optimize-multi-agent-v2").is_none());
        let flattened = flatten(
            "client: {codex: {optimize-multi-agent-v2: false}}\ncodex: {optimize-multi-agent-v2: true}\n",
        );
        let flattened = flattened.unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            value_at(&flattened, "client.codex.optimize-multi-agent-v2").as_deref(),
            Some("false")
        );
        let flattened = flatten("oauth: {providers: {claude: {model-level-cooling: true}}}\n");
        let flattened = flattened.unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            value_at(&flattened, "claude.model-level-cooling").as_deref(),
            Some("true")
        );
        assert!(yaml_path(&flattened.root, "oauth").is_none());
    }

    #[test]
    fn private_ip_alias_is_inverted() {
        let flattened = flatten("codex: {live-media-relay: {allow-private-remote-ips: false}}\n");
        let flattened = flattened.unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            value_at(
                &flattened,
                "codex.live-media-relay.disable-private-remote-ips"
            )
            .as_deref(),
            Some("true")
        );
        assert!(
            yaml_path(
                &flattened.root,
                "codex.live-media-relay.allow-private-remote-ips"
            )
            .is_none()
        );
        assert_eq!(
            flatten("codex: {live-media-relay: {allow-private-remote-ips: [x]}}\n").err(),
            Some(
                "decode codex.live-media-relay.allow-private-remote-ips: yaml: unmarshal errors:\n  \
                 line 1: cannot unmarshal !!seq into bool"
                    .to_owned()
            )
        );
    }

    #[test]
    fn layout_errors_match_upstream() {
        for (text, message) in [
            ("[]", "config must be a mapping"),
            ("server: true", "server must be a mapping"),
            ("server: null", "server must be a mapping"),
            ("routing: false", "routing must be a mapping"),
            ("routing: {retry: false}", "routing.retry must be a mapping"),
            (
                "config-version: 9",
                "unsupported config-version (expected 8)",
            ),
            (
                "config-version: '8'",
                "unsupported config-version (expected 8)",
            ),
            (
                "oauth: {providers: {xai: 5}}",
                "oauth.providers.xai must be a mapping",
            ),
            ("api-keys: {codex: {}}", "api-keys.codex must be a list"),
            (
                "api-keys: {codex: [5]}",
                "api-keys.codex[0] must be a mapping",
            ),
            (
                "api-keys: {codex: [{name: a}]}",
                "api-keys.codex[0].keys must be a list",
            ),
            (
                "api-keys: {codex: [{name: a, keys: [{api-key: a, weight: 1.5}]}]}",
                "api-keys.codex.keys[0].weight: weight must be an integer",
            ),
            (
                "api-keys: {codex: [{name: a, keys: [{weight: 1000001}]}]}",
                "api-keys.codex.keys[0].weight: weight must not exceed 1000000",
            ),
            (
                "api-keys: {codex: [{name: a, cloak: {}, keys: []}]}",
                "api-keys.codex: unsupported group field cloak",
            ),
            (
                "api-keys: {codex: [{name: a, keys: [x]}]}",
                "api-keys.codex key must be a mapping",
            ),
            (
                "api-keys: {codex: [{name: a, keys: [{api-key: a, base-url: https://invalid}]}]}",
                "api-keys.codex: base-url belongs to the group",
            ),
        ] {
            assert_eq!(flatten(text).err().as_deref(), Some(message), "{text}");
        }
        for text in [
            "routing: null",
            "routing: ~",
            "routing:",
            "config-version: 8",
            "{}",
        ] {
            assert!(flatten(text).is_ok(), "{text}");
        }
    }

    #[test]
    fn groups_expand_into_keys() {
        let flattened = flatten(
            "api-keys:\n  claude:\n    - name: g\n      base-url: https://example.invalid\n      priority: 7\n      \
             keys:\n        - {api-key: a, priority: null}\n        - {api-key: b, priority: 0, weight: 3}\n",
        );
        let flattened = flattened.unwrap_or_else(|error| panic!("{error}"));
        let keys = yaml_path(&flattened.root, "claude-api-key").map(|keys| keys.content.clone());
        let keys = keys.unwrap_or_default();
        assert_eq!(keys.len(), 2);
        let field = |index: usize, name: &str| {
            keys.get(index)
                .and_then(|key| yaml_path(key, name))
                .map(|node| node.value.to_string())
        };
        assert_eq!(field(0, "priority").as_deref(), Some("7"));
        assert_eq!(
            field(0, "base-url").as_deref(),
            Some("https://example.invalid")
        );
        assert_eq!(field(0, "name"), None);
        assert_eq!(field(1, "priority").as_deref(), Some("0"));
        assert_eq!(field(1, "weight").as_deref(), Some("3"));
        let compat = flatten(
            "api-keys: {openai-compatibility: [{name: p, base-url: u, keys: [{api-key: k}]}]}",
        );
        let compat = compat.unwrap_or_else(|error| panic!("{error}"));
        let entry =
            yaml_path(&compat.root, "openai-compatibility").and_then(|list| list.content.first());
        assert!(entry.is_some_and(|entry| yaml_path(entry, "api-key-entries").is_some()));
        assert!(entry.is_some_and(|entry| yaml_path(entry, "keys").is_none()));
    }

    // Not upstream's: upstream (0fb50a18) tests the read-only auth_index
    // through a management PUT; this checks that loading drops it.
    #[test]
    fn auth_index_is_dropped_on_load() {
        let flattened = flatten(
            "api-keys:
  codex:
    - base-url: https://example.invalid
      keys:
        - {api-key: a, auth_index: x1, auth-index: x2, priority: 4}
  openai-compatibility:
    - name: p
      base-url: u
      auth_index: g1
      auth-index: g2
      keys:
        - {api-key: k, auth_index: k1, auth-index: k2}
        - plain
",
        );
        let flattened = flattened.unwrap_or_else(|error| panic!("{error}"));
        let codex =
            yaml_path(&flattened.root, "codex-api-key").and_then(|list| list.content.first());
        let codex = codex.cloned().unwrap_or_default();
        assert_eq!(
            yaml_path(&codex, "api-key").map(|node| node.value.to_string()),
            Some("a".to_owned())
        );
        assert_eq!(
            yaml_path(&codex, "priority").map(|node| node.value.to_string()),
            Some("4".to_owned())
        );
        assert!(yaml_path(&codex, "auth_index").is_none());
        assert!(yaml_path(&codex, "auth-index").is_none());
        let compat = yaml_path(&flattened.root, "openai-compatibility")
            .and_then(|list| list.content.first());
        let compat = compat.cloned().unwrap_or_default();
        assert!(yaml_path(&compat, "auth_index").is_none());
        assert!(yaml_path(&compat, "auth-index").is_none());
        let entries = yaml_path(&compat, "api-key-entries").map(|list| list.content.clone());
        let entries = entries.unwrap_or_default();
        assert_eq!(entries.len(), 2);
        let first = entries.first().cloned().unwrap_or_default();
        assert_eq!(
            yaml_path(&first, "api-key").map(|node| node.value.to_string()),
            Some("k".to_owned())
        );
        assert!(yaml_path(&first, "auth_index").is_none());
        assert!(yaml_path(&first, "auth-index").is_none());
        assert_eq!(
            entries.get(1).map(|node| node.value.to_string()),
            Some("plain".to_owned())
        );
    }

    #[test]
    fn legacy_weights_are_checked() {
        let check = |text: &str| {
            let root = parse_document(text).ok().flatten().unwrap_or_default();
            validate_weights(&root).map_err(|error| error.to_string())
        };
        assert_eq!(
            check("codex-api-key: [{weight: 5}, {weight: -3}, {weight: 0x10}]"),
            Ok(())
        );
        assert_eq!(
            check("claude-api-key: [{}, {weight: '5'}]"),
            Err("claude-api-key[1].weight: weight must be an integer".to_owned())
        );
        assert_eq!(
            check("codex-api-key: [{weight: ~}]"),
            Err("codex-api-key[0].weight: weight must be an integer".to_owned())
        );
        assert_eq!(
            check("codex-api-key: [{weight: 18446744073709551615}]"),
            Err("codex-api-key[0].weight: weight must be an integer".to_owned())
        );
        assert_eq!(
            check("openai-compatibility: [{api-key-entries: [{weight: 2000000}]}]"),
            Err(
                "openai-compatibility[0].api-key-entries[0].weight: weight must not exceed 1000000"
                    .to_owned()
            )
        );
        assert_eq!(check("codex-api-key: 5\nweight: x"), Ok(()));
    }
}
