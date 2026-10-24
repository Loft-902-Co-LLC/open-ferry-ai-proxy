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
    expand(node, &mut budget, false, 0, &mut Vec::new())
}

fn expand(
    node: &Node,
    budget: &mut (usize, usize),
    aliased: bool,
    depth: usize,
    scopes: &mut Vec<std::sync::Arc<()>>,
) -> Result<Node, YamlError> {
    if depth > MAX_DEPTH {
        return Err(YamlError::fail(format!(
            "exceeded max depth of {MAX_DEPTH}"
        )));
    }
    if node.kind == Kind::Alias {
        return match &node.alias {
            Some(AliasTarget::Node(target)) => {
                if let Some(scope) = &node.alias_comment_scope {
                    scopes.push(scope.clone());
                }
                let result = expand(target, budget, true, depth, scopes);
                if node.alias_comment_scope.is_some() {
                    scopes.pop();
                }
                result
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
    #[cfg(test)]
    pub(crate) original_sources: Vec<Node>,
    #[cfg(test)]
    pub(crate) original_inputs: Vec<Node>,
    pub(crate) comments: Vec<SourceComment>,
    pub(crate) complete: bool,
}

#[derive(Clone)]
pub(crate) struct SourceComment {
    pub(crate) receipt: super::super::yaml3::CommentReceipt,
    pub(crate) bytes: Vec<u8>,
    pub(crate) column: usize,
    pub(crate) owner_column: usize,
    pub(crate) source_line: usize,
    pub(crate) owner_line: usize,
    pub(crate) placement: Option<super::super::yaml3::CommentId>,
}

/// A complete emission and its explicitly carried physical/structural bindings.
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
    fn identify(node: &mut Node, positions: &CommentNode, data: &[u8], source: &mut CommentSource) {
        if node.kind != positions.kind || node.content.len() != positions.content.len() {
            source.complete = false;
            return;
        }
        let owner = CommentId::fresh();
        node.comment_receipts.owner = Some(owner.clone());
        for (slot, text, positions, spans) in [
            (
                CommentSlot::Head,
                &node.head_comment,
                &positions.positions.head,
                &mut node.comment_receipts.head,
            ),
            (
                CommentSlot::Line,
                &node.line_comment,
                &positions.positions.line,
                &mut node.comment_receipts.line,
            ),
            (
                CommentSlot::Foot,
                &node.foot_comment,
                &positions.positions.foot,
                &mut node.comment_receipts.foot,
            ),
        ] {
            let lines: Vec<_> = text.split('\n').filter(|part| !part.is_empty()).collect();
            if positions.len() != lines.len() {
                source.complete = false;
            }
            for (position, line) in positions.iter().zip(lines) {
                if data.get(position.offset) != Some(&b'#')
                    || text.as_bytes().get(position.at) != Some(&b'#')
                {
                    source.complete = false;
                    continue;
                }
                let receipt = CommentReceipt {
                    id: CommentId::fresh(),
                    owner: owner.clone(),
                    slot,
                    ordinal: text.as_bytes()[..position.at]
                        .iter()
                        .filter(|b| **b == b'\n')
                        .count(),
                };
                spans.push(ReceiptSpan {
                    at: position.at,
                    receipt: receipt.clone(),
                });
                let start = data[..position.offset]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |p| p + 1);
                source.comments.push(SourceComment {
                    receipt,
                    bytes: line.as_bytes().to_vec(),
                    column: position.offset - start,
                    owner_column: node.column.saturating_sub(1),
                    source_line: data[..position.offset]
                        .iter()
                        .filter(|b| **b == b'\n')
                        .count()
                        + 1,
                    owner_line: node.line,
                    placement: None,
                });
            }
        }
        for (child, position) in node.content.iter_mut().zip(&positions.content) {
            identify(child, position, data, source);
        }
    }
    let mut source = CommentSource {
        complete: true,
        ..CommentSource::default()
    };
    let Ok(positions) = super::super::yaml3::parser::comment_positions(data) else {
        return CommentSource::default();
    };
    identify(node, &positions, data, &mut source);
    // The composer resolves aliases before this wrapper is called. Bind those
    // copies by their source node coordinates, never by comment text.
    fn snapshots(
        node: &Node,
        out: &mut Vec<(usize, usize, Kind, super::super::yaml3::CommentReceipts)>,
    ) {
        out.push((
            node.line,
            node.column,
            node.kind,
            node.comment_receipts.clone(),
        ));
        for child in &node.content {
            snapshots(child, out);
        }
    }
    fn aliases(
        node: &mut Node,
        original: &[(usize, usize, Kind, super::super::yaml3::CommentReceipts)],
        copy: bool,
        complete: &mut bool,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            *complete = false;
            return;
        }
        if copy {
            let matches: Vec<_> = original
                .iter()
                .filter(|(line, column, kind, _)| {
                    *line == node.line && *column == node.column && *kind == node.kind
                })
                .collect();
            if let [(_, _, _, receipts)] = matches.as_slice() {
                node.comment_receipts = receipts.clone();
            } else {
                *complete = false;
            }
        }
        if let Some(AliasTarget::Node(target)) = &node.alias {
            let mut target = (**target).clone();
            aliases(&mut target, original, true, complete, depth + 1);
            node.alias = Some(AliasTarget::Node(std::sync::Arc::new(target)));
        }
        for child in &mut node.content {
            aliases(child, original, copy, complete, depth + 1);
        }
    }
    let mut original = Vec::new();
    snapshots(node, &mut original);
    for comment in &mut source.comments {
        if comment.source_line < comment.owner_line {
            comment.placement = original
                .iter()
                .filter(|(line, column, kind, _)| {
                    *line == comment.source_line
                        && *column <= comment.column + 1
                        && matches!(kind, Kind::Mapping | Kind::Sequence)
                })
                .max_by_key(|(_, column, _, _)| column)
                .and_then(|(_, _, _, receipts)| receipts.owner.clone());
        }
    }
    fn ancestor_placement(
        node: &Node,
        wanted: &CommentId,
        line: usize,
        ancestors: &mut Vec<(CommentId, Kind, usize)>,
    ) -> Option<CommentId> {
        if node.comment_receipts.owner.as_ref() == Some(wanted) {
            return ancestors
                .iter()
                .rev()
                .find(|(_, kind, at)| matches!(kind, Kind::Mapping | Kind::Sequence) && *at >= line)
                .map(|(id, _, _)| id.clone());
        }
        if let Some(id) = &node.comment_receipts.owner {
            ancestors.push((id.clone(), node.kind, node.line));
        }
        let found = node
            .content
            .iter()
            .find_map(|child| ancestor_placement(child, wanted, line, ancestors));
        if node.comment_receipts.owner.is_some() {
            ancestors.pop();
        }
        found
    }
    for comment in &mut source.comments {
        if comment.placement.is_none() && comment.source_line < comment.owner_line {
            comment.placement = ancestor_placement(
                node,
                &comment.receipt.owner,
                comment.source_line,
                &mut Vec::new(),
            );
        }
    }
    aliases(node, &original, false, &mut source.complete, 0);
    #[cfg(test)]
    {
        source.original_inputs.push(node.clone());
        let mut original = node.clone();
        if let Some(root) = original.content.first_mut()
            && let Ok(expanded) = expand_config_aliases(root)
        {
            *root = expanded;
        }
        source.original_sources.push(original);
    }
    source
}

impl CommentSource {
    /// Intentional alias copies have distinct identities and the same original
    /// occurrence bytes. Register them before the next emission can defer them.
    pub(crate) fn include_copies(&mut self, node: &Node) {
        let mut additions = Vec::new();
        fn walk(source: &CommentSource, node: &Node, additions: &mut Vec<SourceComment>) {
            for span in node
                .comment_receipts
                .head
                .iter()
                .chain(&node.comment_receipts.line)
                .chain(&node.comment_receipts.foot)
            {
                if source
                    .comments
                    .iter()
                    .any(|c| c.receipt.id == span.receipt.id)
                    || additions.iter().any(|c| c.receipt.id == span.receipt.id)
                {
                    continue;
                }
                if let Some(original) = source
                    .comments
                    .iter()
                    .find(|c| c.receipt.id.same_source(&span.receipt.id))
                {
                    let mut copy = original.clone();
                    copy.receipt = span.receipt.clone();
                    copy.placement = copy
                        .placement
                        .as_ref()
                        .map(|id| id.in_scope_of(&copy.receipt.id));
                    additions.push(copy);
                }
            }
            for child in &node.content {
                walk(source, child, additions);
            }
        }
        walk(self, node, &mut additions);
        self.comments.extend(additions);
    }

    pub(crate) fn append(&mut self, other: Self) {
        self.complete &= other.complete;
        self.comments.extend(other.comments);
        #[cfg(test)]
        {
            self.original_sources.extend(other.original_sources);
            self.original_inputs.extend(other.original_inputs);
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
    if !rendered.receipts.owners.is_empty() || !rendered.receipts.comments.is_empty() {
        rebind_comment_receipts(&mut node, rendered);
    }
    Ok(node)
}

pub(crate) fn rebind_comment_receipts(node: &mut Node, rendered: &ReceiptedYaml) {
    use super::super::yaml3::parser::CommentNode;
    use super::super::yaml3::{CommentReceipts, ReceiptSpan};
    let Ok(positions) = super::super::yaml3::parser::comment_positions(&rendered.bytes) else {
        return;
    };
    fn bind(
        node: &mut Node,
        positions: &CommentNode,
        path: &mut Vec<usize>,
        rendered: &ReceiptedYaml,
    ) {
        if node.kind != positions.kind || node.content.len() != positions.content.len() {
            return;
        }
        node.comment_receipts = CommentReceipts {
            owner: rendered
                .receipts
                .owners
                .iter()
                .find(|owner| owner.path == *path)
                .map(|owner| owner.owner.clone()),
            ..CommentReceipts::default()
        };
        for (positions, spans) in [
            (&positions.positions.head, &mut node.comment_receipts.head),
            (&positions.positions.line, &mut node.comment_receipts.line),
            (&positions.positions.foot, &mut node.comment_receipts.foot),
        ] {
            for position in positions {
                if let Some(emission) = rendered
                    .receipts
                    .comments
                    .iter()
                    .find(|emission| emission.offset == position.offset && emission.length > 0)
                {
                    spans.push(ReceiptSpan {
                        at: position.at,
                        receipt: emission.receipt.clone(),
                    });
                }
            }
        }
        for (i, (child, position)) in node.content.iter_mut().zip(&positions.content).enumerate() {
            path.push(i);
            bind(child, position, path, rendered);
            path.pop();
        }
    }
    bind(node, &positions, &mut Vec::new(), rendered);
}

/// Normalize the same byte vector while updating each actual write's offset.
pub(crate) fn normalize_receipted(mut rendered: ReceiptedYaml) -> ReceiptedYaml {
    let bytes = normalize_comment_indentation(&rendered.bytes);
    let old: Vec<_> = rendered.bytes.split_inclusive(|b| *b == b'\n').collect();
    let new: Vec<_> = bytes.split_inclusive(|b| *b == b'\n').collect();
    if old.len() != new.len() {
        rendered.receipts.comments.clear();
    } else {
        let (mut old_at, mut new_at) = (0, 0);
        for (old, new) in old.into_iter().zip(new) {
            if let Some(removed) = old
                .len()
                .checked_sub(new.len())
                .filter(|n| old.get(*n..) == Some(new))
            {
                for receipt in &mut rendered.receipts.comments {
                    if receipt.offset >= old_at && receipt.offset < old_at + old.len() {
                        receipt.offset = new_at + receipt.offset.saturating_sub(old_at + removed);
                    }
                }
            } else {
                rendered.receipts.comments.clear();
            }
            old_at += old.len();
            new_at += new.len();
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
            for span in spans {
                if let Some(position) = positions.iter().find(|p| p.at == span.at) {
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

/// Keep the existing Unicode-quoting entry point authoritative. If its output
/// differs from the tracked raw emission, no guessed offsets are transported.
pub(crate) fn marshal_config_receipted(node: &Node) -> Result<ReceiptedYaml, YamlError> {
    let bytes = marshal_config(node)?;
    if !owner_protection_enabled() || node.comment_receipts.owner.is_none() {
        return Ok(ReceiptedYaml {
            bytes,
            receipts: Default::default(),
        });
    }
    let Ok(mut rendered) = marshal_receipted(node, CONFIG_INDENT) else {
        return Ok(ReceiptedYaml {
            bytes,
            receipts: Default::default(),
        });
    };
    if rendered.bytes != bytes {
        rendered.receipts = super::super::yaml3::EmissionReceipts::default();
    }
    rendered.bytes = bytes;
    Ok(rendered)
}

impl CommentSource {
    pub(crate) fn holds(&self, node: &Node) -> bool {
        use super::super::yaml3::CommentSlot;
        fn locations(
            node: &Node,
            wanted: &super::super::yaml3::CommentReceipt,
            found: &mut usize,
            owners: &mut usize,
            holds: &mut bool,
        ) {
            *owners += usize::from(node.comment_receipts.owner.as_ref() == Some(&wanted.owner));
            for (slot, text, spans) in [
                (
                    CommentSlot::Head,
                    &node.head_comment,
                    &node.comment_receipts.head,
                ),
                (
                    CommentSlot::Line,
                    &node.line_comment,
                    &node.comment_receipts.line,
                ),
                (
                    CommentSlot::Foot,
                    &node.foot_comment,
                    &node.comment_receipts.foot,
                ),
            ] {
                for span in spans {
                    if span.receipt.id == wanted.id {
                        *found += 1;
                        let part = text
                            .as_bytes()
                            .get(..span.at)
                            .map(|prefix| prefix.iter().filter(|b| **b == b'\n').count());
                        *holds &= node.comment_receipts.owner.as_ref() == Some(&wanted.owner)
                            && slot == wanted.slot
                            && part == Some(wanted.ordinal);
                    }
                }
            }
            for child in &node.content {
                locations(child, wanted, found, owners, holds);
            }
        }
        self.complete
            && self.comments.iter().all(|comment| {
                let (mut found, mut owners, mut holds) = (0, 0, true);
                locations(node, &comment.receipt, &mut found, &mut owners, &mut holds);
                found == 1 && owners == 1 && holds
            })
    }
}

/// Apply a byte edit and keep every unaffected occurrence's exact offset.
fn edit_receipted(rendered: &mut ReceiptedYaml, start: usize, end: usize, bytes: &[u8]) -> bool {
    if start > end
        || end > rendered.bytes.len()
        || rendered
            .receipts
            .comments
            .iter()
            .any(|r| r.offset < end && r.offset + r.length > start)
    {
        return false;
    }
    let difference = bytes.len() as isize - (end - start) as isize;
    for receipt in &mut rendered.receipts.comments {
        if receipt.offset >= end {
            receipt.offset = receipt.offset.saturating_add_signed(difference);
        }
    }
    rendered.bytes.splice(start..end, bytes.iter().copied());
    true
}

fn same_node_values(a: &Node, b: &Node) -> bool {
    a.kind == b.kind
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
        repair_scalar_headers(source, &mut candidate);
        repair_deferred_lines(source, &mut candidate);
    }
    let Ok(parsed) = read_receipted(&candidate) else {
        return candidate;
    };
    fn owner<'a>(node: &'a Node, id: &super::super::yaml3::CommentId) -> Option<&'a Node> {
        if node.comment_receipts.owner.as_ref() == Some(id) {
            return Some(node);
        }
        node.content.iter().find_map(|n| owner(n, id))
    }
    fn actual_holds(node: &Node, receipt: &super::super::yaml3::CommentReceipt) -> bool {
        [
            (CommentSlot::Head, &node.comment_receipts.head),
            (CommentSlot::Line, &node.comment_receipts.line),
            (CommentSlot::Foot, &node.comment_receipts.foot),
        ]
        .into_iter()
        .any(|(slot, spans)| {
            slot == receipt.slot
                && node.comment_receipts.owner.as_ref() == Some(&receipt.owner)
                && spans.iter().any(|s| s.receipt.id == receipt.id)
        }) || node.content.iter().any(|n| actual_holds(n, receipt))
    }
    let mut edits = Vec::new();
    for comment in &source.comments {
        if !faithful && actual_holds(&parsed, &comment.receipt) {
            continue;
        }
        let Some(node) = owner(&parsed, &comment.receipt.owner) else {
            continue;
        };
        let Some(emission) = candidate
            .receipts
            .comments
            .iter()
            .find(|e| e.receipt.id == comment.receipt.id)
        else {
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
    edits.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, bytes) in edits {
        edit_receipted(&mut candidate, start, end, &bytes);
    }
    candidate
}

pub(crate) fn finish_comment_owners(
    source: &CommentSource,
    candidate: ReceiptedYaml,
    repeat: impl FnOnce(&[u8]) -> Option<Vec<u8>>,
) -> Vec<u8> {
    if !owner_protection_enabled() || !source.complete || source.comments.is_empty() {
        return candidate.bytes;
    }
    if candidate.receipts.overwritten.iter().any(|span| {
        !source
            .comments
            .iter()
            .any(|c| c.receipt.id == span.receipt.id)
    }) {
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
    if !source.holds(&parsed)
        || !source.comments.iter().all(|comment| {
            proposed
                .receipts
                .comments
                .iter()
                .find(|e| e.receipt.id == comment.receipt.id)
                .is_some_and(|e| {
                    proposed.bytes.get(e.offset..e.offset + e.length)
                        == Some(comment.bytes.as_slice())
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

/// Recover the two physical header lines whose deferred comments originally
/// belonged to the first block scalar of a mapping's sequence. The key header
/// and scalar header are separate YAML positions; quoting both onto one header
/// cannot retain their common parsed line owner.
fn repair_scalar_headers(source: &CommentSource, candidate: &mut ReceiptedYaml) {
    use super::super::yaml3::{CommentReceipts, CommentSlot, ReceiptEmission, ReceiptSpan, Style};
    fn at<'a>(node: &'a Node, path: &[usize]) -> Option<&'a Node> {
        path.iter().try_fold(node, |node, i| node.content.get(*i))
    }
    fn position<'a>(
        node: &'a super::super::yaml3::parser::CommentNode,
        path: &[usize],
    ) -> Option<&'a super::super::yaml3::parser::CommentNode> {
        path.iter().try_fold(node, |node, i| node.content.get(*i))
    }
    let ids: Vec<_> = source
        .comments
        .iter()
        .filter(|c| c.receipt.slot == CommentSlot::Line && c.receipt.ordinal == 0)
        .map(|c| c.receipt.owner.clone())
        .collect();
    for id in ids {
        let group: Vec<_> = source
            .comments
            .iter()
            .filter(|c| c.receipt.owner == id && c.receipt.slot == CommentSlot::Line)
            .collect();
        if group.len() != 2
            || group[0].source_line >= group[0].owner_line
            || group[1].source_line != group[1].owner_line
        {
            continue;
        }
        let Ok(parsed) = read_receipted(candidate) else {
            continue;
        };
        let Some(owner) = candidate
            .receipts
            .owners
            .iter()
            .find(|owner| owner.owner == id)
        else {
            continue;
        };
        let path = owner.path.clone();
        if path.len() < 3 || path.last() != Some(&0) {
            continue;
        }
        let Some(scalar) = at(&parsed, &path) else {
            continue;
        };
        if scalar.kind != Kind::Scalar || scalar.short_tag() != STR_TAG || scalar.value.is_empty() {
            continue;
        }
        let parent_path = &path[..path.len() - 1];
        if at(&parsed, parent_path).is_none_or(|n| n.kind != Kind::Sequence) {
            continue;
        }
        let Some(value_index) = parent_path.last().copied().filter(|i| i % 2 == 1) else {
            continue;
        };
        let mut key_path = parent_path[..parent_path.len() - 1].to_vec();
        key_path.push(value_index - 1);
        let Some(key) = at(&parsed, &key_path) else {
            continue;
        };
        if !key.line_comment.is_empty() {
            continue;
        }
        let Ok(positions) = super::super::yaml3::parser::comment_positions(&candidate.bytes) else {
            continue;
        };
        let (Some(span), Some(key_span)) =
            (position(&positions, &path), position(&positions, &key_path))
        else {
            continue;
        };
        let mut writes: Vec<_> = group
            .iter()
            .filter_map(|c| {
                candidate
                    .receipts
                    .comments
                    .iter()
                    .find(|e| e.receipt.id == c.receipt.id)
            })
            .collect();
        if writes.len() != 2 {
            continue;
        }
        writes.sort_by_key(|e| e.offset);
        let end = writes[1].offset + writes[1].length;
        if span.end > writes[0].offset || end > candidate.bytes.len() {
            continue;
        }
        if candidate.receipts.comments.iter().any(|e| {
            e.offset >= span.start
                && e.offset < end
                && !group.iter().any(|c| c.receipt.id == e.receipt.id)
        }) {
            continue;
        }
        if candidate.bytes[span.end..writes[0].offset]
            .iter()
            .chain(&candidate.bytes[writes[0].offset + writes[0].length..writes[1].offset])
            .any(|b| !matches!(b, b' ' | b'\r' | b'\n'))
        {
            continue;
        }
        let key_end = candidate.bytes[key_span.end..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|p| key_span.end + p);
        let Some(key_end) = key_end.filter(|end| *end < span.start) else {
            continue;
        };
        if !candidate.bytes[key_span.end..key_end].contains(&b':')
            || candidate.bytes[key_span.end..key_end].contains(&b'#')
        {
            continue;
        }
        let mut local = scalar.clone();
        local.style = Style::LITERAL;
        local.head_comment.clear();
        local.foot_comment.clear();
        local.line_comment = String::from_utf8_lossy(&group[1].bytes).into_owned();
        local.comment_receipts = CommentReceipts {
            owner: Some(id.clone()),
            line: vec![ReceiptSpan {
                at: 0,
                receipt: group[1].receipt.clone(),
            }],
            ..CommentReceipts::default()
        };
        let Ok((bytes, local_receipts)) = super::encode::faithful_node_receipted(&local, 2) else {
            continue;
        };
        if local_receipts.comments.len() != 1 {
            continue;
        }
        let mut replacement = Vec::new();
        let shift = scalar.column.saturating_sub(3);
        for (i, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            if i > 0 {
                replacement.resize(replacement.len() + shift, b' ');
            }
            replacement.extend_from_slice(line);
        }
        if replacement.last() == Some(&b'\n') {
            replacement.pop();
        }
        let last_offset = span.start + local_receipts.comments[0].offset;
        candidate
            .receipts
            .comments
            .retain(|e| !group.iter().any(|c| c.receipt.id == e.receipt.id));
        if !edit_receipted(candidate, span.start, end, &replacement) {
            continue;
        }
        candidate.receipts.comments.push(ReceiptEmission {
            offset: last_offset,
            length: group[1].bytes.len(),
            receipt: group[1].receipt.clone(),
        });
        restore_collection_header(candidate, key_span, key_end, group[0]);
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
    for comment in &source.comments {
        if comment.receipt.slot != CommentSlot::Line {
            continue;
        }
        let Some(placement) = &comment.placement else {
            continue;
        };
        let Ok(parsed) = read_receipted(candidate) else {
            continue;
        };
        // Do not move an occurrence whose source owner already holds.
        let Some(owner) = candidate
            .receipts
            .owners
            .iter()
            .find(|o| o.owner == comment.receipt.owner)
        else {
            continue;
        };
        if at(&parsed, &owner.path).is_some_and(|n| {
            n.comment_receipts
                .line
                .iter()
                .any(|s| s.receipt.id == comment.receipt.id)
        }) {
            continue;
        }
        let Some(physical) = candidate
            .receipts
            .owners
            .iter()
            .find(|o| o.owner == *placement)
        else {
            continue;
        };
        let path = physical.path.clone();
        let Some(node) = at(&parsed, &path) else {
            continue;
        };
        if !matches!(node.kind, Kind::Mapping | Kind::Sequence) {
            continue;
        }
        if path.last().is_none_or(|i| i % 2 == 0) {
            continue;
        }
        let mut key_path = path.clone();
        *key_path.last_mut().unwrap() -= 1;
        let Ok(positions) = super::super::yaml3::parser::comment_positions(&candidate.bytes) else {
            continue;
        };
        let Some(key) = position(&positions, &key_path) else {
            continue;
        };
        let Some(end) = candidate.bytes[key.end..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|p| key.end + p)
        else {
            continue;
        };
        if !candidate.bytes[key.end..end].contains(&b':')
            || candidate.bytes[key.end..end].contains(&b'#')
        {
            continue;
        }
        let Some(write) = candidate
            .receipts
            .comments
            .iter()
            .find(|e| e.receipt.id == comment.receipt.id)
            .cloned()
        else {
            continue;
        };
        if write.offset < end {
            continue;
        }
        candidate
            .receipts
            .comments
            .retain(|e| e.receipt.id != comment.receipt.id);
        let mut remove_start = write.offset;
        while remove_start > 0 && candidate.bytes[remove_start - 1] == b' ' {
            remove_start -= 1;
        }
        if !edit_receipted(candidate, remove_start, write.offset + write.length, &[]) {
            continue;
        }
        restore_collection_header(candidate, key, end, comment);
    }
}

/// A non-specific YAML tag supplies the same parser boundary as an anchor,
/// without retaining a tag or style flag in the parsed collection. Removing
/// comments and faithfully re-emitting therefore recovers the existing bytes.
fn restore_collection_header(
    candidate: &mut ReceiptedYaml,
    key: &super::super::yaml3::parser::CommentNode,
    end: usize,
    comment: &SourceComment,
) -> bool {
    use super::super::yaml3::ReceiptEmission;
    if candidate.bytes[key.end..end]
        .iter()
        .any(|b| !matches!(b, b':' | b' '))
    {
        return false;
    }
    let header = [b" ! ".as_slice(), comment.bytes.as_slice()].concat();
    if !edit_receipted(candidate, end, end, &header) {
        return false;
    }
    candidate.receipts.comments.push(ReceiptEmission {
        offset: end + 3,
        length: comment.bytes.len(),
        receipt: comment.receipt.clone(),
    });
    true
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
