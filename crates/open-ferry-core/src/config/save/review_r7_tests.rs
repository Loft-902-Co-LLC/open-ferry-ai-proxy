use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8, preview_v8};

#[test]
fn review_r7_value_changing_breaks_remain_protected() {
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/review-r7-folded-value-go.json")).unwrap();
    for fixture in fixtures {
        let source = fixture["source"].as_str().unwrap();
        let source = unmarshal(source.as_bytes()).unwrap();
        let expected = yaml_path(&source.content[0], "x-note")
            .unwrap()
            .value
            .clone();
        let go = unmarshal(fixture["go"]["files"][0].as_str().unwrap().as_bytes()).unwrap();
        assert_ne!(yaml_path(&go.content[0], "x-note").unwrap().value, expected);
        let mut protected = source;
        yaml_path_mut(&mut protected.content[0], "auth-dir")
            .unwrap()
            .value = "/x".into();
        let output = marshal_config(&protected).unwrap();
        let output = unmarshal(&output).unwrap();
        assert_eq!(
            yaml_path(&output.content[0], "x-note").unwrap().value,
            expected
        );
    }
}

#[test]
fn review_r7_clip_bytes_match_go_on_nested_save_and_v8_put() {
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/review-r7-go.json")).unwrap();
    let mut count = 0;
    for fixture in fixtures.iter().filter(|f| {
        f["name"]
            .as_str()
            .is_some_and(|n| n.starts_with("one-") || n.starts_with("several-"))
    }) {
        count += 1;
        let source = fixture["source"].as_str().unwrap();
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        match fixture["op"].as_str().unwrap() {
            "nested" => update_nested_scalar(&path, &["auth-dir"], "/x").unwrap(),
            "save" => {
                let mut config = Config::parse(source).unwrap();
                config.auth_dir = "/x".into();
                save_preserving_comments(&path, &config, false).unwrap();
            }
            "put" => {
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
        let output = std::fs::read(&path).unwrap();
        assert_eq!(
            output,
            fixture["go"]["files"][0].as_str().unwrap().as_bytes(),
            "{}",
            fixture["name"]
        );
        Config::load_bytes(&output).unwrap();
    }
    assert_eq!(count, 6);
}

#[test]
fn review_r7_clip_trailing_break_matches_lossless_go_bytes() {
    for source in [
        "x-note: >\n  text\nauth-dir: /old\n",
        "x-note: >\n  first\n\n  second\nauth-dir: /old\n",
    ] {
        let mut doc = unmarshal(source.as_bytes()).unwrap();
        yaml_path_mut(&mut doc.content[0], "auth-dir")
            .unwrap()
            .value = "/x".into();
        let faithful = encode::faithful_node(&doc, 2).unwrap();
        let protected = marshal_config(&doc).unwrap();
        assert_eq!(protected, faithful);
    }
}

#[test]
fn review_r7_equal_text_comments_are_independent_occurrences() {
    for (source, count) in [
        (
            "auth-dir: /old # shared\n? x-note # shared\n: value # own\n",
            2,
        ),
        (
            "auth-dir: /old # shared\ndebug: false # shared\n? x-note # shared\n: value # own\n",
            3,
        ),
    ] {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        update_nested_scalar(&path, &["auth-dir"], "/x").unwrap();
        let output = std::fs::read_to_string(&path).unwrap();
        assert_eq!(output.matches("# shared").count(), count, "{output}");
        assert_eq!(output.matches("# own").count(), 1);
        Config::load_bytes(output.as_bytes()).unwrap();
        let doc = unmarshal(output.as_bytes()).unwrap();
        let root = &doc.content[0];
        assert_eq!(yaml_path(root, "auth-dir").unwrap().value, "/x");
        assert_eq!(yaml_path(root, "x-note").unwrap().value, "value");
    }
}

#[test]
fn review_r7_alias_occurrences_are_distinct_and_stable_across_expansions() {
    let source =
        "auth-dir: /old # shared\na: &shape\n  ? note # shared\n  : value # own\nb: *shape\n";
    let doc = unmarshal(source.as_bytes()).unwrap();
    let first = expand_config_aliases(&doc.content[0]).unwrap();
    let second = expand_config_aliases(&doc.content[0]).unwrap();
    let key_id = |root: &Node, path: &str| {
        yaml_path(root, path).unwrap().content[0]
            .line_comment_id
            .clone()
            .unwrap()
    };
    assert_ne!(key_id(&first, "a"), key_id(&first, "b"));
    assert_eq!(key_id(&first, "a"), key_id(&second, "a"));
    assert_eq!(key_id(&first, "b"), key_id(&second, "b"));
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source).unwrap();
    update_nested_scalar(&path, &["auth-dir"], "/x").unwrap();
    let output = std::fs::read_to_string(&path).unwrap();
    assert_eq!(output.matches("# shared").count(), 3);
    assert_eq!(output.matches("# own").count(), 2);
    Config::load_bytes(output.as_bytes()).unwrap();
}

#[test]
fn review_r7_generated_copy_witness_still_matches_saved_go_bytes() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/plugin-anchor-comment-save.json")).unwrap();
    let config = Config::parse(fixture["config"].as_str().unwrap()).unwrap();
    let output = render_preserving_comments(
        fixture["file"].as_str().unwrap().as_bytes(),
        &config,
        fixture["migrate"].as_bool().unwrap(),
    )
    .unwrap();
    assert_eq!(output, fixture["expected"].as_str().unwrap().as_bytes());
    Config::load_bytes(&output).unwrap();
}

#[test]
fn review_r7_custom_tagged_unicode_archives_survive_two_writes() {
    let mut exercised = 0;
    for separator in ['\u{2028}', '\u{2029}', '\u{0085}'] {
        for scalar in [
            format!("!custom |-\n  a{separator}  b\n"),
            format!("!custom 'a{separator}  b'\n"),
        ] {
            let mut source = format!("config-version: 8\nx: {scalar}auth-dir: /old\n");
            if let Err(error) = unmarshal(source.as_bytes()) {
                assert_eq!(error.kind(), SaveErrorKind::Unwritable);
                let dir = TempDir::new();
                let path = dir.join("config.yaml");
                std::fs::write(&path, &source).unwrap();
                let error = write_file(&path, source.as_bytes()).unwrap_err();
                assert_eq!(error.kind(), SaveErrorKind::Unwritable);
                assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
                source = super::archive_tests::escaped_writer_source(&source);
            }
            exercised += 1;
            let doc = unmarshal(source.as_bytes()).unwrap();
            let original = yaml_path(&doc.content[0], "x").unwrap();
            let expected = (original.tag.clone(), original.value.clone());
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            let mut bytes = source.into_bytes();
            for pass in 0..2 {
                write_file(&path, &bytes).unwrap();
                bytes = std::fs::read(&path).unwrap();
                Config::load_bytes(&bytes).unwrap();
                let text = std::str::from_utf8(&bytes).unwrap();
                let archived: String = text
                    .split('\n')
                    .filter_map(|line| line.strip_prefix("# "))
                    .map(|line| format!("{line}\n"))
                    .collect();
                let doc = unmarshal(archived.as_bytes()).unwrap();
                let saved = yaml_path(&doc.content[0], "x").unwrap();
                assert_eq!(
                    (&saved.tag, &saved.value),
                    (&expected.0, &expected.1),
                    "separator={separator:?}, pass={pass}, archive={archived:?}",
                );
            }
        }
    }
    assert_eq!(
        exercised, 6,
        "every custom-tag shape must reach archive emission"
    );
}

pub(super) fn exact_review_output(
    fixture: &serde_json::Value,
) -> Result<String, Box<dyn std::error::Error>> {
    let source = fixture["source"].as_str().unwrap();
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source).unwrap();
    match fixture["op"].as_str().unwrap() {
        "raw" => write_file(&path, source.as_bytes())?,
        "normalize" => {
            let (out, _) = normalize_config_layout(source.as_bytes(), true)?;
            std::fs::write(&path, out).unwrap();
        }
        "nested" => {
            let keys: Vec<_> = fixture["keys"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| k.as_str().unwrap())
                .collect();
            update_nested_scalar(&path, &keys, "/x")?;
        }
        "save" | "migrate" => {
            let mut cfg = Config::parse(source).unwrap();
            cfg.auth_dir = "/x".into();
            save_preserving_comments(&path, &cfg, fixture["op"] == "migrate")?;
        }
        "put" | "patch" => {
            let patch = fixture["op"] == "patch";
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
            let preview = preview_v8(source.as_bytes(), &edit)?;
            edit_v8(&path, &edit)?;
            assert_eq!(preview, std::fs::read(&path).unwrap());
        }
        _ => unreachable!(),
    }
    Ok(std::fs::read_to_string(&path).unwrap())
}

#[test]
fn review_r7_exact_canonical_comments_preserve_values_and_expose_unlicensed_slots() {
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/review-r7-exact.json")).unwrap();
    for f in fixtures
        .iter()
        .filter(|f| matches!(f["name"].as_str(), Some("M0" | "V4")))
    {
        let out = exact_review_output(f).unwrap();
        Config::load_bytes(out.as_bytes()).unwrap();
        let doc = unmarshal(out.as_bytes()).unwrap();
        let root = &doc.content[0];
        for comment in if f["name"] == "M0" {
            vec!["# k", "# v", "# cv"]
        } else {
            vec!["# hv", "# cv"]
        } {
            assert_eq!(
                out.matches(comment).count(),
                1,
                "{} {} {comment}: {out}",
                f["name"],
                f["op"]
            );
        }
        // M0's nested update and saves follow Go's recipient. Other paths retain the
        // comment but normalization moves it to a child or neighbour; the
        // finer owner check deliberately refuses to certify that A2 placement.
        // V4 save/migrate keeps # hv on config-version, but Go kept it on
        // server.port. Rule (b) does not license discarding that Go owner.
        let owns_every_kept_go_comment = if f["name"] == "M0" {
            matches!(f["op"].as_str(), Some("nested" | "save" | "migrate"))
        } else {
            !matches!(f["op"].as_str(), Some("save" | "migrate"))
        };
        assert_eq!(
            super::parity::retained_comment_ownership(
                f["source"].as_str().unwrap(),
                f["go"]["file"].as_str().unwrap(),
                &super::owner_legacy_tests::main_for_assertion(
                    "r7",
                    f["source"].as_str().unwrap(),
                    f["op"].as_str().unwrap(),
                    &out,
                )
            ),
            owns_every_kept_go_comment,
            "{} {}: {out}",
            f["name"],
            f["op"]
        );
        assert_eq!(yaml_path(root, "config-version").unwrap().value, "8");
        assert_eq!(
            crate::config::AnyValue::parse_yaml(&out).unwrap(),
            crate::config::AnyValue::parse_yaml(f["go"]["file"].as_str().unwrap()).unwrap()
        );
    }
}

#[test]
fn review_r7_exact_probe_matrix_retains_comments_and_matches_lossless_clip() {
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/review-r7-exact.json")).unwrap();
    let mut rows = 0;
    for f in &fixtures {
        rows += 1;
        let source = f["source"].as_str().unwrap();
        let result = exact_review_output(f);
        if f["name"] == "E15" && matches!(f["op"].as_str(), Some("save" | "migrate")) {
            assert!(result.is_err());
            assert!(
                f["go"]["file"] == source || !f["go"]["unloadable"].as_str().unwrap().is_empty()
            );
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, source).unwrap();
            let mut config = Config::parse(source).unwrap();
            config.auth_dir = "/x".into();
            assert!(save_preserving_comments(&path, &config, f["op"] == "migrate").is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
            continue;
        }
        let out = result.unwrap_or_else(|e| panic!("{} {}: {e}", f["name"], f["op"]));
        Config::load_bytes(out.as_bytes()).unwrap();
        let name = f["name"].as_str().unwrap();
        if name.starts_with('F') && name != "F5" {
            assert_eq!(out, f["go"]["file"].as_str().unwrap(), "{name} {}", f["op"]);
        }
        if name.starts_with('W') {
            assert_eq!(
                super::parity::comment_occurrences(&out, "# c118", false),
                super::parity::comment_occurrences(source, "# c118", true),
                "{name} {}: {out}",
                f["op"]
            );
            assert_eq!(
                super::parity::comment_occurrences(&out, "# c122", false),
                Some(1),
                "{name} {}: {out}",
                f["op"]
            );
        }
        if name.starts_with('U') {
            let archive = out
                .lines()
                .filter_map(|l| l.strip_prefix("# "))
                .collect::<Vec<_>>()
                .join("\n");
            let original = unmarshal(source.as_bytes()).unwrap();
            let recovered = unmarshal(archive.as_bytes()).unwrap();
            let original = yaml_path(&original.content[0], "x-note").unwrap();
            let recovered = yaml_path(&recovered.content[0], "x-note").unwrap();
            assert_eq!(
                (&original.tag, &original.value),
                (&recovered.tag, &recovered.value),
                "{name} {}: {out}",
                f["op"]
            );
        }
        if name == "F5" && matches!(f["op"].as_str(), Some("raw" | "nested" | "save")) {
            let doc = unmarshal(out.as_bytes()).unwrap();
            assert_eq!(yaml_path(&doc.content[0], "x-note").unwrap().value, "a\n\n");
        }
    }
    assert_eq!(rows, 114);
}

#[test]
fn review_r7_canonical_barrier_does_not_relocate_a_harmless_go_transfer() {
    let source = "debug: false\nmodels: &m # k\n  catalog: '' # v\nconfig-version: 8\n";
    let (out, _) = normalize_config_layout(source.as_bytes(), true).unwrap();
    assert_eq!(
        super::owner_legacy_tests::main_for_assertion(
            "r7barrier",
            source,
            "normalize",
            &String::from_utf8(out).unwrap(),
        ),
        "models:\n    catalog: '' # v\nconfig-version: 8 # k\nobservability:\n    logs:\n        debug: false\n"
    );
}
