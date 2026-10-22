//! A more-indented first line must not change later folding decisions.
use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8, preview_v8};

fn values(bytes: &[u8]) -> Vec<String> {
    fn visit(node: &Node, out: &mut Vec<String>) {
        if node.kind == Kind::Scalar
            && (node.value.contains("indented") || node.value.contains("ordinary"))
        {
            out.push(node.value.clone());
        }
        for child in &node.content {
            visit(child, out);
        }
    }
    let mut out = Vec::new();
    visit(&unmarshal(bytes).unwrap(), &mut out);
    // Migration keeps unrecognized fields in a commented-out YAML block.
    let archive: String = String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            line.strip_prefix("# ")
                .or_else(|| (line == "#").then_some(""))
        })
        .map(|line| format!("{line}\n"))
        .collect();
    if !archive.is_empty() {
        visit(&unmarshal(archive.as_bytes()).unwrap(), &mut out);
    }
    out
}

fn check(op: &str) {
    let mut count = 0;
    for (prefix, indent) in [
        ("plugins:\n  configs:\n    alpha:\n      template: ", 8),
        ("pprof:\n  template: ", 4),
        ("template: ", 2),
    ] {
        for last in ["# Heading", "next"] {
            for (style, first, tail) in [
                (">2-", " indented", ""),
                (">2-", "\tindented", ""),
                (">2", " indented", "\n"),
                (">2+", " indented", "\n\n"),
                ("|2-", " indented", ""),
                ("|2-", "\tindented", ""),
            ] {
                count += 1;
                let pad = " ".repeat(indent);
                let gap = if style.starts_with('>') { "\n" } else { "" };
                let source = format!(
                    "{prefix}{style}\n{pad}{first}\n{pad}ordinary\n{gap}{pad}{last}\n{tail}auth-dir: /old\n"
                );
                let source = if op == "raw-v8" {
                    format!("config-version: 8\n{source}")
                } else {
                    source
                };
                let before = values(source.as_bytes());
                assert_eq!(before.len(), 1);
                assert!(before[0].contains(&format!("ordinary\n{last}")));
                let dir = TempDir::new();
                let path = dir.join("config.yaml");
                std::fs::write(&path, &source).unwrap();
                match op {
                    "save" | "migrate" => {
                        let mut cfg = Config::parse(&source).unwrap();
                        cfg.auth_dir = "/x".into();
                        save_preserving_comments(&path, &cfg, op == "migrate").unwrap();
                    }
                    "nested" => update_nested_scalar(&path, &["auth-dir"], "/x").unwrap(),
                    "raw" | "raw-v8" => write_file(&path, source.as_bytes()).unwrap(),
                    "normalize" => {
                        let output = normalize_config_layout(source.as_bytes(), true).unwrap().0;
                        std::fs::write(&path, output).unwrap();
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
                let output = std::fs::read(&path).unwrap();
                Config::load_bytes(&output).unwrap();
                assert_eq!(values(&output), before, "{op}: {source}");
                // These shapes need no quoting, including the hash-leading body.
                assert!(String::from_utf8_lossy(&output).contains(&style[..1]));
            }
        }
    }
    assert_eq!(count, 36);
}

macro_rules! path_test {
    ($name:ident, $op:literal) => {
        #[test]
        fn $name() {
            check($op);
        }
    };
}
path_test!(save_preserves_folded_breaks, "save");
path_test!(migration_preserves_folded_breaks, "migrate");
path_test!(nested_preserves_folded_breaks, "nested");
path_test!(raw_preserves_folded_breaks, "raw");
path_test!(raw_v8_preserves_folded_breaks, "raw-v8");
path_test!(normalization_preserves_folded_breaks, "normalize");
path_test!(put_preserves_folded_breaks, "put");
path_test!(patch_preserves_folded_breaks, "patch");
path_test!(delete_preserves_folded_breaks, "delete");

#[test]
fn ordinary_yaml_encoding_keeps_yaml_v3_folding() {
    let source = b"template: >2-\n   indented\n  ordinary\n\n  next\n";
    let doc = unmarshal(source).unwrap();
    let faithful = encode::marshal(&encode::Value::Node(doc.clone())).unwrap();
    assert_eq!(values(&faithful), [" indented\nordinary next"]);
    let protected = encode::protected_node(&doc, 4).unwrap().0;
    assert_eq!(values(&protected), values(source));
}
