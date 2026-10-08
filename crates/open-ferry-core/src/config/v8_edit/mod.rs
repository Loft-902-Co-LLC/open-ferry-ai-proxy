// Ported from CLIProxyAPI internal/api/handlers/management/config_v8.go
// (ConfigV8's PUT, PATCH and DELETE, preserveV8TURNSecrets, configV8Node,
// deleteConfigV8Path, cloneConfigV8Node, mergeConfigV8Patch),
// config_auth_index.go (deleteMapKey, stripAPIKeysAuthIndexesFromGroup,
// stripAPIKeysAuthIndexesFromGroups, stripAPIKeysAuthIndexesFromProvidersMap,
// stripAPIKeysAuthIndexesFromRoot, stripAPIKeysAuthIndexesFromUpdate),
// config_basic.go (WriteConfig, as ConfigV8 calls it) and
// internal/config/config_v8_api.go (NormalizeV8ConfigAliases) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Editing the config file in the v8 layout, as the v8 management API's
//! `PUT`, `PATCH` and `DELETE` of `config`, `config/*path` and
//! `config.yaml` do.
//!
//! [`edit_v8`] reads the file, moves it into the v8 layout, makes a
//! [`V8Edit`] to it and checks the result as upstream does: it must decode,
//! leave the fields Home owns as they were, load as a config and pass
//! [`validate_v8_config`]. It then writes the file with
//! [`super::save::write_file`] and returns the config the file now holds.
//! An edit it refuses returns a [`V8EditError`], which says what the route
//! answers, and writes nothing.
//!
//! On the way, an edit:
//! - moves a setting written at an earlier v8 path to its current one
//!   (upstream's `NormalizeV8ConfigAliases`);
//! - keeps the TURN servers' `username` and `credential` that a JSON write
//!   leaves out, as a JSON read hides them, for a server whose `urls` are
//!   unchanged;
//! - drops the `auth-index` (and `auth_index`) a read adds to the API keys,
//!   from the body and from the file.
//!
//! Each tree here is free of aliases when it is edited: the file's are
//! expanded when it is moved into the v8 layout, and a body's when it
//! replaces the whole config (a JSON body has none).
//!
//! Deviations from upstream:
//! - The file is written atomically and refused when it is a symbolic link
//!   (see [`super::save`]); a refused write answers 500 `write_failed` with
//!   the reason. It is read through a link, as upstream reads it.
//! - The bytes written are checked to load once more as they are written
//!   (`write_file`'s check), after upstream's checks.
//! - A stored file yaml.v3 can't read after its layout is normalized
//!   answers 500 `invalid_config` with the message; upstream gives no
//!   message. The normalization reads the file first, so this doesn't
//!   happen.
//! - A path of more than 256 keys answers 400 `invalid_path`; upstream
//!   creates the mappings.
//! - A Home-owned field whose value doesn't decode compares as absent;
//!   upstream compares what its decoder got before it stopped. Two values
//!   that are maps with keys that aren't all strings compare equal.
//! - The decode checks allow 256 levels of nesting, as the loader does
//!   (see the `config::yaml` module).
//! - Those of [`validate_v8_config`].

mod known;
mod schema;
mod validate;

use std::fmt;
use std::path::Path;

use open_ferry_translate::go::json_valid;

pub use known::{KnownKind, KnownPath, is_known_v8_path, known_v8_paths};
pub use validate::validate_v8_config;

use super::AnyValue;
use super::Config;
use super::layout::decode_any_value;
use super::save::{
    self, Node, copy_yaml_path_value, delete_yaml_path, expand_config_aliases, find_map_key_index,
    marshal, normalize_config_layout, project_v8_config_aliases, remove_map_key,
    set_yaml_path_with_comments, unmarshal, v8_aliases, yaml_path, yaml_path_mut,
};
use super::v8::V8_SHARED_STRUCT_PATHS;
use super::yaml::{self as loader_yaml, Kind as LoaderKind};
use super::yaml3::{Kind, MAP_TAG, MAX_DEPTH, NULL_TAG, STR_TAG};

/// The fields Home owns, which an edit must leave as they were.
const READ_ONLY_FIELDS: [&str; 3] = [
    "credentials/concurrency/lifecycle-config-revision",
    "credentials/concurrency/observation-barrier-revision",
    "plugins/auth-revision",
];

/// Where the live media relay's TURN servers are (`v8ICEServersPath`).
const ICE_SERVERS_PATH: [&str; 5] = [
    "oauth",
    "providers",
    "codex",
    "live-media-relay",
    "ice-servers",
];

/// The TURN server fields a JSON read hides.
const TURN_SECRETS: [&str; 2] = ["username", "credential"];

/// The index a read adds to each API key and group.
const AUTH_INDEX_KEYS: [&str; 2] = ["auth_index", "auth-index"];

/// The method of a v8 config edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum V8Method {
    /// Replaces the value at the path, or the whole config.
    Put,
    /// Merges the body into the value at the path, or into the whole
    /// config: mappings key by key, anything else replaced. `null` is kept,
    /// not deleted.
    Patch,
    /// Removes the value at the path, and the mappings it leaves empty.
    Delete,
}

/// A v8 config edit, as the route received it.
#[derive(Clone, PartialEq, Eq)]
pub struct V8Edit {
    /// What to do.
    pub method: V8Method,
    /// The keys leading to the value, from the request path split on `/`;
    /// empty for the whole config. A part may be empty, which an edit with
    /// a body refuses.
    pub path: Vec<String>,
    /// The request body, for `PUT` and `PATCH`.
    pub body: Vec<u8>,
    /// Whether the route was `PUT /v8/management/config.yaml`: the body is
    /// YAML, and need not be JSON.
    pub yaml: bool,
}

/// The body's length only: it may hold secrets.
impl fmt::Debug for V8Edit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("V8Edit")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("body_len", &self.body.len())
            .field("yaml", &self.yaml)
            .finish()
    }
}

/// Why [`edit_v8`] made no change, and so what the route answers. No
/// message quotes a value from the config or the body.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum V8EditError {
    /// The file couldn't be read: 500 `read_failed`.
    ReadFailed,
    /// The file as it is doesn't read in the v8 layout: 500
    /// `invalid_config` with the message.
    StoredInvalid(String),
    /// `DELETE` of the whole config: 400 `cannot_delete_config`.
    CannotDeleteConfig,
    /// `DELETE` of a path that doesn't exist: 404 `not_found`.
    NotFound,
    /// The body is empty or isn't YAML: 400 `invalid_body`.
    InvalidBody,
    /// A body sent to a JSON route isn't JSON: 400 `invalid_json`.
    InvalidJson,
    /// A body for the whole config isn't a mapping: 400
    /// `config_must_be_object`.
    ConfigMustBeObject,
    /// A part of the path is empty, or passes through a value that isn't a
    /// mapping: 400 `invalid_path`.
    InvalidPath,
    /// The result isn't a valid v8 config: 400 `invalid_config` with the
    /// message.
    InvalidConfig(String),
    /// The edit changes a field Home owns: 400 `read_only_field` naming
    /// it.
    ReadOnlyField(String),
    /// The result doesn't load as a config: 422 `invalid_config` with the
    /// message.
    Unprocessable(String),
    /// The file couldn't be written: 500 `write_failed` with the message.
    WriteFailed(String),
}

impl fmt::Display for V8EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadFailed => f.write_str("read_failed"),
            Self::StoredInvalid(message) => write!(f, "invalid_config: {message}"),
            Self::CannotDeleteConfig => f.write_str("cannot_delete_config"),
            Self::NotFound => f.write_str("not_found"),
            Self::InvalidBody => f.write_str("invalid_body"),
            Self::InvalidJson => f.write_str("invalid_json"),
            Self::ConfigMustBeObject => f.write_str("config_must_be_object"),
            Self::InvalidPath => f.write_str("invalid_path"),
            Self::InvalidConfig(message) | Self::Unprocessable(message) => {
                write!(f, "invalid_config: {message}")
            }
            Self::ReadOnlyField(field) => write!(f, "read_only_field: {field}"),
            Self::WriteFailed(message) => write!(f, "write_failed: {message}"),
        }
    }
}

impl std::error::Error for V8EditError {}

/// Makes `edit` to the config file at `path` in the v8 layout, checks the
/// result and writes it, and returns the config the file now holds
/// (upstream's `ConfigV8` for `PUT`, `PATCH` and `DELETE`, up to its
/// `WriteConfig`).
pub fn edit_v8(path: &Path, edit: &V8Edit) -> Result<Config, V8EditError> {
    let data = std::fs::read(path).map_err(|_| V8EditError::ReadFailed)?;
    let (out, config) = render(&data, edit)?;
    save::write_file(path, &out).map_err(|error| V8EditError::WriteFailed(error.to_string()))?;
    Ok(config)
}

/// The bytes [`edit_v8`] would write for the config file `data`, with the
/// same checks, written nowhere. Not upstream's: `open-ferry config` shows
/// what a change would make before it makes it.
pub fn preview_v8(data: &[u8], edit: &V8Edit) -> Result<Vec<u8>, V8EditError> {
    render(data, edit).map(|(data, _)| data)
}

/// The bytes [`edit_v8`] writes for the file `data`, and the config they
/// load as.
pub(crate) fn render(data: &[u8], edit: &V8Edit) -> Result<(Vec<u8>, Config), V8EditError> {
    let stored = |error: save::SaveError| V8EditError::StoredInvalid(error.to_string());
    let (data, _) = normalize_config_layout(data, true).map_err(stored)?;
    let mut doc = unmarshal(&data).map_err(stored)?;
    let Some(root) = doc.content.first_mut() else {
        return Err(V8EditError::StoredInvalid("empty config".to_owned()));
    };
    let before = root.clone();
    let parts: Vec<&str> = edit.path.iter().map(String::as_str).collect();
    project_v8_config_aliases(root, &parts.join("."));
    if edit.method == V8Method::Delete {
        if parts.is_empty() {
            return Err(V8EditError::CannotDeleteConfig);
        }
        if !delete_config_v8_path(root, &parts) {
            return Err(V8EditError::NotFound);
        }
    } else {
        apply_body(root, &parts, edit)?;
    }
    normalize_v8_config_aliases(root).map_err(V8EditError::InvalidConfig)?;
    if !edit.yaml && edit.method != V8Method::Delete {
        preserve_v8_turn_secrets(root, &before);
    }
    for field in READ_ONLY_FIELDS {
        if read_only_value(&before, field) != read_only_value(root, field) {
            return Err(V8EditError::ReadOnlyField(field.to_owned()));
        }
    }
    strip_auth_indexes_from_root(root);
    let data = marshal(&doc).map_err(|error| V8EditError::InvalidConfig(error.to_string()))?;
    let config =
        Config::parse(&data).map_err(|error| V8EditError::Unprocessable(error.to_string()))?;
    validate_v8_config(&data).map_err(|error| V8EditError::InvalidConfig(error.to_string()))?;
    let (data, _) = normalize_config_layout(&data, true)
        .map_err(|error| V8EditError::InvalidConfig(error.to_string()))?;
    Ok((data, config))
}

/// A `PUT` or `PATCH`: reads the body and puts it at the path.
fn apply_body(root: &mut Node, parts: &[&str], edit: &V8Edit) -> Result<(), V8EditError> {
    if !edit.yaml && !json_valid(&edit.body) {
        return Err(V8EditError::InvalidJson);
    }
    let mut update = unmarshal(&edit.body).map_err(|_| V8EditError::InvalidBody)?;
    strip_auth_indexes_from_update(parts, &mut update);
    let Some(mut value) = update.content.into_iter().next() else {
        return Err(V8EditError::InvalidBody);
    };
    if parts.is_empty() {
        if value.kind != Kind::Mapping {
            return Err(V8EditError::ConfigMustBeObject);
        }
        normalize_v8_config_aliases(&mut value).map_err(V8EditError::InvalidConfig)?;
    }
    // Paths name keys, never list indexes; lists are replaced whole.
    if parts.len() > MAX_DEPTH {
        return Err(V8EditError::InvalidPath);
    }
    let mut dst = root;
    for part in parts {
        if part.is_empty() || dst.kind != Kind::Mapping {
            return Err(V8EditError::InvalidPath);
        }
        let index = match find_map_key_index(dst, part) {
            Some(index) => index + 1,
            None => {
                dst.content.push(Node::scalar(STR_TAG, part));
                dst.content.push(Node::mapping());
                dst.content.len() - 1
            }
        };
        let Some(next) = dst.content.get_mut(index) else {
            return Err(V8EditError::InvalidPath);
        };
        dst = next;
    }
    if edit.method == V8Method::Patch {
        merge_config_v8_patch(dst, value);
    } else {
        if dst.kind == Kind::Scalar && value.kind == Kind::Scalar {
            take_comments(&mut value, dst);
        }
        *dst = value;
    }
    Ok(())
}

/// Gives `node` the comments of `from` (which loses them).
fn take_comments(node: &mut Node, from: &mut Node) {
    node.head_comment = std::mem::take(&mut from.head_comment);
    node.line_comment = std::mem::take(&mut from.line_comment);
    node.foot_comment = std::mem::take(&mut from.foot_comment);
}

/// `configV8Node`: the value under `parts`, one mapping key each, the
/// first key with that text.
fn config_v8_node<'a>(root: &'a Node, parts: &[&str]) -> Option<&'a Node> {
    let mut current = root;
    for part in parts {
        let index = find_map_key_index(current, part)?;
        current = current.content.get(index + 1)?;
    }
    Some(current)
}

/// [`config_v8_node`], to change the value.
fn config_v8_node_mut<'a>(root: &'a mut Node, parts: &[&str]) -> Option<&'a mut Node> {
    let mut current = root;
    for part in parts {
        let index = find_map_key_index(current, part)?;
        current = current.content.get_mut(index + 1)?;
    }
    Some(current)
}

/// `deleteConfigV8Path`: removes the key under `parts` with its value, and
/// the mappings on the way it leaves empty. Other empty mappings stay: they
/// can mean something. Whether the key was there.
fn delete_config_v8_path(root: &mut Node, parts: &[&str]) -> bool {
    let Some((first, rest)) = parts.split_first() else {
        return false;
    };
    let Some(index) = find_map_key_index(root, first) else {
        return false;
    };
    if !rest.is_empty() {
        let Some(child) = root.content.get_mut(index + 1) else {
            return false;
        };
        if !delete_config_v8_path(child, rest) {
            return false;
        }
        if !child.content.is_empty() {
            return true;
        }
    }
    root.content
        .drain(index..(index + 2).min(root.content.len()));
    true
}

/// `mergeConfigV8Patch`: merges `src` into `dst`, mappings key by key.
/// Anything else replaces `dst`; a scalar keeps `dst`'s comments. Unlike a
/// JSON merge patch, `null` is kept: a key's override uses it to inherit
/// its group's value. `DELETE` removes a field.
fn merge_config_v8_patch(dst: &mut Node, mut src: Node) {
    if dst.kind != Kind::Mapping || src.kind != Kind::Mapping {
        if dst.kind == Kind::Scalar && src.kind == Kind::Scalar {
            take_comments(&mut src, dst);
        }
        *dst = src;
        return;
    }
    let mut items = src.content.into_iter();
    while let (Some(key), Some(value)) = (items.next(), items.next()) {
        if let Some(index) = find_map_key_index(dst, &key.value)
            && let Some(old) = dst.content.get_mut(index + 1)
        {
            merge_config_v8_patch(old, value);
        } else {
            dst.content.push(key);
            dst.content.push(value);
        }
    }
}

/// `NormalizeV8ConfigAliases`: checks that `root` decodes, expands its
/// aliases, and moves each setting at an earlier v8 path to its current
/// one, unless the current one is set. A `null` shared section that held
/// settings sets each of them to `null`, so merging a `PATCH` of the whole
/// config can't make it an empty mapping that changes nothing. The error
/// is the decoder's message.
pub(crate) fn normalize_v8_config_aliases(root: &mut Node) -> Result<(), String> {
    let decoded = loader_yaml::from_writer_node(root).map_err(|error| error.message())?;
    loader_yaml::check_decode_any(&decoded).map_err(|error| error.message())?;
    *root = expand_config_aliases(root).map_err(|error| error.to_string())?;
    for &(container, _) in V8_SHARED_STRUCT_PATHS {
        if yaml_path(root, container).is_none_or(|value| value.tag != NULL_TAG) {
            continue;
        }
        let Some(mut copy) = copy_yaml_path_value(root, container) else {
            continue;
        };
        let prefix = format!("{container}.");
        for &(old, current) in v8_aliases() {
            if old.starts_with(&prefix) && yaml_path(root, current).is_none() {
                set_yaml_path_with_comments(root, current, &copy);
                copy.head_comment.clear();
            }
        }
        delete_yaml_path(root, container);
    }
    for &(old, current) in v8_aliases() {
        if yaml_path(root, old).is_none() {
            continue;
        }
        if yaml_path(root, current).is_none()
            && let Some(copy) = copy_yaml_path_value(root, old)
        {
            set_yaml_path_with_comments(root, current, &copy);
        }
        delete_yaml_path(root, old);
    }
    for &(old, current) in V8_SHARED_STRUCT_PATHS {
        let Some(value) = yaml_path(root, old) else {
            continue;
        };
        let empty = value.kind == Kind::Mapping && value.content.is_empty();
        if value.tag != NULL_TAG && !empty {
            continue;
        }
        if yaml_path(root, current).is_none()
            && let Some(copy) = copy_yaml_path_value(root, old)
        {
            set_yaml_path_with_comments(root, current, &copy);
            if let Some(value) = yaml_path_mut(root, current) {
                value.kind = Kind::Mapping;
                MAP_TAG.clone_into(&mut value.tag);
                value.value.clear();
            }
        }
        delete_yaml_path(root, old);
    }
    Ok(())
}

/// `preserveV8TURNSecrets`: gives each TURN server in `root` the
/// `username` and `credential` it lacks from the server in `before` with
/// the same `urls`, each old server matched once. A JSON read hides them,
/// so a client that writes back what it read keeps them; a server whose
/// URLs changed doesn't inherit another's. An explicit empty string or
/// `null` clears one.
fn preserve_v8_turn_secrets(root: &mut Node, before: &Node) {
    let Some(previous) = config_v8_node(before, &ICE_SERVERS_PATH) else {
        return;
    };
    let Some(next) = config_v8_node_mut(root, &ICE_SERVERS_PATH) else {
        return;
    };
    if next.kind != Kind::Sequence || previous.kind != Kind::Sequence {
        return;
    }
    let mut matched = vec![false; previous.content.len()];
    for server in &mut next.content {
        let Some(urls) = config_v8_node(server, &["urls"]).and_then(decode_urls) else {
            continue;
        };
        for (old, matched) in previous.content.iter().zip(matched.iter_mut()) {
            if *matched {
                continue;
            }
            let old_urls = config_v8_node(old, &["urls"]).and_then(decode_urls);
            if old_urls.as_ref() != Some(&urls) {
                continue;
            }
            *matched = true;
            for name in TURN_SECRETS {
                if config_v8_node(server, &[name]).is_some() {
                    continue;
                }
                if let Some(secret) = config_v8_node(old, &[name]) {
                    server.content.push(Node::scalar(STR_TAG, name));
                    server.content.push(secret.clone());
                }
            }
            break;
        }
    }
}

/// A server's `urls` decoded as yaml.v3 decodes into a Go `[]string`:
/// `Some(None)` for `null` (a nil slice), the strings of a list with its
/// `null` items dropped, or `None` when the decode fails.
fn decode_urls(node: &Node) -> Option<Option<Vec<String>>> {
    let node = loader_yaml::from_writer_node(node).ok()?;
    match node.kind {
        LoaderKind::Scalar => {
            let resolved = loader_yaml::resolve_node(&node).ok()?;
            (resolved.value == loader_yaml::Scalar::Null).then_some(None)
        }
        LoaderKind::Sequence => {
            let mut urls = Vec::with_capacity(node.content.len());
            for item in &node.content {
                if item.kind != LoaderKind::Scalar {
                    return None;
                }
                if let Some(url) = loader_yaml::scalar_string(item).ok()? {
                    urls.push(url);
                }
            }
            Some(Some(urls))
        }
        LoaderKind::Mapping | LoaderKind::Poison => None,
    }
}

/// The value of the Home-owned `field` (keys joined by `/`) as yaml.v3
/// decodes it into Go's `any`; `None` when it is absent or `null`, or
/// doesn't decode.
fn read_only_value(root: &Node, field: &str) -> Option<AnyValue> {
    let parts: Vec<&str> = field.split('/').collect();
    let node = config_v8_node(root, &parts)?;
    let node = loader_yaml::from_writer_node(node).ok()?;
    match decode_any_value(&node) {
        Ok(AnyValue::Null) | Err(_) => None,
        Ok(value) => Some(value),
    }
}

/// `stripAPIKeysAuthIndexesFromGroup`: drops the auth index from a group
/// and from each of its keys.
fn strip_auth_indexes_from_group(group: &mut Node) {
    if group.kind != Kind::Mapping {
        return;
    }
    for key in AUTH_INDEX_KEYS {
        remove_map_key(group, key);
    }
    if let Some(keys) = config_v8_node_mut(group, &["keys"])
        && keys.kind == Kind::Sequence
    {
        for key in &mut keys.content {
            for name in AUTH_INDEX_KEYS {
                remove_map_key(key, name);
            }
        }
    }
}

/// `stripAPIKeysAuthIndexesFromGroups`: a list of groups, or one group.
fn strip_auth_indexes_from_groups(groups: &mut Node) {
    match groups.kind {
        Kind::Sequence => groups
            .content
            .iter_mut()
            .for_each(strip_auth_indexes_from_group),
        Kind::Mapping => strip_auth_indexes_from_group(groups),
        _ => {}
    }
}

/// `stripAPIKeysAuthIndexesFromProvidersMap`: each provider's groups.
fn strip_auth_indexes_from_providers(api_keys: &mut Node) {
    if api_keys.kind != Kind::Mapping {
        return;
    }
    for [_, groups] in api_keys.content.as_chunks_mut::<2>().0 {
        strip_auth_indexes_from_groups(groups);
    }
}

/// `stripAPIKeysAuthIndexesFromRoot`: the groups under `api-keys`.
fn strip_auth_indexes_from_root(root: &mut Node) {
    if root.kind != Kind::Mapping {
        return;
    }
    if let Some(api_keys) = config_v8_node_mut(root, &["api-keys"]) {
        strip_auth_indexes_from_providers(api_keys);
    }
}

/// `stripAPIKeysAuthIndexesFromUpdate`: the auth indexes of a body put at
/// `parts`: the whole config, `api-keys`, or a provider's groups under it.
fn strip_auth_indexes_from_update(parts: &[&str], update: &mut Node) {
    let Some(node) = update.content.first_mut() else {
        return;
    };
    match parts {
        [] => strip_auth_indexes_from_root(node),
        ["api-keys"] => strip_auth_indexes_from_providers(node),
        ["api-keys", ..] => strip_auth_indexes_from_groups(node),
        _ => {}
    }
}

#[cfg(test)]
mod tests;
