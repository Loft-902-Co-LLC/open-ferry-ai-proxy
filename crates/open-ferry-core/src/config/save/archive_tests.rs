//! Archived YAML remains recoverable across every config write path.
use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8, preview_v8};

/// Re-express the writer's decoded test value with explicit escapes. This
/// source agrees in both parsers while still requiring archive quoting after
/// the escapes have been decoded. Keep raw tags, anchors and aliases intact.
pub(super) fn escaped_writer_source(source: &str) -> String {
    let mut doc = compose::unmarshal(source.as_bytes()).unwrap();
    // Exercise the actual quoting boundary on the original literal/block
    // presentation too. The escaped source below would otherwise carry an
    // already-quoted style and make an omitted archive quote invisible.
    let mut archive = doc.clone();
    v8::quote_archive_breaks(&mut archive);
    let quoted = String::from_utf8(encode::faithful_node(&archive, 4).unwrap()).unwrap();
    assert!(!quoted.contains(['\u{0085}', '\u{2028}', '\u{2029}']));
    fn quote(node: &mut Node) {
        if node.kind == Kind::Scalar && node.value.contains(['\u{0085}', '\u{2028}', '\u{2029}']) {
            node.style |= super::super::yaml3::Style::DOUBLE_QUOTED;
        }
        for child in &mut node.content {
            quote(child);
        }
    }
    quote(&mut doc);
    String::from_utf8(encode::faithful_node(&doc, 4).unwrap()).unwrap()
}

fn value(bytes: &[u8]) -> (String, String) {
    let doc = unmarshal(bytes).unwrap();
    if let Some(n) = yaml_path(&doc.content[0], "x") {
        return (n.tag.clone(), n.value.clone());
    }
    let text = String::from_utf8_lossy(bytes);
    let archived: String = text[text.find("# x:").expect("archive header")..]
        .lines()
        .filter_map(|l| l.strip_prefix("# ").or_else(|| (l == "#").then_some("")))
        .map(|l| format!("{l}\n"))
        .collect();
    let doc = unmarshal(archived.as_bytes()).unwrap();
    let n = yaml_path(&doc.content[0], "x").unwrap();
    (n.tag.clone(), n.value.clone())
}
fn check(op: &str) {
    let mut shapes = vec!["x: >+\n  first\n\n  second\n\n".to_owned()];
    for sep in ['\u{2028}', '\u{2029}', '\u{0085}'] {
        for body in [
            format!("x: |-\n  a{sep}  b\n"),
            format!("x: |-\n  a{sep}\n"),
            format!("x: |\n  a{sep}  b\n# after\n"),
        ] {
            shapes.push(body);
        }
    }
    assert_eq!(shapes.len(), 10);
    let mut exercised = 0;
    for mut source in shapes {
        source.push_str("auth-dir: /old\ndebug: false\n");
        if op == "raw-v8" {
            source = format!("config-version: 8\n{source}");
        }
        if let Err(error) = unmarshal(source.as_bytes()) {
            assert_eq!(error.kind(), SaveErrorKind::Unwritable);
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, &source).unwrap();
            let error = match op {
                "save" | "migrate" => save_preserving_comments(
                    &path,
                    &Config::parse(&source).unwrap(),
                    op == "migrate",
                )
                .unwrap_err()
                .to_string(),
                "nested" => update_nested_scalar(&path, &["auth-dir"], "/x")
                    .unwrap_err()
                    .to_string(),
                "raw" | "raw-v8" => write_file(&path, source.as_bytes())
                    .unwrap_err()
                    .to_string(),
                "normalize" => normalize_config_layout(source.as_bytes(), true)
                    .unwrap_err()
                    .to_string(),
                _ => {
                    let edit = V8Edit {
                        method: match op {
                            "put" => V8Method::Put,
                            "patch" => V8Method::Patch,
                            _ => V8Method::Delete,
                        },
                        path: vec!["oauth".into(), "auth-dir".into()],
                        body: br#""/x""#.to_vec(),
                        yaml: false,
                    };
                    assert!(preview_v8(source.as_bytes(), &edit).is_err());
                    edit_v8(&path, &edit).unwrap_err().to_string()
                }
            };
            assert!(error.contains("write refused, file unchanged"));
            assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
            source = escaped_writer_source(&source);
        }
        exercised += 1;
        let original = value(source.as_bytes());
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, &source).unwrap();
        match op {
            "save" | "migrate" => {
                let mut c = Config::parse(&source).unwrap();
                c.auth_dir = "/x".into();
                save_preserving_comments(&path, &c, op == "migrate").unwrap();
            }
            "nested" => update_nested_scalar(&path, &["auth-dir"], "/x").unwrap(),
            "raw" | "raw-v8" => write_file(&path, source.as_bytes()).unwrap(),
            "normalize" => {
                let b = normalize_config_layout(source.as_bytes(), true).unwrap().0;
                std::fs::write(&path, b).unwrap();
            }
            _ => {
                let edit = V8Edit {
                    method: match op {
                        "put" => V8Method::Put,
                        "patch" => V8Method::Patch,
                        _ => V8Method::Delete,
                    },
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
        Config::load_bytes(&bytes).unwrap();
        assert_eq!(value(&bytes), original, "{op}: {source:?}");
        if source.contains(">+") {
            assert!(String::from_utf8_lossy(&bytes).contains(">+"));
        }
        if source.contains("# after") && op != "delete" {
            assert_eq!(
                String::from_utf8_lossy(&bytes).matches("# after").count(),
                1
            );
        }
    }
    assert_eq!(exercised, 10, "every shape must reach archive emission");
}
macro_rules! path_test {
    ($name:ident,$op:literal) => {
        #[test]
        fn $name() {
            check($op);
        }
    };
}
path_test!(save_keeps_unicode_archive_and_folded_keep, "save");
path_test!(migration_keeps_unicode_archive_and_folded_keep, "migrate");
path_test!(nested_keeps_unicode_archive_and_folded_keep, "nested");
path_test!(raw_keeps_unicode_archive_and_folded_keep, "raw");
path_test!(raw_v8_keeps_unicode_archive_and_folded_keep, "raw-v8");
path_test!(normalize_keeps_unicode_archive_and_folded_keep, "normalize");
path_test!(put_keeps_unicode_archive_and_folded_keep, "put");
path_test!(patch_keeps_unicode_archive_and_folded_keep, "patch");
path_test!(delete_keeps_unicode_archive_and_folded_keep, "delete");
