//! Comment-free inputs and faithful output witnesses for offline comparison.
use super::super::{Node, unmarshal};

/// Complete parse on loadable text. For a rejected emission, document
/// boundaries must still occur on boundary lines; conservatively refuse any
/// such line after content, including one that might have been quoted data.
pub fn single_document(text: &str) -> bool {
    use crate::config::yaml;
    if yaml::parse_document(text).is_ok() {
        if yaml::has_more_documents(text) {
            return false;
        }
        // The loader does not split comments on these YAML line breaks, but
        // the writer does. Scan them too, even after a successful loader parse.
        if !text.contains(['\u{0085}', '\u{2028}', '\u{2029}']) {
            return true;
        }
    }
    let (mut started, mut ended) = (false, false);
    for line in text.split(['\n', '\r', '\u{0085}', '\u{2028}', '\u{2029}']) {
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indicator = |s: &str| {
            line.strip_prefix(s).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('#')
            })
        };
        if indicator("---") {
            if started || ended {
                return false;
            }
            started = true;
        } else if indicator("...") {
            ended = true;
        } else {
            if ended {
                return false;
            }
            started = true;
        }
    }
    true
}

pub fn has_binary_values(text: &str) -> Option<bool> {
    fn binary(node: &Node) -> bool {
        node.short_tag() == "!!binary" || node.content.iter().any(binary)
    }
    Some(binary(&unmarshal(text.as_bytes()).ok()?))
}

fn clear(n: &mut Node) {
    n.head_comment.clear();
    n.line_comment.clear();
    n.line_comment_id = None;
    n.foot_comment.clear();
    for child in &mut n.content {
        clear(child);
    }
}

/// Remove parsed comments and emit through the faithful encoder.
pub fn faithful_without_comments(text: &str) -> Option<String> {
    if !single_document(text) {
        return None;
    }
    let mut node = unmarshal(text.as_bytes()).ok()?;
    clear(&mut node);
    String::from_utf8(super::super::super::yaml3::encode::faithful_node(&node, 2).ok()?).ok()
}

/// A comment-free source must retain every decoded input value. Refuse a
/// faithful-emitter loss rather than changing a scalar style for the oracle.
pub fn comment_free_source(text: &str) -> Option<String> {
    let out = faithful_without_comments(text)?;
    (same_values(text, &out)?).then_some(out)
}

/// Decoded YAML values, including untyped/plugin values. Non-string-keyed maps
/// are represented only by `AnyMap`, so their contents cannot be compared.
pub fn same_values(a: &str, b: &str) -> Option<bool> {
    if !single_document(a) || !single_document(b) {
        return None;
    }
    if has_binary_values(a)? || has_binary_values(b)? {
        return None;
    }
    use crate::config::AnyValue;
    fn complete(value: &AnyValue) -> bool {
        match value {
            AnyValue::AnyMap => false,
            AnyValue::Map(values) => values.values().all(complete),
            AnyValue::Seq(values) => values.iter().all(complete),
            _ => true,
        }
    }
    let (a, b) = (AnyValue::parse_yaml(a).ok()?, AnyValue::parse_yaml(b).ok()?);
    (complete(&a) && complete(&b)).then_some(a == b)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_mapping_markers_never_prove_value_equality() {
        for source in [
            "x: {1: old}\n",
            "plugins:\n  configs:\n    demo:\n      - {true: old}\n",
        ] {
            assert_eq!(same_values(source, source), None);
            assert_eq!(same_values(source, &source.replace("old", "changed")), None);
            assert!(comment_free_source(source).is_none());
        }
        assert_eq!(
            same_values("x: {'1': old}\n", "x: {'1': old}\n"),
            Some(true)
        );
        assert_eq!(
            same_values("x: {'1': old}\n", "x: {'1': changed}\n"),
            Some(false)
        );
    }
}
