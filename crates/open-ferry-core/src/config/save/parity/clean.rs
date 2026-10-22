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

/// All YAML values, including untyped/plugin values, not just typed settings.
pub fn same_values(a: &str, b: &str) -> Option<bool> {
    use crate::config::AnyValue;
    Some(AnyValue::parse_yaml(a).ok()? == AnyValue::parse_yaml(b).ok()?)
}
