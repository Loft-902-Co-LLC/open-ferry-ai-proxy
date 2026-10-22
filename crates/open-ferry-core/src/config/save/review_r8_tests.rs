use super::*;
use crate::config::testing::TempDir;

const PENDING: &str = "plugins:\n  configs:\n    demo:\n      ? note # pending\n      : value # own\n? x-note # overwrite\n: value # other\n";

#[test]
fn review_r8_pruned_mapping_foot_comment_remains_writable() {
    let source = include_str!("../testdata/review-r8-pruned-mapping-foot.yaml");
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    write_file(&path, source.as_bytes()).unwrap();
    let before = Config::load(&path).unwrap();
    save_preserving_comments(&path, &before, false).unwrap();
    let out = std::fs::read_to_string(&path).unwrap();
    let after = Config::load(&path).unwrap();
    // Go's independently comment-free run completes and also prunes the
    // rejected rules. Compare every reloaded setting to that value oracle.
    let expected = Config::parse(include_str!(
        "../testdata/review-r8-pruned-mapping-go-values.yaml"
    ))
    .unwrap();
    assert!(
        after == expected,
        "{:?}",
        crate::config::diff::build_change_details(&expected, &after)
    );
    for comment in ["# c200", "#   c254 with words"] {
        assert_eq!(parity::comment_occurrences(&out, comment, false), Some(1));
    }
    assert_eq!(
        yaml_path(
            &unmarshal(out.as_bytes()).unwrap().content[0],
            "plugins.configs.alpha.enabled"
        )
        .unwrap()
        .value,
        "true"
    );
}

#[test]
fn review_r8_empty_collections_flush_a_previous_collections_foot_comment() {
    let mut failures = Vec::new();
    for (name, mut value, flow) in [
        ("block mapping", Node::mapping(), false),
        ("flow mapping", Node::mapping(), true),
        ("block sequence", Node::sequence(), false),
        ("flow sequence", Node::sequence(), true),
    ] {
        if flow {
            value.style = super::super::yaml3::Style::FLOW;
        }
        let kind = value.kind;
        let mut previous = Node::mapping();
        previous.content.extend([
            Node::scalar(STR_TAG, "child"),
            Node::scalar(STR_TAG, "value"),
        ]);
        previous.foot_comment = "# previous foot".into();
        let mut parent = Node::mapping();
        parent.content.extend([
            Node::scalar(STR_TAG, "previous"),
            previous,
            Node::scalar(STR_TAG, "empty"),
            value,
        ]);
        let mut root = Node::mapping();
        root.content
            .extend([Node::scalar(STR_TAG, "parent"), parent]);
        let faithful = encode::faithful_node(&root, 2).unwrap();
        assert!(unmarshal(&faithful).is_err());
        let (out, _) = encode::protected_node(&root, 2).unwrap();
        // This Round 8 fix isn't one of the six Round 7 protections that
        // diagnostic ablation is authorized to disable.
        #[cfg(feature = "parity")]
        {
            use parity::ablation::{Protection::*, with_disabled};
            let disabled = with_disabled(
                &[
                    EmptyMappingPendingComment,
                    CanonicalVersionComment,
                    DroppedSingleKeyComment,
                    InlineEmptyNullComment,
                    EffectiveBlockScalarComment,
                    BlockScalarCommentIndentation,
                ],
                || encode::protected_node(&root, 2).unwrap().0,
            );
            assert_eq!(out, disabled, "{name}");
        }
        let parsed = match unmarshal(&out) {
            Ok(parsed) => parsed,
            Err(error) => {
                failures.push((name, error.to_string()));
                continue;
            }
        };
        let empty = yaml_path(&parsed.content[0], "parent.empty").unwrap();
        assert_eq!(empty.kind, kind);
        assert!(empty.content.is_empty());
        assert_eq!(
            String::from_utf8(out)
                .unwrap()
                .matches("# previous foot")
                .count(),
            1
        );
    }
    assert!(failures.is_empty(), "{failures:?}");
}

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

#[test]
fn review_r8_generated_node_rebind_uses_the_emitted_comment_position() {
    use crate::config::yaml3::{CommentId, Style};
    let id = CommentId::fresh();
    let mut value = Node::scalar(STR_TAG, "/a\u{2028}b");
    value.style = Style::SINGLE_QUOTED;
    value.line_comment = "# pending".into();
    value.line_comment_id = Some(id.clone());
    let (bytes, emitted) = encode::marshal_with_comment_ids(&encode::Value::Node(value)).unwrap();
    let mut parsed = unmarshal(&bytes).unwrap();
    assert_ne!(parsed.content[0].line, parsed.content[0].line_comment_line);
    assert_ne!(parsed.content[0].line_comment_id, Some(id.clone()));
    rebind_generated_comments(&mut parsed, &emitted);
    assert_eq!(parsed.content[0].line_comment_id, Some(id));
}

#[test]
fn review_r8_archive_tag_table_survives_two_writes() {
    fn scalar_values(n: &Node) -> Vec<(String, String)> {
        let mut result = Vec::new();
        if n.kind == Kind::Scalar {
            result.push((n.tag.clone(), n.value.clone()));
        }
        for child in &n.content {
            result.extend(scalar_values(child));
        }
        result
    }
    let cases = [
        ("explicit-str", "x: !!str |-\n  a\u{2029}  b\n"),
        ("non-specific", "x: ! 'a\u{2028}b'\n"),
        (
            "verbatim",
            "x: !<tag:example.com,2000:x> |-\n  a\u{2029}  b\n",
        ),
        ("binary", "x: !!binary YWJj\n"),
        ("null", "x: !!null \"\"\n"),
        ("flow", "x: {a: 'b\u{2028}c'}\n"),
        ("sequence", "x:\n- !t 'a\u{2028}b'\n"),
        ("anchor", "x: &a !t 'a\u{2028}b'\ny: *a\n"),
        ("only-escapes", "x: \"a\\u2028b\\u2029c\\u0085d\"\n"),
    ];
    for (name, shape) in cases {
        let source = format!("config-version: 8\n{shape}auth-dir: /old\n");
        let original = unmarshal(source.as_bytes()).unwrap();
        let original = expand_config_aliases(&original.content[0]).unwrap();
        let expected = scalar_values(yaml_path(&original, "x").unwrap());
        for op in ["raw", "normalize", "put"] {
            let fixture = serde_json::json!({"source":source,"op":op});
            let mut out = super::review_r7_tests::exact_review_output(&fixture).unwrap();
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            for pass in 0..2 {
                if pass > 0 {
                    write_file(&path, out.as_bytes()).unwrap();
                    out = std::fs::read_to_string(&path).unwrap();
                }
                Config::parse(&out).unwrap();
                let archive = out
                    .split('\n')
                    .filter_map(|l| {
                        if l == "#" {
                            Some("")
                        } else {
                            l.strip_prefix("# ")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let archived = unmarshal(archive.as_bytes()).unwrap();
                let archived = expand_config_aliases(&archived.content[0]).unwrap();
                assert_eq!(
                    scalar_values(yaml_path(&archived, "x").unwrap()),
                    expected,
                    "{name} {op} pass {pass}: {archive}"
                );
            }
        }
    }
}
