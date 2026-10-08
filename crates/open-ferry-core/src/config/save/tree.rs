// Ported from CLIProxyAPI internal/config/config_yaml.go
// (NormalizeCommentIndentation, getOrCreateMapValue, findMapKeyIndex,
// removeMapKey, deepCopyNode) and config_v8.go (yamlPath, setYAMLPath,
// setYAMLPathWithComments, copyYAMLPathValue, deleteYAMLPath, legacyPath,
// expandConfigAliases) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Editing a [`Node`] tree by dotted paths, as upstream's config writer
//! does: finding, setting, moving and deleting values with their comments,
//! expanding aliases and merge keys, and writing the tree out.
//!
//! A path is keys joined by `.`; each key is looked up as the first
//! mapping key with that text. Setting a path creates the mappings on the
//! way, turning a value that isn't a mapping into an empty one. Deleting a
//! path also removes the mappings it leaves empty.
//!
//! Upstream's `deepCopyNode` is [`Clone`].
//!
//! Deviations from upstream:
//! - [`expand_config_aliases`] refuses an alias inside the node its anchor
//!   names (`yaml: anchor 'a' value contains itself`), where upstream
//!   recurses until the stack overflows, and a document whose aliases
//!   expand to more than 2^20 nodes (`yaml: document contains excessive
//!   aliasing`) or nest deeper than 256 levels (`yaml: exceeded max depth
//!   of 256`), where upstream expands without limit. Upstream only expands
//!   documents yaml.v3 has decoded, which refuses the first and limits the
//!   second by ratio; [`super::write_file`] expands one it hasn't.
//! - Where upstream would index past the end of a node's content (a path
//!   it assumes exists), nothing happens.

use super::super::yaml3::compose::{MAX_ALIAS_COPIES, YamlError};
use super::super::yaml3::encode::marshal_node;
use super::super::yaml3::{AliasTarget, Kind, MAP_TAG, MAX_DEPTH, MERGE_TAG, Node, STR_TAG};

/// The indent yaml.v3's `Marshal` writes with.
const MARSHAL_INDENT: usize = 4;

/// The indent upstream writes its config file with (`Encoder.SetIndent(2)`).
const CONFIG_INDENT: usize = 2;

/// `findMapKeyIndex`: the index of the first key `key` in a mapping's
/// content, if the node is a mapping and has one.
pub(crate) fn find_map_key_index(node: &Node, key: &str) -> Option<usize> {
    if node.kind != Kind::Mapping {
        return None;
    }
    (0..node.content.len()).step_by(2).find(|&index| {
        index + 1 < node.content.len() && node.content.get(index).is_some_and(|k| k.value == key)
    })
}

/// `yamlPath`: the node at a dotted path.
pub(crate) fn yaml_path<'a>(node: &'a Node, path: &str) -> Option<&'a Node> {
    let mut current = node;
    for part in path.split('.') {
        let index = find_map_key_index(current, part)?;
        current = current.content.get(index + 1)?;
    }
    Some(current)
}

/// [`yaml_path`], to change the node.
pub(crate) fn yaml_path_mut<'a>(node: &'a mut Node, path: &str) -> Option<&'a mut Node> {
    let mut current = node;
    for part in path.split('.') {
        let index = find_map_key_index(current, part)?;
        current = current.content.get_mut(index + 1)?;
    }
    Some(current)
}

/// `legacyPath`: the node at a legacy path, except an `api-keys` mapping,
/// which is the v8 API-key groups rather than the legacy client keys.
pub(crate) fn legacy_path<'a>(root: &'a Node, path: &str) -> Option<&'a Node> {
    let node = yaml_path(root, path)?;
    (path != "api-keys" || node.kind != Kind::Mapping).then_some(node)
}

/// `getOrCreateMapValue`: the value of `key` in a mapping, added as an
/// empty string when missing. A node that isn't a mapping becomes an empty
/// one first.
pub(crate) fn get_or_create_map_value<'a>(node: &'a mut Node, key: &str) -> &'a mut Node {
    if node.kind != Kind::Mapping {
        node.kind = Kind::Mapping;
        MAP_TAG.clone_into(&mut node.tag);
        node.content.clear();
    }
    let index = match find_map_key_index(node, key) {
        Some(index) => index + 1,
        None => {
            node.content.push(Node::scalar(STR_TAG, key));
            node.content.push(Node::scalar(STR_TAG, ""));
            node.content.len() - 1
        }
    };
    // In bounds: find_map_key_index only returns a key with a value after it.
    &mut node.content[index]
}

/// `setYAMLPath`: sets the node at a dotted path to a copy of `value`.
pub(crate) fn set_yaml_path(root: &mut Node, path: &str, value: &Node) {
    let mut current = root;
    for part in path.split('.') {
        current = get_or_create_map_value(current, part);
    }
    value.clone_into(current);
}

/// `setYAMLPathWithComments`: [`set_yaml_path`], with `value`'s head and
/// foot comments put on the key, where yaml.v3 writes them.
pub(crate) fn set_yaml_path_with_comments(root: &mut Node, path: &str, value: &Node) {
    set_yaml_path(root, path, value);
    let (parent, last) = match path.rsplit_once('.') {
        Some((parent, last)) => (yaml_path_mut(root, parent), last),
        None => (Some(root), path),
    };
    let Some(parent) = parent else {
        return;
    };
    let Some(index) = find_map_key_index(parent, last) else {
        return;
    };
    if let Some(key) = parent.content.get_mut(index) {
        value.head_comment.clone_into(&mut key.head_comment);
        value.foot_comment.clone_into(&mut key.foot_comment);
    }
    if let Some(node) = parent.content.get_mut(index + 1) {
        node.head_comment.clear();
        node.foot_comment.clear();
    }
}

/// `copyYAMLPathValue`: a copy of the node at a dotted path, carrying its
/// key's comments, and those of the mappings above it that hold nothing
/// else (which a move removes), as head and foot comments.
pub(crate) fn copy_yaml_path_value(root: &Node, path: &str) -> Option<Node> {
    let mut copy = yaml_path(root, path)?.clone();
    let parts: Vec<&str> = path.split('.').collect();
    let mut parent = root;
    let mut parents = vec![root];
    if let Some((_, ancestors)) = parts.split_last() {
        for part in ancestors {
            parent = yaml_path(parent, part)?;
            parents.push(parent);
        }
    }
    if let Some(last) = parts.last()
        && let Some(index) = find_map_key_index(parent, last)
        && let Some(key) = parent.content.get(index)
    {
        copy.head_comment =
            join_trimmed(&[&key.head_comment, &key.line_comment, &copy.head_comment]);
        copy.foot_comment = join_trimmed(&[&copy.foot_comment, &key.foot_comment]);
    }
    // A move also prunes single-child ancestors: carry their comments.
    let mut level = parents.len() - 1;
    while level > 0 {
        let (Some(node), Some(above), Some(part)) = (
            parents.get(level),
            parents.get(level - 1),
            parts.get(level - 1),
        ) else {
            break;
        };
        if node.content.len() != 2 {
            break;
        }
        let Some(key) = find_map_key_index(above, part).and_then(|index| above.content.get(index))
        else {
            break;
        };
        copy.head_comment = join_trimmed(&[
            &key.head_comment,
            &key.line_comment,
            &node.head_comment,
            &node.line_comment,
            &copy.head_comment,
        ]);
        copy.foot_comment =
            join_trimmed(&[&copy.foot_comment, &node.foot_comment, &key.foot_comment]);
        level -= 1;
    }
    Some(copy)
}

/// Comments joined by line breaks, trimmed (`strings.TrimSpace(a + "\n" +
/// b ...)`).
fn join_trimmed(parts: &[&str]) -> String {
    parts.join("\n").trim().to_owned()
}

/// `deleteYAMLPath`: removes the key at a dotted path and the mappings it
/// leaves empty. Whether the key was found.
pub(crate) fn delete_yaml_path(node: &mut Node, path: &str) -> bool {
    let (key, rest) = match path.split_once('.') {
        Some((key, rest)) => (key, Some(rest)),
        None => (path, None),
    };
    let Some(index) = find_map_key_index(node, key) else {
        return false;
    };
    if let Some(rest) = rest {
        let Some(child) = node.content.get_mut(index + 1) else {
            return false;
        };
        if !delete_yaml_path(child, rest) {
            return false;
        }
        if !child.content.is_empty() {
            return true;
        }
    }
    node.content
        .drain(index..(index + 2).min(node.content.len()));
    true
}

/// `removeMapKey`: removes the first `key` of a mapping with its value.
pub(crate) fn remove_map_key(node: &mut Node, key: &str) {
    if node.kind != Kind::Mapping || key.is_empty() {
        return;
    }
    if let Some(index) = find_map_key_index(node, key) {
        node.content
            .drain(index..(index + 2).min(node.content.len()));
    }
}

/// `expandConfigAliases`: a copy of `node` with every alias replaced by a
/// copy of its anchor's node, merge keys (`<<`) replaced by the keys they
/// bring in that the mapping doesn't have, and no anchors. An alias's own
/// comments go; its anchor node's come along.
pub(crate) fn expand_config_aliases(node: &Node) -> Result<Node, YamlError> {
    let mut budget = MAX_ALIAS_COPIES;
    expand(node, &mut budget, false, 0)
}

fn expand(node: &Node, budget: &mut usize, aliased: bool, depth: usize) -> Result<Node, YamlError> {
    if depth > MAX_DEPTH {
        return Err(YamlError::fail(format!(
            "exceeded max depth of {MAX_DEPTH}"
        )));
    }
    if node.kind == Kind::Alias {
        return match &node.alias {
            Some(AliasTarget::Node(target)) => expand(target, budget, true, depth),
            Some(AliasTarget::Cycle) => Err(YamlError::fail(format!(
                "anchor '{}' value contains itself",
                node.value
            ))),
            None => Err(YamlError::fail(format!(
                "unknown anchor '{}' referenced",
                node.value
            ))),
        };
    }
    if aliased {
        *budget = budget
            .checked_sub(1)
            .ok_or_else(|| YamlError::fail("document contains excessive aliasing"))?;
    }
    let mut copy = Node {
        kind: node.kind,
        style: node.style,
        tag: node.tag.clone(),
        value: node.value.clone(),
        anchor: String::new(),
        alias: None,
        content: Vec::with_capacity(node.content.len()),
        head_comment: node.head_comment.clone(),
        line_comment: node.line_comment.clone(),
        foot_comment: node.foot_comment.clone(),
        line: node.line,
        column: node.column,
    };
    for child in &node.content {
        copy.content
            .push(expand(child, budget, aliased, depth.saturating_add(1))?);
    }
    if copy.kind != Kind::Mapping {
        return Ok(copy);
    }
    let mut index = 0;
    while index < copy.content.len() {
        if copy
            .content
            .get(index)
            .is_none_or(|key| key.tag != MERGE_TAG)
        {
            index += 2;
            continue;
        }
        let end = (index + 2).min(copy.content.len());
        let Some(merge) = copy.content.drain(index..end).nth(1) else {
            continue;
        };
        let mappings = if merge.kind == Kind::Sequence {
            merge.content
        } else {
            vec![merge]
        };
        for mapping in mappings {
            let mut items = mapping.content.into_iter();
            while let (Some(key), Some(value)) = (items.next(), items.next()) {
                if find_map_key_index(&copy, &key.value).is_none() {
                    copy.content.push(key);
                    copy.content.push(value);
                }
            }
        }
    }
    Ok(copy)
}

/// `NormalizeCommentIndentation`: comment lines moved to the start of the
/// line.
pub(crate) fn normalize_comment_indentation(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for (index, line) in data.split(|&byte| byte == b'\n').enumerate() {
        if index > 0 {
            out.push(b'\n');
        }
        let start = line
            .iter()
            .position(|&byte| byte != b' ' && byte != b'\t')
            .unwrap_or(line.len());
        let trimmed = line.get(start..).unwrap_or_default();
        if trimmed.first() == Some(&b'#') {
            out.extend_from_slice(trimmed);
        } else {
            out.extend_from_slice(line);
        }
    }
    out
}

/// `yaml.Marshal(node)`: the tree written out with yaml.v3's emitter,
/// indented by 4.
pub(crate) fn marshal(node: &Node) -> Result<Vec<u8>, YamlError> {
    marshal_node(node, MARSHAL_INDENT)
}

/// The tree written out as upstream writes its config file: a
/// `yaml.Encoder` with `SetIndent(2)`. Upstream then runs
/// [`normalize_comment_indentation`] on the result.
pub(crate) fn marshal_config(node: &Node) -> Result<Vec<u8>, YamlError> {
    marshal_node(node, CONFIG_INDENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Node)]) -> Node {
        let mut node = Node::mapping();
        for (key, value) in pairs {
            node.content.push(Node::scalar(STR_TAG, key));
            node.content.push(value.clone());
        }
        node
    }

    fn text(value: &str) -> Node {
        Node::scalar(STR_TAG, value)
    }

    // Not upstream's: paths find the first key, create mappings on the way
    // and delete the mappings they empty.
    #[test]
    fn paths_set_find_and_delete() {
        let mut root = map(&[("a", map(&[("b", text("1"))])), ("c", text("x"))]);
        assert_eq!(yaml_path(&root, "a.b").map(|n| n.value.as_str()), Some("1"));
        assert!(yaml_path(&root, "a.b.c").is_none());
        set_yaml_path(&mut root, "c.d", &text("2"));
        assert_eq!(yaml_path(&root, "c.d").map(|n| n.value.as_str()), Some("2"));
        assert_eq!(yaml_path(&root, "c").map(|n| n.tag.as_str()), Some(MAP_TAG));
        assert!(delete_yaml_path(&mut root, "a.b"));
        assert!(yaml_path(&root, "a").is_none());
        assert!(!delete_yaml_path(&mut root, "a.b"));
        assert_eq!(root.content.len(), 2);
    }

    // Not upstream's: a moved value carries its key's comments and those of
    // the single-child mappings above it.
    #[test]
    fn copies_carry_comments_of_pruned_ancestors() {
        let mut leaf_key = text("leaf");
        leaf_key.head_comment = "# leaf head".into();
        leaf_key.line_comment = "# leaf line".into();
        let mut inner = Node::mapping();
        inner.content.push(leaf_key);
        inner.content.push(text("v"));
        let mut section_key = text("section");
        section_key.head_comment = "# section".into();
        let mut root = Node::mapping();
        root.content.push(section_key);
        root.content.push(inner);
        let copy = copy_yaml_path_value(&root, "section.leaf").unwrap_or_default();
        assert_eq!(
            copy.head_comment,
            "# section\n\n\n\n# leaf head\n# leaf line"
        );
        set_yaml_path_with_comments(&mut root, "moved", &copy);
        let key = root.content.get(2).cloned().unwrap_or_default();
        assert_eq!(key.head_comment, copy.head_comment);
        assert!(yaml_path(&root, "moved").is_some_and(|n| n.head_comment.is_empty()));
    }

    // Not upstream's: merge keys bring in only missing keys, and an alias
    // inside its own anchor is refused.
    #[test]
    fn aliases_and_merges_expand() {
        let base = map(&[("a", text("1")), ("b", text("2"))]);
        let mut merge_key = Node::scalar(MERGE_TAG, "<<");
        merge_key.tag = MERGE_TAG.into();
        let mut root = Node::mapping();
        root.content.push(text("b"));
        root.content.push(text("3"));
        root.content.push(merge_key);
        root.content.push(Node {
            kind: Kind::Alias,
            value: "base".into(),
            alias: Some(AliasTarget::Node(std::sync::Arc::new(base))),
            ..Node::default()
        });
        let expanded = expand_config_aliases(&root).unwrap_or_default();
        let keys: Vec<&str> = expanded.pairs().map(|(k, _)| k.value.as_str()).collect();
        assert_eq!(keys, ["b", "a"]);
        assert_eq!(
            yaml_path(&expanded, "b").map(|n| n.value.as_str()),
            Some("3")
        );

        let cycle = Node {
            kind: Kind::Alias,
            value: "x".into(),
            alias: Some(AliasTarget::Cycle),
            ..Node::default()
        };
        let error = expand_config_aliases(&cycle)
            .err()
            .unwrap_or_else(|| YamlError(String::new()));
        assert_eq!(error.message(), "yaml: anchor 'x' value contains itself");
    }

    // Not upstream's: NormalizeCommentIndentation moves only comment lines.
    #[test]
    fn comment_lines_lose_their_indentation() {
        let data = b"a:\n    # note\n  b: 1 # kept\n\t#tab\n";
        assert_eq!(
            normalize_comment_indentation(data),
            b"a:\n# note\n  b: 1 # kept\n#tab\n"
        );
        assert_eq!(normalize_comment_indentation(b"x: 1"), b"x: 1");
    }
}
