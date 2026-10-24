//! Whole-file regressions from the review probes. Expected files are frozen;
//! plugin save/migration/nested outputs also match independent Go quoted-source
//! controls in the Round 5 evidence. Unknown fields are archived on migration.
use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8, preview_v8};
use serde_json::{Value, json};
fn data(node: &Node) -> Value {
    json!([
        format!("{:?}", node.kind),
        node.tag,
        node.value,
        node.content.iter().map(data).collect::<Vec<_>>()
    ])
}
fn find<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
    if node.kind == Kind::Mapping
        && let Some(i) = find_map_key_index(node, key)
    {
        return node.content.get(i + 1);
    }
    node.content.iter().find_map(|child| find(child, key))
}
fn comments(node: &Node, out: &mut Vec<String>) {
    for text in [&node.head_comment, &node.line_comment, &node.foot_comment] {
        out.extend(text.lines().map(str::to_owned));
    }
    for child in &node.content {
        comments(child, out)
    }
}
fn check(op: &str) {
    let fixtures: Value =
        serde_json::from_str(include_str!("../testdata/block-comment-write-paths.json")).unwrap();
    let mut count = 0;
    for case in fixtures
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["op"] == op)
    {
        count += 1;
        let source = case["source"].as_str().unwrap();
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        let original =
            expand_config_aliases(&unmarshal(source.as_bytes()).unwrap().content[0]).unwrap();
        match op {
            "save" | "migrate" => {
                let mut cfg = Config::parse(source).unwrap();
                cfg.auth_dir = "/x".into();
                save_preserving_comments(&path, &cfg, op == "migrate").unwrap()
            }
            "nested" => update_nested_scalar(&path, &["auth-dir"], "/x").unwrap(),
            "raw" | "raw-v8" => write_file(&path, source.as_bytes()).unwrap(),
            _ => {
                let edit = V8Edit {
                    method: match op {
                        "put" => V8Method::Put,
                        "patch" => V8Method::Patch,
                        _ => V8Method::Delete,
                    },
                    path: vec!["observability".into(), "logs".into(), "debug".into()],
                    body: b"true".to_vec(),
                    yaml: false,
                };
                let preview = preview_v8(source.as_bytes(), &edit).unwrap();
                edit_v8(&path, &edit).unwrap();
                assert_eq!(
                    preview,
                    std::fs::read(&path).unwrap(),
                    "preview: {} {op}",
                    case["name"]
                );
            }
        }
        let file = std::fs::read_to_string(&path).unwrap();
        if !(case["name"] == "m1-escape" && matches!(op, "raw-v8" | "put" | "patch" | "delete")) {
            assert_eq!(
                super::owner_legacy_tests::main_for_assertion("block", source, op, &file),
                case["file"].as_str().unwrap(),
                "{} {op}",
                case["name"]
            );
        }
        Config::load_bytes(file.as_bytes()).unwrap();
        let saved = unmarshal(file.as_bytes()).unwrap();
        if let Some(plugin) = yaml_path(&original, "plugins.configs") {
            assert_eq!(
                data(plugin),
                data(yaml_path(&saved.content[0], "plugins.configs").unwrap()),
                "plugin values: {} {op}",
                case["name"]
            );
        }
        for key in ["template", "k2", "x-note"] {
            if let Some(before) = find(&original, key)
                && before.kind == Kind::Scalar
                && let Some(after) = find(&saved, key)
            {
                assert_eq!(
                    before.value, after.value,
                    "block value: {} {op} {key}",
                    case["name"]
                );
            }
        }
        let mut expected_comments = Vec::new();
        comments(&original, &mut expected_comments);
        for comment in expected_comments {
            let occurrences = file
                .match_indices(&comment)
                .filter(|(i, _)| {
                    file.get(i + comment.len()..)
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('\n'))
                })
                .count();
            assert_eq!(occurrences, 1, "comment: {} {op} {comment}", case["name"]);
        }
    }
    assert_eq!(count, 9);
}
macro_rules! path_test {
    ($name:ident,$op:literal) => {
        #[test]
        fn $name() {
            check($op)
        }
    };
}
path_test!(save_keeps_block_comment_values, "save");
path_test!(migration_keeps_block_comment_values, "migrate");
path_test!(nested_keeps_block_comment_values, "nested");
path_test!(legacy_raw_keeps_block_comment_values, "raw");
path_test!(v8_raw_keeps_block_comment_values, "raw-v8");
path_test!(v8_put_keeps_block_comment_values_and_preview, "put");
path_test!(v8_patch_keeps_block_comment_values_and_preview, "patch");
path_test!(v8_delete_keeps_block_comment_values_and_preview, "delete");
