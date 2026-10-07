// Ported from CLIProxyAPI internal/config/config_v8.go (NormalizeConfigLayout
// as a v8 write runs it, groupLegacyKeys, commentUnknownV8Sections,
// commentUnknownV8Fields), config_v8_api.go (ProjectV8ConfigAliases) and
// internal/api/handlers/management/config_v8.go (configV8Node,
// deleteConfigV8Path and the TURN secret redaction of ConfigV8), and
// config_v8.go (v8AllowedRoots, and the `models` section
// commentUnknownV8Sections keeps) (v8.0.15, MIT), with the rules of
// gopkg.in/yaml.v3 v3.0.1
// decode.go (decoding into `any`: decoder.scalar, decoder.mapping,
// isStringMap; Apache-2.0).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/go-yaml/yaml

//! The config file in the v8 layout, as the v8 management API shows it.
//!
//! [`V8Document::migrate`] reads a config file and moves every legacy
//! setting to its v8 path, as upstream does before it answers a v8 config
//! read: a setting already present at its v8 path wins and the legacy one
//! is dropped, legacy API-key lists become groups under `api-keys`, empty
//! or null legacy sections become empty v8 ones, sections the v8 layout
//! doesn't know are dropped, and `config-version` is set to 8. The file
//! itself is never touched.
//!
//! [`V8Document::project_aliases`] then moves settings back to an earlier
//! v8 spelling when that is what a read asks for, and
//! [`V8Document::value`] decodes the node at a path as yaml.v3 decodes into
//! Go's `any`. For a JSON read, [`V8Document::set_api_key_auth_indexes`]
//! first shows on each API key the index of the credential it makes.
//!
//! Upstream writes the migrated tree out as YAML and reads it back before
//! using it, which can change what a value decodes to: a timestamp in a
//! flow mapping comes back a string, as do an empty mapping key and an
//! empty flow value. [`V8Document::migrate`] gives the tree the tags it
//! would come back with.
//!
//! The `Debug` of a [`V8Document`] or an [`AnyValue`] shows its shape, never
//! the config's text, which holds secrets.
//!
//! Deviations from upstream:
//! - The tables of v8 sections and the fields they hold are fixed tables,
//!   checked against v8.0.15; upstream builds them by reflecting over its
//!   `Config` struct.
//! - Upstream comments out the sections it drops, and logs a warning for
//!   each; they are dropped here without a trace, as a [`V8Document`] is
//!   only shown, never written. The config writer ([`super::save`]) comments
//!   them out and warns, as upstream does.
//! - A value whose scalars aliases expanded to more than 64 MiB of text
//!   fails to decode with `yaml: document contains excessive aliasing`;
//!   upstream decodes it.

mod auth_index;

use std::collections::BTreeMap;
use std::fmt;

use super::v8::{
    SHARED_KEY_FIELDS, V8_CLIENT_PATHS, V8_KEY_FAMILIES, V8_PATHS, V8_SHARED_PATHS,
    V8_SHARED_STRUCT_PATHS, flatten_v8, normalize_private_ip_alias,
};
use super::yaml::{
    AliasBudget, Kind, Node, Scalar, Text, Timestamp, delete_yaml_path, expand_merges,
    find_map_key_index, parse_document, parse_timestamp, resolve_node, scalar_string,
    set_yaml_path, write_and_read_back, yaml_path,
};
use super::{ConfigError, ConfigErrorKind};

/// Legacy sections and their v8 paths, as `(legacy, v8)`, in upstream's
/// field order (its `v8StructPaths`).
pub(crate) const V8_STRUCT_PATHS: &[(&str, &str)] = &[
    ("claude-code", "upstream.claude"),
    ("streaming", "requests.streaming"),
    ("tls", "server.tls"),
    ("credential-concurrency", "credentials.concurrency"),
    ("credential-in-flight", "credentials.in-flight"),
    ("remote-management", "management"),
    ("pprof", "observability.pprof"),
    ("discovery", "server.discovery"),
    ("discovery.interfaces", "server.discovery.interfaces"),
    ("antigravity", "oauth.providers.antigravity"),
    (
        "antigravity.connection-pool",
        "oauth.providers.antigravity.connection-pool",
    ),
    ("devin", "oauth.providers.devin"),
    ("xai", "upstream.xai"),
    ("codex", "oauth.providers.codex"),
    (
        "codex.live-media-relay",
        "oauth.providers.codex.live-media-relay",
    ),
    (
        "codex-header-defaults",
        "oauth.providers.codex.header-defaults",
    ),
    ("claude", "upstream.claude"),
    ("claude-header-defaults", "upstream.claude.header-defaults"),
    ("payload", "requests.payload"),
];

/// The top-level keys of the v8 layout (upstream's `v8AllowedRoots`), with
/// open-ferry's `claude-cli`, which stays at the top level in both layouts.
pub(crate) const V8_ROOTS: &[&str] = &[
    "access",
    "api-keys",
    "claude-cli",
    "client",
    "config-version",
    "credentials",
    "management",
    "models",
    "multimedia",
    "oauth",
    "observability",
    "plugins",
    "quota-exceeded",
    "requests",
    "routing",
    "server",
    "upstream",
];

/// The keys each v8 section may hold, by the section's dotted path. A
/// section that isn't listed, such as `api-keys`, keeps whatever it holds
/// (the `children` table of upstream's `commentUnknownV8Sections`).
pub(crate) const V8_CHILDREN: &[(&str, &[&str])] = &[
    ("access", &["api-keys"]),
    ("client", &["codex"]),
    (
        "client.codex",
        &["enable-apply-patch", "optimize-multi-agent-v2"],
    ),
    ("credentials", &["concurrency", "in-flight"]),
    (
        "credentials.concurrency",
        &[
            "busy-retry-max",
            "busy-retry-min",
            "cleanup-interval",
            "cpa-cancel-bound",
            "cpa-heartbeat-timeout",
            "lifecycle-config-revision",
            "max-limit",
            "observation-barrier-revision",
            "reclaim-grace",
            "release-flush-interval",
            "release-max-backoff",
        ],
    ),
    (
        "credentials.in-flight",
        &[
            "max-aggregate-groups",
            "max-details",
            "max-part-bytes",
            "max-part-count",
            "max-revision-bytes",
            "max-string-bytes",
            "snapshot-interval",
            "staging-retention",
            "stale-after",
        ],
    ),
    (
        "management",
        &[
            "allow-remote",
            "base-url",
            "disable-auto-update-panel",
            "disable-control-panel",
            "panel-github-repository",
            "secret-key",
        ],
    ),
    ("models", &["catalog", "codex-catalog", "devin-catalog"]),
    (
        "multimedia",
        &[
            "disable-image-generation",
            "gpt-image-2-base-model",
            "video-result-auth-cache-ttl",
        ],
    ),
    (
        "oauth",
        &[
            "auth-auto-refresh-workers",
            "auth-dir",
            "excluded-models",
            "model-alias",
            "providers",
            "request-scoped-errors",
            "settings",
        ],
    ),
    (
        "oauth.providers",
        &["aistudio", "antigravity", "codex", "devin"],
    ),
    ("oauth.providers.aistudio", &["ws-auth"]),
    (
        "oauth.providers.antigravity",
        &[
            "antigravity-credits",
            "connection-pool",
            "sensitive-words",
            "signature-bypass-strict",
            "signature-cache-enabled",
        ],
    ),
    (
        "oauth.providers.antigravity.connection-pool",
        &["enabled", "idle-conn-timeout", "max-idle-conns-per-host"],
    ),
    (
        "oauth.providers.codex",
        &["header-defaults", "live-media-relay"],
    ),
    (
        "oauth.providers.codex.header-defaults",
        &["beta-features", "user-agent"],
    ),
    (
        "oauth.providers.codex.live-media-relay",
        &[
            "disable-private-remote-ips",
            "enabled",
            "ice-servers",
            "max-sessions",
            "public-ip",
            "udp-port-max",
            "udp-port-min",
        ],
    ),
    ("oauth.providers.devin", &["sensitive-words"]),
    ("observability", &["logs", "pprof", "usage"]),
    (
        "observability.logs",
        &[
            "debug",
            "error-logs-max-files",
            "logging-to-file",
            "logs-max-total-size-mb",
            "request-log",
        ],
    ),
    ("observability.pprof", &["addr", "enable"]),
    (
        "observability.usage",
        &[
            "redis-usage-queue-retention-seconds",
            "usage-statistics-enabled",
        ],
    ),
    (
        "plugins",
        &[
            "auth-revision",
            "configs",
            "dir",
            "enabled",
            "store-auth",
            "store-sources",
        ],
    ),
    (
        "quota-exceeded",
        &[
            "antigravity-credits",
            "switch-preview-model",
            "switch-project",
        ],
    ),
    (
        "requests",
        &[
            "nonstream-keepalive-interval",
            "passthrough-headers",
            "payload",
            "proxy-url",
            "streaming",
        ],
    ),
    (
        "requests.payload",
        &[
            "default",
            "default-raw",
            "filter",
            "override",
            "override-raw",
        ],
    ),
    (
        "requests.streaming",
        &["bootstrap-retries", "keepalive-seconds"],
    ),
    (
        "routing",
        &[
            "cooldown",
            "force-model-prefix",
            "retry",
            "session-affinity",
            "session-affinity-subagents",
            "session-affinity-ttl",
            "strategy",
        ],
    ),
    (
        "routing.cooldown",
        &[
            "disable-cooling",
            "save-cooldown-status",
            "transient-error-cooldown-seconds",
        ],
    ),
    (
        "routing.retry",
        &[
            "max-retry-credentials",
            "max-retry-interval",
            "request-retry",
        ],
    ),
    (
        "server",
        &[
            "commercial-mode",
            "discovery",
            "host",
            "port",
            "tls",
            "trusted-proxies",
        ],
    ),
    (
        "server.discovery",
        &[
            "advertise-management",
            "auth-required",
            "enabled",
            "interfaces",
            "service-name",
            "service-type",
            "subtypes",
        ],
    ),
    ("server.discovery.interfaces", &["exclude", "include"]),
    ("server.tls", &["cert", "enable", "key"]),
    ("upstream", &["claude", "codex", "xai"]),
    (
        "upstream.claude",
        &[
            "disable-claude-cloak-mode",
            "disable-cloaking-model-list",
            "header-defaults",
            "model-level-cooling",
        ],
    ),
    (
        "upstream.claude.header-defaults",
        &[
            "arch",
            "os",
            "package-version",
            "runtime-version",
            "stabilize-device-profile",
            "timeout",
            "timezone",
            "user-agent",
        ],
    ),
    (
        "upstream.codex",
        &[
            "disable-codex-cloaking",
            "model-level-cooling",
            "orphan-delegation-compatibility",
            "response-steering",
            "stream-bootstrap-buffering",
            "stream-bootstrap-timeout",
        ],
    ),
    ("upstream.xai", &["inject-x-search"]),
];

/// Where the TURN servers of the Codex live media relay are listed.
const ICE_SERVERS: &[&str] = &[
    "oauth",
    "providers",
    "codex",
    "live-media-relay",
    "ice-servers",
];

/// Where the management key is in the v8 layout.
const SECRET_KEY: &str = "management.secret-key";

/// A config file in the v8 layout. Its `Debug` shows the root's kind and
/// size, not the config.
#[derive(Clone)]
pub struct V8Document {
    root: Node,
}

impl fmt::Debug for V8Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("V8Document")
            .field("root", &self.root)
            .finish()
    }
}

/// A YAML value decoded as yaml.v3 decodes into Go's `any`. Its `Debug`
/// shows the kind of value and the size of a sequence or mapping, not what
/// it holds.
#[derive(Clone, PartialEq)]
pub enum AnyValue {
    Null,
    Bool(bool),
    /// An integer that fits in an `int64`.
    Int(i64),
    /// A larger integer that fits in a `uint64`.
    Uint(u64),
    Float(f64),
    Str(String),
    /// A timestamp, which yaml.v3 decodes to a `time.Time`: the RFC 3339
    /// text Go's JSON encoder writes for it, or `None` when the encoder
    /// refuses it (a zone 24 hours or more from UTC), and the time itself,
    /// which two values compare as Go's `reflect.DeepEqual` does. Make one
    /// with [`AnyValue::time`].
    Time(Option<String>, YamlTime),
    Seq(Vec<AnyValue>),
    /// A mapping whose keys are all strings, sorted.
    Map(BTreeMap<String, AnyValue>),
    /// A mapping with a key that isn't a string. yaml.v3 decodes it into a
    /// `map[any]any`, which Go's JSON encoder can't write.
    AnyMap,
}

impl fmt::Debug for AnyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("Null"),
            Self::Bool(_) => f.write_str("Bool(..)"),
            Self::Int(_) => f.write_str("Int(..)"),
            Self::Uint(_) => f.write_str("Uint(..)"),
            Self::Float(_) => f.write_str("Float(..)"),
            Self::Str(_) => f.write_str("Str(..)"),
            Self::Time(..) => f.write_str("Time(..)"),
            Self::Seq(items) => f.debug_struct("Seq").field("items", &items.len()).finish(),
            Self::Map(entries) => f
                .debug_struct("Map")
                .field("entries", &entries.len())
                .finish(),
            Self::AnyMap => f.write_str("AnyMap"),
        }
    }
}

impl AnyValue {
    /// The value yaml.v3 decodes the timestamp `text` to, or `None` when
    /// `text` isn't one.
    pub fn time(text: &str) -> Option<Self> {
        parse_timestamp(text).map(Self::from_timestamp)
    }

    pub(crate) fn from_timestamp(time: Timestamp) -> Self {
        Self::Time(time.json_text(), YamlTime(time))
    }
}

/// The `time.Time` yaml.v3 decodes a timestamp to, as `reflect.DeepEqual`
/// compares it: two are equal when they are the same instant in the same
/// zone, where `Z` and no zone are UTC, and a numeric offset, even
/// `+00:00`, isn't. Its `Debug` doesn't show the time.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct YamlTime(Timestamp);

impl YamlTime {
    /// A timestamp text that decodes to this time again.
    pub(crate) fn text(&self) -> String {
        self.0.text()
    }
}

impl fmt::Debug for YamlTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("YamlTime(..)")
    }
}

impl V8Document {
    /// Reads a config file's contents into the v8 layout (upstream's
    /// `NormalizeConfigLayout` as a v8 configuration write runs it). The
    /// error is upstream's: a file that isn't YAML, has no document, or
    /// doesn't make a valid v8 layout.
    pub fn migrate(data: &[u8]) -> Result<Self, ConfigError> {
        let Ok(text) = std::str::from_utf8(data) else {
            return Err(ConfigError::new(
                ConfigErrorKind::Syntax,
                "yaml: input is not valid UTF-8",
            ));
        };
        let root = match parse_document(text) {
            Ok(Some(root)) => root,
            Ok(None) => return Err(ConfigError::new(ConfigErrorKind::Empty, "empty config")),
            Err(error) => return Err(ConfigError::new(ConfigErrorKind::Syntax, error.message())),
        };
        flatten_v8(&root)?;
        let mut root = expand_merges(&root);
        normalize_private_ip_alias(&mut root)?;

        // Empty legacy structs have no fields to move: they move as empty
        // v8 mappings, and so do null ones.
        let mut paths: Vec<(&str, &str)> = V8_CLIENT_PATHS
            .iter()
            .chain(V8_SHARED_PATHS)
            .chain(V8_PATHS)
            .copied()
            .collect();
        for &(old, current) in V8_STRUCT_PATHS.iter().chain(V8_SHARED_STRUCT_PATHS) {
            let Some(node) = yaml_path_mut(&mut root, old) else {
                continue;
            };
            if node.tag == "!!null" {
                node.kind = Kind::Mapping;
                node.tag = "!!map".into();
                node.value = Text::default();
            } else if !node.is_mapping() || !node.content.is_empty() {
                continue;
            }
            paths.push((old, current));
        }
        for (old, current) in paths {
            let Some(value) = legacy_path(&root, old).cloned() else {
                continue;
            };
            let present = yaml_path(&root, current).is_some();
            delete_yaml_path(&mut root, old);
            if !present {
                set_yaml_path(&mut root, current, &value);
            }
        }
        for &(old, group) in V8_KEY_FAMILIES {
            let Some(keys) = yaml_path(&root, old).cloned() else {
                continue;
            };
            let path = format!("api-keys.{group}");
            if yaml_path(&root, &path).is_none() {
                set_yaml_path(&mut root, &path, &group_legacy_keys(&keys, group));
            }
            delete_yaml_path(&mut root, old);
        }
        drop_unknown_sections(&mut root);
        set_yaml_path(&mut root, "config-version", &Node::scalar("!!int", "8"));
        write_and_read_back(&mut root);
        Ok(Self { root })
    }

    /// Moves settings to the earlier v8 spellings under the dotted `path`
    /// a read asks for, so that the read finds them there (upstream's
    /// `ProjectV8ConfigAliases`). A section that holds fields of its own
    /// stays where it is.
    pub fn project_aliases(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        let aliases = V8_CLIENT_PATHS
            .iter()
            .chain(V8_SHARED_PATHS)
            .chain(V8_SHARED_STRUCT_PATHS);
        for &(old, current) in aliases {
            let related = path == old
                || old
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('.'))
                || path
                    .strip_prefix(old)
                    .is_some_and(|rest| rest.starts_with('.'));
            if !related {
                continue;
            }
            let Some(value) = yaml_path(&self.root, current) else {
                continue;
            };
            if value.is_mapping() && !value.content.is_empty() {
                continue;
            }
            let value = value.clone();
            set_yaml_path(&mut self.root, old, &value);
            delete_yaml_path(&mut self.root, current);
        }
    }

    /// Drops the `username` and `credential` of each TURN server of the
    /// Codex live media relay, as upstream's JSON reads do.
    pub fn redact_turn_secrets(&mut self) {
        let Some(servers) = node_at_mut(&mut self.root, ICE_SERVERS) else {
            return;
        };
        if servers.kind != Kind::Sequence {
            return;
        }
        for server in &mut servers.content {
            remove_key(server, "username");
            remove_key(server, "credential");
        }
    }

    /// The management key, when it is set and not a bcrypt hash: what
    /// upstream hashes when it loads the config.
    pub fn plain_management_key(&self) -> Option<String> {
        let node = yaml_path(&self.root, SECRET_KEY)?;
        if node.kind != Kind::Scalar {
            return None;
        }
        let key = scalar_string(node).ok()??;
        (!key.is_empty() && !looks_like_bcrypt(&key)).then_some(key)
    }

    /// Replaces the management key with `hash`, as upstream writes the
    /// hash it makes of a plain key into the file.
    pub fn set_management_key_hash(&mut self, hash: &str) {
        set_yaml_path(&mut self.root, SECRET_KEY, &Node::scalar("!!str", hash));
    }

    /// The value at `parts`, one mapping key each, decoded as yaml.v3
    /// decodes into Go's `any`; `None` when there is no such key, or a
    /// value on the way isn't a mapping (upstream's `configV8Node`). The
    /// error is yaml.v3's, as when a value doesn't resolve to its tag, or
    /// excessive aliasing.
    pub fn value(&self, parts: &[&str]) -> Option<Result<AnyValue, ConfigError>> {
        let node = node_at(&self.root, parts)?;
        let budget = AliasBudget::default();
        Some(
            decode_any(node, &budget)
                .map_err(|message| ConfigError::new(ConfigErrorKind::Decode, message)),
        )
    }
}

/// Whether `secret` looks like a bcrypt hash (upstream's `looksLikeBcrypt`).
fn looks_like_bcrypt(secret: &str) -> bool {
    secret.len() > 4
        && ["$2a$", "$2b$", "$2y$"]
            .iter()
            .any(|prefix| secret.starts_with(prefix))
}

/// Upstream's `legacyPath`: the node at `path`, except an `api-keys`
/// mapping, which is the v8 API-key groups rather than the legacy client
/// keys.
fn legacy_path<'a>(root: &'a Node, path: &str) -> Option<&'a Node> {
    let node = yaml_path(root, path)?;
    (path != "api-keys" || !node.is_mapping()).then_some(node)
}

/// The node at a dotted path, to change.
fn yaml_path_mut<'a>(node: &'a mut Node, path: &str) -> Option<&'a mut Node> {
    let mut current = node;
    for part in path.split('.') {
        let index = find_map_key_index(current, part)?;
        current = current.content.get_mut(index + 1)?;
    }
    Some(current)
}

/// The node under `parts`, one mapping key each (upstream's
/// `configV8Node`).
fn node_at<'a>(node: &'a Node, parts: &[&str]) -> Option<&'a Node> {
    let mut current = node;
    for part in parts {
        let index = find_map_key_index(current, part)?;
        current = current.content.get(index + 1)?;
    }
    Some(current)
}

/// [`node_at`], to change.
fn node_at_mut<'a>(node: &'a mut Node, parts: &[&str]) -> Option<&'a mut Node> {
    let mut current = node;
    for part in parts {
        let index = find_map_key_index(current, part)?;
        current = current.content.get_mut(index + 1)?;
    }
    Some(current)
}

/// Removes the first `key` of a mapping and its value (upstream's
/// `deleteConfigV8Path` with one part).
fn remove_key(node: &mut Node, key: &str) {
    if let Some(index) = find_map_key_index(node, key) {
        node.content
            .drain(index..(index + 2).min(node.content.len()));
    }
}

/// Upstream's `groupLegacyKeys`: a legacy API-key list as v8 groups. An
/// OpenAI-compatible provider becomes a group as it is, its
/// `api-key-entries` renamed `keys`; any other entry becomes a group of
/// its own named `<provider>-<n>`, holding its endpoint and shared fields,
/// with the rest of the entry as its one key.
fn group_legacy_keys(keys: &Node, provider: &str) -> Node {
    let mut out = Node::sequence();
    for (index, entry) in keys.content.iter().enumerate() {
        let group = if provider == "openai-compatibility" {
            let mut group = entry.clone();
            let entries = yaml_path(&group, "api-key-entries")
                .cloned()
                .unwrap_or_else(Node::sequence);
            set_yaml_path(&mut group, "keys", &entries);
            delete_yaml_path(&mut group, "api-key-entries");
            group
        } else {
            let mut group = Node::mapping();
            let name = format!("{provider}-{}", index + 1);
            set_yaml_path(&mut group, "name", &Node::scalar("!!str", &name));
            let mut key = entry.clone();
            for (field, value) in entry.pairs() {
                let field = field.value.as_str();
                if field == "base-url" || SHARED_KEY_FIELDS.contains(&field) {
                    set_yaml_path(&mut group, field, value);
                    delete_yaml_path(&mut key, field);
                }
            }
            let mut keys = Node::sequence();
            keys.content.push(key);
            set_yaml_path(&mut group, "keys", &keys);
            group
        };
        out.content.push(group);
    }
    out
}

/// Drops the keys the v8 layout doesn't know, at the top level and in each
/// section it lists the fields of (upstream's `commentUnknownV8Sections`,
/// which comments them out).
fn drop_unknown_sections(root: &mut Node) {
    keep_keys(root, V8_ROOTS);
    for [key, value] in root.content.as_chunks_mut::<2>().0 {
        drop_unknown_fields(value, &key.value);
    }
}

/// [`drop_unknown_sections`] for the section at `path` and those under it.
fn drop_unknown_fields(node: &mut Node, path: &str) {
    if !node.is_mapping() {
        return;
    }
    let Some((_, allowed)) = V8_CHILDREN.iter().find(|(section, _)| *section == path) else {
        return;
    };
    keep_keys(node, allowed);
    for [key, value] in node.content.as_chunks_mut::<2>().0 {
        drop_unknown_fields(value, &format!("{path}.{}", key.value));
    }
}

/// Removes the keys of a mapping that aren't in `allowed`, with their
/// values.
fn keep_keys(node: &mut Node, allowed: &[&str]) {
    let mut items = std::mem::take(&mut node.content).into_iter();
    while let Some(key) = items.next() {
        match items.next() {
            Some(value) => {
                if allowed.contains(&key.value.as_str()) {
                    node.content.push(key);
                    node.content.push(value);
                }
            }
            None => node.content.push(key),
        }
    }
}

/// Decodes `node` as yaml.v3 decodes into Go's `any`, with a budget of its
/// own for the text aliases produce. The error is yaml.v3's message.
pub(crate) fn decode_any_value(node: &Node) -> Result<AnyValue, String> {
    decode_any(node, &AliasBudget::default())
}

/// Decodes `node` as yaml.v3 decodes into Go's `any`, counting the text
/// aliases produce against `budget`. The error is yaml.v3's message.
fn decode_any(node: &Node, budget: &AliasBudget) -> Result<AnyValue, String> {
    match node.kind {
        Kind::Poison => Err(node.value.to_string()),
        Kind::Scalar => decode_scalar(node, budget),
        Kind::Sequence => node
            .content
            .iter()
            .map(|item| decode_any(item, budget))
            .collect::<Result<_, _>>()
            .map(AnyValue::Seq),
        Kind::Mapping => decode_mapping(node, budget),
    }
}

/// A scalar: `!!str` and unknown tags as strings, a timestamp as a time,
/// `!!binary` decoded (yaml.v3's `decoder.scalar`).
fn decode_scalar(node: &Node, budget: &AliasBudget) -> Result<AnyValue, String> {
    budget.charge(node).map_err(|error| error.message())?;
    let resolved = resolve_node(node).map_err(|error| error.message())?;
    Ok(match resolved.value {
        Scalar::Null => AnyValue::Null,
        Scalar::Bool(value) => AnyValue::Bool(value),
        Scalar::Int(value) => AnyValue::Int(value),
        Scalar::Uint(value) => AnyValue::Uint(value),
        Scalar::Float(value) => AnyValue::Float(value),
        Scalar::Timestamp(time) => AnyValue::from_timestamp(time),
        Scalar::Str(value) => AnyValue::Str(value.to_string()),
    })
}

/// A mapping: a `map[string]any` when every key is a string, else a
/// `map[any]any` (yaml.v3's `decoder.mapping` and `isStringMap`). A key
/// repeated, or one that is a sequence or mapping, fails.
fn decode_mapping(node: &Node, budget: &AliasBudget) -> Result<AnyValue, String> {
    let pairs: Vec<(&Node, &Node)> = node.pairs().collect();
    for (index, (key, _)) in pairs.iter().enumerate() {
        let repeated = pairs
            .iter()
            .skip(index + 1)
            .any(|(other, _)| other.kind == key.kind && other.value == key.value);
        if repeated {
            return Err(format!(
                "yaml: unmarshal errors:\n  line {}: mapping key already defined",
                key.line
            ));
        }
    }
    let string_keys = pairs
        .iter()
        .all(|(key, _)| key.tag == "!!str" || key.tag == "!!merge");
    if string_keys {
        let mut map = BTreeMap::new();
        for (key, value) in pairs {
            budget.charge(key).map_err(|error| error.message())?;
            map.insert(key.value.to_string(), decode_any(value, budget)?);
        }
        return Ok(AnyValue::Map(map));
    }
    for (key, value) in pairs {
        if matches!(key.kind, Kind::Sequence | Kind::Mapping) {
            return Err("yaml: invalid map key".to_owned());
        }
        decode_any(key, budget)?;
        decode_any(value, budget)?;
    }
    Ok(AnyValue::AnyMap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrate(text: &str) -> V8Document {
        V8Document::migrate(text.as_bytes()).expect("migrates")
    }

    fn get(document: &V8Document, path: &str) -> Option<AnyValue> {
        let parts: Vec<&str> = if path.is_empty() {
            Vec::new()
        } else {
            path.split('/').collect()
        };
        document.value(&parts).map(|value| value.expect("decodes"))
    }

    fn map(entries: &[(&str, AnyValue)]) -> AnyValue {
        AnyValue::Map(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        )
    }

    fn text(value: &str) -> AnyValue {
        AnyValue::Str(value.to_owned())
    }

    fn time(value: &str) -> AnyValue {
        AnyValue::time(value).expect("a timestamp")
    }

    /// Not upstream's: a time compares as Go's `reflect.DeepEqual`
    /// compares the `time.Time` yaml.v3 decodes it to: the same instant in
    /// the same zone, where `Z` and no zone are UTC and a numeric offset,
    /// even a zero one, isn't. The JSON text alone doesn't tell them apart.
    #[test]
    fn times_compare_by_instant_and_zone() {
        let same = [
            ("2001-12-14", "2001-12-14T00:00:00Z"),
            ("2001-12-14 21:59:43.10", "2001-12-14T21:59:43.1Z"),
            ("2001-12-14T21:59:43+00:00", "2001-12-14T21:59:43-00:00"),
            (
                "2001-12-14t21:59:43.10-05:00",
                "2001-12-14T21:59:43.1-05:00",
            ),
            ("2001-12-14T21:59:43+01:60", "2001-12-14T21:59:43+02:00"),
        ];
        for (a, b) in same {
            assert_eq!(time(a), time(b), "{a} {b}");
        }
        // The text a time is handed on as decodes to it again.
        for input in [
            "2001-12-14",
            "2001-12-14T21:59:43.000000001+00:00",
            "2001-12-14T21:59:43-24:60",
            "2001-12-14T21:59:43+24:59",
        ] {
            let AnyValue::Time(_, yaml_time) = time(input) else {
                unreachable!()
            };
            assert_eq!(time(&yaml_time.text()), time(input), "{input}");
        }
        let different = [
            ("2001-12-14T21:59:43Z", "2001-12-14T21:59:43+00:00"),
            ("2001-12-14", "2001-12-14T00:00:00-00:00"),
            ("2001-12-14T22:59:43+01:00", "2001-12-14T21:59:43Z"),
            ("2001-12-14T21:59:43+24:00", "2001-12-14T21:59:43+24:30"),
        ];
        for (a, b) in different {
            assert_ne!(time(a), time(b), "{a} {b}");
        }
        let AnyValue::Time(text, _) = time("2001-12-14T21:59:43+00:00") else {
            unreachable!()
        };
        assert_eq!(text.as_deref(), Some("2001-12-14T21:59:43Z"));
        let AnyValue::Time(text, _) = time("2001-12-14T21:59:43+24:00") else {
            unreachable!()
        };
        assert_eq!(text, None);
        assert_eq!(AnyValue::time("2001-12-14T21:59:43"), None);
    }

    /// Not upstream's: the struct table matches the paths it was generated
    /// with, and every v8 section the paths name is in the children table.
    #[test]
    fn tables_agree_with_the_v8_paths() {
        for (_, current) in V8_PATHS {
            let (parent, field) = current.rsplit_once('.').unwrap_or(("", current));
            if parent.is_empty() {
                assert!(V8_ROOTS.contains(&field), "{current}");
                continue;
            }
            let (_, fields) = V8_CHILDREN
                .iter()
                .find(|(section, _)| *section == parent)
                .unwrap_or_else(|| panic!("no section {parent}"));
            assert!(fields.contains(&field), "{current}");
        }
        for (_, current) in V8_STRUCT_PATHS {
            let root = current.split('.').next().unwrap();
            assert!(V8_ROOTS.contains(&root), "{current}");
        }
    }

    /// Ported from upstream's config_v8_test.go
    /// (TestConfigV8MigrationAndLegacyAPI): legacy settings move to their
    /// v8 paths and empty or null sections become empty v8 ones.
    #[test]
    fn legacy_settings_move_to_their_v8_paths() {
        let document = migrate(
            "# Keep this configuration\nport: 8317\nrequest-retry: 3\napi-keys: [client]\n\
             ws-auth: true\ntls: {}\npayload: null\ncodex: {live-media-relay: {}}\n",
        );
        let empty = map(&[]);
        assert_eq!(
            get(&document, ""),
            Some(map(&[
                (
                    "access",
                    map(&[("api-keys", AnyValue::Seq(vec![text("client")]))])
                ),
                ("config-version", AnyValue::Int(8)),
                (
                    "oauth",
                    map(&[(
                        "providers",
                        map(&[
                            ("aistudio", map(&[("ws-auth", AnyValue::Bool(true))])),
                            ("codex", map(&[("live-media-relay", empty.clone())])),
                        ]),
                    )]),
                ),
                ("requests", map(&[("payload", empty.clone())])),
                (
                    "routing",
                    map(&[("retry", map(&[("request-retry", AnyValue::Int(3))]))])
                ),
                (
                    "server",
                    map(&[("port", AnyValue::Int(8317)), ("tls", empty)]),
                ),
            ]))
        );
    }

    /// Not upstream's: a v8 setting wins over its legacy spelling, which is
    /// dropped, and keys the v8 layout doesn't know are dropped.
    #[test]
    fn v8_settings_win_and_unknown_keys_go() {
        let document = migrate(
            "port: 1\nserver: {port: 2, bogus: 3}\nmystery: {a: 1}\n\
             api-keys: {codex: [{base-url: u, keys: [{api-key: k}]}]}\n",
        );
        assert_eq!(
            get(&document, "server"),
            Some(map(&[("port", AnyValue::Int(2))]))
        );
        assert_eq!(get(&document, "mystery"), None);
        assert_eq!(get(&document, "port"), None);
        assert_eq!(
            get(&document, "api-keys/codex"),
            Some(AnyValue::Seq(vec![map(&[
                ("base-url", text("u")),
                ("keys", AnyValue::Seq(vec![map(&[("api-key", text("k"))])])),
            ])]))
        );
    }

    /// Not upstream's: the `models` section (v8.0.15) is kept, from a legacy
    /// file or a v8 one, less the keys it doesn't know, and a source is
    /// read as the file has it, even one the loader would refuse. Recorded
    /// from upstream's `ConfigV8` (v8.0.15) under Go 1.26.4.
    #[test]
    fn model_catalog_sources_are_kept() {
        let document = migrate(
            "port: 1\nmodels:\n  catalog: https://catalog.invalid/models.json\n  \
             codex-catalog: ''\n  bogus: 1\n",
        );
        assert_eq!(
            get(&document, "models"),
            Some(map(&[
                ("catalog", text("https://catalog.invalid/models.json")),
                ("codex-catalog", text("")),
            ]))
        );
        let document = migrate(
            "config-version: 8\nmodels: {devin-catalog: https://devin.invalid/d.json, extra: [1]}\n\
             server: {port: 2}\n",
        );
        assert_eq!(
            get(&document, "models"),
            Some(map(&[(
                "devin-catalog",
                text("https://devin.invalid/d.json")
            )]))
        );
        assert_eq!(get(&document, "models/catalog"), None);
        let document = migrate("port: 1\nmodels: {catalog: relative/models.json}\n");
        assert_eq!(
            get(&document, "models"),
            Some(map(&[("catalog", text("relative/models.json"))]))
        );
    }

    /// Not upstream's: legacy key lists become groups, an OpenAI-compatible
    /// provider's entries its keys.
    #[test]
    fn legacy_key_lists_become_groups() {
        let document = migrate(
            "codex-api-key:\n  - {api-key: k1, base-url: b, prefix: p, weight: 2}\n  - {api-key: k2}\n\
             openai-compatibility:\n  - {name: n, base-url: o, api-key-entries: [{api-key: x}]}\n  - {name: m}\n",
        );
        let group = |name: &str, fields: &[(&str, AnyValue)], key: AnyValue| {
            let mut entries = vec![("name", text(name)), ("keys", AnyValue::Seq(vec![key]))];
            entries.extend_from_slice(fields);
            map(&entries)
        };
        assert_eq!(
            get(&document, "api-keys/codex"),
            Some(AnyValue::Seq(vec![
                group(
                    "codex-1",
                    &[("base-url", text("b")), ("prefix", text("p"))],
                    map(&[("api-key", text("k1")), ("weight", AnyValue::Int(2))]),
                ),
                group("codex-2", &[], map(&[("api-key", text("k2"))])),
            ]))
        );
        assert_eq!(
            get(&document, "api-keys/openai-compatibility"),
            Some(AnyValue::Seq(vec![
                map(&[
                    ("base-url", text("o")),
                    ("keys", AnyValue::Seq(vec![map(&[("api-key", text("x"))])])),
                    ("name", text("n")),
                ]),
                map(&[("keys", AnyValue::Seq(Vec::new())), ("name", text("m"))]),
            ]))
        );
        assert_eq!(get(&document, "codex-api-key"), None);
    }

    /// Ported from upstream's config_v8_client_test.go
    /// (TestConfigV8ClientCodexOptimizeMultiAgentV2): each earlier spelling
    /// reads at the v8 path and back at its own.
    #[test]
    fn client_aliases_read_at_each_spelling() {
        for raw in [
            "codex: {optimize-multi-agent-v2: true}\n",
            "providers: {codex: {optimize-multi-agent-v2: true}}\n",
            "oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}\n",
        ] {
            let text = format!("{raw}client: {{codex: {{enable-apply-patch: true}}}}\n");
            let document = migrate(&text);
            assert_eq!(
                get(&document, "client/codex/optimize-multi-agent-v2"),
                Some(AnyValue::Bool(true)),
                "{raw}"
            );
            for path in [
                "providers/codex/optimize-multi-agent-v2",
                "oauth/providers/codex/optimize-multi-agent-v2",
                "codex/optimize-multi-agent-v2",
            ] {
                let mut document = document.clone();
                document.project_aliases(&path.replace('/', "."));
                assert_eq!(
                    get(&document, path),
                    Some(AnyValue::Bool(true)),
                    "{raw} {path}"
                );
            }
        }
    }

    /// Ported from upstream's config_v8_compatibility_test.go
    /// (TestConfigV8HistoricalProviderSubtrees): a historical provider
    /// subtree gathers its shared settings.
    #[test]
    fn historical_subtrees_gather_shared_settings() {
        let mut document = migrate(
            "upstream: {codex: {response-steering: true, stream-bootstrap-buffering: true}, xai: {inject-x-search: true}}\n\
             oauth: {providers: {codex: {header-defaults: {user-agent: oauth-agent}}}}\n\
             client: {codex: {optimize-multi-agent-v2: true, enable-apply-patch: true}}\n",
        );
        document.project_aliases("oauth.providers.codex");
        let Some(AnyValue::Map(codex)) = get(&document, "oauth/providers/codex") else {
            panic!("no codex section");
        };
        assert_eq!(codex.get("response-steering"), Some(&AnyValue::Bool(true)));
        assert_eq!(
            codex.get("optimize-multi-agent-v2"),
            Some(&AnyValue::Bool(true))
        );
        assert_eq!(get(&document, "upstream/codex"), None);
    }

    /// Ported from upstream's config_v8_test.go
    /// (TestConfigV8JSONTURNSecrets): JSON reads leave out TURN secrets.
    #[test]
    fn turn_secrets_are_redacted() {
        let mut document = migrate(
            "codex:\n  live-media-relay:\n    ice-servers:\n      - urls: [turn:a]\n        username: u\n        credential: c\n      - urls: [turn:b]\n",
        );
        document.redact_turn_secrets();
        assert_eq!(
            get(
                &document,
                "oauth/providers/codex/live-media-relay/ice-servers"
            ),
            Some(AnyValue::Seq(vec![
                map(&[("urls", AnyValue::Seq(vec![text("turn:a")]))]),
                map(&[("urls", AnyValue::Seq(vec![text("turn:b")]))]),
            ]))
        );
    }

    /// Not upstream's: a plain management key is found wherever it was
    /// written, and a hash isn't.
    #[test]
    fn plain_management_keys_are_found() {
        let document = migrate("remote-management: {secret-key: 12345}\n");
        assert_eq!(document.plain_management_key().as_deref(), Some("12345"));
        let mut document = migrate("management: {secret-key: plain}\n");
        assert_eq!(document.plain_management_key().as_deref(), Some("plain"));
        document.set_management_key_hash("$2a$10$hash");
        assert_eq!(document.plain_management_key(), None);
        assert_eq!(
            get(&document, "management/secret-key"),
            Some(text("$2a$10$hash"))
        );
        assert_eq!(
            migrate("management: {secret-key: ''}\n").plain_management_key(),
            None
        );
    }

    /// Not upstream's: values decode as yaml.v3 decodes into `any`.
    #[test]
    fn values_decode_as_yaml_v3_does() {
        let document = migrate(
            "plugins:\n  configs:\n    a: 2024-01-02\n    b: 9999999999999999999\n    c: 1.5\n    d: ~\n    e: !!binary aGk=\n    f: '7'\n    g: !custom x\n    h: []\n    i: {}\n    j: {1: x}\n",
        );
        assert_eq!(
            get(&document, "plugins/configs"),
            Some(map(&[
                ("a", time("2024-01-02T00:00:00Z")),
                ("b", AnyValue::Uint(9_999_999_999_999_999_999)),
                ("c", AnyValue::Float(1.5)),
                ("d", AnyValue::Null),
                ("e", text("hi")),
                ("f", text("7")),
                ("g", text("x")),
                ("h", AnyValue::Seq(Vec::new())),
                ("i", map(&[])),
                ("j", AnyValue::AnyMap),
            ]))
        );
        assert_eq!(get(&document, "plugins/configs/a/b"), None);
        // Loading checks the whole file first, so only a tree changed since
        // fails here.
        assert!(decode_any(&Node::scalar("!!int", "x"), &AliasBudget::default()).is_err());
        let mut repeated = Node::mapping();
        repeated.content = vec![
            Node::scalar("!!str", "a"),
            Node::scalar("!!null", ""),
            Node::scalar("!!str", "a"),
            Node::scalar("!!null", ""),
        ];
        assert!(decode_any(&repeated, &AliasBudget::default()).is_err());
    }

    /// Not upstream's: values read as upstream reads them once it has
    /// written the migrated document out and read it back: a timestamp in
    /// a flow mapping and an empty flow value come back strings. Recorded
    /// from upstream's `GetConfigV8` handler under Go 1.26.4.
    #[test]
    fn values_read_as_written_out_and_read_back() {
        let document = migrate(
            "plugins:\n  configs:\n    flow: {value: 2024-01-02T03:04:05+24:00, empty: }\n    block: 2024-01-02T03:04:05Z\n    seq: [2002-12-14, a:b]\n",
        );
        assert_eq!(
            get(&document, "plugins/configs/flow"),
            Some(map(&[
                ("empty", text("")),
                ("value", text("2024-01-02T03:04:05+24:00")),
            ]))
        );
        assert_eq!(
            get(&document, "plugins/configs/block"),
            Some(time("2024-01-02T03:04:05Z"))
        );
        assert_eq!(
            get(&document, "plugins/configs/seq"),
            Some(AnyValue::Seq(vec![
                time("2002-12-14T00:00:00Z"),
                text("a:b"),
            ]))
        );
    }

    /// Not upstream's: a value whose aliases expand to more than 64 MiB of
    /// text fails to decode, though the document holds it in a little more
    /// than one copy.
    #[test]
    fn values_stop_at_excessive_aliasing() {
        let long = "x".repeat(1 << 20);
        let copies = vec!["*big"; 65].join(", ");
        let document = migrate(&format!(
            "plugins:\n  configs:\n    big: &big {long}\n    copies: [{copies}]\n"
        ));
        assert_eq!(
            document
                .value(&["plugins", "configs", "big"])
                .map(|value| value.is_ok()),
            Some(true)
        );
        let Some(Err(error)) = document.value(&["plugins", "configs", "copies"]) else {
            panic!("the copies don't decode");
        };
        assert_eq!(
            (error.kind(), error.to_string().as_str()),
            (
                ConfigErrorKind::Decode,
                "yaml: document contains excessive aliasing"
            )
        );
    }

    /// Not upstream's: the `Debug` of a document and of its values shows
    /// their shape, never the management key or a provider's key.
    #[test]
    fn debug_leaves_out_secrets() {
        let document = migrate(
            "remote-management:\n  secret-key: marker-secret-41\nclaude-api-key:\n  - api-key: marker-key-42\n",
        );
        let mut shown = vec![format!("{document:?}"), format!("{document:#?}")];
        for path in ["", "management", "management/secret-key", "api-keys/claude"] {
            let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
            let Some(Ok(value)) = document.value(&parts) else {
                panic!("{path} decodes");
            };
            shown.push(format!("{value:?}"));
        }
        assert_eq!(
            get(&document, "management/secret-key"),
            Some(text("marker-secret-41"))
        );
        for shown in &shown {
            assert!(!shown.contains("marker-"), "{shown}");
        }
        assert!(
            shown[0].starts_with("V8Document { root: Node { kind: Mapping"),
            "{}",
            shown[0]
        );
        assert_eq!(shown[4], "Str(..)");
    }

    /// Not upstream's: files that don't make a v8 layout fail as upstream
    /// fails them.
    #[test]
    fn bad_files_fail() {
        for (input, kind, message) in [
            ("", ConfigErrorKind::Empty, "empty config"),
            (
                "- a\n",
                ConfigErrorKind::Invalid,
                "config must be a mapping",
            ),
            (
                "server: 1\n",
                ConfigErrorKind::Invalid,
                "server must be a mapping",
            ),
            (
                "plugins: {configs: {a: !!int x}}\n",
                ConfigErrorKind::Decode,
                "yaml: cannot decode !!str as a !!int",
            ),
        ] {
            let error = V8Document::migrate(input.as_bytes()).expect_err(input);
            assert_eq!((error.kind(), error.to_string().as_str()), (kind, message));
        }
        let error = V8Document::migrate(b"a: \xff\n").expect_err("not UTF-8");
        assert_eq!(error.to_string(), "yaml: input is not valid UTF-8");
    }
}
