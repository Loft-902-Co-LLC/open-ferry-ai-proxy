// Ported from CLIProxyAPI internal/config/config_yaml.go
// (NormalizeCommentIndentation, getOrCreateMapValue, findMapKeyIndex,
// removeMapKey, deepCopyNode) and config_v8.go (yamlPath, setYAMLPath,
// setYAMLPathWithComments, copyYAMLPathValue, deleteYAMLPath, legacyPath,
// expandConfigAliases) (v8.0.38, MIT).
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

use std::collections::{HashMap, HashSet};

use super::super::yaml3::compose::{MAX_ALIAS_COPIES, MAX_ALIAS_COPY_TEXT, YamlError};
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
        if value.comment_key_owner.is_some() {
            key.comment_receipts.owner = value.comment_key_owner.clone();
        }
        key.comment_receipts
            .head
            .clone_from(&value.comment_receipts.head);
        key.comment_receipts
            .foot
            .clone_from(&value.comment_receipts.foot);
        value.head_comment.clone_into(&mut key.head_comment);
        value.foot_comment.clone_into(&mut key.foot_comment);
    }
    if let Some(node) = parent.content.get_mut(index + 1) {
        node.head_comment.clear();
        node.foot_comment.clear();
        node.comment_receipts.head.clear();
        node.comment_receipts.foot.clear();
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
        copy.comment_key_owner = key.comment_receipts.owner.clone();
        (copy.head_comment, copy.comment_receipts.head) = join_receipted(&[
            (&key.head_comment, &key.comment_receipts.head),
            (&key.line_comment, &key.comment_receipts.line),
            (&copy.head_comment, &copy.comment_receipts.head),
        ]);
        (copy.foot_comment, copy.comment_receipts.foot) = join_receipted(&[
            (&copy.foot_comment, &copy.comment_receipts.foot),
            (&key.foot_comment, &key.comment_receipts.foot),
        ]);
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
        (copy.head_comment, copy.comment_receipts.head) = join_receipted(&[
            (&key.head_comment, &key.comment_receipts.head),
            (&key.line_comment, &key.comment_receipts.line),
            (&node.head_comment, &node.comment_receipts.head),
            (&node.line_comment, &node.comment_receipts.line),
            (&copy.head_comment, &copy.comment_receipts.head),
        ]);
        (copy.foot_comment, copy.comment_receipts.foot) = join_receipted(&[
            (&copy.foot_comment, &copy.comment_receipts.foot),
            (&node.foot_comment, &node.comment_receipts.foot),
            (&key.foot_comment, &key.comment_receipts.foot),
        ]);
        level -= 1;
    }
    Some(copy)
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
    let mut budget = (MAX_ALIAS_COPIES, MAX_ALIAS_COPY_TEXT);
    expand(node, &mut budget, false, 0, &Default::default())
}

fn expand(
    node: &Node,
    budget: &mut (usize, usize),
    aliased: bool,
    depth: usize,
    scopes: &super::super::yaml3::AliasScopes,
) -> Result<Node, YamlError> {
    if depth > MAX_DEPTH {
        return Err(YamlError::fail(format!(
            "exceeded max depth of {MAX_DEPTH}"
        )));
    }
    if node.kind == Kind::Alias {
        return match &node.alias {
            Some(AliasTarget::Node(target)) => {
                let scopes = node
                    .alias_comment_scope
                    .as_ref()
                    .map_or_else(|| scopes.clone(), |scope| scopes.pushed(scope.clone()));
                expand(target, budget, true, depth, &scopes)
            }
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
        budget.0 = budget
            .0
            .checked_sub(1)
            .ok_or_else(|| YamlError::fail("document contains excessive aliasing"))?;
        let text = [
            &node.tag,
            &node.value,
            &node.head_comment,
            &node.line_comment,
            &node.foot_comment,
        ]
        .into_iter()
        .try_fold(0usize, |size, text| size.checked_add(text.len()));
        budget.1 = text
            .and_then(|text| budget.1.checked_sub(text))
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
        comment_receipts: node.comment_receipts.in_aliases(scopes),
        comment_key_owner: node
            .comment_key_owner
            .as_ref()
            .map(|id| id.in_aliases(scopes)),
        head_comment: node.head_comment.clone(),
        line_comment: node.line_comment.clone(),
        line_comment_id: node
            .line_comment_id
            .as_ref()
            .map(|id| id.in_aliases(scopes)),
        line_comment_line: node.line_comment_line,
        alias_comment_scope: node.alias_comment_scope.clone(),
        foot_comment: node.foot_comment.clone(),
        line: node.line,
        column: node.column,
    };
    for child in &node.content {
        copy.content.push(expand(
            child,
            budget,
            aliased,
            depth.saturating_add(1),
            scopes,
        )?);
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
/// line. Scalar body lines are data, including Markdown headings. The same
/// scanner that reads the document identifies them without guessing from
/// indentation or looking for `|` in a line of text.
pub(crate) fn normalize_comment_indentation(data: &[u8]) -> Vec<u8> {
    use super::super::yaml3::parser::Parser;
    use super::super::yaml3::types::{ScalarStyle, TokenType};
    let mut parser = Parser::new(data);
    let mut bodies = Vec::new();
    let (mut block_item, mut flow_sequences, mut flow_mapping_item) = (false, 0usize, false);
    while let Ok(token) = parser.scan() {
        if matches!(token.typ, TokenType::No | TokenType::StreamEnd) {
            break;
        }
        // Reuse the body scan to avoid a second parse for ordinary block
        // collections and empty flow mappings used as mapping values.
        if token.typ == TokenType::FlowSequenceStart {
            flow_sequences += 1;
        } else if token.typ == TokenType::FlowSequenceEnd {
            flow_sequences = flow_sequences.saturating_sub(1);
        }
        if token.typ == TokenType::FlowMappingStart && (block_item || flow_sequences > 0) {
            flow_mapping_item = true;
        }
        if !matches!(token.typ, TokenType::Anchor | TokenType::Tag) {
            block_item = token.typ == TokenType::BlockEntry;
        }
        if token.typ == TokenType::Scalar
            && matches!(token.style, ScalarStyle::Literal | ScalarStyle::Folded)
        {
            bodies.push(
                token.start_mark.line.saturating_add(1)
                    ..token
                        .end_mark
                        .line
                        .saturating_add(usize::from(token.end_mark.column > 0)),
            );
        }
    }
    #[cfg(feature = "parity")]
    if !super::parity::ablation::enabled(
        super::parity::ablation::Protection::BlockScalarCommentIndentation,
    ) {
        bodies.clear();
    }
    let mut out = Vec::with_capacity(data.len());
    let item_feet = if flow_mapping_item {
        flow_item_foot_lines(data)
    } else {
        std::collections::BTreeSet::new()
    };
    let mut logical_line = 0usize;
    for (index, line) in data.split(|&byte| byte == b'\n').enumerate() {
        if index > 0 {
            out.push(b'\n');
        }
        let start = line
            .iter()
            .position(|&byte| byte != b' ' && byte != b'\t')
            .unwrap_or(line.len());
        let trimmed = line.get(start..).unwrap_or_default();
        if trimmed.first() == Some(&b'#')
            && !bodies.iter().any(|body| body.contains(&logical_line))
            && !item_feet.contains(&index)
        {
            out.extend_from_slice(trimmed);
        } else {
            out.extend_from_slice(line);
        }
        // The scanner counts YAML line separators too; CR before this split's
        // LF belongs to the same CRLF break, rather than a second line.
        let without_crlf = line.strip_suffix(b"\r").unwrap_or(line);
        let extra_breaks = String::from_utf8_lossy(without_crlf)
            .chars()
            .filter(|c| matches!(c, '\r' | '\u{85}' | '\u{2028}' | '\u{2029}'))
            .count();
        logical_line = logical_line.saturating_add(1).saturating_add(extra_breaks);
    }
    #[cfg(feature = "parity")]
    super::parity::ablation::record_normalization(data, &out);
    out
}

/// Keep the feet of consecutive flow mappings outside the next item's head.
/// Column-zero comments before a flow mapping otherwise reload on that next
/// item. Only actual foot paragraphs, bounded by their parsed sibling nodes,
/// retain indentation; matching comment text elsewhere is not a witness.
fn flow_item_foot_lines(data: &[u8]) -> std::collections::BTreeSet<usize> {
    use super::super::yaml3::{parser::Parser, types::TokenType};
    let mut lines = std::collections::BTreeSet::new();
    if !flow_item_feet_enabled() {
        return lines;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return lines;
    };
    // Node marks count YAML breaks; physical LF indices cannot witness a
    // paragraph in a document containing other YAML line separators.
    if text.contains(['\u{85}', '\u{2028}', '\u{2029}'])
        || text
            .as_bytes()
            .iter()
            .enumerate()
            .any(|(i, b)| *b == b'\r' && data.get(i + 1) != Some(&b'\n'))
    {
        return lines;
    }
    let Ok(doc) = super::super::yaml3::compose::unmarshal(data) else {
        return lines;
    };
    if !has_multiple_flow_item_feet(&doc) {
        return lines;
    }
    let source: Vec<_> = text.lines().collect();
    // A matched collection end is beyond all its scalar tokens, including
    // quoted values containing comment-looking lines or closing braces.
    let mut parser = Parser::new(data);
    let (mut stack, mut ends) = (Vec::new(), Vec::new());
    while let Ok(token) = parser.scan() {
        match token.typ {
            TokenType::No | TokenType::StreamEnd => break,
            TokenType::FlowMappingStart | TokenType::FlowSequenceStart => {
                stack.push((token.typ, token.start_mark))
            }
            TokenType::FlowMappingEnd | TokenType::FlowSequenceEnd => {
                if let Some((kind, start)) = stack.pop()
                    && kind == TokenType::FlowMappingStart
                {
                    ends.push(((start.line + 1, start.column + 1), token.end_mark.line + 1));
                }
            }
            _ => {}
        }
    }
    fn visit(
        n: &Node,
        source: &[&str],
        ends: &[((usize, usize), usize)],
        lines: &mut std::collections::BTreeSet<usize>,
    ) {
        if n.kind == Kind::Mapping {
            for index in (2..n.content.len()).step_by(2) {
                let key = &n.content[index];
                let Some(seq) = flow_item_boundary(n, index) else {
                    continue;
                };
                let mut feet = Vec::new();
                for (i, item) in seq.content.iter().enumerate() {
                    let next = seq
                        .content
                        .get(i + 1)
                        .map_or(key.line.saturating_sub(1), |s| s.line.saturating_sub(1));
                    let Some(start) = ends
                        .iter()
                        .filter(|(s, _)| *s >= (item.line, item.column))
                        .min_by_key(|(s, _)| *s)
                        .map(|(_, e)| *e)
                    else {
                        continue;
                    };
                    let Some(indent) = source
                        .get(item.line.saturating_sub(1))
                        .filter(|s| s.trim_start().starts_with('-'))
                        .map(|s| s.len() - s.trim_start_matches([' ', '\t']).len())
                    else {
                        continue;
                    };
                    let mut foot = item
                        .foot_comment
                        .lines()
                        .filter(|s| !s.is_empty())
                        .peekable();
                    let mut paragraph = Vec::new();
                    for (at, line) in source.iter().enumerate().take(next).skip(start) {
                        let trimmed = line.trim_start_matches([' ', '\t']);
                        if line.len() - trimmed.len() >= indent
                            && foot.peek().is_some_and(|s| trimmed == *s)
                        {
                            paragraph.push(at);
                            foot.next();
                        }
                    }
                    if foot.peek().is_none() && !paragraph.is_empty() {
                        feet.push(paragraph);
                    }
                }
                if feet.len() <= 1 {
                    continue;
                }
                let mut head_start = feet.iter().flatten().max().map_or(0, |i| i + 1);
                lines.extend(feet.into_iter().flatten());
                // Only the one-column root block rebuilt from source is
                // known to belong here. Other emitted root feet can already
                // have moved from inner keys before normalization sees them.
                if source
                    .get(key.line.saturating_sub(1))
                    .is_some_and(|line| !line.starts_with([' ', '\t']))
                {
                    let end = key.line.saturating_sub(1);
                    let head = comment_lines_before(&key.head_comment, end, source);
                    if let Some(head) = head {
                        let end = head.first().copied().unwrap_or(end);
                        if let Some(foot) =
                            comment_lines_before(&n.content[index - 2].foot_comment, end, source)
                            && foot.first().is_some_and(|at| *at >= head_start)
                            && foot.iter().all(|at| source[*at].starts_with(" #"))
                        {
                            head_start = foot.last().unwrap() + 1;
                            lines.extend(foot);
                        }
                    }
                }
                if let Some(head) =
                    comment_lines_before(&key.head_comment, key.line.saturating_sub(1), source)
                    && head.first().is_none_or(|at| *at >= head_start)
                {
                    lines.extend(head);
                }
            }
        }
        for child in &n.content {
            visit(child, source, ends, lines);
        }
    }
    visit(&doc, &source, &ends, &mut lines);
    lines
}

fn comment_lines_before(comment: &str, end: usize, physical: &[&str]) -> Option<Vec<usize>> {
    let expected: Vec<_> = comment.lines().filter(|s| !s.is_empty()).collect();
    if expected.is_empty() {
        return Some(Vec::new());
    }
    let mut wanted = expected.iter().rev();
    let mut found = Vec::new();
    for at in (0..end).rev() {
        let line = physical.get(at)?.trim_start_matches([' ', '\t']);
        if line.is_empty() {
            continue;
        }
        if wanted.next().is_none_or(|s| line != *s) {
            return None;
        }
        found.push(at);
        if found.len() == expected.len() {
            found.reverse();
            return Some(found);
        }
    }
    None
}

/// Do not let normalization preserve a key-foot paragraph as the next key's
/// head. At this boundary we still have the source tree: accept only an exact
/// split of its foot into the emitted foot plus a prefix of the emitted head.
pub(super) fn source_flow_item_heads(node: &Node, data: Vec<u8>) -> Vec<u8> {
    source_flow_item_heads_with_receipts(node, data, &mut Default::default())
}

pub(super) fn source_flow_item_heads_receipted(
    node: &Node,
    mut rendered: ReceiptedYaml,
) -> ReceiptedYaml {
    rendered.bytes =
        source_flow_item_heads_with_receipts(node, rendered.bytes, &mut rendered.receipts);
    rendered
}

fn source_flow_item_heads_with_receipts(
    node: &Node,
    data: Vec<u8>,
    receipts: &mut super::super::yaml3::EmissionReceipts,
) -> Vec<u8> {
    fn candidate(n: &Node) -> bool {
        (n.kind == Kind::Mapping
            && (2..n.content.len()).step_by(2).any(|i| {
                n.content[i - 2].foot_comment.contains("\n\n") && flow_item_boundary(n, i).is_some()
            }))
            || n.content.iter().any(candidate)
    }
    if !flow_item_feet_enabled() || !candidate(node) {
        return data;
    }
    let Ok(text) = std::str::from_utf8(&data) else {
        return data;
    };
    if text.contains(['\u{85}', '\u{2028}', '\u{2029}'])
        || data
            .iter()
            .enumerate()
            .any(|(i, b)| *b == b'\r' && data.get(i + 1) != Some(&b'\n'))
    {
        return data;
    }
    let Ok(emitted) = super::super::yaml3::compose::unmarshal(&data) else {
        return data;
    };
    fn comments(text: &str) -> Vec<&str> {
        text.lines().filter(|s| !s.is_empty()).collect()
    }
    fn visit(
        source: &Node,
        emitted: &Node,
        physical: &[&str],
        marked: &mut std::collections::BTreeMap<usize, usize>,
        breaks: &mut std::collections::BTreeSet<usize>,
        skipped: &mut std::collections::BTreeSet<usize>,
    ) {
        if source.kind != emitted.kind || source.content.len() != emitted.content.len() {
            return;
        }
        if source.kind == Kind::Mapping {
            for i in (2..source.content.len()).step_by(2) {
                if flow_item_boundary(source, i).is_none()
                    || flow_item_boundary(emitted, i)
                        .is_none_or(|seq| seq.style.has(super::super::yaml3::Style::FLOW))
                    || source.content[i].value != emitted.content[i].value
                    || source.content[i - 2].value != emitted.content[i - 2].value
                {
                    continue;
                }
                let foot = comments(&source.content[i - 2].foot_comment);
                let written = comments(&emitted.content[i - 2].foot_comment);
                let head = comments(&source.content[i].head_comment);
                let emitted_head = comments(&emitted.content[i].head_comment);
                let Some(moved) = foot.strip_prefix(written.as_slice()) else {
                    continue;
                };
                if moved.is_empty()
                    || !emitted_head.starts_with(moved)
                    || emitted_head.get(moved.len()..) != Some(head.as_slice())
                {
                    continue;
                }
                let key = &emitted.content[i];
                let Some(head_lines) =
                    comment_lines_before(&key.head_comment, key.line.saturating_sub(1), physical)
                else {
                    continue;
                };
                let Some(first_head) = head_lines.first() else {
                    continue;
                };
                let Some(mut foot_lines) = comment_lines_before(
                    &emitted.content[i - 2].foot_comment,
                    *first_head,
                    physical,
                ) else {
                    continue;
                };
                foot_lines.extend(head_lines.into_iter().take(moved.len()));
                let first = foot_lines[0];
                let mut gap_start = first;
                while gap_start > 0 && physical[gap_start - 1].trim().is_empty() {
                    gap_start -= 1;
                }
                if gap_start == first {
                    breaks.insert(first);
                } else {
                    skipped.extend(gap_start..first - 1);
                }
                // Keep one block of foot paragraphs, rather than merging
                // parser fields and losing their separating blank on repeat.
                // At the root an indented foot must stay above column zero.
                let indent = usize::from(
                    physical
                        .get(key.line.saturating_sub(1))
                        .is_some_and(|line| !line.starts_with([' ', '\t'])),
                );
                marked.extend(foot_lines.into_iter().map(|at| (at, indent)));
            }
        }
        for (source, emitted) in source.content.iter().zip(&emitted.content) {
            visit(source, emitted, physical, marked, breaks, skipped);
        }
    }
    let mut marked = std::collections::BTreeMap::new();
    let mut breaks = std::collections::BTreeSet::new();
    let mut skipped = std::collections::BTreeSet::new();
    visit(
        node,
        &emitted,
        &text.lines().collect::<Vec<_>>(),
        &mut marked,
        &mut breaks,
        &mut skipped,
    );
    if marked.is_empty() {
        return data;
    }
    let mut out = Vec::with_capacity(data.len());
    receipts.comments.sort_by_key(|c| c.offset);
    let (mut receipt, mut old_at) = (0, 0);
    for (i, line) in data.split(|b| *b == b'\n').enumerate() {
        if skipped.contains(&i) {
            old_at += line.len() + 1;
            continue;
        }
        if i > 0 {
            out.push(b'\n');
        }
        if breaks.contains(&i) {
            out.push(b'\n');
        }
        let new_at = out.len();
        let mut removed = 0;
        let mut added = 0;
        if let Some(indent) = marked.get(&i) {
            let start = line
                .iter()
                .position(|b| !matches!(b, b' ' | b'\t'))
                .unwrap_or(line.len());
            removed = start;
            added = *indent;
            out.extend(std::iter::repeat_n(b' ', *indent));
            out.extend_from_slice(&line[start..]);
        } else {
            out.extend_from_slice(line);
        }
        while let Some(c) = receipts.comments.get_mut(receipt) {
            if c.offset >= old_at + line.len() + 1 {
                break;
            }
            c.offset = new_at + added + c.offset.saturating_sub(old_at + removed);
            receipt += 1;
        }
        old_at += line.len() + 1;
    }
    #[cfg(feature = "parity")]
    super::parity::ablation::record_normalization(&data, &out);
    out
}

/// A repaired sequence whose sibling mapping feet need flow endings.
pub(super) fn has_multiple_flow_item_feet(node: &Node) -> bool {
    (node.kind == Kind::Mapping
        && (2..node.content.len())
            .step_by(2)
            .any(|i| flow_item_boundary(node, i).is_some()))
        || node.content.iter().any(has_multiple_flow_item_feet)
}

/// Compatibility gate for the bounded flow-foot repair. The older switch's
/// standalone diagnostic outputs remain identical to main.
fn flow_item_feet_enabled() -> bool {
    #[cfg(feature = "parity")]
    {
        use crate::config::save::parity::ablation::{Protection, enabled};
        enabled(Protection::MultipleFlowItemFeet) && enabled(Protection::EmptyMappingPendingComment)
    }
    #[cfg(not(feature = "parity"))]
    {
        true
    }
}

/// A trailing sequence with multiple flow-item foot comments.
fn pending_flow_item_feet(node: &Node) -> Option<&Node> {
    super::super::yaml3::encode::pending_flow_item_feet(node)
}

/// Both the preflight and paragraph search use this emitted-tree boundary.
fn flow_item_boundary(node: &Node, index: usize) -> Option<&Node> {
    let value = node.content.get(index + 1)?;
    let previous = node.content.get(index.checked_sub(1)?)?;
    if matches!(value.kind, Kind::Mapping | Kind::Sequence) && value.content.is_empty() {
        pending_flow_item_feet(previous)
    } else {
        None
    }
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
    let mut node = node.clone();
    super::encode::quote_config_unicode(&mut node);
    let emitted = marshal_node(&node, CONFIG_INDENT)?;
    Ok(source_flow_item_heads(&node, emitted))
}

/// One save call's original occurrences. It never enters a global cache.
#[derive(Clone, Default)]
pub(crate) struct CommentSource {
    originals: Vec<OriginalComments>,
    #[cfg(test)]
    pub(crate) original_locations: HashMap<super::super::yaml3::CommentId, String>,
    pub(crate) comments: Vec<SourceComment>,
    pub(crate) indexed_comments: bool,
    pub(crate) repair_key_feet: bool,
    pub(crate) complete: bool,
}

#[derive(Clone)]
struct OriginalComments {
    bytes: std::sync::Arc<[u8]>,
    comments: std::sync::Arc<[std::sync::Arc<[u8]>]>,
}

#[derive(Clone)]
pub(crate) struct SourceComment {
    pub(crate) receipt: super::super::yaml3::CommentReceipt,
    pub(crate) bytes: std::sync::Arc<[u8]>,
    pub(crate) column: usize,
    pub(crate) owner_column: usize,
    pub(crate) source_line: usize,
    pub(crate) owner_line: usize,
    pub(crate) placement: Option<super::super::yaml3::CommentId>,
    pub(crate) key_foot: bool,
}

/// A complete emission and its explicitly carried physical/structural bindings.
#[derive(Clone)]
pub(crate) struct ReceiptedYaml {
    pub(crate) bytes: Vec<u8>,
    pub(crate) receipts: super::super::yaml3::EmissionReceipts,
}

pub(crate) fn read_comment_source(data: &[u8]) -> Result<(Node, CommentSource), super::SaveError> {
    let mut node = super::unmarshal(data)?;
    let source = if owner_protection_enabled() && data.contains(&b'#') {
        identify_comment_source(data, &mut node)
    } else {
        CommentSource {
            complete: owner_protection_enabled(),
            ..CommentSource::default()
        }
    };
    Ok((node, source))
}

pub(crate) fn identify_comment_source(data: &[u8], node: &mut Node) -> CommentSource {
    use super::super::yaml3::parser::CommentNode;
    use super::super::yaml3::{CommentId, CommentReceipt, CommentSlot, ReceiptSpan};
    fn has_alias(n: &Node) -> bool {
        n.kind == Kind::Alias || n.content.iter().any(has_alias)
    }
    let aliases_present = has_alias(node);
    fn comment_count(n: &Node) -> usize {
        [&n.head_comment, &n.line_comment, &n.foot_comment]
            .into_iter()
            .map(|s| s.lines().filter(|l| !l.is_empty()).count())
            .sum::<usize>()
            + n.content.iter().map(comment_count).sum::<usize>()
    }
    let count = comment_count(node);
    if count == 0 {
        return CommentSource {
            originals: vec![OriginalComments {
                bytes: data.into(),
                comments: Vec::new().into(),
            }],
            complete: true,
            ..Default::default()
        };
    }
    let Ok(positions) = super::super::yaml3::parser::comment_positions(data) else {
        return CommentSource::default();
    };
    let mut starts = vec![0];
    starts.extend(
        data.iter()
            .enumerate()
            .filter_map(|(i, b)| (*b == b'\n').then_some(i + 1)),
    );
    fn identify(
        n: &mut Node,
        p: &CommentNode,
        data: &[u8],
        starts: &[usize],
        out: &mut CommentSource,
        mapping_key: bool,
    ) -> bool {
        comment_work(1);
        if n.kind != p.kind || n.content.len() != p.content.len() {
            out.complete = false;
            return false;
        }
        let mut needed =
            !n.head_comment.is_empty() || !n.line_comment.is_empty() || !n.foot_comment.is_empty();
        for (i, (child, pos)) in n.content.iter_mut().zip(&p.content).enumerate() {
            needed |= identify(
                child,
                pos,
                data,
                starts,
                out,
                n.kind == Kind::Mapping && i % 2 == 0,
            );
        }
        if !needed {
            return false;
        }
        let owner = CommentId::fresh();
        n.comment_receipts.owner = Some(owner.clone());
        if n.kind == Kind::Mapping {
            for pair in n.content.as_chunks_mut::<2>().0 {
                if pair[1].comment_receipts.owner.is_some()
                    && pair[0].comment_receipts.owner.is_none()
                {
                    pair[0].comment_receipts.owner = Some(CommentId::fresh());
                }
            }
        }
        for (slot, text, positions) in [
            (CommentSlot::Head, &n.head_comment, &p.positions.head),
            (CommentSlot::Line, &n.line_comment, &p.positions.line),
            (CommentSlot::Foot, &n.foot_comment, &p.positions.foot),
        ] {
            let mut at = 0;
            let mut parts = Vec::new();
            for (ordinal, line) in text.split('\n').enumerate() {
                if !line.is_empty() {
                    parts.push((at, ordinal, line));
                }
                at += line.len() + 1;
            }
            if parts.len() != positions.len() {
                out.complete = false;
            }
            let mut spans = Vec::with_capacity(parts.len());
            for (pos, (at, ordinal, line)) in positions.iter().zip(parts) {
                if pos.at != at
                    || data.get(pos.offset..pos.offset + line.len()) != Some(line.as_bytes())
                {
                    out.complete = false;
                    continue;
                }
                let receipt = CommentReceipt {
                    id: CommentId::fresh(),
                    owner: owner.clone(),
                    slot,
                    ordinal,
                };
                spans.push(ReceiptSpan {
                    at,
                    receipt: receipt.clone(),
                });
                let line_index = starts
                    .partition_point(|start| *start <= pos.offset)
                    .saturating_sub(1);
                out.comments.push(SourceComment {
                    receipt,
                    bytes: line.as_bytes().into(),
                    column: pos.offset - starts[line_index],
                    owner_column: n.column.saturating_sub(1),
                    source_line: line_index + 1,
                    owner_line: n.line,
                    placement: None,
                    key_foot: mapping_key && slot == CommentSlot::Foot,
                });
            }
            match slot {
                CommentSlot::Head => n.comment_receipts.head = spans,
                CommentSlot::Line => n.comment_receipts.line = spans,
                CommentSlot::Foot => n.comment_receipts.foot = spans,
            }
        }
        true
    }
    let mut source = CommentSource {
        complete: true,
        indexed_comments: true,
        comments: Vec::with_capacity(count),
        ..Default::default()
    };
    identify(node, &positions, data, &starts, &mut source, false);
    source.originals.push(OriginalComments {
        bytes: data.into(),
        comments: source
            .comments
            .iter()
            .map(|comment| comment.bytes.clone())
            .collect::<Vec<_>>()
            .into(),
    });
    for (index, comment) in source.comments.iter().enumerate() {
        comment.receipt.id.assign_comment_index(index);
    }
    let mut containers: HashMap<usize, Vec<(usize, CommentId)>> = HashMap::new();
    fn collect(n: &Node, out: &mut HashMap<usize, Vec<(usize, CommentId)>>) {
        if matches!(n.kind, Kind::Mapping | Kind::Sequence)
            && let Some(id) = &n.comment_receipts.owner
        {
            out.entry(n.line).or_default().push((n.column, id.clone()));
        }
        for c in &n.content {
            collect(c, out);
        }
    }
    collect(node, &mut containers);
    let mut early: HashMap<CommentId, Vec<usize>> = HashMap::new();
    for (i, c) in source.comments.iter_mut().enumerate() {
        if c.source_line < c.owner_line {
            c.placement = containers
                .get(&c.source_line)
                .and_then(|v| {
                    v.iter()
                        .filter(|(column, _)| *column <= c.column + 1)
                        .max_by_key(|(column, _)| *column)
                })
                .map(|(_, id)| id.clone());
            if c.placement.is_none() {
                early.entry(c.receipt.owner.clone()).or_default().push(i);
            }
        }
    }
    fn ancestors(
        n: &Node,
        stack: &mut Vec<(CommentId, Kind, usize)>,
        early: &HashMap<CommentId, Vec<usize>>,
        comments: &mut [SourceComment],
    ) {
        if let Some(id) = &n.comment_receipts.owner {
            if let Some(indices) = early.get(id) {
                for i in indices {
                    let c = &mut comments[*i];
                    c.placement = stack
                        .iter()
                        .rev()
                        .find(|(_, kind, line)| {
                            matches!(kind, Kind::Mapping | Kind::Sequence) && *line >= c.source_line
                        })
                        .map(|(id, _, _)| id.clone());
                }
            }
            stack.push((id.clone(), n.kind, n.line));
        }
        for child in &n.content {
            ancestors(child, stack, early, comments);
        }
        if n.comment_receipts.owner.is_some() {
            stack.pop();
        }
    }
    ancestors(node, &mut Vec::new(), &early, &mut source.comments);
    if aliases_present {
        // Composer aliases already share each anchor snapshot. Annotate each
        // distinct snapshot once, and preserve that sharing across aliases.
        type Key = (usize, usize, u8);
        fn snapshots(n: &Node, out: &mut HashMap<Key, super::super::yaml3::SparseReceipts>) {
            if n.comment_receipts.owner.is_some() {
                out.insert((n.line, n.column, n.kind as u8), n.comment_receipts.clone());
            }
            for c in &n.content {
                snapshots(c, out);
            }
        }
        fn bind_aliases(
            n: &mut Node,
            original: &HashMap<Key, super::super::yaml3::SparseReceipts>,
            copies: &mut HashMap<usize, std::sync::Arc<Node>>,
            copy: bool,
            depth: usize,
        ) -> bool {
            comment_work(1);
            if depth > MAX_DEPTH {
                return false;
            }
            if copy {
                n.comment_receipts = original
                    .get(&(n.line, n.column, n.kind as u8))
                    .cloned()
                    .unwrap_or_default();
            }
            if let Some(AliasTarget::Node(target)) = &n.alias {
                let key = std::sync::Arc::as_ptr(target) as usize;
                let annotated = if let Some(existing) = copies.get(&key) {
                    existing.clone()
                } else {
                    let mut annotated = (**target).clone();
                    if !bind_aliases(&mut annotated, original, copies, true, depth + 1) {
                        return false;
                    }
                    let annotated = std::sync::Arc::new(annotated);
                    copies.insert(key, annotated.clone());
                    annotated
                };
                n.alias = Some(AliasTarget::Node(annotated));
            }
            n.content
                .iter_mut()
                .all(|c| bind_aliases(c, original, copies, copy, depth + 1))
        }
        let mut original = HashMap::new();
        snapshots(node, &mut original);
        source.complete &= bind_aliases(node, &original, &mut HashMap::new(), false, 0);
    }
    #[cfg(test)]
    if CAPTURE_LOCATIONS.get() {
        fn labels(n: &Node, path: &str, out: &mut HashMap<CommentId, String>) {
            if let Some(id) = &n.comment_receipts.owner {
                out.insert(id.clone(), path.into());
            }
            match n.kind {
                Kind::Mapping => {
                    for (k, v) in n.pairs() {
                        let p = format!("{path}.{}", k.value);
                        labels(k, &format!("{p}:key"), out);
                        labels(v, &p, out);
                    }
                }
                Kind::Document => {
                    for c in &n.content {
                        labels(c, path, out);
                    }
                }
                _ => {
                    for (i, c) in n.content.iter().enumerate() {
                        labels(c, &format!("{path}[{i}]"), out);
                    }
                }
            }
        }
        labels(node, "$", &mut source.original_locations);
        if aliases_present {
            let mut expanded = node.clone();
            if let Some(root) = expanded.content.first_mut()
                && let Ok(copy) = expand_config_aliases(root)
            {
                *root = copy;
            }
            labels(&expanded, "$", &mut source.original_locations);
        }
    }
    source
}

#[cfg(test)]
thread_local! { static CAPTURE_LOCATIONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
pub(crate) fn with_owner_locations<T>(call: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            CAPTURE_LOCATIONS.set(self.0);
        }
    }
    let _reset = Reset(CAPTURE_LOCATIONS.replace(true));
    call()
}
#[cfg(test)]
thread_local! { static COMMENT_WORK: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
pub(crate) fn comment_work(n: usize) {
    COMMENT_WORK.with(|c| {
        if let Some(old) = c.get() {
            c.set(Some(old + n));
        }
    });
}
#[cfg(not(test))]
#[inline(always)]
fn comment_work(_: usize) {}
#[cfg(test)]
pub(crate) fn count_comment_work<T>(call: impl FnOnce() -> T) -> (T, usize) {
    struct Reset(Option<usize>);
    impl Drop for Reset {
        fn drop(&mut self) {
            COMMENT_WORK.set(self.0);
        }
    }
    let _reset = Reset(COMMENT_WORK.replace(Some(0)));
    let value = call();
    let count = COMMENT_WORK.get().unwrap();
    (value, count)
}

impl CommentSource {
    /// Intentional alias copies have distinct identities and the same original
    /// occurrence bytes. Register them before the next emission can defer them.
    pub(crate) fn include_copies(&mut self, node: &Node) {
        if !self.complete || self.comments.is_empty() {
            return;
        }
        fn scoped(n: &Node) -> bool {
            n.comment_receipts
                .head
                .iter()
                .chain(&n.comment_receipts.line)
                .chain(&n.comment_receipts.foot)
                .any(|s| s.receipt.id.is_scoped())
                || n.content.iter().any(scoped)
        }
        if !scoped(node) {
            return;
        }
        fn copied_spans(n: &Node) -> usize {
            n.comment_receipts
                .head
                .iter()
                .chain(&n.comment_receipts.line)
                .chain(&n.comment_receipts.foot)
                .filter(|s| s.receipt.id.is_scoped())
                .count()
                + n.content.iter().map(copied_spans).sum::<usize>()
        }
        let copy_count = copied_spans(node);
        let mut known = HashSet::with_capacity(self.comments.len() + copy_count);
        known.extend(self.comments.iter().map(|c| c.receipt.id.clone()));
        let mut originals = HashMap::new();
        for comment in &self.comments {
            originals
                .entry(comment.receipt.id.source_key())
                .or_insert(comment);
        }
        let mut additions = Vec::with_capacity(copy_count);
        fn walk(
            n: &Node,
            known: &mut HashSet<super::super::yaml3::CommentId>,
            originals: &HashMap<usize, &SourceComment>,
            out: &mut Vec<SourceComment>,
        ) {
            for span in n
                .comment_receipts
                .head
                .iter()
                .chain(&n.comment_receipts.line)
                .chain(&n.comment_receipts.foot)
            {
                if !known.insert(span.receipt.id.clone()) {
                    continue;
                }
                if let Some(original) = originals.get(&span.receipt.id.source_key()) {
                    out.push(SourceComment {
                        receipt: span.receipt.clone(),
                        bytes: original.bytes.clone(),
                        column: original.column,
                        owner_column: original.owner_column,
                        source_line: original.source_line,
                        owner_line: original.owner_line,
                        key_foot: original.key_foot,
                        placement: original
                            .placement
                            .as_ref()
                            .map(|id| id.in_scope_of(&span.receipt.id)),
                    });
                }
            }
            for c in &n.content {
                walk(c, known, originals, out);
            }
        }
        walk(node, &mut known, &originals, &mut additions);
        for (index, comment) in additions.iter().enumerate() {
            comment
                .receipt
                .id
                .assign_comment_index(self.comments.len() + index);
        }
        self.comments.extend(additions);
    }

    pub(crate) fn append(&mut self, other: Self) {
        self.originals.extend(other.originals);
        self.complete &= other.complete;
        self.indexed_comments &= other.indexed_comments || other.comments.is_empty();
        for (index, comment) in other.comments.iter().enumerate() {
            comment
                .receipt
                .id
                .assign_comment_index(self.comments.len() + index);
        }
        self.comments.extend(other.comments);
        #[cfg(test)]
        {
            self.original_locations.extend(other.original_locations);
        }
    }
}

pub(crate) fn marshal_receipted(node: &Node, indent: usize) -> Result<ReceiptedYaml, YamlError> {
    let (bytes, _, receipts) = super::encode::protected_node_receipted(node, indent)?;
    Ok(ReceiptedYaml { bytes, receipts })
}

/// Rebind by actual write offsets and the encoder's structural event paths.
/// A parse cannot turn its new recipient into a source receipt.
pub(crate) fn read_receipted(rendered: &ReceiptedYaml) -> Result<Node, super::SaveError> {
    let mut node = super::unmarshal(&rendered.bytes)?;
    if (!rendered.receipts.owners.is_empty() || !rendered.receipts.comments.is_empty())
        && !rebind_comment_receipts(&mut node, rendered)
    {
        return Err(super::invalid(
            "comment receipt does not match parsed bytes",
        ));
    }
    Ok(node)
}

/// An intermediate writer stage may continue without tracking when only the
/// bindings fail. The outer gate then lacks the source owners and keeps the
/// writer's bytes. Invalid YAML retains its existing parse error.
pub(crate) fn read_receipted_for_render(
    rendered: &ReceiptedYaml,
) -> Result<Node, super::SaveError> {
    read_receipted(rendered).or_else(|_| super::unmarshal(&rendered.bytes))
}

struct ReceiptPath<'a>(&'a [usize]);
impl std::hash::Hash for ReceiptPath<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(self.0, state);
    }
}
impl PartialEq for ReceiptPath<'_> {
    fn eq(&self, other: &Self) -> bool {
        comment_work(1);
        self.0 == other.0
    }
}
impl Eq for ReceiptPath<'_> {}

pub(crate) fn rebind_comment_receipts(node: &mut Node, rendered: &ReceiptedYaml) -> bool {
    use super::super::yaml3::parser::CommentNode;
    use super::super::yaml3::{CommentId, CommentReceipts, ReceiptEmission, ReceiptSpan};
    let Ok(positions) = super::super::yaml3::parser::comment_positions(&rendered.bytes) else {
        return false;
    };
    let owners: HashMap<ReceiptPath<'_>, &CommentId> = rendered
        .receipts
        .owners
        .iter()
        .map(|o| (ReceiptPath(o.path.as_slice()), &o.owner))
        .collect();
    let mut emissions: HashMap<usize, Vec<&ReceiptEmission>> = HashMap::new();
    for e in &rendered.receipts.comments {
        emissions.entry(e.offset).or_default().push(e);
    }
    fn bind(
        n: &mut Node,
        p: &CommentNode,
        path: &mut Vec<usize>,
        owners: &HashMap<ReceiptPath<'_>, &CommentId>,
        emissions: &HashMap<usize, Vec<&ReceiptEmission>>,
        bytes: &[u8],
    ) -> bool {
        comment_work(1);
        if n.kind != p.kind || n.content.len() != p.content.len() {
            return false;
        }
        let mut receipts = CommentReceipts {
            owner: owners
                .get(&ReceiptPath(path.as_slice()))
                .map(|id| (*id).clone()),
            ..Default::default()
        };
        for (text, positions, spans) in [
            (&n.head_comment, &p.positions.head, &mut receipts.head),
            (&n.line_comment, &p.positions.line, &mut receipts.line),
            (&n.foot_comment, &p.positions.foot, &mut receipts.foot),
        ] {
            for pos in positions {
                if let Some(writes) = emissions.get(&pos.offset) {
                    for e in writes {
                        if e.length == 0 {
                            continue;
                        }
                        let line = text
                            .as_bytes()
                            .get(pos.at..)
                            .and_then(|tail| tail.split(|b| *b == b'\n').next());
                        let emitted = bytes.get(e.offset..e.offset + e.length);
                        if emitted != line {
                            return false;
                        }
                        debug_assert!(
                            emitted == line,
                            "comment receipt bytes must equal the parsed line"
                        );
                        spans.push(ReceiptSpan {
                            at: pos.at,
                            receipt: e.receipt.clone(),
                        });
                    }
                }
            }
        }
        n.comment_receipts = receipts.into();
        for (i, (child, pos)) in n.content.iter_mut().zip(&p.content).enumerate() {
            path.push(i);
            if !bind(child, pos, path, owners, emissions, bytes) {
                return false;
            }
            path.pop();
        }
        true
    }
    let valid = bind(
        node,
        &positions,
        &mut Vec::new(),
        &owners,
        &emissions,
        &rendered.bytes,
    );
    if !valid {
        fn clear(node: &mut Node) {
            node.comment_receipts = Default::default();
            for child in &mut node.content {
                clear(child);
            }
        }
        clear(node);
    }
    valid
}

/// Preserve the writer's first actual emission when a generated copy shares
/// a source identity. Later copies keep their existing physical bytes.
fn first_emissions(
    rendered: &ReceiptedYaml,
) -> HashMap<&super::super::yaml3::CommentId, &super::super::yaml3::ReceiptEmission> {
    let mut index = HashMap::new();
    for emission in &rendered.receipts.comments {
        index.entry(&emission.receipt.id).or_insert(emission);
    }
    index
}

/// Normalize the same byte vector while updating each actual write's offset.
pub(crate) fn normalize_receipted(mut rendered: ReceiptedYaml) -> ReceiptedYaml {
    let bytes = normalize_comment_indentation(&rendered.bytes);
    if rendered.receipts.comments.is_empty() {
        rendered.bytes = bytes;
        return rendered;
    }
    let old: Vec<_> = rendered.bytes.split_inclusive(|b| *b == b'\n').collect();
    let new: Vec<_> = bytes.split_inclusive(|b| *b == b'\n').collect();
    if old.len() != new.len() {
        rendered.receipts.comments.clear();
    } else {
        let (mut before, mut after) = (0, 0);
        let mut lines = Vec::with_capacity(old.len());
        let mut valid = true;
        for (a, b) in old.into_iter().zip(new) {
            let a_indent = a.iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
            let b_indent = b.iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
            if a[a_indent..] == b[b_indent..] {
                lines.push((before, after, a_indent, b_indent));
            } else {
                valid = false;
                break;
            }
            before += a.len();
            after += b.len();
        }
        if !valid {
            rendered.receipts.comments.clear();
        } else {
            for receipt in &mut rendered.receipts.comments {
                let i = lines
                    .partition_point(|(at, _, _, _)| *at <= receipt.offset)
                    .saturating_sub(1);
                let (a, b, old_indent, new_indent) = lines[i];
                receipt.offset = b + new_indent + receipt.offset.saturating_sub(a + old_indent);
            }
        }
    }
    rendered.bytes = bytes;
    rendered
}

/// Join comment fragments and their occurrence spans with precisely the old
/// strings.TrimSpace byte behavior; provenance is adjusted, never text-matched.
pub(crate) fn join_receipted(
    parts: &[(&str, &[super::super::yaml3::ReceiptSpan])],
) -> (String, Vec<super::super::yaml3::ReceiptSpan>) {
    let mut text = String::new();
    let mut spans = Vec::new();
    for (i, (part, receipts)) in parts.iter().enumerate() {
        if i > 0 {
            text.push('\n');
        }
        let base = text.len();
        spans.extend(receipts.iter().cloned().map(|mut span| {
            span.at += base;
            span
        }));
        text.push_str(part);
    }
    let start = text.len() - text.trim_start().len();
    let end = text.trim_end().len();
    spans.retain(|span| span.at >= start && span.at < end);
    for span in &mut spans {
        span.at -= start;
    }
    (text.trim().to_owned(), spans)
}

#[cfg(feature = "parity")]
pub(crate) fn owner_protection_enabled() -> bool {
    super::parity::ablation::enabled(super::parity::ablation::Protection::DeferredCommentOwners)
}
#[cfg(all(test, not(feature = "parity")))]
thread_local! { static TEST_OWNER_DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(all(test, not(feature = "parity")))]
pub(crate) fn owner_protection_enabled() -> bool {
    !TEST_OWNER_DISABLED.get()
}
#[cfg(not(any(feature = "parity", test)))]
pub(crate) fn owner_protection_enabled() -> bool {
    true
}

/// The same single diagnostic switch is available to standalone core tests.
/// Only a boolean is thread-local; occurrence provenance is always call-scoped.
#[cfg(test)]
pub(crate) fn without_owner_protection<T>(call: impl FnOnce() -> T) -> T {
    #[cfg(feature = "parity")]
    {
        super::parity::ablation::with_disabled(
            &[super::parity::ablation::Protection::DeferredCommentOwners],
            call,
        )
    }
    #[cfg(not(feature = "parity"))]
    {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                TEST_OWNER_DISABLED.set(self.0);
            }
        }
        let _restore = Restore(TEST_OWNER_DISABLED.replace(true));
        call()
    }
}

pub(crate) fn source_yaml(data: &[u8], node: &Node) -> ReceiptedYaml {
    use super::super::yaml3::{EmissionReceipts, NodeOwnerEmission, ReceiptEmission};
    fn record(
        node: &Node,
        positions: &super::super::yaml3::parser::CommentNode,
        path: &mut Vec<usize>,
        receipts: &mut EmissionReceipts,
    ) {
        if node.kind != positions.kind || node.content.len() != positions.content.len() {
            return;
        }
        if let Some(owner) = &node.comment_receipts.owner {
            receipts.owners.push(NodeOwnerEmission {
                path: path.clone(),
                owner: owner.clone(),
            });
        }
        for (text, spans, positions) in [
            (
                &node.head_comment,
                &node.comment_receipts.head,
                &positions.positions.head,
            ),
            (
                &node.line_comment,
                &node.comment_receipts.line,
                &positions.positions.line,
            ),
            (
                &node.foot_comment,
                &node.comment_receipts.foot,
                &positions.positions.foot,
            ),
        ] {
            if spans.is_empty() {
                continue;
            }
            let mut by_at = HashMap::new();
            if positions.len() > 1 {
                for position in positions {
                    by_at.entry(position.at).or_insert(position);
                }
            }
            for span in spans {
                let position = if positions.len() == 1 {
                    positions.first().filter(|p| p.at == span.at)
                } else {
                    by_at.get(&span.at).copied()
                };
                if let Some(position) = position {
                    let length = text.as_bytes()[span.at..]
                        .iter()
                        .position(|b| *b == b'\n')
                        .unwrap_or(text.len() - span.at);
                    receipts.comments.push(ReceiptEmission {
                        offset: position.offset,
                        length,
                        receipt: span.receipt.clone(),
                    });
                }
            }
        }
        for (i, (child, position)) in node.content.iter().zip(&positions.content).enumerate() {
            path.push(i);
            record(child, position, path, receipts);
            path.pop();
        }
    }
    let mut receipts = EmissionReceipts::default();
    if node.comment_receipts.owner.is_none() {
        return ReceiptedYaml {
            bytes: data.to_vec(),
            receipts,
        };
    }
    if let Ok(positions) = super::super::yaml3::parser::comment_positions(data) {
        record(node, &positions, &mut Vec::new(), &mut receipts);
    }
    ReceiptedYaml {
        bytes: data.to_vec(),
        receipts,
    }
}

/// An empty source inventory needs only bytes; keep the tracked emitter for
/// sources whose original occurrences still need to be checked.
pub(crate) fn marshal_config_for_source(
    node: &Node,
    source: &CommentSource,
) -> Result<ReceiptedYaml, YamlError> {
    if source.comments.is_empty() {
        return Ok(ReceiptedYaml {
            bytes: marshal_config(node)?,
            receipts: super::super::yaml3::EmissionReceipts::default(),
        });
    }
    marshal_config_receipted(node)
}

/// Quote Unicode once before the single authoritative tracked emission.
pub(crate) fn marshal_config_receipted(node: &Node) -> Result<ReceiptedYaml, YamlError> {
    let mut node = node.clone();
    super::encode::quote_config_unicode(&mut node);
    let emitted = marshal_receipted(&node, CONFIG_INDENT)?;
    Ok(source_flow_item_heads_receipted(&node, emitted))
}

struct ExpectedComment<'a> {
    receipt: &'a super::super::yaml3::CommentReceipt,
    emitted: bool,
    count: usize,
    valid: bool,
}

/// Fresh source annotations and deduplicated alias copies have dense indices.
/// Subsets and stale hints use an identity map; a hint can never prove ownership.
struct ExpectedComments<'a> {
    entries: Vec<ExpectedComment<'a>>,
    lookup: Option<HashMap<&'a super::super::yaml3::CommentId, usize>>,
}

impl<'a> ExpectedComments<'a> {
    fn build_lookup(&mut self) {
        self.lookup = Some(
            self.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (&entry.receipt.id, index))
                .collect(),
        );
    }

    fn len(&self) -> usize {
        self.lookup
            .as_ref()
            .map_or(self.entries.len(), HashMap::len)
    }

    fn values(&self) -> impl Iterator<Item = &ExpectedComment<'a>> {
        self.entries.iter()
    }

    fn index_for(&mut self, id: &super::super::yaml3::CommentId) -> Option<usize> {
        if self.lookup.is_none() {
            let index = id.comment_index();
            if self
                .entries
                .get(index)
                .is_some_and(|entry| entry.receipt.id == *id)
            {
                return Some(index);
            }
            self.build_lookup();
        }
        self.lookup
            .as_ref()
            .and_then(|lookup| lookup.get(id).copied())
    }

    fn get_mut(&mut self, id: &super::super::yaml3::CommentId) -> Option<&mut ExpectedComment<'a>> {
        let index = self.index_for(id)?;
        self.entries.get_mut(index)
    }

    fn contains_key(&mut self, id: &super::super::yaml3::CommentId) -> bool {
        self.index_for(id).is_some()
    }
}

fn expected_comments(source: &CommentSource) -> ExpectedComments<'_> {
    let mut found = ExpectedComments {
        entries: source
            .comments
            .iter()
            .map(|comment| ExpectedComment {
                receipt: &comment.receipt,
                emitted: false,
                count: 0,
                valid: true,
            })
            .collect(),
        lookup: None,
    };
    if !source.indexed_comments
        || source
            .comments
            .iter()
            .enumerate()
            .any(|(index, comment)| comment.receipt.id.comment_index() != index)
    {
        found.build_lookup();
    }
    found
}

impl CommentSource {
    pub(crate) fn key_feet_need_restoring(&self, candidate: &ReceiptedYaml) -> bool {
        let required = Self {
            complete: self.complete,
            comments: self
                .comments
                .iter()
                .filter(|comment| comment.key_foot)
                .cloned()
                .collect(),
            ..Default::default()
        };
        !required.comments.is_empty()
            && !read_receipted(candidate).is_ok_and(|node| required.holds(&node))
    }

    pub(crate) fn holds(&self, node: &Node) -> bool {
        use super::super::yaml3::{CommentId, CommentSlot};
        if !self.complete {
            return false;
        }
        let mut owners: HashMap<CommentId, usize> = HashMap::new();
        let mut found = expected_comments(self);
        if found.len() != self.comments.len() {
            return false;
        }
        fn visit(
            n: &Node,
            owners: &mut HashMap<CommentId, usize>,
            found: &mut ExpectedComments<'_>,
        ) {
            comment_work(1);
            if let Some(id) = &n.comment_receipts.owner {
                *owners.entry(id.clone()).or_default() += 1;
            }
            for (slot, text, spans) in [
                (CommentSlot::Head, &n.head_comment, &n.comment_receipts.head),
                (CommentSlot::Line, &n.line_comment, &n.comment_receipts.line),
                (CommentSlot::Foot, &n.foot_comment, &n.comment_receipts.foot),
            ] {
                let (mut at, mut part) = (0, 0);
                for span in spans {
                    let valid = span.at >= at && span.at <= text.len();
                    if valid {
                        part += text.as_bytes()[at..span.at]
                            .iter()
                            .filter(|b| **b == b'\n')
                            .count();
                        at = span.at;
                    }
                    if let Some(entry) = found.get_mut(&span.receipt.id) {
                        entry.count += 1;
                        entry.valid &= valid
                            && n.comment_receipts.owner.as_ref() == Some(&entry.receipt.owner)
                            && slot == entry.receipt.slot
                            && part == entry.receipt.ordinal
                            && span.receipt.owner == entry.receipt.owner
                            && span.receipt.slot == entry.receipt.slot
                            && span.receipt.ordinal == entry.receipt.ordinal;
                    }
                }
            }
            for c in &n.content {
                visit(c, owners, found);
            }
        }
        visit(node, &mut owners, &mut found);
        found.values().all(|entry| {
            entry.count == 1 && entry.valid && owners.get(&entry.receipt.owner) == Some(&1)
        })
    }
}

/// A batch is validated before any byte or emission is changed. Offsets are
/// transported once while rebuilding the output, rather than splicing per comment.
struct ReceiptEdit {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
    added: Vec<(usize, super::super::yaml3::CommentReceipt, usize)>,
}
fn apply_receipt_edits(
    candidate: &ReceiptedYaml,
    mut edits: Vec<ReceiptEdit>,
    removed: &HashSet<super::super::yaml3::CommentId>,
) -> Option<ReceiptedYaml> {
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|e| (e.start, e.end));
    let mut previous = 0;
    for e in &edits {
        if e.start < previous || e.start > e.end || e.end > candidate.bytes.len() {
            return None;
        }
        previous = e.end;
    }
    let retained: Vec<_> = candidate
        .receipts
        .comments
        .iter()
        .filter(|e| !removed.contains(&e.receipt.id))
        .collect();
    for emission in &retained {
        let at = edits.partition_point(|e| e.end <= emission.offset);
        if edits
            .get(at)
            .is_some_and(|e| e.start < emission.offset + emission.length && e.end > emission.offset)
        {
            return None;
        }
    }
    let mut output = candidate.clone();
    output.bytes.clear();
    output.receipts.comments.clear();
    let mut cursor = 0;
    let mut shifts = Vec::with_capacity(edits.len());
    let mut delta = 0isize;
    for e in edits {
        output
            .bytes
            .extend_from_slice(&candidate.bytes[cursor..e.start]);
        let base = output.bytes.len();
        output.bytes.extend_from_slice(&e.bytes);
        for (relative, receipt, length) in e.added {
            output
                .receipts
                .comments
                .push(super::super::yaml3::ReceiptEmission {
                    offset: base + relative,
                    length,
                    receipt,
                });
        }
        delta += e.bytes.len() as isize - (e.end - e.start) as isize;
        shifts.push((e.end, delta));
        cursor = e.end;
    }
    output.bytes.extend_from_slice(&candidate.bytes[cursor..]);
    for emission in retained {
        let mut emission = emission.clone();
        let at = shifts.partition_point(|(end, _)| *end <= emission.offset);
        if at > 0 {
            emission.offset = emission.offset.saturating_add_signed(shifts[at - 1].1);
        }
        output.receipts.comments.push(emission);
    }
    output.receipts.comments.sort_by_key(|e| e.offset);
    Some(output)
}

/// A separate key line can carry a mapping key's own line comment without a
/// collection tag. The ordinary complete gate still checks the whole result.
fn repair_key_lines(source: &CommentSource, candidate: &mut ReceiptedYaml) {
    use super::super::yaml3::{CommentId, CommentSlot, Style};
    fn at<'a>(n: &'a Node, path: &[usize]) -> Option<&'a Node> {
        path.iter().try_fold(n, |n, i| n.content.get(*i))
    }
    fn position<'a>(
        n: &'a super::super::yaml3::parser::CommentNode,
        path: &[usize],
    ) -> Option<&'a super::super::yaml3::parser::CommentNode> {
        path.iter().try_fold(n, |n, i| n.content.get(*i))
    }
    fn held(n: &Node, ids: &mut HashSet<CommentId>) {
        for (slot, text, spans) in [
            (CommentSlot::Head, &n.head_comment, &n.comment_receipts.head),
            (CommentSlot::Line, &n.line_comment, &n.comment_receipts.line),
            (CommentSlot::Foot, &n.foot_comment, &n.comment_receipts.foot),
        ] {
            let (mut at, mut part) = (0, 0);
            for span in spans {
                if span.at < at || span.at > text.len() {
                    continue;
                }
                part += text.as_bytes()[at..span.at]
                    .iter()
                    .filter(|b| **b == b'\n')
                    .count();
                at = span.at;
                if n.comment_receipts.owner.as_ref() == Some(&span.receipt.owner)
                    && slot == span.receipt.slot
                    && part == span.receipt.ordinal
                {
                    ids.insert(span.receipt.id.clone());
                }
            }
        }
        for c in &n.content {
            held(c, ids);
        }
    }
    let Ok(parsed) = read_receipted(candidate) else {
        return;
    };
    let Ok(positions) = super::super::yaml3::parser::comment_positions(&candidate.bytes) else {
        return;
    };
    let mut held_ids = HashSet::new();
    held(&parsed, &mut held_ids);
    let owners: HashMap<_, _> = candidate
        .receipts
        .owners
        .iter()
        .map(|o| (&o.owner, o.path.as_slice()))
        .collect();
    let emissions = first_emissions(candidate);
    let mut counts = HashMap::new();
    for c in &source.comments {
        if c.receipt.slot == CommentSlot::Line {
            *counts.entry(&c.receipt.owner).or_insert(0usize) += 1;
        }
    }
    let mut removed = HashSet::new();
    let mut edits = Vec::new();
    for comment in &source.comments {
        let receipt = &comment.receipt;
        if receipt.slot != CommentSlot::Line
            || held_ids.contains(&receipt.id)
            || counts.get(&receipt.owner) != Some(&1)
        {
            continue;
        }
        let Some(path) = owners.get(&receipt.owner) else {
            continue;
        };
        let Some((&index, parent_path)) = path.split_last() else {
            continue;
        };
        if index % 2 != 0
            || at(&parsed, parent_path)
                .is_none_or(|n| n.kind != Kind::Mapping || n.style.has(Style::FLOW))
        {
            continue;
        }
        if at(&parsed, path).is_none_or(|n| n.kind != Kind::Scalar) {
            continue;
        }
        let mut value_path = path.to_vec();
        *value_path.last_mut().unwrap() += 1;
        let (Some(key_span), Some(value_span), Some(write)) = (
            position(&positions, path),
            position(&positions, &value_path),
            emissions.get(&receipt.id),
        ) else {
            continue;
        };
        if key_span.end > value_span.start {
            continue;
        }
        let delimiter = &candidate.bytes[key_span.end..value_span.start];
        if delimiter.iter().filter(|b| **b == b':').count() != 1
            || delimiter.iter().any(|b| !matches!(b, b':' | b' ' | b'\n'))
        {
            continue;
        }
        let start = candidate.bytes[..key_span.start]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        let prefix = &candidate.bytes[start..key_span.start];
        let indent = prefix.iter().take_while(|b| **b == b' ').count();
        if indent > 256 {
            continue;
        }
        let mut markers = &prefix[indent..];
        let mut items = 0;
        while let Some(rest) = markers.strip_prefix(b"- ") {
            items += 1;
            markers = rest;
        }
        let explicit = markers == b"? ";
        let item = items > 0 && markers.is_empty();
        if !explicit && !markers.is_empty() {
            continue;
        }
        let mut header = Vec::new();
        let comment_offset;
        if item {
            if !delimiter.starts_with(b":") || delimiter.contains(&b'\n') {
                continue;
            }
            header.extend_from_slice(b": ");
            comment_offset = header.len();
            header.extend_from_slice(&comment.bytes);
            header.push(b'\n');
            header.resize(header.len() + prefix.len() + 2, b' ');
        } else {
            header.push(b' ');
            comment_offset = header.len();
            header.extend_from_slice(&comment.bytes);
            if explicit {
                header.extend_from_slice(delimiter);
            } else {
                if !delimiter.starts_with(b":") {
                    continue;
                }
                header.push(b'\n');
                header.resize(header.len() + indent, b' ');
                header.push(b':');
                header.extend_from_slice(&delimiter[1..]);
                edits.push(ReceiptEdit {
                    start: key_span.start,
                    end: key_span.start,
                    bytes: b"? ".to_vec(),
                    added: Vec::new(),
                });
            }
        }
        edits.push(ReceiptEdit {
            start: key_span.end,
            end: value_span.start,
            bytes: header,
            added: vec![(comment_offset, receipt.clone(), comment.bytes.len())],
        });
        let (comment_start, comment_end) =
            comment_removal_span(&candidate.bytes, write.offset, write.length);
        edits.push(ReceiptEdit {
            start: comment_start,
            end: comment_end,
            bytes: Vec::new(),
            added: Vec::new(),
        });
        removed.insert(receipt.id.clone());
    }
    let Some(proposal) = apply_receipt_edits(candidate, edits, &removed) else {
        return;
    };
    let Ok(reparsed) = read_receipted(&proposal) else {
        return;
    };
    let required = CommentSource {
        complete: source.complete,
        comments: source
            .comments
            .iter()
            .filter(|c| held_ids.contains(&c.receipt.id) || removed.contains(&c.receipt.id))
            .cloned()
            .collect(),
        ..Default::default()
    };
    if same_node_values(&parsed, &reparsed) && required.holds(&reparsed) {
        *candidate = proposal;
    }
}

fn comment_removal_span(bytes: &[u8], offset: usize, length: usize) -> (usize, usize) {
    let mut start = offset;
    while start > 0 && matches!(bytes[start - 1], b' ' | b'\t') {
        start -= 1;
    }
    let mut end = offset + length;
    if start == 0 || bytes[start - 1] == b'\n' {
        if bytes.get(end..end + 2) == Some(b"\r\n") {
            end += 2;
        } else if bytes.get(end) == Some(&b'\n') {
            end += 1;
        }
    }
    (start, end)
}

pub(crate) fn physical_comment_counts(bytes: &[u8]) -> Option<HashMap<&[u8], usize>> {
    let mut counts = HashMap::new();
    for (offset, length) in super::super::yaml3::parser::physical_comment_lines(bytes)? {
        *counts.entry(&bytes[offset..offset + length]).or_insert(0) += 1;
    }
    Some(counts)
}

fn same_node_values(a: &Node, b: &Node) -> bool {
    a.kind == b.kind
        && a.style == b.style
        && a.short_tag() == b.short_tag()
        && a.value == b.value
        && a.content.len() == b.content.len()
        && a.content
            .iter()
            .zip(&b.content)
            .all(|(a, b)| same_node_values(a, b))
}

/// Build a conservative indentation proposal using each source occurrence's
/// relative placement. Incomplete or unrepresentable proposals are discarded.
pub(crate) fn repair_comment_candidate(
    source: &CommentSource,
    mut candidate: ReceiptedYaml,
) -> ReceiptedYaml {
    use super::super::yaml3::CommentSlot;
    let Ok(parsed) = read_receipted(&candidate) else {
        return candidate;
    };
    let faithful = source.holds(&parsed);
    if !faithful {
        repair_key_lines(source, &mut candidate);
        if read_receipted(&candidate).is_ok_and(|n| source.holds(&n)) {
            return remove_writer_comment_copies(source, candidate);
        }
        repair_scalar_headers(source, &mut candidate);
        repair_deferred_lines(source, &mut candidate);
    }
    let Ok(parsed) = read_receipted(&candidate) else {
        return candidate;
    };
    fn index<'a>(
        node: &'a Node,
        owners: &mut HashMap<&'a super::super::yaml3::CommentId, &'a Node>,
        held: &mut HashSet<&'a super::super::yaml3::CommentId>,
    ) {
        if let Some(id) = &node.comment_receipts.owner {
            owners.insert(id, node);
        }
        for (slot, spans) in [
            (CommentSlot::Head, &node.comment_receipts.head),
            (CommentSlot::Line, &node.comment_receipts.line),
            (CommentSlot::Foot, &node.comment_receipts.foot),
        ] {
            for span in spans {
                if slot == span.receipt.slot
                    && node.comment_receipts.owner.as_ref() == Some(&span.receipt.owner)
                {
                    held.insert(&span.receipt.id);
                }
            }
        }
        for c in &node.content {
            index(c, owners, held);
        }
    }
    let mut owners = HashMap::new();
    let mut held = HashSet::new();
    index(&parsed, &mut owners, &mut held);
    let emissions = first_emissions(&candidate);
    let mut edits = Vec::new();
    for comment in &source.comments {
        if !faithful && held.contains(&comment.receipt.id) {
            continue;
        }
        let Some(node) = owners.get(&comment.receipt.owner) else {
            continue;
        };
        let Some(emission) = emissions.get(&comment.receipt.id) else {
            continue;
        };
        let start = candidate.bytes[..emission.offset]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |p| p + 1);
        if candidate.bytes[start..emission.offset]
            .iter()
            .any(|b| *b != b' ')
        {
            continue;
        }
        let relative = comment.column as isize - comment.owner_column as isize;
        let indent = node
            .column
            .saturating_sub(1)
            .saturating_add_signed(relative);
        if indent <= 256 && indent != emission.offset - start {
            edits.push((start, emission.offset, vec![b' '; indent]));
        }
    }
    let edits = edits
        .into_iter()
        .map(|(start, end, bytes)| ReceiptEdit {
            start,
            end,
            bytes,
            added: Vec::new(),
        })
        .collect();
    let candidate = apply_receipt_edits(&candidate, edits, &HashSet::new()).unwrap_or(candidate);
    remove_writer_comment_copies(source, candidate)
}

fn comment_multiset_matches(source: &CommentSource, bytes: &[u8]) -> bool {
    let mut expected = HashMap::new();
    for comment in &source.comments {
        *expected.entry(comment.bytes.as_ref()).or_insert(0usize) += 1;
    }
    // Parsed comments may omit stream-level text or later documents. A repair
    // must also account for every physical comment in the original file.
    for original in &source.originals {
        let Some(lines) = super::super::yaml3::parser::physical_comment_lines(&original.bytes)
        else {
            return false;
        };
        if lines.len() != original.comments.len() {
            return false;
        }
        let mut available = HashMap::new();
        for comment in original.comments.iter() {
            *available.entry(comment.as_ref()).or_insert(0usize) += 1;
        }
        for (offset, length) in lines {
            let Some(count) = available.get_mut(&original.bytes[offset..offset + length]) else {
                return false;
            };
            let Some(left) = count.checked_sub(1) else {
                return false;
            };
            *count = left;
        }
    }
    let Some(lines) = super::super::yaml3::parser::physical_comment_lines(bytes) else {
        return false;
    };
    for (offset, length) in lines {
        let Some(count) = expected.get_mut(&bytes[offset..offset + length]) else {
            return false;
        };
        let Some(left) = count.checked_sub(1) else {
            return false;
        };
        *count = left;
    }
    expected.values().all(|count| *count == 0)
}

/// Only a scanner-confirmed, unreceipted writer copy can be removed. The final
/// gate still requires the complete source multiset and every original owner.
fn remove_writer_comment_copies(source: &CommentSource, candidate: ReceiptedYaml) -> ReceiptedYaml {
    let Some(lines) = super::super::yaml3::parser::physical_comment_lines(&candidate.bytes) else {
        return candidate;
    };
    let mut expected = HashMap::new();
    for comment in &source.comments {
        *expected.entry(comment.bytes.as_ref()).or_insert(0usize) += 1;
    }
    let mut actual = HashMap::new();
    for &(offset, length) in &lines {
        *actual
            .entry(&candidate.bytes[offset..offset + length])
            .or_insert(0usize) += 1;
    }
    let emitted: HashSet<_> = candidate
        .receipts
        .comments
        .iter()
        .filter(|e| e.length > 0)
        .map(|e| e.offset)
        .collect();
    let mut edits = Vec::new();
    for (offset, length) in lines {
        let text = &candidate.bytes[offset..offset + length];
        let count = actual.get_mut(text).unwrap();
        if *count > expected.get(text).copied().unwrap_or(0) && !emitted.contains(&offset) {
            let mut start = offset;
            while start > 0 && matches!(candidate.bytes[start - 1], b' ' | b'\t') {
                start -= 1;
            }
            edits.push(ReceiptEdit {
                start,
                end: offset + length,
                bytes: Vec::new(),
                added: Vec::new(),
            });
            *count -= 1;
        }
    }
    if actual != expected {
        return candidate;
    }
    apply_receipt_edits(&candidate, edits, &HashSet::new()).unwrap_or(candidate)
}

/// Match preorder owner paths directly, sharing the preflight indexes with
/// the physical-position check. No second path or found-occurrence map is built.
fn receipts_hold(
    candidate: &ReceiptedYaml,
    owners: &HashMap<&super::super::yaml3::CommentId, usize>,
    found: &mut ExpectedComments<'_>,
) -> bool {
    use super::super::yaml3::{CommentSlot, NodeOwnerEmission, ReceiptEmission};
    if !candidate
        .receipts
        .owners
        .windows(2)
        .all(|pair| pair[0].path < pair[1].path)
    {
        return false;
    }
    let Ok(positions) = super::super::yaml3::parser::comment_positions(&candidate.bytes) else {
        return false;
    };
    let mut emissions: HashMap<usize, &ReceiptEmission> =
        HashMap::with_capacity(candidate.receipts.comments.len());
    for e in &candidate.receipts.comments {
        if emissions.insert(e.offset, e).is_some() {
            return false;
        }
    }
    fn visit(
        p: &super::super::yaml3::parser::CommentNode,
        path: &mut Vec<usize>,
        records: &[NodeOwnerEmission],
        next_owner: &mut usize,
        emissions: &HashMap<usize, &ReceiptEmission>,
        bytes: &[u8],
        found: &mut ExpectedComments<'_>,
    ) -> bool {
        comment_work(1);
        let next = records.get(*next_owner);
        if next.is_some_and(|record| record.path.as_slice() < path.as_slice()) {
            return false;
        }
        let owner = next.filter(|record| record.path == *path).map(|record| {
            *next_owner += 1;
            &record.owner
        });
        for (slot, positions) in [
            (CommentSlot::Head, &p.positions.head),
            (CommentSlot::Line, &p.positions.line),
            (CommentSlot::Foot, &p.positions.foot),
        ] {
            let mut text_bytes = 0;
            for pos in positions {
                let line = bytes
                    .get(pos.offset..)
                    .and_then(|tail| tail.split(|b| matches!(b, b'\n' | b'\r')).next())
                    .unwrap_or_default();
                let part = pos.at.saturating_sub(text_bytes);
                text_bytes += line.len();
                if let Some(e) = emissions.get(&pos.offset)
                    && e.length > 0
                    && let Some(entry) = found.get_mut(&e.receipt.id)
                {
                    entry.count += 1;
                    entry.valid &= owner == Some(&entry.receipt.owner)
                        && slot == entry.receipt.slot
                        && part == entry.receipt.ordinal
                        && line.len() == e.length
                        && e.receipt.owner == entry.receipt.owner
                        && e.receipt.slot == entry.receipt.slot
                        && e.receipt.ordinal == entry.receipt.ordinal;
                }
            }
        }
        for (i, c) in p.content.iter().enumerate() {
            path.push(i);
            if !visit(c, path, records, next_owner, emissions, bytes, found) {
                return false;
            }
            path.pop();
        }
        true
    }
    let mut next_owner = 0;
    visit(
        &positions,
        &mut Vec::new(),
        &candidate.receipts.owners,
        &mut next_owner,
        &emissions,
        &candidate.bytes,
        found,
    ) && next_owner == candidate.receipts.owners.len()
        && found.values().all(|entry| {
            entry.count == 1 && entry.valid && owners.get(&entry.receipt.owner) == Some(&1)
        })
}

pub(crate) fn finish_comment_owners(
    source: &CommentSource,
    candidate: ReceiptedYaml,
    repeat: impl FnOnce(&[u8]) -> Option<Vec<u8>>,
) -> Vec<u8> {
    if !owner_protection_enabled() || !source.complete || source.comments.is_empty() {
        return candidate.bytes;
    }
    let faithful = {
        let mut found = expected_comments(source);
        if found.len() != source.comments.len()
            || candidate
                .receipts
                .overwritten
                .iter()
                .any(|span| !found.contains_key(&span.receipt.id))
        {
            return candidate.bytes;
        }
        let mut owners = HashMap::with_capacity(candidate.receipts.owners.len());
        for record in &candidate.receipts.owners {
            *owners.entry(&record.owner).or_insert(0) += 1;
        }
        if source
            .comments
            .iter()
            .any(|comment| !owners.contains_key(&comment.receipt.owner))
        {
            return candidate.bytes;
        }
        for emission in &candidate.receipts.comments {
            if emission.length > 0
                && let Some(comment) = found.get_mut(&emission.receipt.id)
            {
                comment.emitted = true;
            }
        }
        if found.values().any(|comment| !comment.emitted) {
            return candidate.bytes;
        }
        receipts_hold(&candidate, &owners, &mut found)
    };
    if faithful {
        return candidate.bytes;
    }
    let Ok(original) = read_receipted(&candidate) else {
        return candidate.bytes;
    };
    // This exact complete-candidate gate is essential: even a harmless owner-
    // preserving indentation proposal must not change faithful output bytes.
    if source.holds(&original) {
        return candidate.bytes;
    }
    let original_bytes = candidate.bytes.clone();
    let proposed = repair_comment_candidate(source, candidate);
    let Ok(parsed) = read_receipted(&proposed) else {
        return original_bytes;
    };
    let emissions = first_emissions(&proposed);
    if !source.holds(&parsed)
        || !comment_multiset_matches(source, &proposed.bytes)
        || proposed.bytes.split(|b| *b == b'\n').any(|line| {
            line.strip_suffix(b"\r")
                .unwrap_or(line)
                .last()
                .is_some_and(|b| matches!(b, b' ' | b'\t'))
        })
        || !source.comments.iter().all(|comment| {
            emissions
                .get(&comment.receipt.id)
                .copied()
                .is_some_and(|e| {
                    proposed.bytes.get(e.offset..e.offset + e.length)
                        == Some(comment.bytes.as_ref())
                })
        })
        || !same_node_values(&original, &parsed)
        || super::super::Config::load_bytes(&proposed.bytes).is_err()
        || !same_loader_values(&original_bytes, &proposed.bytes)
    {
        return original_bytes;
    }
    if repeat(&proposed.bytes).as_ref() != Some(&proposed.bytes) {
        return original_bytes;
    }
    proposed.bytes
}

fn same_loader_values(a: &[u8], b: &[u8]) -> bool {
    fn same(a: &super::loader_yaml::Node, b: &super::loader_yaml::Node) -> bool {
        a.kind == b.kind
            && a.tag == b.tag
            && a.value == b.value
            && a.content.len() == b.content.len()
            && a.content.iter().zip(&b.content).all(|(a, b)| same(a, b))
    }
    match (super::loader_root(a), super::loader_root(b)) {
        (Ok(Some(a)), Ok(Some(b))) => same(&a, &b),
        (Ok(None), Ok(None)) => true,
        _ => false,
    }
}

/// An additional history guard may only reject a repair. Match the final
/// preview to its actual physical emissions before checking its source owners.
pub(crate) fn output_holds_source(
    source: &CommentSource,
    candidate: ReceiptedYaml,
    output: &[u8],
) -> bool {
    let rendered = if candidate.bytes == output {
        candidate
    } else {
        repair_comment_candidate(source, candidate)
    };
    rendered.bytes == output
        && read_receipted(&rendered).is_ok_and(|node| source.holds(&node))
        && comment_multiset_matches(source, output)
}

/// Recover the two physical header lines whose deferred comments originally
/// belonged to the first block scalar of a mapping's sequence. The key header
/// and scalar header are separate YAML positions; quoting both onto one header
/// cannot retain their common parsed line owner.
pub(super) fn repair_scalar_headers(source: &CommentSource, candidate: &mut ReceiptedYaml) {
    use super::super::yaml3::{CommentReceipts, CommentSlot, ReceiptSpan};
    fn at<'a>(node: &'a Node, path: &[usize]) -> Option<&'a Node> {
        path.iter().try_fold(node, |n, i| n.content.get(*i))
    }
    fn position<'a>(
        node: &'a super::super::yaml3::parser::CommentNode,
        path: &[usize],
    ) -> Option<&'a super::super::yaml3::parser::CommentNode> {
        path.iter().try_fold(node, |n, i| n.content.get(*i))
    }
    let mut groups: HashMap<_, Vec<_>> = HashMap::new();
    for comment in &source.comments {
        if comment.receipt.slot == CommentSlot::Line {
            groups
                .entry(&comment.receipt.owner)
                .or_default()
                .push(comment);
        }
    }
    let mut groups: Vec<_> = groups
        .into_values()
        .filter(|g| {
            g.len() == 2
                && g[0].source_line < g[0].owner_line
                && g[1].source_line == g[1].owner_line
        })
        .collect();
    if groups.is_empty() {
        return;
    }
    groups.sort_by_key(|g| g[0].source_line);
    let Ok(parsed) = read_receipted(candidate) else {
        return;
    };
    let Ok(positions) = super::super::yaml3::parser::comment_positions(&candidate.bytes) else {
        return;
    };
    let owners: HashMap<_, _> = candidate
        .receipts
        .owners
        .iter()
        .map(|o| (&o.owner, o.path.as_slice()))
        .collect();
    let emissions = first_emissions(candidate);
    let mut ordered: Vec<_> = candidate.receipts.comments.iter().collect();
    ordered.sort_by_key(|e| e.offset);
    let mut edits = Vec::new();
    let mut removed = HashSet::new();
    for group in groups {
        let id = &group[0].receipt.owner;
        let Some(path) = owners.get(id) else {
            continue;
        };
        if path.len() < 3 || path.last() != Some(&0) {
            continue;
        }
        let Some(scalar) = at(&parsed, path)
            .filter(|n| n.kind == Kind::Scalar && n.short_tag() == STR_TAG && !n.value.is_empty())
        else {
            continue;
        };
        let parent_path = &path[..path.len() - 1];
        if at(&parsed, parent_path).is_none_or(|n| n.kind != Kind::Sequence) {
            continue;
        }
        let Some(index) = parent_path.last().copied().filter(|i| i % 2 == 1) else {
            continue;
        };
        let mut key_path = parent_path[..parent_path.len() - 1].to_vec();
        key_path.push(index - 1);
        if at(&parsed, &key_path).is_none_or(|n| !n.line_comment.is_empty()) {
            continue;
        }
        let (Some(span), Some(key)) = (position(&positions, path), position(&positions, &key_path))
        else {
            continue;
        };
        let (Some(first), Some(second)) = (
            emissions.get(&group[0].receipt.id),
            emissions.get(&group[1].receipt.id),
        ) else {
            continue;
        };
        let mut writes = [*first, *second];
        writes.sort_by_key(|e| e.offset);
        let end = writes[1].offset + writes[1].length;
        if span.end > writes[0].offset || end > candidate.bytes.len() {
            continue;
        }
        let first = ordered.partition_point(|e| e.offset < span.start);
        let last = ordered.partition_point(|e| e.offset < end);
        if ordered[first..last]
            .iter()
            .any(|e| !group.iter().any(|c| c.receipt.id == e.receipt.id))
        {
            continue;
        }
        if candidate.bytes[span.end..writes[0].offset]
            .iter()
            .chain(&candidate.bytes[writes[0].offset + writes[0].length..writes[1].offset])
            .any(|b| !matches!(b, b' ' | b'\r' | b'\n'))
        {
            continue;
        }
        let Some(key_end) = candidate.bytes[key.end..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| key.end + i)
            .filter(|end| *end < span.start)
        else {
            continue;
        };
        let Some(header) = collection_header_edit(candidate, key, key_end, group[0]) else {
            continue;
        };
        let mut local = scalar.clone();
        local.head_comment.clear();
        local.foot_comment.clear();
        local.line_comment = String::from_utf8_lossy(&group[1].bytes).into_owned();
        local.comment_receipts = CommentReceipts {
            owner: Some(id.clone()),
            line: vec![ReceiptSpan {
                at: 0,
                receipt: group[1].receipt.clone(),
            }],
            ..Default::default()
        }
        .into();
        let Ok((bytes, receipts)) = super::encode::faithful_node_receipted(&local, 2) else {
            continue;
        };
        if receipts.comments.len() != 1 {
            continue;
        }
        let shift = scalar.column.saturating_sub(3);
        let mut replacement = Vec::new();
        for (i, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            if i > 0 {
                replacement.resize(replacement.len() + shift, b' ');
            }
            replacement.extend_from_slice(line);
        }
        if replacement.last() == Some(&b'\n') {
            replacement.pop();
        }
        let local_offset = receipts.comments[0].offset;
        let offset = local_offset
            + shift
                * bytes[..local_offset]
                    .iter()
                    .filter(|b| **b == b'\n')
                    .count();
        edits.push(header);
        edits.push(ReceiptEdit {
            start: span.start,
            end,
            bytes: replacement,
            added: vec![(offset, group[1].receipt.clone(), group[1].bytes.len())],
        });
        removed.extend(group.iter().map(|c| c.receipt.id.clone()));
    }
    if let Some(proposal) = apply_receipt_edits(candidate, edits, &removed) {
        *candidate = proposal;
    }
}

fn repair_deferred_lines(source: &CommentSource, candidate: &mut ReceiptedYaml) {
    use super::super::yaml3::CommentSlot;
    fn at<'a>(node: &'a Node, path: &[usize]) -> Option<&'a Node> {
        path.iter().try_fold(node, |n, i| n.content.get(*i))
    }
    fn position<'a>(
        node: &'a super::super::yaml3::parser::CommentNode,
        path: &[usize],
    ) -> Option<&'a super::super::yaml3::parser::CommentNode> {
        path.iter().try_fold(node, |n, i| n.content.get(*i))
    }
    if !source.comments.iter().any(|c| {
        source.repair_key_feet && c.key_foot
            || c.receipt.slot == CommentSlot::Line && c.placement.is_some()
    }) {
        return;
    }
    let Ok(parsed) = read_receipted(candidate) else {
        return;
    };
    let Ok(positions) = super::super::yaml3::parser::comment_positions(&candidate.bytes) else {
        return;
    };
    let owners: HashMap<_, _> = candidate
        .receipts
        .owners
        .iter()
        .map(|o| (&o.owner, o.path.as_slice()))
        .collect();
    let emissions = first_emissions(candidate);
    let mut edits = Vec::new();
    let mut removed = HashSet::new();
    let mut headers = HashSet::new();
    let mut feet: HashMap<_, Vec<_>> = HashMap::new();
    for comment in &source.comments {
        if comment.receipt.slot == CommentSlot::Foot {
            feet.entry(&comment.receipt.owner)
                .or_default()
                .push(comment);
        }
    }
    let mut moved_feet = HashSet::new();
    for comment in &source.comments {
        if comment.receipt.slot == CommentSlot::Foot {
            if !source.repair_key_feet || !comment.key_foot {
                continue;
            }
            if !moved_feet.insert(&comment.receipt.owner) {
                continue;
            }
            let Some(path) = owners.get(&comment.receipt.owner) else {
                continue;
            };
            let Some((&index, parent)) = path.split_last() else {
                continue;
            };
            if index % 2 != 0 || at(&parsed, parent).is_none_or(|n| n.kind != Kind::Mapping) {
                continue;
            }
            let Some(key) = at(&parsed, path) else {
                continue;
            };
            let Some(value) = at(&parsed, parent).and_then(|n| n.content.get(index + 1)) else {
                continue;
            };
            let group = &feet[&comment.receipt.owner];
            let held: HashSet<_> = key
                .comment_receipts
                .foot
                .iter()
                .map(|span| &span.receipt.id)
                .collect();
            if value.kind != Kind::Sequence
                || value.style.has(super::super::yaml3::Style::FLOW)
                || group
                    .iter()
                    .all(|comment| held.contains(&comment.receipt.id))
            {
                continue;
            }
            let mut value_path = path.to_vec();
            *value_path.last_mut().unwrap() += 1;
            let Some(span) = position(&positions, &value_path) else {
                continue;
            };
            fn data_end(node: &Node, span: &super::super::yaml3::parser::CommentNode) -> usize {
                if !node.style.has(super::super::yaml3::Style::FLOW)
                    && let (Some(child), Some(position)) =
                        (node.content.last(), span.content.last())
                {
                    return data_end(child, position);
                }
                span.end
            }
            let data_end = data_end(value, span);
            let Some(insert) = candidate.bytes[data_end..]
                .iter()
                .position(|b| *b == b'\n')
                .map(|at| data_end + at + 1)
            else {
                continue;
            };
            let indent = key.column.saturating_sub(1);
            if indent > 256 {
                continue;
            }
            let mut block = Vec::new();
            let mut added = Vec::new();
            let mut deletes = Vec::new();
            for comment in group {
                let Some(write) = emissions.get(&comment.receipt.id) else {
                    break;
                };
                let start = candidate.bytes[..write.offset]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |at| at + 1);
                if candidate.bytes[start..write.offset]
                    .iter()
                    .any(|b| *b != b' ')
                {
                    break;
                }
                block.resize(block.len() + indent, b' ');
                added.push((block.len(), comment.receipt.clone(), comment.bytes.len()));
                block.extend_from_slice(&comment.bytes);
                block.push(b'\n');
                let end = write.offset + write.length;
                deletes.push(ReceiptEdit {
                    start,
                    end: end + usize::from(candidate.bytes.get(end) == Some(&b'\n')),
                    bytes: Vec::new(),
                    added: Vec::new(),
                });
            }
            if deletes.len() == group.len() {
                let next =
                    deletes.iter().fold(
                        insert,
                        |at, edit| if edit.start == at { edit.end } else { at },
                    );
                if candidate.bytes.get(next) != Some(&b'\n') {
                    block.push(b'\n');
                }
                edits.extend(deletes);
                edits.push(ReceiptEdit {
                    start: insert,
                    end: insert,
                    bytes: block,
                    added,
                });
                removed.extend(group.iter().map(|comment| comment.receipt.id.clone()));
            }
            continue;
        }
        if comment.receipt.slot != CommentSlot::Line {
            continue;
        }
        let Some(placement) = &comment.placement else {
            continue;
        };
        let Some(owner) = owners.get(&comment.receipt.owner) else {
            continue;
        };
        if at(&parsed, owner).is_some_and(|n| {
            n.comment_receipts
                .line
                .iter()
                .any(|s| s.receipt.id == comment.receipt.id)
        }) {
            continue;
        }
        let Some(path) = owners.get(placement) else {
            continue;
        };
        if at(&parsed, path).is_none_or(|n| !matches!(n.kind, Kind::Mapping | Kind::Sequence))
            || path.last().is_none_or(|i| i % 2 == 0)
        {
            continue;
        }
        let mut key_path = path.to_vec();
        *key_path.last_mut().unwrap() -= 1;
        let Some(key) = position(&positions, &key_path) else {
            continue;
        };
        let Some(end) = candidate.bytes[key.end..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| key.end + i)
        else {
            continue;
        };
        let Some(write) = emissions.get(&comment.receipt.id) else {
            continue;
        };
        if write.offset < end || headers.contains(&end) {
            continue;
        }
        let Some(header) = collection_header_edit(candidate, key, end, comment) else {
            continue;
        };
        let (start, comment_end) =
            comment_removal_span(&candidate.bytes, write.offset, write.length);
        edits.push(header);
        edits.push(ReceiptEdit {
            start,
            end: comment_end,
            bytes: Vec::new(),
            added: Vec::new(),
        });
        removed.insert(comment.receipt.id.clone());
        headers.insert(end);
    }
    if let Some(proposal) = apply_receipt_edits(candidate, edits, &removed) {
        *candidate = proposal;
    }
}

/// A non-specific YAML tag supplies the same parser boundary as an anchor,
/// without retaining a tag or style flag in the parsed collection. Removing
/// comments and faithfully re-emitting therefore recovers the existing bytes.
fn collection_header_edit(
    candidate: &ReceiptedYaml,
    key: &super::super::yaml3::parser::CommentNode,
    end: usize,
    comment: &SourceComment,
) -> Option<ReceiptEdit> {
    if key.end > end
        || end > candidate.bytes.len()
        || candidate.bytes[key.end..end]
            .iter()
            .any(|b| !matches!(b, b':' | b' '))
    {
        return None;
    }
    let colon = key.end
        + candidate.bytes[key.end..end]
            .iter()
            .position(|b| *b == b':')?;
    Some(ReceiptEdit {
        start: colon + 1,
        end,
        bytes: [b" ! ".as_slice(), comment.bytes.as_ref()].concat(),
        added: vec![(3, comment.receipt.clone(), comment.bytes.len())],
    })
}

/// A bounded replay used only by an outer final-candidate stability check.
pub(crate) fn finish_comment_owners_replay(
    source: &CommentSource,
    candidate: ReceiptedYaml,
) -> Vec<u8> {
    finish_comment_owners(source, candidate, |bytes| Some(bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_round6_removed_collection_comment_line_leaves_no_empty_line() {
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../testdata/writer/owner_round6.json")).unwrap();
        let row = rows
            .iter()
            .find(|row| row["case"] == "blank-explicit")
            .unwrap();
        let data = row["source"].as_str().unwrap().as_bytes();
        let (mut candidate, source) = super::super::candidate_nested_scalar(
            data,
            &["observability", "pprof", "addr"],
            "localhost:7070",
        )
        .unwrap();
        let original = candidate.bytes.clone();
        // Exercise the collection-header alternative before the preferred
        // separate key line consumes this deferred comment.
        repair_deferred_lines(&source, &mut candidate);
        assert!(candidate.bytes != original);
        assert!(source.holds(&read_receipted(&candidate).unwrap()));
        assert!(same_node_values(
            &super::super::unmarshal(&original).unwrap(),
            &super::super::unmarshal(&candidate.bytes).unwrap()
        ));
        assert!(
            super::super::owner_legacy_tests::independent_canonical_owners(data)
                == super::super::owner_legacy_tests::independent_canonical_owners(&candidate.bytes)
        );
        assert!(super::super::Config::load_bytes(&candidate.bytes).is_ok());
        assert!(
            !std::str::from_utf8(&candidate.bytes)
                .unwrap()
                .lines()
                .any(|line| line.trim().is_empty()),
            "empty collection comment line remains"
        );
        let before = candidate.bytes.clone();
        repair_deferred_lines(&source, &mut candidate);
        assert!(candidate.bytes == before);
    }

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

    #[test]
    fn owner_gate_rejects_changes_to_scalar_style_or_explicit_tags() {
        for (before, after) in [
            (b"x: plain\n".as_slice(), b"x: 'plain'\n".as_slice()),
            (b"x: plain\n".as_slice(), b"x: !!str plain\n".as_slice()),
            (b"x: [plain]\n".as_slice(), b"x: !!seq [plain]\n".as_slice()),
        ] {
            let a = super::super::unmarshal(before).unwrap();
            let b = super::super::unmarshal(after).unwrap();
            assert!(
                !same_node_values(&a, &b),
                "owner gate accepted a style or explicit-tag change"
            );
        }
    }

    #[test]
    fn scalar_body_lines_are_not_comments_or_header_guesses() {
        for indicator in ["|-", ">-", "|2-", ">2-"] {
            let source = format!(
                "note: {indicator} # header\n  before\n  # Heading\n  after\n  # body tail\n# outside\nnext: 1\n  # last comment\n"
            );
            let expected = source.replace("  # last comment", "# last comment");
            assert_eq!(
                normalize_comment_indentation(source.as_bytes()),
                expected.as_bytes()
            );
        }
        let source = b"note: 'contains | and >'\n  # actual comment\nnext: 1\n";
        assert_eq!(
            normalize_comment_indentation(source),
            b"note: 'contains | and >'\n# actual comment\nnext: 1\n"
        );
        for separator in ["\r\n", "\u{85}", "\u{2028}", "\u{2029}"] {
            let source =
                format!("note: |-\n  before{separator}  body\n  # Heading\nnext: 1\n  # outside\n");
            let expected = source.replace("  # outside", "# outside");
            assert_eq!(
                normalize_comment_indentation(source.as_bytes()),
                expected.as_bytes()
            );
        }
    }
}

#[cfg(test)]
#[path = "writer_r2_tree_tests.rs"]
mod writer_r2_tree_tests;
