// Ported from CLIProxyAPI internal/config/config_v8.go (flattenV8,
// expandV8Groups, groupLegacyKeys, NormalizeConfigLayout, IsV8ConfigLayout,
// commentUnknownV8Sections, commentUnknownV8Fields,
// warnUnrecognizedV8Section, normalizeV8PrivateIPAlias, restoreV8Layout,
// preserveV8Comments) and config_v8_api.go (ProjectV8ConfigAliases)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The v8 layout on a [`Node`] tree with its comments, as upstream's
//! config writer moves a document between the v8 and legacy layouts.
//!
//! [`flatten_v8`] moves a document into the legacy layout, where the writer
//! merges the settings in; [`restore_v8_layout`] moves them back to where
//! the file had them; [`normalize_config_layout`] moves a whole file into
//! the v8 layout. Comments move with the settings.
//!
//! The loader's [`super::super::v8::flatten_v8`] checks a document before
//! these run and gives upstream's errors for one that doesn't make a valid
//! layout; the functions here fail rather than panic on a document it
//! hasn't checked.
//!
//! Deviations from upstream:
//! - The tables of v8 paths are the loader's fixed tables, checked against
//!   v8.0.15's; upstream builds them by reflecting over its `Config` struct.
//! - [`is_v8_config_layout`] fails on a document whose aliases can't be
//!   expanded, where upstream recurses until its stack overflows on a
//!   cyclic alias.
//! - [`expand_v8_groups`] leaves the credential weight checks to the
//!   loader, which makes them first.

use super::super::Config;
use super::super::layout::{V8_CHILDREN, V8_ROOTS, V8_STRUCT_PATHS};
use super::super::v8::{
    SHARED_KEY_FIELDS, V8_CLIENT_PATHS, V8_KEY_FAMILIES, V8_PATHS, V8_SHARED_PATHS,
    V8_SHARED_STRUCT_PATHS,
};
use super::super::yaml::go_quote;
use super::super::yaml3::compose::unmarshal;
use super::super::yaml3::{BOOL_TAG, INT_TAG, Kind, MAP_TAG, NULL_TAG, Node, STR_TAG};
use super::generate::family_value;
use super::tree::{
    copy_yaml_path_value, delete_yaml_path, expand_config_aliases, find_map_key_index, legacy_path,
    marshal, set_yaml_path, set_yaml_path_with_comments, yaml_path, yaml_path_mut,
};
use super::{SaveError, SaveErrorKind, check_layout};

/// The deprecated relay flag and the setting it is the inverse of.
const PRIVATE_IP_ALIAS: &str = "codex.live-media-relay.allow-private-remote-ips";
const PRIVATE_IP_CANONICAL: &str = "codex.live-media-relay.disable-private-remote-ips";

/// The v8 roots shared with the legacy layout, which alone don't make a
/// document v8.
const SHARED_ROOTS: [&str; 5] = ["api-keys", "plugins", "quota-exceeded", "routing", "client"];

fn invalid(message: impl Into<String>) -> SaveError {
    SaveError::new(SaveErrorKind::Invalid, message)
}

/// Upstream's `v8Aliases`: the client and shared paths.
pub(crate) fn v8_aliases() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    V8_CLIENT_PATHS.iter().chain(V8_SHARED_PATHS)
}

/// `flattenV8`: a copy of the mapping `node` in the legacy layout, aliases
/// expanded, with each setting's comments carried along.
pub(crate) fn flatten_v8(node: &Node) -> Result<Node, SaveError> {
    if node.kind != Kind::Mapping {
        return Err(invalid("config must be a mapping"));
    }
    let mut node = expand_config_aliases(node)?;
    normalize_private_ip_alias(&mut node, true)?;
    for &(_, current) in V8_PATHS.iter().chain(v8_aliases()) {
        let parts: Vec<&str> = current.split('.').collect();
        for end in 1..parts.len() {
            let parent_path = parts.get(..end).unwrap_or_default().join(".");
            let Some(parent) = yaml_path(&node, &parent_path) else {
                break;
            };
            // Routing is shared with the legacy layout, where null means
            // defaults.
            if end == 1 && parent_path == "routing" && parent.tag == NULL_TAG {
                break;
            }
            if parent.kind != Kind::Mapping {
                return Err(invalid(format!("{parent_path} must be a mapping")));
            }
        }
    }
    let mut root = node.clone();
    for &(old, current) in v8_aliases() {
        let Some(value) = copy_yaml_path_value(&root, old) else {
            continue;
        };
        if yaml_path(&root, current).is_none() {
            set_yaml_path_with_comments(&mut root, current, &value);
        }
        delete_yaml_path(&mut root, old);
    }
    for &(old, current) in V8_SHARED_STRUCT_PATHS {
        let Some(value) = yaml_path(&root, old) else {
            continue;
        };
        if value.tag != NULL_TAG && value.kind != Kind::Mapping {
            return Err(invalid(format!("{old} must be a mapping")));
        }
        if value.tag != NULL_TAG && !value.content.is_empty() {
            continue;
        }
        if yaml_path(&root, current).is_none()
            && let Some(mut copy) = copy_yaml_path_value(&root, old)
        {
            copy.kind = Kind::Mapping;
            MAP_TAG.clone_into(&mut copy.tag);
            copy.value.clear();
            set_yaml_path_with_comments(&mut root, current, &copy);
        }
        delete_yaml_path(&mut root, old);
    }
    if let Some(version) = yaml_path(&root, "config-version")
        && (version.tag != INT_TAG || version.value != "8")
    {
        return Err(invalid("unsupported config-version (expected 8)"));
    }
    // The v8 upstream map reuses the legacy client-key field name.
    if yaml_path(&root, "api-keys").is_some_and(|keys| keys.kind == Kind::Mapping) {
        delete_yaml_path(&mut root, "api-keys");
    }
    for &(old, current) in V8_PATHS {
        if let Some(copy) = copy_yaml_path_value(&root, current) {
            delete_yaml_path(&mut root, current);
            set_yaml_path_with_comments(&mut root, old, &copy);
        }
    }
    for &(old, current) in V8_KEY_FAMILIES {
        if let Some(groups) = yaml_path(&node, &format!("api-keys.{current}")) {
            let keys = expand_v8_groups(groups, current)?;
            set_yaml_path(&mut root, old, &keys);
        }
    }
    Ok(root)
}

/// The auth index a management read adds to keys.
const AUTH_INDEX_FIELDS: [&str; 2] = ["auth_index", "auth-index"];

/// `expandV8Groups`: the v8 groups of the family `provider` as the legacy
/// list of keys, each key with its group's shared fields.
pub(crate) fn expand_v8_groups(groups: &Node, provider: &str) -> Result<Node, SaveError> {
    if groups.kind != Kind::Sequence {
        return Err(invalid(format!("api-keys.{provider} must be a list")));
    }
    let mut out = Node::sequence();
    for (index, group) in groups.content.iter().enumerate() {
        if group.kind != Kind::Mapping {
            return Err(invalid(format!(
                "api-keys.{provider}[{index}] must be a mapping"
            )));
        }
        let Some(keys) = yaml_path(group, "keys").filter(|keys| keys.kind == Kind::Sequence) else {
            return Err(invalid(format!(
                "api-keys.{provider}[{index}].keys must be a list"
            )));
        };
        if provider == "openai-compatibility" {
            let mut item = group.clone();
            delete_yaml_path(&mut item, "keys");
            for field in AUTH_INDEX_FIELDS {
                delete_yaml_path(&mut item, field);
            }
            let mut clean = keys.clone();
            for key in &mut clean.content {
                for field in AUTH_INDEX_FIELDS {
                    delete_yaml_path(key, field);
                }
            }
            set_yaml_path(&mut item, "api-key-entries", &clean);
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
            if key.kind != Kind::Mapping {
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
                if value.tag != NULL_TAG {
                    set_yaml_path(&mut item, &field.value, value);
                }
            }
            out.content.push(item);
        }
    }
    Ok(out)
}

/// `groupLegacyKeys`: a legacy list of keys as v8 groups, one group per
/// key.
pub(crate) fn group_legacy_keys(keys: &Node, provider: &str) -> Node {
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
            set_yaml_path(
                &mut group,
                "name",
                &Node::scalar(STR_TAG, &format!("{provider}-{}", index + 1)),
            );
            let mut key = entry.clone();
            for (field, value) in entry.pairs() {
                if field.value == "base-url" || SHARED_KEY_FIELDS.contains(&field.value.as_str()) {
                    set_yaml_path(&mut group, &field.value, value);
                    delete_yaml_path(&mut key, &field.value);
                }
            }
            let mut list = Node::sequence();
            list.content.push(key);
            set_yaml_path(&mut group, "keys", &list);
            group
        };
        out.content.push(group);
    }
    out
}

/// `NormalizeConfigLayout(data, migrate)`: the file `data` with legacy
/// settings that a v8 setting overrides removed, and, when `migrate`,
/// every legacy setting moved to its v8 path, sections the v8 layout
/// doesn't know commented out, and `config-version: 8` set. Whether
/// anything changed; when nothing did, the data comes back as it was.
pub(crate) fn normalize_config_layout(
    data: &[u8],
    migrate: bool,
) -> Result<(Vec<u8>, bool), SaveError> {
    let mut doc = unmarshal(data)?;
    let Some(first) = doc.content.first() else {
        if !migrate {
            return Ok((data.to_vec(), false));
        }
        return Err(invalid("empty config"));
    };
    check_layout(data)?;
    let mut root = expand_config_aliases(first)?;
    let mut changed = normalize_private_ip_alias(&mut root, migrate)?;
    // Empty legacy structs have no leaf fields to move. Preserve them as
    // empty v8 mappings; null structs also mean defaults.
    let mut paths: Vec<(&str, &str)> = v8_aliases().chain(V8_PATHS).copied().collect();
    for &(old, current) in V8_STRUCT_PATHS.iter().chain(V8_SHARED_STRUCT_PATHS) {
        let present = yaml_path(&root, current).is_some();
        let Some(node) = yaml_path_mut(&mut root, old) else {
            continue;
        };
        if !migrate && !present {
            continue;
        }
        if node.tag == NULL_TAG {
            node.kind = Kind::Mapping;
            MAP_TAG.clone_into(&mut node.tag);
            node.value.clear();
        } else if node.kind != Kind::Mapping || !node.content.is_empty() {
            continue;
        }
        paths.push((old, current));
    }
    for (old, current) in paths {
        if legacy_path(&root, old).is_none() {
            continue;
        }
        let present = yaml_path(&root, current).is_some();
        if !present && !migrate {
            continue;
        }
        let copy = copy_yaml_path_value(&root, old);
        delete_yaml_path(&mut root, old);
        if !present && let Some(copy) = copy {
            set_yaml_path_with_comments(&mut root, current, &copy);
        }
        changed = true;
    }
    for &(old, group) in V8_KEY_FAMILIES {
        let Some(keys) = yaml_path(&root, old) else {
            continue;
        };
        let path = format!("api-keys.{group}");
        if yaml_path(&root, &path).is_none() {
            if !migrate {
                continue;
            }
            let groups = group_legacy_keys(keys, group);
            set_yaml_path(&mut root, &path, &groups);
        }
        delete_yaml_path(&mut root, old);
        changed = true;
    }
    if migrate {
        // Unknown fields are ignored at runtime: keep them as comments.
        comment_unknown_v8_sections(&mut root)?;
        set_yaml_path(&mut root, "config-version", &Node::scalar(INT_TAG, "8"));
        changed = true;
    }
    if !changed {
        return Ok((data.to_vec(), false));
    }
    if let Some(first) = doc.content.first_mut() {
        *first = root;
    }
    Ok((marshal(&doc)?, true))
}

/// `IsV8ConfigLayout`: whether a document uses the v8 layout, by its
/// version or its v8 paths. Roots shared with the legacy layout don't
/// count alone.
pub(crate) fn is_v8_config_layout(root: &Node) -> Result<bool, SaveError> {
    if root.kind != Kind::Mapping {
        return Ok(false);
    }
    let root = expand_config_aliases(root)?;
    for (key, _) in root.pairs() {
        let key = key.value.as_str();
        if V8_ROOTS.contains(&key) && !SHARED_ROOTS.contains(&key) {
            return Ok(true);
        }
    }
    if yaml_path(&root, "api-keys").is_some_and(|keys| keys.kind == Kind::Mapping) {
        return Ok(true);
    }
    let paths = V8_PATHS.iter().chain(V8_STRUCT_PATHS).chain(v8_aliases());
    for &(old, current) in paths {
        if old != current && yaml_path(&root, current).is_some() {
            return Ok(true);
        }
        if old.starts_with("providers.") && yaml_path(&root, old).is_some() {
            return Ok(true);
        }
        if old != current
            && let Some(rest) = current.strip_prefix("routing.")
        {
            let child = rest.split('.').next().unwrap_or_default();
            if yaml_path(&root, &format!("routing.{child}")).is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// `commentUnknownV8Sections`: comments out, at the end of the document,
/// every section and field the v8 layout doesn't know.
fn comment_unknown_v8_sections(root: &mut Node) -> Result<(), SaveError> {
    let mut foot = std::mem::take(&mut root.foot_comment);
    let result = comment_unknown_in(root, &mut foot);
    root.foot_comment = foot;
    result
}

fn comment_unknown_in(root: &mut Node, foot: &mut String) -> Result<(), SaveError> {
    comment_unknown_v8_fields(root, foot, V8_ROOTS, "")?;
    for [key, value] in root.content.as_chunks_mut::<2>().0 {
        walk_unknown(value, &key.value, foot)?;
    }
    Ok(())
}

/// The walk of `commentUnknownV8Sections`: a section with a table of the
/// fields it may hold, and the sections in it.
fn walk_unknown(node: &mut Node, path: &str, foot: &mut String) -> Result<(), SaveError> {
    if node.kind != Kind::Mapping {
        return Ok(());
    }
    let Some(&(_, allowed)) = V8_CHILDREN.iter().find(|(section, _)| *section == path) else {
        return Ok(());
    };
    comment_unknown_v8_fields(node, foot, allowed, path)?;
    for [key, value] in node.content.as_chunks_mut::<2>().0 {
        let child = format!("{path}.{}", key.value);
        walk_unknown(value, &child, foot)?;
    }
    Ok(())
}

/// `commentUnknownV8Fields`: removes the fields of `node` not in `allowed`
/// and adds them, written out with their full path as the key, to the
/// document's foot comment `foot`.
fn comment_unknown_v8_fields(
    node: &mut Node,
    foot: &mut String,
    allowed: &[&str],
    path: &str,
) -> Result<(), SaveError> {
    let mut comments = Vec::new();
    let mut index = 0;
    while index + 1 < node.content.len() {
        let Some(key) = node.content.get(index) else {
            break;
        };
        if allowed.contains(&key.value.as_str()) {
            index += 2;
            continue;
        }
        let section = if path.is_empty() {
            key.value.clone()
        } else {
            format!("{path}.{}", key.value)
        };
        tracing::warn!(
            "unrecognized configuration section {} commented out during v8 migration",
            go_quote(&section)
        );
        let mut entry = Node::mapping();
        entry.content = node.content.drain(index..index + 2).collect();
        if let Some(key) = entry.content.first_mut() {
            key.value = section;
        }
        let data = marshal(&entry)?;
        let text = String::from_utf8_lossy(&data);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        comments.push(format!("# {}", text.replace('\n', "\n# ")));
    }
    if !comments.is_empty() {
        *foot = format!("{foot}\n{}", comments.join("\n")).trim().to_owned();
    }
    Ok(())
}

/// `normalizeV8PrivateIPAlias`: resolves the deprecated
/// `allow-private-remote-ips` relay flag, the inverse of
/// `disable-private-remote-ips`. It goes when the OAuth-scoped setting is
/// present; otherwise, when `migrate` and the setting isn't, it becomes
/// the setting. Whether anything changed.
pub(crate) fn normalize_private_ip_alias(
    root: &mut Node,
    migrate: bool,
) -> Result<bool, SaveError> {
    let Some(value) = yaml_path(root, PRIVATE_IP_ALIAS) else {
        return Ok(false);
    };
    if yaml_path(root, &format!("oauth.providers.{PRIVATE_IP_CANONICAL}")).is_some() {
        return Ok(delete_yaml_path(root, PRIVATE_IP_ALIAS));
    }
    if !migrate || yaml_path(root, PRIVATE_IP_CANONICAL).is_some() {
        return Ok(false);
    }
    let Some(allow) = decode_bool(value) else {
        return Err(invalid(format!(
            "decode {PRIVATE_IP_ALIAS}: yaml: unmarshal errors:\n  line {}: cannot unmarshal {} into bool",
            value.line,
            value.short_tag()
        )));
    };
    let replacement = Node::scalar(BOOL_TAG, if allow { "false" } else { "true" });
    set_yaml_path(root, PRIVATE_IP_CANONICAL, &replacement);
    delete_yaml_path(root, PRIVATE_IP_ALIAS);
    Ok(true)
}

/// A scalar decoded into a Go `bool` as yaml.v3 decodes one, with YAML
/// 1.1's spellings; `None` where yaml.v3 fails.
fn decode_bool(node: &Node) -> Option<bool> {
    if node.kind != Kind::Scalar {
        return None;
    }
    let tag = node.short_tag();
    let value = node.value.as_str();
    if tag == NULL_TAG {
        return Some(false);
    }
    if tag == BOOL_TAG {
        return match value {
            "true" | "True" | "TRUE" => Some(true),
            "false" | "False" | "FALSE" => Some(false),
            _ => None,
        };
    }
    if tag != STR_TAG {
        return None;
    }
    match value {
        "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
        "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
        _ => None,
    }
}

/// `restoreV8Layout`: moves the merged settings of `root` back to the v8
/// paths the file (`layout`, its tree with aliases expanded) had them at.
/// A key family keeps the file's groups unless the settings changed it
/// (`original` is the file, `cfg` the settings written); then it is
/// regrouped, one key per group. Comments on the file's nodes are put
/// back.
pub(crate) fn restore_v8_layout(
    root: &mut Node,
    layout: &Node,
    original: &[u8],
    cfg: &Config,
) -> Result<(), SaveError> {
    let upstreams_map =
        yaml_path(layout, "api-keys").is_some_and(|keys| keys.kind == Kind::Mapping);
    for &(old, current) in V8_PATHS {
        let collision = old == "api-keys" && upstreams_map;
        if yaml_path(layout, current).is_none() && !collision {
            continue;
        }
        if legacy_path(root, old).is_none() {
            continue;
        }
        if let Some(copy) = copy_yaml_path_value(root, old) {
            delete_yaml_path(root, old);
            set_yaml_path_with_comments(root, current, &copy);
        }
    }
    let mut baseline: Option<Config> = None;
    for &(old, current) in V8_KEY_FAMILIES {
        let path = format!("api-keys.{current}");
        let Some(groups) = yaml_path(layout, &path) else {
            continue;
        };
        if baseline.is_none() {
            let parsed = Config::parse(original).map_err(|error| invalid(error.to_string()))?;
            baseline = Some(parsed);
        }
        let changed = baseline
            .as_ref()
            .is_none_or(|before| family_value(before, old) != family_value(cfg, old));
        let groups = if changed {
            let keys = yaml_path(root, old).cloned().unwrap_or_else(Node::sequence);
            group_legacy_keys(&keys, current)
        } else {
            groups.clone()
        };
        delete_yaml_path(root, old);
        set_yaml_path(root, &path, &groups);
    }
    preserve_v8_comments(root, layout);
    Ok(())
}

/// `preserveV8Comments`: the comments of `src` put on `dst`, and those of
/// each key of a mapping and its value on the same key of `dst`.
fn preserve_v8_comments(dst: &mut Node, src: &Node) {
    src.head_comment.clone_into(&mut dst.head_comment);
    src.line_comment.clone_into(&mut dst.line_comment);
    src.foot_comment.clone_into(&mut dst.foot_comment);
    if dst.kind != Kind::Mapping || src.kind != Kind::Mapping {
        return;
    }
    for (key, value) in src.pairs() {
        let Some(index) = find_map_key_index(dst, &key.value) else {
            continue;
        };
        if let Some(dst_key) = dst.content.get_mut(index) {
            preserve_v8_comments(dst_key, key);
        }
        if let Some(dst_value) = dst.content.get_mut(index + 1) {
            preserve_v8_comments(dst_value, value);
        }
    }
}

/// `ProjectV8ConfigAliases`: moves the settings related to the dotted
/// `path` back to their earlier v8 spellings, with their comments, so a
/// request for `path` finds them there. A section that holds fields of its
/// own stays.
pub(crate) fn project_v8_config_aliases(root: &mut Node, path: &str) {
    if path.is_empty() {
        return;
    }
    for &(old, current) in v8_aliases().chain(V8_SHARED_STRUCT_PATHS) {
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
        let Some(value) = yaml_path(root, current) else {
            continue;
        };
        // Struct aliases only stand for empty containers.
        if value.kind == Kind::Mapping && !value.content.is_empty() {
            continue;
        }
        if let Some(copy) = copy_yaml_path_value(root, current) {
            set_yaml_path_with_comments(root, old, &copy);
            delete_yaml_path(root, current);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::save::tree::marshal_config;

    fn root(text: &str) -> Node {
        let doc = unmarshal(text.as_bytes()).expect("yaml");
        doc.content.first().cloned().expect("root")
    }

    // Not upstream's: a scalar decodes into a bool as yaml.v3 decodes one.
    #[test]
    fn bools_decode_as_yaml_v3_does() {
        let cases = [
            ("a: true", Some(true)),
            ("a: False", Some(false)),
            ("a: yes", Some(true)),
            ("a: 'off'", Some(false)),
            ("a: ~", Some(false)),
            ("a: 'true'", None),
            ("a: 1", None),
            ("a: [x]", None),
        ];
        for (text, want) in cases {
            let node = root(text);
            let value = yaml_path(&node, "a").expect("a");
            assert_eq!(decode_bool(value), want, "{text}");
        }
    }

    // Not upstream's: a legacy file isn't v8; a v8 root, an `api-keys`
    // mapping or a v8 path makes it so.
    #[test]
    fn v8_layout_is_detected() {
        assert!(!is_v8_config_layout(&root("port: 1\nrouting:\n  strategy: x\n")).expect("ok"));
        assert!(is_v8_config_layout(&root("server:\n  port: 1\n")).expect("ok"));
        assert!(is_v8_config_layout(&root("api-keys:\n  codex: []\n")).expect("ok"));
        assert!(is_v8_config_layout(&root("config-version: 8\n")).expect("ok"));
    }

    // Not upstream's: settings move back to the earlier v8 spellings a
    // request names, with their comments, as Go's ProjectV8ConfigAliases
    // moves them (Go's answers, recorded).
    #[test]
    fn aliases_project_to_earlier_spellings() {
        let text = "# head\nupstream:\n  # steering comment\n  codex:\n    response-steering: true # inline\n  claude: {}\nclient:\n  codex:\n    optimize-multi-agent-v2: true\nserver:\n  port: 8317\n";
        let cases = [
            (
                "oauth.providers.codex",
                "# head\nupstream:\n  claude: {}\nserver:\n  port: 8317\noauth:\n  providers:\n    codex:\n      optimize-multi-agent-v2: true\n      # steering comment\n      response-steering: true # inline\n",
            ),
            (
                "oauth.providers.claude",
                "# head\nupstream:\n  # steering comment\n  codex:\n    response-steering: true # inline\nclient:\n  codex:\n    optimize-multi-agent-v2: true\nserver:\n  port: 8317\noauth:\n  providers:\n    claude:\n      claude-code: {}\n",
            ),
            (
                "providers",
                "# head\nupstream:\n  # steering comment\n  codex:\n    response-steering: true # inline\n  claude: {}\nserver:\n  port: 8317\nproviders:\n  codex:\n    optimize-multi-agent-v2: true\n",
            ),
            (
                "oauth",
                "server:\n  port: 8317\noauth:\n  providers:\n    codex:\n      optimize-multi-agent-v2: true\n      # steering comment\n      response-steering: true # inline\n    claude:\n      # head\n      claude-code: {}\n",
            ),
            ("", text),
        ];
        for (path, want) in cases {
            let mut doc = unmarshal(text.as_bytes()).expect("yaml");
            let root = doc.content.first_mut().expect("root");
            project_v8_config_aliases(root, path);
            let out = String::from_utf8(marshal_config(&doc).expect("marshal")).expect("utf-8");
            assert_eq!(out, want, "{path}");
        }
    }

    // Not upstream's: a group's shared fields go to each of its keys, and
    // regrouping gives one group per key.
    #[test]
    fn groups_expand_and_regroup() {
        let groups = root(
            "- name: a\n  base-url: https://x\n  priority: 2\n  keys:\n    - api-key: k1\n    - api-key: k2\n      auth-index: 3\n",
        );
        let keys = expand_v8_groups(&groups, "codex").expect("groups");
        let text = String::from_utf8(marshal(&keys).expect("marshal")).expect("utf-8");
        assert_eq!(
            text,
            "- base-url: https://x\n  priority: 2\n  api-key: k1\n- base-url: https://x\n  priority: 2\n  api-key: k2\n"
        );
        let regrouped = group_legacy_keys(&keys, "codex");
        let text = String::from_utf8(marshal(&regrouped).expect("marshal")).expect("utf-8");
        assert_eq!(
            text,
            "- name: codex-1\n  base-url: https://x\n  priority: 2\n  keys:\n    - api-key: k1\n- name: codex-2\n  base-url: https://x\n  priority: 2\n  keys:\n    - api-key: k2\n"
        );
    }
}
