// Ported from CLIProxyAPI internal/config/config_yaml.go
// (mergeMappingPreserve, mergeNodePreserve, appendPath, isKnownDefaultValue,
// pruneKnownDefaultsInNewNode, isZeroValueNode, copyNodeShallow,
// reorderSequenceForMerge, matchSequenceElement, sequenceElementIdentity,
// mappingScalarValue, nodesStructurallyEqual, pruneMappingToGeneratedKeys,
// pruneMissingMapKeys, shouldPruneNestedMappingKeys,
// normalizeCollectionNodeStyles, removeLegacyOpenAICompatAPIKeys,
// removeRemovedIntegrationKeys, removeLegacyGenerativeLanguageKeys,
// removeLegacyAuthBlock, isPluginConfigsPath, isPluginConfigsSubtreePath)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Merging the settings as upstream writes them into the file's tree.
//!
//! [`merge_mapping_preserve`] walks the generated settings and updates the
//! file's tree in place: existing values take the new value but keep their
//! comments, quoting and position; new keys are added at the end of their
//! mapping unless they hold a zero value or one of upstream's known
//! defaults; list items are matched to the file's by an identifying field
//! (`api-key`, `name`, ...) so that reordering a list keeps each item's
//! comments. Keys the file has and the settings don't are kept, except in
//! list items, in a credential's `headers` (and Claude's `cloak`), and in
//! the four OAuth maps, which [`prune_mapping_to_generated_keys`] trims.
//!
//! A path here is the mapping keys from the root, without list indexes, as
//! upstream's `[]string` path is.
//!
//! Deviations from upstream:
//! - The settings this port doesn't type (see [`UNTYPED`]) are never written
//!   from the generated tree, which only holds their defaults: the file's
//!   value is kept as it is, a mapping only gains keys it lacks (through the
//!   same new-key rules), and a null value is filled in as upstream fills
//!   it. Upstream re-renders them from what it decoded, which keeps the
//!   same values but can change their spelling (`yes` to `true`).
//! - List items are matched without their untyped keys, and those keys are
//!   never pruned from an item, since the generated item lacks them.
//! - `plugins.configs` isn't replaced from the generated tree
//!   (`replacePluginConfigsSubtree`): the plugin host isn't ported, so the
//!   file's plugin settings stay as they are. Upstream drops an empty
//!   `plugins.configs` mapping.

use super::super::types::DEFAULT_PANEL_GITHUB_REPOSITORY;
use super::super::yaml3::{
    AliasTarget, BOOL_TAG, FLOAT_TAG, INT_TAG, Kind, NULL_TAG, Node, SEQ_TAG, STR_TAG, Style,
};
use super::generate::DEFAULT_PPROF_ADDR;
use super::tree::{find_map_key_index, remove_map_key};

/// The settings this port doesn't type, by path. The generated tree holds
/// upstream's defaults for those it writes at all; a path under one of
/// these is untyped too.
const UNTYPED: &[&[&str]] = &[
    &["models"],
    &["claude-code"],
    &["gpt-image-2-base-model"],
    &["video-result-auth-cache-ttl"],
    &["credential-concurrency"],
    &["credential-in-flight"],
    &["plugins"],
    &["pprof"],
    &["discovery"],
    &["antigravity-signature-cache-enabled"],
    &["antigravity-signature-bypass-strict"],
    &["antigravity"],
    &["devin"],
    &["claude-header-defaults"],
    &["disable-claude-cloak-mode"],
    &["codex", "disable-codex-cloaking"],
    &["codex", "live-media-relay"],
    &["codex-header-defaults", "user-agent"],
    &["routing", "session-affinity"],
    &["routing", "session-affinity-ttl"],
    &["claude-api-key", "cloak"],
    &["claude-api-key", "fingerprint-profile"],
    &["claude-api-key", "experimental-cch-signing"],
    &["codex-api-key", "disable-codex-cloaking"],
    &["xai-api-key", "disable-codex-cloaking"],
    &["meta-api-key", "disable-codex-cloaking"],
];

/// The OAuth maps a save keeps, emptied, when the settings have none.
const OAUTH_MAPS: [&str; 4] = [
    "oauth-excluded-models",
    "oauth-model-alias",
    "oauth-request-scoped-errors",
    "oauth-settings",
];

/// Whether the setting at `path` is one this port doesn't type.
fn is_untyped(path: &[String]) -> bool {
    UNTYPED.iter().any(|prefix| {
        path.len() >= prefix.len() && prefix.iter().zip(path).all(|(a, b)| *a == b.as_str())
    })
}

/// Whether the key `key` under `path` is untyped.
fn is_untyped_child(path: &[String], key: &str) -> bool {
    let mut child = path.to_vec();
    child.push(key.to_owned());
    is_untyped(&child)
}

/// `isPluginConfigsPath`.
fn is_plugin_configs_path(path: &[String]) -> bool {
    matches!(path, [plugins, configs] if plugins == "plugins" && configs == "configs")
}

/// `isPluginConfigsSubtreePath`.
fn is_plugin_configs_subtree_path(path: &[String]) -> bool {
    matches!(path, [plugins, configs, ..] if plugins == "plugins" && configs == "configs")
}

/// `mergeMappingPreserve`: merges the mapping `src` into `dst`, keeping the
/// order and comments of `dst`'s keys. A new key is added only when its
/// value, with its defaults pruned, isn't zero or a known default. A `dst`
/// that isn't a mapping takes `src`'s kind, tag, value and content.
pub(crate) fn merge_mapping_preserve(dst: &mut Node, src: &Node, path: &mut Vec<String>) {
    if dst.kind != Kind::Mapping || src.kind != Kind::Mapping {
        copy_node_shallow(dst, src);
        return;
    }
    for (key, value) in src.pairs() {
        path.push(key.value.clone());
        if !is_plugin_configs_path(path) {
            match find_map_key_index(dst, &key.value) {
                Some(index) => {
                    if let Some(existing) = dst.content.get_mut(index + 1) {
                        if is_untyped(path) {
                            merge_untyped(existing, value, path);
                        } else {
                            merge_node_preserve(existing, value, path);
                        }
                    }
                }
                None => {
                    let mut candidate = value.clone();
                    prune_known_defaults_in_new_node(path, &mut candidate);
                    if !is_known_default_value(path, &candidate) {
                        dst.content.push(key.clone());
                        dst.content.push(candidate);
                    }
                }
            }
        }
        path.pop();
    }
}

/// An untyped setting the file has: kept, except that a mapping gains the
/// keys it lacks and a null is merged as upstream merges it.
fn merge_untyped(dst: &mut Node, src: &Node, path: &mut Vec<String>) {
    if dst.kind == Kind::Mapping && src.kind == Kind::Mapping {
        merge_mapping_preserve(dst, src, path);
    } else if dst.kind == Kind::Scalar && dst.tag == NULL_TAG {
        merge_node_preserve(dst, src, path);
    }
}

/// `mergeNodePreserve`: merges `src` into `dst`, reusing `dst`'s nodes to
/// keep their comments, anchors and scalar styles. Lists are merged item by
/// item after [`reorder_sequence_for_merge`].
pub(crate) fn merge_node_preserve(dst: &mut Node, src: &Node, path: &mut Vec<String>) {
    match src.kind {
        Kind::Mapping => {
            if dst.kind != Kind::Mapping {
                copy_node_shallow(dst, src);
            }
            merge_mapping_preserve(dst, src, path);
            if should_prune_nested_mapping_keys(path) {
                prune_missing_map_keys(dst, src, path);
            }
        }
        Kind::Sequence => {
            // An explicit null stays null when the list is empty.
            if dst.kind == Kind::Scalar && dst.tag == NULL_TAG && src.content.is_empty() {
                return;
            }
            if dst.kind != Kind::Sequence {
                dst.kind = Kind::Sequence;
                SEQ_TAG.clone_into(&mut dst.tag);
                dst.content.clear();
            }
            merge_sequence(dst, src, path);
        }
        Kind::Scalar | Kind::Alias => {
            // The style stays, to keep the quoting.
            dst.kind = src.kind;
            dst.tag.clone_from(&src.tag);
            dst.value.clone_from(&src.value);
        }
        Kind::Zero => {}
        Kind::Document => copy_node_shallow(dst, src),
    }
}

/// The sequence part of `mergeNodePreserve`: the items matched by
/// `reorderSequenceForMerge` are merged in place, the others are copies of
/// `src`'s, and `dst` ends with `src`'s length.
fn merge_sequence(dst: &mut Node, src: &Node, path: &mut Vec<String>) {
    let slots = reorder_sequence_for_merge(dst, src, path);
    let mut merged = Vec::with_capacity(src.content.len());
    for (slot, item) in slots.into_iter().zip(&src.content) {
        match slot {
            None => merged.push(item.clone()),
            Some(mut existing) => {
                merge_node_preserve(&mut existing, item, path);
                if existing.kind == Kind::Mapping && item.kind == Kind::Mapping {
                    prune_missing_map_keys(&mut existing, item, path);
                }
                merged.push(existing);
            }
        }
    }
    let have = merged.len();
    merged.extend(src.content.iter().skip(have).cloned());
    dst.content = merged;
}

/// `reorderSequenceForMerge`: `dst`'s items, taken out of it, in `src`'s
/// order: each of `src`'s items gets the first unused item of `dst` that
/// matches it, or nothing; unmatched items of `dst` are dropped. When
/// either list is empty, `dst`'s items stay in their order.
fn reorder_sequence_for_merge(
    dst: &mut Node,
    src: &Node,
    path: &mut Vec<String>,
) -> Vec<Option<Node>> {
    let original = std::mem::take(&mut dst.content);
    if original.is_empty() || src.content.is_empty() {
        return original.into_iter().map(Some).collect();
    }
    let mut original: Vec<Option<Node>> = original.into_iter().map(Some).collect();
    src.content
        .iter()
        .map(|target| {
            match_sequence_element(&original, target, path)
                .and_then(|index| original.get_mut(index))
                .and_then(Option::take)
        })
        .collect()
}

/// `matchSequenceElement`: the first unused item of `original` that has
/// `target`'s identity (mappings) or trimmed value (scalars), or else is
/// structurally equal to it.
fn match_sequence_element(
    original: &[Option<Node>],
    target: &Node,
    path: &mut Vec<String>,
) -> Option<usize> {
    let unused = || {
        original
            .iter()
            .enumerate()
            .filter_map(|(index, item)| Some((index, item.as_ref()?)))
    };
    match target.kind {
        Kind::Mapping => {
            let id = sequence_element_identity(target, path);
            if !id.is_empty()
                && let Some((index, _)) = unused().find(|(_, item)| {
                    item.kind == Kind::Mapping && sequence_element_identity(item, path) == id
                })
            {
                return Some(index);
            }
        }
        Kind::Scalar => {
            let value = target.value.trim();
            if !value.is_empty()
                && let Some((index, _)) = unused()
                    .find(|(_, item)| item.kind == Kind::Scalar && item.value.trim() == value)
            {
                return Some(index);
            }
        }
        _ => {}
    }
    unused()
        .find(|(_, item)| nodes_structurally_equal(item, target, path))
        .map(|(index, _)| index)
}

/// `sequenceElementIdentity`: the first identifying field a mapping item
/// has (`id=...`, `name=...`, ...), or else its first non-empty scalar
/// field. Untyped fields don't count.
fn sequence_element_identity(node: &Node, path: &[String]) -> String {
    if node.kind != Kind::Mapping {
        return String::new();
    }
    const IDENTITY_KEYS: [&str; 9] = [
        "id", "name", "alias", "api-key", "api_key", "apikey", "key", "provider", "model",
    ];
    for key in IDENTITY_KEYS {
        let value = mapping_scalar_value(node, key);
        if !value.is_empty() {
            return format!("{key}={value}");
        }
    }
    for (key, value) in node.pairs() {
        if value.kind != Kind::Scalar || is_untyped_child(path, &key.value) {
            continue;
        }
        let value = value.value.trim();
        if !value.is_empty() {
            return format!("{}={value}", go_lower(key.value.trim()));
        }
    }
    String::new()
}

/// `mappingScalarValue`: the trimmed value of the first scalar field whose
/// trimmed key is `key`, ignoring case.
fn mapping_scalar_value<'a>(node: &'a Node, key: &str) -> &'a str {
    node.pairs()
        .find(|(k, v)| v.kind == Kind::Scalar && go_lower(k.value.trim()) == key)
        .map_or("", |(_, v)| v.value.trim())
}

/// Go's `strings.ToLower`: each character's simple lowercase.
fn go_lower(text: &str) -> String {
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// `nodesStructurallyEqual`: same kinds, the same trimmed scalar values and
/// the same items, compared in order. A mapping's untyped keys don't
/// count.
fn nodes_structurally_equal(a: &Node, b: &Node, path: &mut Vec<String>) -> bool {
    if a.kind != b.kind {
        return false;
    }
    match a.kind {
        Kind::Mapping => {
            let left = typed_pairs(a, path);
            let right = typed_pairs(b, path);
            left.len() == right.len()
                && left.iter().zip(&right).all(|((ak, av), (bk, bv))| {
                    if !nodes_structurally_equal(ak, bk, path) {
                        return false;
                    }
                    path.push(ak.value.clone());
                    let equal = nodes_structurally_equal(av, bv, path);
                    path.pop();
                    equal
                })
        }
        Kind::Sequence => {
            a.content.len() == b.content.len()
                && a.content
                    .iter()
                    .zip(&b.content)
                    .all(|(x, y)| nodes_structurally_equal(x, y, path))
        }
        Kind::Alias => match (&a.alias, &b.alias) {
            (Some(AliasTarget::Node(x)), Some(AliasTarget::Node(y))) => {
                nodes_structurally_equal(x, y, path)
            }
            _ => false,
        },
        Kind::Scalar | Kind::Document | Kind::Zero => a.value.trim() == b.value.trim(),
    }
}

/// A mapping's pairs without its untyped keys.
fn typed_pairs<'a>(node: &'a Node, path: &[String]) -> Vec<(&'a Node, &'a Node)> {
    node.pairs()
        .filter(|(key, _)| !is_untyped_child(path, &key.value))
        .collect()
}

/// `isKnownDefaultValue`: whether a new value at `path` is zero or one of
/// the non-zero defaults upstream leaves out of the file.
fn is_known_default_value(path: &[String], node: &Node) -> bool {
    if is_plugin_configs_subtree_path(path) {
        return false;
    }
    if matches!(path, [plugins] if plugins == "plugins")
        && node.kind == Kind::Mapping
        && let Some(configs) =
            find_map_key_index(node, "configs").and_then(|index| node.content.get(index + 1))
        && configs.kind == Kind::Mapping
        && !configs.content.is_empty()
    {
        return false;
    }
    let last = path.last().map(String::as_str);
    // Pointer-backed: an explicit zero or false is meaningful.
    if matches!(last, Some("weight" | "request-retry"))
        && node.kind == Kind::Scalar
        && node.tag == INT_TAG
    {
        return false;
    }
    if matches!(last, Some("cache-user-id" | "disable-cooling"))
        && node.kind == Kind::Scalar
        && node.tag == BOOL_TAG
    {
        return false;
    }
    if is_zero_value_node(node) {
        return true;
    }
    if path.is_empty() || node.kind != Kind::Scalar {
        return false;
    }
    let full = path.join(".");
    if node.tag == STR_TAG {
        let default = match full.as_str() {
            "pprof.addr" => Some(DEFAULT_PPROF_ADDR),
            "remote-management.panel-github-repository" => Some(DEFAULT_PANEL_GITHUB_REPOSITORY),
            "plugins.dir" => Some("plugins"),
            "routing.strategy" => Some("round-robin"),
            _ => None,
        };
        if let Some(default) = default {
            return node.value == default;
        }
    }
    node.tag == INT_TAG && full == "error-logs-max-files" && node.value == "10"
}

/// `pruneKnownDefaultsInNewNode`: removes the zero and default values from
/// a new value, and the mappings and lists that leaves empty.
fn prune_known_defaults_in_new_node(path: &mut Vec<String>, node: &mut Node) {
    if is_plugin_configs_subtree_path(path) {
        return;
    }
    match node.kind {
        Kind::Mapping => {
            let mut items = std::mem::take(&mut node.content).into_iter();
            while let (Some(key), Some(mut value)) = (items.next(), items.next()) {
                path.push(key.value.clone());
                let keep = !is_known_default_value(path, &value) && {
                    prune_known_defaults_in_new_node(path, &mut value);
                    !(matches!(value.kind, Kind::Mapping | Kind::Sequence)
                        && value.content.is_empty())
                };
                path.pop();
                if keep {
                    node.content.push(key);
                    node.content.push(value);
                }
            }
        }
        Kind::Sequence => {
            for child in &mut node.content {
                prune_known_defaults_in_new_node(path, child);
            }
        }
        _ => {}
    }
}

/// `isZeroValueNode`: a `false`, `0`, `0.0`, empty string or null scalar, or
/// a list or mapping holding only zero values.
fn is_zero_value_node(node: &Node) -> bool {
    match node.kind {
        Kind::Scalar => match node.tag.as_str() {
            BOOL_TAG => node.value == "false",
            INT_TAG | FLOAT_TAG => node.value == "0" || node.value == "0.0",
            STR_TAG => node.value.is_empty(),
            NULL_TAG => true,
            _ => false,
        },
        Kind::Sequence => node.content.iter().all(is_zero_value_node),
        Kind::Mapping => node.pairs().all(|(_, value)| is_zero_value_node(value)),
        _ => false,
    }
}

/// `copyNodeShallow`: `dst` takes `src`'s kind, tag, value and a copy of its
/// content, keeping its own comments, anchor and style.
fn copy_node_shallow(dst: &mut Node, src: &Node) {
    dst.kind = src.kind;
    dst.tag.clone_from(&src.tag);
    dst.value.clone_from(&src.value);
    dst.content.clone_from(&src.content);
}

/// `pruneMappingToGeneratedKeys` for one top-level key: the file's mapping
/// at `key` keeps only the keys the generated one has. When the settings
/// have none, the four OAuth maps stay as empty mappings (their presence
/// matters in the v8 layout) and any other key goes. A generated value that
/// isn't a mapping, or a file value that isn't, replaces the file's.
pub(crate) fn prune_mapping_to_generated_keys(dst: &mut Node, src: &Node, key: &str) {
    if key.is_empty() || dst.kind != Kind::Mapping || src.kind != Kind::Mapping {
        return;
    }
    let Some(dst_index) = find_map_key_index(dst, key) else {
        return;
    };
    let Some(src_index) = find_map_key_index(src, key) else {
        if OAUTH_MAPS.contains(&key) {
            if let Some(value) = dst.content.get_mut(dst_index + 1) {
                *value = Node::mapping();
            }
        } else {
            remove_map_key(dst, key);
        }
        return;
    };
    let (Some(src_value), Some(dst_value)) = (
        src.content.get(src_index + 1),
        dst.content.get_mut(dst_index + 1),
    ) else {
        return;
    };
    if src_value.kind != Kind::Mapping || dst_value.kind != Kind::Mapping {
        src_value.clone_into(dst_value);
        return;
    }
    prune_missing_map_keys(dst_value, src_value, &[key.to_owned()]);
}

/// `pruneMissingMapKeys`: removes the keys of the mapping `dst` that `src`
/// lacks, comparing trimmed keys. Untyped keys stay.
fn prune_missing_map_keys(dst: &mut Node, src: &Node, path: &[String]) {
    if dst.kind != Kind::Mapping || src.kind != Kind::Mapping {
        return;
    }
    let keep: Vec<&str> = src
        .pairs()
        .map(|(key, _)| key.value.trim())
        .filter(|key| !key.is_empty())
        .collect();
    let mut items = std::mem::take(&mut dst.content).into_iter();
    while let Some(key) = items.next() {
        let Some(value) = items.next() else {
            dst.content.push(key);
            break;
        };
        if keep.contains(&key.value.trim()) || is_untyped_child(path, &key.value) {
            dst.content.push(key);
            dst.content.push(value);
        }
    }
}

/// `shouldPruneNestedMappingKeys`: a credential's `headers`, and a Claude
/// key's `cloak`, lose the keys the settings no longer have.
fn should_prune_nested_mapping_keys(path: &[String]) -> bool {
    let [.., parent, last] = path else {
        return false;
    };
    match parent.as_str() {
        "claude-api-key" => last == "cloak" || last == "headers",
        "codex-api-key"
        | "gemini-api-key"
        | "interactions-api-key"
        | "xai-api-key"
        | "meta-api-key"
        | "vertex-api-key"
        | "openai-compatibility" => last == "headers",
        _ => false,
    }
}

/// `normalizeCollectionNodeStyles`: every mapping and non-empty list in
/// block style, empty lists as `[]`. Scalars keep their style.
pub(crate) fn normalize_collection_node_styles(node: &mut Node) {
    match node.kind {
        Kind::Mapping => node.style = Style::NONE,
        Kind::Sequence if node.content.is_empty() => node.style = Style::FLOW,
        Kind::Sequence => node.style = Style::NONE,
        _ => return,
    }
    for child in &mut node.content {
        normalize_collection_node_styles(child);
    }
}

/// `removeLegacyOpenAICompatAPIKeys`: drops the legacy `api-keys` of each
/// OpenAI-compatible provider.
pub(crate) fn remove_legacy_openai_compat_api_keys(root: &mut Node) {
    if root.kind != Kind::Mapping {
        return;
    }
    let Some(list) = find_map_key_index(root, "openai-compatibility")
        .and_then(|index| root.content.get_mut(index + 1))
    else {
        return;
    };
    if list.kind != Kind::Sequence {
        return;
    }
    for item in &mut list.content {
        remove_map_key(item, "api-keys");
    }
}

/// `removeRemovedIntegrationKeys`: drops the Amp integration's keys.
pub(crate) fn remove_removed_integration_keys(root: &mut Node) {
    for key in [
        "ampcode",
        "amp-upstream-url",
        "amp-upstream-api-key",
        "amp-restrict-management-to-localhost",
        "amp-model-mappings",
    ] {
        remove_map_key(root, key);
    }
}

/// `removeLegacyGenerativeLanguageKeys`.
pub(crate) fn remove_legacy_generative_language_keys(root: &mut Node) {
    remove_map_key(root, "generative-language-api-key");
}

/// `removeLegacyAuthBlock`.
pub(crate) fn remove_legacy_auth_block(root: &mut Node) {
    remove_map_key(root, "auth");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::yaml3::compose::unmarshal;

    fn root(text: &str) -> Node {
        let document = unmarshal(text.as_bytes()).expect("test YAML parses");
        document.content.into_iter().next().expect("a root node")
    }

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    // Not upstream's: isZeroValueNode and isKnownDefaultValue on the values
    // config_yaml.go names.
    #[test]
    fn zero_and_default_values_are_known() {
        for zero in [
            "false",
            "0",
            "0.0",
            "''",
            "~",
            "[]",
            "{}",
            "[0, false]",
            "{a: ''}",
        ] {
            assert!(is_zero_value_node(&root(zero)), "{zero}");
        }
        for value in ["true", "1", "x", "[1]", "{a: 1}", "!!binary aGk="] {
            assert!(!is_zero_value_node(&root(value)), "{value}");
        }
        assert!(is_known_default_value(
            &path(&["pprof", "addr"]),
            &root("127.0.0.1:8316")
        ));
        assert!(is_known_default_value(
            &path(&["plugins", "dir"]),
            &root("plugins")
        ));
        assert!(is_known_default_value(
            &path(&["routing", "strategy"]),
            &root("round-robin")
        ));
        assert!(is_known_default_value(
            &path(&["error-logs-max-files"]),
            &root("10")
        ));
        assert!(!is_known_default_value(
            &path(&["error-logs-max-files"]),
            &root("'10'")
        ));
        assert!(!is_known_default_value(&path(&["weight"]), &root("0")));
        assert!(!is_known_default_value(
            &path(&["disable-cooling"]),
            &root("false")
        ));
        assert!(!is_known_default_value(
            &path(&["plugins", "configs"]),
            &root("{}")
        ));
    }

    // Not upstream's: list items are matched by identity before position.
    #[test]
    fn reordered_items_keep_their_comments() {
        let mut dst = root("- api-key: a # first\n- api-key: b # second\n");
        let src = root("- api-key: b\n- api-key: a\n- api-key: c\n");
        merge_node_preserve(&mut dst, &src, &mut path(&["codex-api-key"]));
        let keys: Vec<(&str, &str)> = dst
            .content
            .iter()
            .filter_map(|item| {
                let value = item.content.get(1)?;
                Some((value.value.as_str(), value.line_comment.as_str()))
            })
            .collect();
        assert_eq!(keys, [("b", "# second"), ("a", "# first"), ("c", "")]);
    }

    // Not upstream's: untyped item keys survive the item's pruning.
    #[test]
    fn untyped_item_keys_are_kept() {
        let mut dst = root("- api-key: a\n  cloak: {mode: auto}\n  stale: 1\n");
        let src = root("- api-key: a\n");
        merge_node_preserve(&mut dst, &src, &mut path(&["claude-api-key"]));
        let item = dst.content.first().expect("an item");
        let keys: Vec<&str> = item.pairs().map(|(key, _)| key.value.as_str()).collect();
        assert_eq!(keys, ["api-key", "cloak"]);
    }

    // Not upstream's: an untyped section keeps the file's values and only
    // gains missing non-default keys.
    #[test]
    fn untyped_sections_are_add_only() {
        let mut dst = root("pprof:\n  enable: yes\n");
        let src = root("pprof:\n  enable: false\n  addr: 127.0.0.1:8316\n");
        merge_mapping_preserve(&mut dst, &src, &mut Vec::new());
        let pprof = dst.content.get(1).expect("pprof");
        let keys: Vec<(&str, &str)> = pprof
            .pairs()
            .map(|(key, value)| (key.value.as_str(), value.value.as_str()))
            .collect();
        assert_eq!(keys, [("enable", "yes")]);
    }
}
