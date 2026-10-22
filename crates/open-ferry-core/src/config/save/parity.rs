//! Source-only counterfactuals for the offline parity harness. This uses the
//! writer's own parser, protection predicate and double-quoted encoder. It
//! never saves a config or rewrites any byte outside the selected scalars.

use super::super::yaml3::parser::Parser;
use super::super::yaml3::types::{ScalarStyle, TokenType};
use super::{
    Node, STR_TAG, SaveError, Style, marshal_config, needs_block_scalar_comment_protection,
    unmarshal,
};

/// Quote only source block scalars the writer protects from multiline line
/// comments. Anchors, explicit tags, header comments and all other bytes are
/// kept exactly. Unsupported token/position shapes are refused.
///
/// # Errors
/// Returns a parse/encode error for invalid YAML.
pub fn quoted_source(source: &str) -> Result<Option<String>, SaveError> {
    fn collect<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
        if needs_block_scalar_comment_protection(node) {
            out.push(node)
        }
        for child in &node.content {
            collect(child, out)
        }
    }
    let original = unmarshal(source.as_bytes())?;
    let mut protected = Vec::new();
    collect(&original, &mut protected);
    if protected.is_empty() {
        return Ok(None);
    }
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in &lines {
        starts.push(offset);
        offset += line.len()
    }
    fn position(
        lines: &[&str],
        starts: &[usize],
        line: usize,
        column: usize,
        len: usize,
    ) -> Option<usize> {
        if line == lines.len() && column == 0 {
            return Some(len);
        }
        let text = *lines.get(line)?;
        let byte = if column == text.chars().count() {
            text.len()
        } else {
            text.char_indices().nth(column)?.0
        };
        Some(*starts.get(line)? + byte)
    }
    let mut parser = Parser::new(source.as_bytes());
    let mut edits = Vec::new();
    while let Ok(token) = parser.scan() {
        if matches!(token.typ, TokenType::No | TokenType::StreamEnd) {
            break;
        }
        if token.typ != TokenType::Scalar
            || !matches!(token.style, ScalarStyle::Literal | ScalarStyle::Folded)
        {
            continue;
        }
        let Some(node) = protected.iter().find(|node| {
            node.line == token.start_mark.line + 1 && node.value.as_bytes() == token.value
        }) else {
            continue;
        };
        let (Some(start), Some(end)) = (
            position(
                &lines,
                &starts,
                token.start_mark.line,
                token.start_mark.column,
                source.len(),
            ),
            position(
                &lines,
                &starts,
                token.end_mark.line,
                token.end_mark.column,
                source.len(),
            ),
        ) else {
            return Ok(None);
        };
        let Some(raw) = source.get(start..end) else {
            return Ok(None);
        };
        let header = raw.split_inclusive('\n').next().unwrap_or_default();
        let indicator_len = header
            .bytes()
            .take_while(|b| matches!(b, b'|' | b'>' | b'+' | b'-' | b'1'..=b'9'))
            .count();
        if indicator_len == 0 {
            return Ok(None);
        }
        let mut quoted = Node::scalar(STR_TAG, &node.value);
        quoted.style = Style::DOUBLE_QUOTED;
        let rendered = marshal_config(&quoted)?;
        let Ok(rendered) = std::str::from_utf8(&rendered) else {
            return Ok(None);
        };
        let after = format!(
            "{}{}",
            rendered.strip_suffix('\n').unwrap_or(rendered),
            &header[indicator_len..]
        );
        edits.push((start, end, after));
    }
    if edits.len() != protected.len() {
        return Ok(None);
    }
    let mut rewritten = String::with_capacity(source.len());
    let mut last = 0;
    for (start, end, after) in edits {
        if start < last {
            return Ok(None);
        }
        rewritten.push_str(&source[last..start]);
        rewritten.push_str(&after);
        last = end;
    }
    rewritten.push_str(&source[last..]);
    fn same_values(a: &Node, b: &Node) -> bool {
        a.kind == b.kind
            && a.tag == b.tag
            && a.value == b.value
            && a.anchor == b.anchor
            && a.content.len() == b.content.len()
            && a.content
                .iter()
                .zip(&b.content)
                .all(|(a, b)| same_values(a, b))
    }
    if !same_values(&original, &unmarshal(rewritten.as_bytes())?) {
        return Ok(None);
    }
    Ok(Some(rewritten))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rewrite_changes_only_block_token_and_body_preserving_tag_anchor_and_comments() {
        let source = "# top\nplugins:\n  configs:\n    a:\n      tags: &s # moved\n        - !!str |- # own\n          a\n        - b\nnext: unchanged # tail\n";
        let expected = source.replace("|- # own\n          a\n", "\"a\" # own\n");
        assert_eq!(quoted_source(source).unwrap(), Some(expected));
        for unchanged in ["x: |-\n  # Heading\n  data\n", "x: >- # own\n  a\n"] {
            assert_eq!(quoted_source(unchanged).unwrap(), None)
        }
    }
}
