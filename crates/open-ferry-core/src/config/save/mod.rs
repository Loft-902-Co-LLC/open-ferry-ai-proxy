// Ported from CLIProxyAPI internal/config/config_yaml.go
// (SaveConfigPreserveComments, SaveConfigPreserveCommentsUpdateNestedScalar)
// and internal/api/handlers/management/config_basic.go (WriteConfig)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Writing the config file back with its comments, its layout and the
//! settings this port doesn't type.
//!
//! - [`save_preserving_comments`] writes a [`Config`]'s settings into the
//!   file, as upstream's `SaveConfigPreserveComments` does: it merges the
//!   settings into the file's tree, so existing keys keep their comments,
//!   order and quoting, new keys are added unless they hold a default, and
//!   a file in the v8 layout stays in it (with `migrate_v8`, or when the
//!   file already uses the v8 layout, the whole file moves to it).
//! - [`update_nested_scalar`] sets one string at a path of mapping keys
//!   (`SaveConfigPreserveCommentsUpdateNestedScalar`).
//! - [`write_file`] writes a whole file, as the management API's
//!   `WriteConfig` does: a file in the v8 layout is completed into it
//!   first, and comment lines are moved to the start of their line.
//!
//! YAML is read and written with a port of gopkg.in/yaml.v3 (the
//! crate-private `config::yaml3`), so the bytes written are upstream's.
//! Settings, sections and keys the [`Config`] doesn't type stay as the file
//! has them, with their comments.
//!
//! Every write is checked to load as a config ([`Config::load`]'s rules
//! and wording) before anything is written; it is atomic, refuses a
//! symbolic link, and keeps the file's previous contents as
//! `<file name>.bak` (see `write`). A refused write leaves the file as it
//! was and returns a [`SaveError`] that says why.
//!
//! Deviations from upstream:
//! - The check, the backup, the atomic replacement and the symbolic link
//!   refusal are this port's; upstream writes the file in place.
//! - A file that isn't UTF-8 is refused (`yaml: input is not valid
//!   UTF-8`); yaml.v3 also reads UTF-16.
//! - After a save that moves the file to the v8 layout, upstream updates
//!   the config's `OAuthOnlyFields` from the written file; the `&Config`
//!   here isn't changed, so the caller reloads the file.
//! - Upstream decodes a migrated file once more and fails with `decode
//!   migrated config: ...`; the load check covers it, with its own wording.
//! - Each submodule lists its own deviations.

mod generate;
mod merge;
mod tree;
mod v8;
mod write;

use std::borrow::Cow;
use std::path::Path;
use std::{fmt, io};

use super::Config;
use super::v8 as loader_v8;
use super::yaml as loader_yaml;
use super::yaml3::compose::{self, YamlError};
use super::yaml3::encode;
use super::yaml3::{Kind, MAP_TAG, NULL_TAG, STR_TAG};

// The tree operations the management API's v8 config edits build on.
#[allow(unused_imports)]
pub(crate) use super::yaml3::Node;
#[allow(unused_imports)]
pub(crate) use tree::{
    copy_yaml_path_value, delete_yaml_path, expand_config_aliases, find_map_key_index,
    get_or_create_map_value, legacy_path, marshal, marshal_config, normalize_comment_indentation,
    set_yaml_path, set_yaml_path_with_comments, yaml_path, yaml_path_mut,
};
#[allow(unused_imports)]
pub(crate) use v8::{is_v8_config_layout, normalize_config_layout, project_v8_config_aliases};

/// Why a config write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SaveErrorKind {
    /// Reading or writing a file failed.
    Io,
    /// The file isn't YAML yaml.v3 reads, or the tree can't be written.
    Yaml,
    /// The YAML doesn't have the shape of a config file.
    Invalid,
    /// The result wouldn't load as a config.
    Check,
    /// The path is a symbolic link.
    Symlink,
    /// The config holds a value the writer can't express.
    Unwritable,
}

/// A refused config write; nothing was written. The message is upstream's
/// where upstream has one.
#[derive(Debug)]
pub struct SaveError {
    kind: SaveErrorKind,
    message: String,
    source: Option<io::Error>,
}

impl SaveError {
    pub(crate) fn new(kind: SaveErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn io(message: impl Into<String>, source: io::Error) -> Self {
        Self {
            kind: SaveErrorKind::Io,
            message: message.into(),
            source: Some(source),
        }
    }

    /// What kind of failure this is.
    pub fn kind(&self) -> SaveErrorKind {
        self.kind
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

impl From<YamlError> for SaveError {
    fn from(error: YamlError) -> Self {
        Self::new(SaveErrorKind::Yaml, error.message())
    }
}

fn invalid(message: impl Into<String>) -> SaveError {
    SaveError::new(SaveErrorKind::Invalid, message)
}

/// `yaml.Unmarshal(data, &node)`: the document node of the first document
/// in `data`, with its comments, or an empty node when there is none.
pub(crate) fn unmarshal(data: &[u8]) -> Result<Node, SaveError> {
    Ok(compose::unmarshal(data)?)
}

/// The loader's tree of `data`, or `None` when it holds no document.
fn loader_root(data: &[u8]) -> Result<Option<loader_yaml::Node>, SaveError> {
    let text = std::str::from_utf8(data)
        .map_err(|_| SaveError::new(SaveErrorKind::Yaml, "yaml: input is not valid UTF-8"))?;
    loader_yaml::parse_document(text)
        .map_err(|error| SaveError::new(SaveErrorKind::Yaml, error.message()))
}

/// Upstream's `flattenV8` checks of a document, with its errors: the
/// loader's. Upstream runs them before it moves anything.
pub(crate) fn check_layout(data: &[u8]) -> Result<(), SaveError> {
    if let Some(root) = loader_root(data)? {
        loader_v8::flatten_v8(&root).map_err(|error| invalid(error.to_string()))?;
    }
    Ok(())
}

/// Writes `cfg`'s settings into the config file at `path`, keeping the
/// file's comments, order, layout and the settings `cfg` doesn't type
/// (upstream's `SaveConfigPreserveComments`). With `migrate_v8`, or when
/// the file uses the v8 layout, the file is written in the v8 layout.
pub fn save_preserving_comments(
    path: &Path,
    cfg: &Config,
    migrate_v8: bool,
) -> Result<(), SaveError> {
    let data = write::read(path)?;
    let out = render_preserving_comments(&data, cfg, migrate_v8)?;
    write::commit(path, &out)
}

/// Sets the string at the path of mapping `keys` in the config file at
/// `path`, creating the mappings on the way and keeping everything else
/// (upstream's `SaveConfigPreserveCommentsUpdateNestedScalar`).
pub fn update_nested_scalar(path: &Path, keys: &[&str], value: &str) -> Result<(), SaveError> {
    let data = write::read(path)?;
    let out = render_nested_scalar(&data, keys, value)?;
    write::commit(path, &out)
}

/// Writes `bytes` as the config file at `path`, as the management API's
/// `WriteConfig` does: a file in the v8 layout is moved fully into it
/// (`NormalizeConfigLayout`), and comment lines are moved to the start of
/// their line.
pub fn write_file(path: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    write::refuse_link(path)?;
    let out = render_write_file(bytes)?;
    write::commit(path, &out)
}

/// The bytes [`write_file`] writes for `data`.
pub(crate) fn render_write_file(data: &[u8]) -> Result<Vec<u8>, SaveError> {
    let doc = unmarshal(data)?;
    let mut out = Cow::Borrowed(data);
    if let Some(root) = doc.content.first()
        && is_v8_config_layout(root)?
    {
        out = Cow::Owned(normalize_config_layout(data, true)?.0);
    }
    Ok(normalize_comment_indentation(&out))
}

/// The generated tree: `cfg` as upstream marshals it, read back.
fn generated_root(cfg: &Config) -> Result<Node, SaveError> {
    let value = generate::legacy_config(cfg)
        .map_err(|error| SaveError::new(SaveErrorKind::Unwritable, error.0))?;
    let rendered = encode::marshal(&value)?;
    let generated = compose::unmarshal(&rendered)?;
    if generated.kind != Kind::Document {
        return Err(invalid("invalid generated yaml structure"));
    }
    let Some(root) = generated.content.into_iter().next() else {
        return Err(invalid("invalid generated yaml structure"));
    };
    if root.kind != Kind::Mapping {
        return Err(invalid("expected generated root mapping node"));
    }
    Ok(root)
}

/// The OAuth maps whose keys follow the settings exactly.
const OAUTH_MAPS: [&str; 4] = [
    "oauth-excluded-models",
    "oauth-model-alias",
    "oauth-request-scoped-errors",
    "oauth-settings",
];

/// The bytes [`save_preserving_comments`] writes for the file `data`.
pub(crate) fn render_preserving_comments(
    data: &[u8],
    cfg: &Config,
    migrate_v8: bool,
) -> Result<Vec<u8>, SaveError> {
    let mut original = unmarshal(data)?;
    if original.kind != Kind::Document || original.content.is_empty() {
        return Err(invalid("invalid yaml document structure"));
    }
    let Some(first) = original.content.first_mut() else {
        return Err(invalid("invalid yaml document structure"));
    };
    if first.kind != Kind::Mapping {
        return Err(invalid("expected root mapping node"));
    }
    check_layout(data)?;
    let flat = v8::flatten_v8(first)?;
    let layout = expand_config_aliases(first)?;
    *first = flat;
    let root = first;
    let migrating = migrate_v8 || is_v8_config_layout(&layout)?;
    let generated = generated_root(cfg)?;

    // Keep obsolete roots until a v8 migration can keep them as comments.
    if !migrating {
        merge::remove_legacy_auth_block(root);
        merge::remove_removed_integration_keys(root);
        merge::remove_legacy_generative_language_keys(root);
    }
    merge::remove_legacy_openai_compat_api_keys(root);
    for key in OAUTH_MAPS {
        merge::prune_mapping_to_generated_keys(root, &generated, key);
    }
    merge::merge_mapping_preserve(root, &generated, &mut Vec::new());
    v8::restore_v8_layout(root, &layout, data, cfg)?;
    if !migrating {
        // Keep the earlier client paths where the file had them, so the next
        // save of a legacy file doesn't look like a v8 one.
        for &(old, current) in loader_v8::V8_CLIENT_PATHS {
            if yaml_path(&layout, old).is_none()
                || yaml_path(&layout, current).is_some()
                || yaml_path(root, current).is_none()
            {
                continue;
            }
            if let Some(copy) = copy_yaml_path_value(root, current) {
                delete_yaml_path(root, current);
                set_yaml_path_with_comments(root, old, &copy);
            }
        }
    }
    merge::normalize_collection_node_styles(root);

    let mut out = normalize_comment_indentation(&marshal_config(&original)?);
    if migrating {
        out = normalize_config_layout(&out, true)?.0;
    }
    Ok(out)
}

/// The bytes [`update_nested_scalar`] writes for the file `data`.
pub(crate) fn render_nested_scalar(
    data: &[u8],
    keys: &[&str],
    value: &str,
) -> Result<Vec<u8>, SaveError> {
    let mut doc = unmarshal(data)?;
    if doc.kind != Kind::Document || doc.content.is_empty() {
        return Err(invalid("invalid yaml document structure"));
    }
    // `root.Decode(&map[string]any)`: duplicate keys, merge rules and a
    // root that isn't a mapping fail.
    if let Some(root) = loader_root(data)? {
        if root.is_mapping() {
            loader_yaml::check_shape(&root)
                .map_err(|error| SaveError::new(SaveErrorKind::Yaml, error.message()))?;
        } else if root.tag != NULL_TAG {
            return Err(SaveError::new(
                SaveErrorKind::Yaml,
                format!(
                    "yaml: unmarshal errors:\n  {}",
                    loader_yaml::type_error(&root, "map[string]interface {}")
                ),
            ));
        }
    }
    let Some(first) = doc.content.first_mut() else {
        return Err(invalid("invalid yaml document structure"));
    };
    *first = expand_config_aliases(first)?;
    let mut node = first;
    let last = keys.len().saturating_sub(1);
    for (index, key) in keys.iter().enumerate() {
        let next = get_or_create_map_value(node, key);
        if index == last {
            next.kind = Kind::Scalar;
            STR_TAG.clone_into(&mut next.tag);
            value.clone_into(&mut next.value);
        } else if next.kind != Kind::Mapping {
            next.kind = Kind::Mapping;
            MAP_TAG.clone_into(&mut next.tag);
        }
        node = next;
    }
    Ok(normalize_comment_indentation(&marshal_config(&doc)?))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::testing::TempDir;

    fn root(data: &[u8]) -> Node {
        let doc = unmarshal(data).expect("yaml");
        doc.content.first().cloned().expect("root")
    }

    fn scalar<'a>(root: &'a Node, path: &str) -> Option<&'a str> {
        yaml_path(root, path).map(|node| node.value.as_str())
    }

    // Ports TestV8MovedCommentsSurviveV0Saves (config_v8_comments_test.go),
    // but for upstream's ValidateV8Config check, which isn't ported: the
    // load check runs instead.
    #[test]
    fn v8_moved_comments_survive_v0_saves() {
        let raw = "# DOCUMENT HEAD\nconfig-version: 8\noauth: # OAUTH INLINE\n  providers: # PROVIDERS INLINE\n    xai: # PROVIDER INLINE\n      # FIELD HEAD\n      inject-x-search: true # FIELD INLINE\n\n      # FIELD FOOT\n\n    # PROVIDER FOOT\nserver:\n  port: 8317 # UNRELATED INLINE\n# DOCUMENT FOOT\n";
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(&file, raw).expect("seed");
        let mut cfg = Config::load(&file).expect("load");
        for port in [8318, 8319, 8320] {
            cfg.port = port;
            save_preserving_comments(&file, &cfg, false).expect("save");
            let data = fs::read_to_string(&file).expect("read");
            for marker in [
                "DOCUMENT HEAD",
                "OAUTH INLINE",
                "PROVIDERS INLINE",
                "PROVIDER INLINE",
                "FIELD HEAD",
                "FIELD INLINE",
                "FIELD FOOT",
                "PROVIDER FOOT",
                "UNRELATED INLINE",
                "DOCUMENT FOOT",
            ] {
                assert_eq!(data.matches(marker).count(), 1, "{marker}:\n{data}");
            }
            let port_text = port.to_string();
            assert_eq!(
                scalar(&root(data.as_bytes()), "server.port"),
                Some(port_text.as_str())
            );
        }
    }

    // Ports TestV8MigrationCarriesProviderKeyFootComments
    // (config_v8_comments_test.go).
    #[test]
    fn v8_migration_carries_provider_key_foot_comments() {
        let mut doc =
            unmarshal(b"oauth: {providers: {xai: {inject-x-search: true}}}\n").expect("yaml");
        let root = doc.content.first_mut().expect("root");
        let provider = yaml_path_mut(root, "oauth.providers").expect("providers");
        let index = find_map_key_index(provider, "xai").expect("xai");
        provider.content[index].foot_comment = "# PROVIDER KEY FOOT".to_owned();
        let field = yaml_path_mut(root, "oauth.providers.xai").expect("xai");
        let index = find_map_key_index(field, "inject-x-search").expect("field");
        field.content[index].foot_comment = "# FIELD KEY FOOT".to_owned();
        let raw = marshal(&doc).expect("marshal");
        let raw_text = String::from_utf8_lossy(&raw).into_owned();
        for marker in ["PROVIDER KEY FOOT", "FIELD KEY FOOT"] {
            assert!(raw_text.contains(marker), "fixture lacks {marker}");
        }
        let (data, _) = normalize_config_layout(&raw, true).expect("normalize");
        let data = String::from_utf8(data).expect("utf-8");
        for marker in ["PROVIDER KEY FOOT", "FIELD KEY FOOT"] {
            assert_eq!(data.matches(marker).count(), 1, "{marker}:\n{data}");
        }
    }

    // Ports TestIsV8ConfigLayout (config_v8_save_layout_test.go).
    #[test]
    fn is_v8_config_layout_table() {
        let cases = [
            (
                "legacy",
                "request-retry: 3\ncodex: {response-steering: true}\n",
                false,
            ),
            (
                "legacy common roots",
                "routing: {strategy: fill-first}\nplugins: {configs: {example: {enabled: true}}}\nquota-exceeded: {switch-project: true}\nclient: {codex: {enable-apply-patch: true}}\napi-keys: [client-key]\n",
                false,
            ),
            (
                "legacy optimize alias",
                "codex: {optimize-multi-agent-v2: true}\n",
                false,
            ),
            ("declared v8", "config-version: 8\nrequest-retry: 3\n", true),
            (
                "historical v8",
                "oauth: {providers: {codex: {response-steering: true}}}\n",
                true,
            ),
            ("empty v8", "server: {}\n", true),
            (
                "latest v8",
                "upstream: {xai: {inject-x-search: false}}\n",
                true,
            ),
            ("grouped credentials", "api-keys: {}\n", true),
            (
                "partial v8 retry",
                "request-retry: 3\nrouting: {retry: {max-retry-credentials: 2}}\n",
                true,
            ),
            ("empty v8 retry", "routing: {retry: {}}\n", true),
            (
                "historical client",
                "providers: {codex: {optimize-multi-agent-v2: true}}\n",
                true,
            ),
            (
                "latest client",
                "client: {codex: {optimize-multi-agent-v2: false}}\n",
                true,
            ),
        ];
        for (name, raw, v8) in cases {
            let got = is_v8_config_layout(&root(raw.as_bytes())).expect("layout");
            assert_eq!(got, v8, "{name}");
        }
    }

    // Ports TestV0SaveUpgradesHistoricalV8Layout
    // (config_v8_save_layout_test.go), but for its Home mode and
    // ValidateV8Config parts, which aren't ported, and its check of the
    // OAuth scope, which the loader's tests cover.
    #[test]
    fn v0_save_upgrades_historical_v8_layout() {
        for version in ["", "config-version: 8\n"] {
            let dir = TempDir::new();
            let file = dir.path().join("config.yaml");
            let raw = format!(
                "{version}# Preserve provider settings\noauth: {{providers: {{codex: {{response-steering: true, header-defaults: {{user-agent: oauth-agent}}}}, xai: {{inject-x-search: true}}}}}}\n"
            );
            fs::write(&file, &raw).expect("seed");
            let mut cfg = Config::load(&file).expect("load");
            cfg.port = 8318;
            save_preserving_comments(&file, &cfg, false).expect("save");
            let data = fs::read(&file).expect("read");
            let root = root(&data);
            assert!(
                yaml_path(&root, "upstream.codex.response-steering").is_some(),
                "{version}"
            );
            assert!(
                yaml_path(&root, "upstream.xai.inject-x-search").is_some(),
                "{version}"
            );
            assert_eq!(scalar(&root, "server.port"), Some("8318"), "{version}");
            let text = String::from_utf8_lossy(&data);
            assert!(text.contains("# Preserve provider settings"), "{text}");
        }
    }

    // Not upstream's: a save keeps the sections the config doesn't type and
    // writes what it changes, with upstream's flow collections in block
    // style and the settings it adds (Go's answer, recorded).
    #[test]
    fn save_keeps_untyped_sections() {
        let raw = "# head\nport: 8317 # the port\nplugins:\n  enabled: false # off\n  configs: {example: {enabled: true}}\npprof:\n  enable: true\n  addr: \"127.0.0.1:6060\"\nunknown: [a, b]\n";
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(&file, raw).expect("seed");
        let mut cfg = Config::load(&file).expect("load");
        cfg.port = 8318;
        save_preserving_comments(&file, &cfg, false).expect("save");
        assert_eq!(
            fs::read_to_string(&file).expect("read"),
            "# head\nport: 8318 # the port\nplugins:\n  enabled: false # off\n  configs:\n    example:\n      enabled: true\npprof:\n  enable: true\n  addr: \"127.0.0.1:6060\"\nunknown:\n  - a\n  - b\ncredential-concurrency:\n  cpa-heartbeat-timeout: 3s\n  cpa-cancel-bound: 5s\n  reclaim-grace: 5s\n  cleanup-interval: 5s\n  release-flush-interval: 250ms\n  release-max-backoff: 2s\n  busy-retry-min: 250ms\n  busy-retry-max: 1s\n  max-limit: 1000000\ncredential-in-flight:\n  snapshot-interval: 2s\n  stale-after: 10s\n  max-part-bytes: 262144\n  max-part-count: 64\n  max-revision-bytes: 16777216\n  max-aggregate-groups: 100000\n  max-details: 10000\n  max-string-bytes: 256\n  staging-retention: 1m\ndiscovery:\n  service-type: _ai-gateway._tcp\n  subtypes:\n    - _chat-completions\n    - _responses\n    - _messages\n    - _generate-content\n    - _interactions\nredis-usage-queue-retention-seconds: 60\ndisable-cooling: false\nrequest-retry: 0\nws-auth: true\n"
        );
        assert_eq!(
            fs::read_to_string(write::backup_path(&file)).expect("backup"),
            raw
        );
    }

    // Not upstream's: a nested update sets the string and keeps the rest,
    // as Go's SaveConfigPreserveCommentsUpdateNestedScalar does (Go's
    // answer, recorded).
    #[test]
    fn nested_scalar_is_set() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(
            &file,
            "# head\nport: 8317 # the port\nremote-management:\n  allow-remote: false\n",
        )
        .expect("seed");
        update_nested_scalar(
            &file,
            &["remote-management", "secret-key"],
            "$2a$10$abcdefghijklmnopqrstuv",
        )
        .expect("update");
        assert_eq!(
            fs::read_to_string(&file).expect("read"),
            "# head\nport: 8317 # the port\nremote-management:\n  allow-remote: false\n  secret-key: $2a$10$abcdefghijklmnopqrstuv\n"
        );
    }

    // Not upstream's: Go writes `port: {inner: value}` here, which its own
    // LoadConfig then refuses ("cannot unmarshal !!map into int"); the
    // load check refuses it before anything is written.
    #[test]
    fn nested_scalar_that_does_not_load_is_refused() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        let raw = "# head\nport: 8317 # the port\ndebug: false\n";
        fs::write(&file, raw).expect("seed");
        let error = update_nested_scalar(&file, &["port", "inner"], "value").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Check);
        assert_eq!(
            error.to_string(),
            "failed to parse config file: yaml: unmarshal errors:\n  line 3: cannot unmarshal !!map into int"
        );
        assert_eq!(fs::read_to_string(&file).expect("read"), raw);
    }

    // Not upstream's: a management write of a v8 file completes the v8
    // layout and moves comment lines to the start of their line, as
    // WriteConfig does.
    #[test]
    fn write_file_normalizes_a_v8_file() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        write_file(&file, b"config-version: 8\nserver:\n  port: 8318\n").expect("write");
        let data = fs::read(&file).expect("read");
        let root = root(&data);
        assert_eq!(scalar(&root, "server.port"), Some("8318"));
        assert_eq!(scalar(&root, "config-version"), Some("8"));
        write_file(&file, b"port: 8319\n    # indented\ndebug: true\n").expect("write");
        assert_eq!(
            fs::read_to_string(&file).expect("read"),
            "port: 8319\n# indented\ndebug: true\n"
        );
        let error = write_file(&file, b"port: [\n").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Yaml);
        assert_eq!(
            error.to_string(),
            "yaml: line 1: did not find expected node content"
        );
    }
}
