use super::*;
use crate::config::testing::TempDir;

#[test]
fn review_r9_pending_collection_foot_precedes_the_next_empty_keys_head() {
    for flow in [false, true] {
        for sequence in [false, true] {
            for nested in [false, true] {
                let mut previous = Node::mapping();
                previous.content.extend([
                    Node::scalar(STR_TAG, "child"),
                    Node::scalar(STR_TAG, "value"),
                ]);
                previous.foot_comment = "# previous foot".into();
                let mut key = Node::scalar(STR_TAG, "empty");
                key.head_comment = "# empty head".into();
                let mut empty = if sequence {
                    Node::sequence()
                } else {
                    Node::mapping()
                };
                if flow {
                    empty.style = super::super::yaml3::Style::FLOW;
                }
                let mut root = Node::mapping();
                root.content
                    .extend([Node::scalar(STR_TAG, "previous"), previous, key, empty]);
                let shape = if sequence { "[]" } else { "{}" };
                let source = format!(
                    "previous:\n  child: value\n# previous foot\n\n# empty head\nempty: {shape}\n"
                );
                let source = if nested {
                    let mut parent = Node::mapping();
                    parent
                        .content
                        .extend([Node::scalar(STR_TAG, "parent"), root]);
                    root = parent;
                    format!(
                        "parent:\n{}",
                        source
                            .lines()
                            .map(|line| format!("  {line}\n"))
                            .collect::<String>()
                    )
                } else {
                    source
                };
                let out = String::from_utf8(encode::protected_node(&root, 2).unwrap().0).unwrap();
                assert!(
                    parity::retained_unloadable_comments(&source, &out, &[]),
                    "{flow} {sequence} {nested}: {out}"
                );
                for comment in ["# previous foot", "# empty head"] {
                    assert_eq!(parity::comment_occurrences(&out, comment, false), Some(1));
                }
                // With no pending foot, preserve the faithful emitter's bytes.
                let parent = if nested {
                    &mut root.content[1]
                } else {
                    &mut root
                };
                parent.content[1].foot_comment.clear();
                assert_eq!(
                    encode::protected_node(&root, 2).unwrap().0,
                    encode::faithful_node(&root, 2).unwrap()
                );
            }
        }
    }
}

#[test]
fn review_r9_exact_top_level_input_keeps_both_comment_owners() {
    let source = "plugins:\n  enabled: false\n# plugins foot\n\n# models head\nmodels: {}\nconfig-version: 8\n";
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    write_file(&path, source.as_bytes()).unwrap();
    let out = std::fs::read_to_string(&path).unwrap();
    assert!(parity::retained_unloadable_comments(source, &out, &[]));
}
