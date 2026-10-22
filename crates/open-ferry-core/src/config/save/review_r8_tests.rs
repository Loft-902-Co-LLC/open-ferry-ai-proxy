use super::*;
use crate::config::testing::TempDir;

const PENDING: &str = "plugins:\n  configs:\n    demo:\n      ? note # pending\n      : value # own\n? x-note # overwrite\n: value # other\n";

#[test]
fn review_r8_generated_multiline_comment_keeps_occurrence_identity() {
    for value in [
        "/a\u{2028}b".to_owned(),
        "/a\u{2029}b".to_owned(),
        "/a\u{0085}b".to_owned(),
        format!("/{}", "long path ".repeat(40)),
    ] {
        for migrate in [false, true] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, PENDING).unwrap();
            let mut cfg = Config::parse(PENDING).unwrap();
            cfg.auth_dir = value.clone();
            save_preserving_comments(&path, &cfg, migrate).unwrap();
            let out = std::fs::read_to_string(&path).unwrap();
            for comment in ["# pending", "# own", "# overwrite", "# other"] {
                assert_eq!(
                    out.matches(comment).count(),
                    1,
                    "{migrate} {value:?} {comment}: {out}"
                );
            }
            assert_eq!(Config::parse(&out).unwrap().auth_dir, value);
            let doc = unmarshal(out.as_bytes()).unwrap();
            assert_eq!(
                yaml_path(&doc.content[0], "plugins.configs.demo.note")
                    .unwrap()
                    .value,
                "value"
            );
        }
    }
}

#[test]
fn review_r8_version_paragraphs_reload_at_the_version_owner() {
    for paragraphs in [
        "# version paragraph one\n\n# version paragraph two",
        "# version paragraph one\n\n# version paragraph two\n\n# version paragraph three",
    ] {
        let source = format!(
            "models: {{}}\nplugins: {{}}\nconfig-version:\n  {}\n  8 # version inline\n",
            paragraphs.replace('\n', "\n  ")
        );
        for op in ["raw", "normalize", "save", "migrate", "put"] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, &source).unwrap();
            match op {
                "raw" => write_file(&path, source.as_bytes()).unwrap(),
                "normalize" => {
                    let (out, _) = normalize_config_layout(source.as_bytes(), true).unwrap();
                    std::fs::write(&path, out).unwrap();
                }
                "save" | "migrate" => save_preserving_comments(
                    &path,
                    &Config::parse(&source).unwrap(),
                    op == "migrate",
                )
                .unwrap(),
                "put" => {
                    use crate::config::v8_edit::*;
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
                _ => unreachable!(),
            }
            let out = std::fs::read_to_string(&path).unwrap();
            assert!(
                parity::retained_unloadable_comments(&source, &out, &[]),
                "{op}: {out}"
            );
            for line in paragraphs
                .lines()
                .filter(|l| !l.is_empty())
                .chain(["# version inline"])
            {
                assert_eq!(out.matches(line).count(), 1, "{op}: {out}");
            }
            Config::parse(&out).unwrap();
        }
    }
}

#[test]
fn review_r8_version_foot_paragraphs_keep_their_parsed_owners() {
    let source =
        "models: {}\nplugins: {}\nconfig-version: 8 # inline\n  # foot one\n\n  # foot two\n";
    for op in ["raw", "normalize", "save", "migrate", "put"] {
        let fixture = serde_json::json!({"source":source,"op":op});
        let out = super::review_r7_tests::exact_review_output(&fixture).unwrap();
        assert!(
            parity::retained_unloadable_comments(source, &out, &[]),
            "{op}: {out}"
        );
        for comment in ["# inline", "# foot one", "# foot two"] {
            assert_eq!(out.matches(comment).count(), 1, "{op}: {out}");
        }
    }
}
