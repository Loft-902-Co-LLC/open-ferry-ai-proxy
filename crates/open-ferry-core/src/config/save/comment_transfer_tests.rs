use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8, preview_v8};
use serde_json::Value;

fn check(op: &str) {
    let fixtures: Value = serde_json::from_str(include_str!(
        "../testdata/lossless-key-comment-controls.json"
    ))
    .unwrap();
    let mut count = 0;
    for f in fixtures
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["op"] == op)
    {
        count += 1;
        let source = f["source"].as_str().unwrap();
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        match op {
            "nested" => update_nested_scalar(&path, &["auth-dir"], "/x").unwrap(),
            "save" => {
                let mut cfg = Config::parse(source).unwrap();
                cfg.auth_dir = "/x".into();
                save_preserving_comments(&path, &cfg, false).unwrap();
            }
            _ => {
                let edit = V8Edit {
                    method: V8Method::Put,
                    path: vec!["oauth".into(), "auth-dir".into()],
                    body: br#""/x""#.to_vec(),
                    yaml: false,
                };
                let preview = preview_v8(source.as_bytes(), &edit).unwrap();
                edit_v8(&path, &edit).unwrap();
                assert_eq!(preview, std::fs::read(&path).unwrap());
            }
        }
        let bytes = std::fs::read(&path).unwrap();
        let actual = std::str::from_utf8(&bytes).unwrap();
        let go = f["file"].as_str().unwrap();
        if op == "put" && (!go.contains("# c1") || !go.contains("# c2")) {
            // Go drops these in the isolated archive. Preserve both instead.
            assert_eq!(actual.matches("# c1").count(), 1);
            assert_eq!(actual.matches("# c2").count(), 1);
            let split = |text: &str| text.find("# x-note:").unwrap();
            assert_eq!(&actual[..split(actual)], &go[..split(go)]);
            fn archive(text: &str) -> Node {
                let text: String = text
                    .lines()
                    .filter_map(|l| l.strip_prefix("# "))
                    .map(|l| format!("{l}\n"))
                    .collect();
                unmarshal(text.as_bytes()).unwrap()
            }
            fn data(n: &Node) -> Value {
                serde_json::json!([
                    n.tag,
                    n.value,
                    n.content.iter().map(data).collect::<Vec<_>>()
                ])
            }
            assert_eq!(data(&archive(actual)), data(&archive(go)));
        } else {
            assert_eq!(actual, go, "{} {op}", f["name"]);
        }
        Config::load_bytes(&bytes).unwrap();
    }
    assert_eq!(count, 3);
}
#[test]
fn nested_matches_lossless_go_key_comments() {
    check("nested");
}
#[test]
fn settings_save_matches_lossless_go_key_comments() {
    check("save");
}
#[test]
fn v8_put_matches_lossless_go_key_comments() {
    check("put");
}
