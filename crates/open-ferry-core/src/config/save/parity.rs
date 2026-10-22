//! Source-only counterfactuals for the offline parity harness. This uses the
//! writer's own parser, effective-comment protection and double-quoted encoder. It
//! never saves a config or rewrites any byte outside the selected scalars.

use super::super::yaml3::Style;
use super::super::yaml3::parser::Parser;
use super::super::yaml3::types::{ScalarStyle, TokenType};
use super::{Node, STR_TAG, SaveError, marshal_config, unmarshal};

mod folded;
pub use folded::{folded_scalar_reencode, folded_scalar_values};

/// Quote only source block scalars the writer protects from multiline line
/// comments. Anchors, explicit tags, header comments and all other bytes are
/// kept exactly. Unsupported token/position shapes are refused.
///
/// # Errors
/// Returns a parse/encode error for invalid YAML.
pub fn quoted_source(source: &str) -> Result<Option<String>, SaveError> {
    let original = unmarshal(source.as_bytes())?;
    let mut normalized = original.clone();
    super::normalize_collection_node_styles(&mut normalized);
    let (_, protected) = super::super::yaml3::encode::protected_node(&normalized, 2)?;
    if protected.is_empty() {
        return Ok(None);
    }
    fn find(node: &Node, line: usize, column: usize) -> Option<&Node> {
        if node.kind == super::Kind::Scalar && node.line == line + 1 && node.column == column + 1 {
            return Some(node);
        }
        node.content
            .iter()
            .find_map(|child| find(child, line, column))
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
        let Some(mark) = protected
            .iter()
            .find(|mark| mark.line == token.start_mark.line)
        else {
            continue;
        };
        let Some(node) = find(&original, mark.line, mark.column) else {
            return Ok(None);
        };
        if node.value.as_bytes() != token.value {
            return Ok(None);
        }
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

/// Prove the exact archived-comment loss: a source block scalar's second
/// header comment became its first data line in upstream's archive. Only
/// those scalar spans and the restored comment line may differ.
pub fn archived_scalar_comment_loss(sources: &[&str], go: &str, rust: &str) -> bool {
    fn attempt(sources: &[&str], go: &str, rust: &str) -> Option<bool> {
        fn scalars(source: &str) -> Option<Vec<(usize, usize, ScalarStyle, String)>> {
            fn offset(source: &str, line: usize, column: usize) -> Option<usize> {
                let start = source.split_inclusive('\n').take(line).map(str::len).sum();
                let text = source.get(start..)?;
                Some(start + text.char_indices().nth(column).map_or(text.len(), |p| p.0))
            }
            let mut parser = Parser::new(source.as_bytes());
            let mut result = Vec::new();
            loop {
                let token = parser.scan().ok()?;
                if matches!(token.typ, TokenType::No | TokenType::StreamEnd) {
                    break;
                }
                if token.typ == TokenType::Scalar {
                    result.push((
                        offset(source, token.start_mark.line, token.start_mark.column)?,
                        offset(source, token.end_mark.line, token.end_mark.column)?,
                        token.style,
                        String::from_utf8(token.value).ok()?,
                    ));
                }
            }
            Some(result)
        }
        fn witness(node: &Node, value: &str, comment: &str) -> bool {
            (node.kind == super::Kind::Scalar
                && node.tag == STR_TAG
                && node.style.has(Style::LITERAL | Style::FOLDED)
                && node.value == value
                && node.line_comment == comment)
                || node.content.iter().any(|n| witness(n, value, comment))
        }
        let source_nodes = sources
            .iter()
            .map(|s| unmarshal(s.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        let gs = scalars(go)?;
        let rs = scalars(rust)?;
        if gs.len() != rs.len() {
            return None;
        }
        let mut rewritten = String::new();
        let mut last = 0;
        let mut changed = false;
        for ((start, end, style, value), (rstart, rend, rstyle, rvalue)) in gs.iter().zip(&rs) {
            if value == rvalue {
                continue;
            }
            if !matches!(style, ScalarStyle::Literal | ScalarStyle::Folded)
                || *rstyle != ScalarStyle::DoubleQuoted
                || rvalue.is_empty()
                || rvalue.contains(['\n', '\r'])
            {
                return None;
            }
            let suffix = rust.get(*rend..)?;
            let header_end = suffix.find('\n')?;
            let head = suffix[..header_end].trim_matches([' ', '\t']);
            let remaining = &suffix[header_end + 1..];
            let tail_end = remaining.find('\n')?;
            let tail = remaining[..tail_end].trim_matches([' ', '\t']);
            if !head.starts_with('#')
                || !tail.starts_with('#')
                || head == tail
                || !source_nodes
                    .iter()
                    .any(|n| witness(n, rvalue, &format!("{head}\n{tail}")))
                || value
                    != &format!(
                        "{tail}{}{rvalue}",
                        if *style == ScalarStyle::Folded {
                            " "
                        } else {
                            "\n"
                        }
                    )
            {
                return None;
            }
            let raw = go.get(*start..*end)?;
            let gheader = raw.split('\n').next()?;
            let indicator = gheader
                .bytes()
                .take_while(|b| matches!(b, b'|' | b'>' | b'+' | b'-' | b'1'..=b'9'))
                .count();
            if gheader.get(indicator..)?.trim_matches([' ', '\t']) != head || *start < last {
                return None;
            }
            rewritten.push_str(go.get(last..*start)?);
            rewritten.push_str(rust.get(*rstart..*rend + header_end + 1 + tail_end + 1)?);
            last = *end;
            changed = true;
        }
        rewritten.push_str(go.get(last..)?);
        Some(changed && rewritten == rust)
    }
    attempt(sources, go, rust) == Some(true)
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
