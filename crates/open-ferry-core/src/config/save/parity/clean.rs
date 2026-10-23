//! Comment-free inputs and faithful output witnesses for offline comparison.
use super::super::{Node, unmarshal};

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
