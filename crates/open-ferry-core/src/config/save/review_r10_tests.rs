use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8, preview_v8};
use serde_json::Value;

pub(super) fn outputs(source: &str, op: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source)?;
    let twice = op.ends_with('2');
    let mut result = Vec::new();
    for pass in 0..if twice { 2 } else { 1 } {
        let current = std::fs::read(&path)?;
        match op.trim_end_matches('2') {
            "raw" => write_file(&path, &current)?,
            "normalize" => {
                let (out, _) = normalize_config_layout(&current, true)?;
                std::fs::write(&path, out)?;
            }
            "save" | "migrate" => {
                let mut config = Config::load(&path)?;
                config.auth_dir = if pass == 0 { "/x" } else { "/y" }.into();
                save_preserving_comments(&path, &config, op.starts_with("migrate"))?;
            }
            "nested" => {
                update_nested_scalar(&path, &["auth-dir"], if pass == 0 { "/x" } else { "/y" })?
            }
            "put" | "patch" => {
                let patch = op == "patch";
                let edit = V8Edit {
                    method: if patch {
                        V8Method::Patch
                    } else {
                        V8Method::Put
                    },
                    path: if patch {
                        vec!["oauth".into()]
                    } else {
                        vec!["oauth".into(), "auth-dir".into()]
                    },
                    body: if patch {
                        br#"{"auth-dir":"/x"}"#.to_vec()
                    } else {
                        br#""/x""#.to_vec()
                    },
                    yaml: false,
                };
                let preview = preview_v8(&current, &edit)?;
                edit_v8(&path, &edit)?;
                assert_eq!(preview, std::fs::read(&path)?);
            }
            _ => unreachable!(),
        }
        result.push(std::fs::read_to_string(&path)?);
    }
    Ok(result)
}

fn node(spec: &Value) -> Node {
    let mut node = match spec["k"].as_str().unwrap() {
        "map" => Node::mapping(),
        "seq" => Node::sequence(),
        _ => Node::scalar(STR_TAG, spec["v"].as_str().unwrap_or("")),
    };
    if spec["style"] == "flow" {
        node.style = super::super::yaml3::Style::FLOW;
    }
    node.head_comment = spec["head"].as_str().unwrap_or("").into();
    node.line_comment = spec["line"].as_str().unwrap_or("").into();
    node.foot_comment = spec["foot"].as_str().unwrap_or("").into();
    if let Some(children) = spec["c"].as_array() {
        node.content = children.iter().map(node_from_spec).collect();
    }
    node
}

fn node_from_spec(spec: &Value) -> Node {
    node(spec)
}

#[test]
fn review_r10_empty_boundary_reconstructed_controls_match_faithful_and_go() {
    let controls: Vec<Value> = serde_json::from_str(include_str!(
        "../testdata/config-save-r10-foot-controls.json"
    ))
    .unwrap();
    for control in controls {
        let root = node(&control["tree"]);
        let faithful = String::from_utf8(encode::faithful_node(&root, 2).unwrap()).unwrap();
        let protected = String::from_utf8(encode::protected_node(&root, 2).unwrap().0).unwrap();
        assert_eq!(
            faithful,
            control["go"].as_str().unwrap(),
            "{}",
            control["name"]
        );
        assert_eq!(protected, faithful, "{}", control["name"]);
        if compose::unmarshal(faithful.as_bytes()).is_ok() {
            assert!(parity::retained_comment_ownership(
                &faithful, &faithful, &protected
            ));
        } else {
            assert!(!parity::retained_unloadable_comments(
                &faithful,
                &protected,
                &[]
            ));
        }
    }
}

#[test]
fn review_r10_styled_item_foot_and_empty_key_head_keep_their_owners() {
    for between in ["      # item foot\n\n", "# item foot\n"] {
        let source = format!(
            "oauth:\n  model-alias:\n    antigravity:\n      - {{name: a, alias: b}}\n{between}  # rse head\n  request-scoped-errors: {{}}\nconfig-version: 8\n"
        );
        let mut doc = compose::unmarshal(source.as_bytes()).unwrap();
        normalize_collection_node_styles(&mut doc);
        let styled = String::from_utf8(encode::protected_node(&doc, 4).unwrap().0).unwrap();
        assert!(
            parity::retained_unloadable_comments(&source, &styled, &[]),
            "{styled}"
        );
        let parsed = compose::unmarshal(styled.as_bytes()).unwrap();
        let root = &parsed.content[0];
        assert_eq!(
            yaml_path(root, "oauth.model-alias.antigravity")
                .unwrap()
                .content[0]
                .foot_comment,
            "# item foot"
        );
        let oauth = yaml_path(root, "oauth").unwrap();
        let i = find_map_key_index(oauth, "request-scoped-errors").unwrap();
        assert_eq!(oauth.content[i].head_comment.trim(), "# rse head");
        // Steer 1 item 5 defers the column-zero normalizer's A2 owner changes.
        // Keep the end-to-end evidence as a negative rather than normalizing
        // this defect away in the certifier or claiming it is repaired.
        for op in ["save", "migrate", "save2", "put"] {
            for out in outputs(&source, op).unwrap() {
                Config::load_bytes(out.as_bytes()).unwrap();
                assert!(
                    !parity::retained_unloadable_comments(&source, &out, &[]),
                    "{op}: {out}"
                );
                for comment in ["# item foot", "# rse head"] {
                    assert_eq!(parity::comment_occurrences(&out, comment, false), Some(1));
                }
            }
        }
    }
}

#[test]
fn review_r10_empty_key_own_comments_follow_the_previous_foot() {
    for (head, line) in [
        ("# empty head", ""),
        ("", "# empty line"),
        ("# empty head", "# empty line"),
    ] {
        let mut previous = Node::mapping();
        previous.content.extend([
            Node::scalar(STR_TAG, "child"),
            Node::scalar(STR_TAG, "value"),
        ]);
        previous.foot_comment = "# previous foot".into();
        let mut key = Node::scalar(STR_TAG, "empty");
        key.head_comment = head.into();
        key.line_comment = line.into();
        let mut parent = Node::mapping();
        parent.content.extend([
            Node::scalar(STR_TAG, "previous"),
            previous,
            key,
            Node::mapping(),
        ]);
        let mut root = Node::mapping();
        root.content
            .extend([Node::scalar(STR_TAG, "parent"), parent]);
        let out = String::from_utf8(encode::protected_node(&root, 2).unwrap().0).unwrap();
        let source = format!(
            "parent:\n  previous:\n    child: value\n  # previous foot\n\n  {head}\n  ? empty {line}\n  : {{}}\n"
        );
        assert!(
            parity::retained_unloadable_comments(&source, &out, &[]),
            "{out}"
        );
        for comment in ["# previous foot", head, line]
            .into_iter()
            .filter(|s| !s.is_empty())
        {
            assert_eq!(parity::comment_occurrences(&out, comment, false), Some(1));
        }
    }
}

#[test]
fn review_r10_y12_control_keeps_go_bytes_and_owners() {
    let source = "plugins:\n  enabled: false\n# plugins foot\n\n# models head\nmodels: {}\nconfig-version: 8\n";
    let controls: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/config-save-r10-y12.json")).unwrap();
    for control in controls {
        let op = control["op"].as_str().unwrap();
        let out = outputs(source, op).unwrap().pop().unwrap();
        assert_eq!(out, control["go"].as_str().unwrap(), "{op}");
        assert!(parity::retained_comment_ownership(source, &out, &out));
    }
}
