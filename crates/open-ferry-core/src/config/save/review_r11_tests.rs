use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8};

fn operation(source: &str, op: &str) -> (Result<(), String>, String) {
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source).unwrap();
    let result = match op {
        "nested" => update_nested_scalar(&path, &["auth-dir"], "/x").map_err(|e| e.to_string()),
        "raw" => write_file(&path, source.as_bytes()).map_err(|e| e.to_string()),
        "put" => edit_v8(
            &path,
            &V8Edit {
                method: V8Method::Put,
                path: vec!["oauth".into(), "auth-dir".into()],
                body: br#""/x""#.to_vec(),
                yaml: false,
            },
        )
        .map(|_| ())
        .map_err(|e| e.to_string()),
        "save" => save_preserving_comments(&path, &Config::load(&path).unwrap(), false)
            .map_err(|e| e.to_string()),
        _ => unreachable!(),
    };
    let output = std::fs::read_to_string(path).unwrap();
    (result, output)
}

#[test]
fn review_r11_unicode_shapes_follow_go() {
    for source in [
        "# c\u{0085}auth-dir: /secret\n",
        "x: &a 1\ny: &a\u{2028}  !!str 1\nz: *a\nauth-dir: /old\n",
    ] {
        for op in ["save", "nested"] {
            let (result, out) = operation(source, op);
            assert!(result.is_ok(), "{op}");
            Config::load_bytes(out.as_bytes()).unwrap();
        }
    }
    for (source, _line, path) in [
        ("x: 1\nyy: 2\na: 1 # c\u{2028}b: 2\n", 3, "b"),
        ("--- # c\u{2028}a: 1\n", 1, "a"),
        ("a: 1 # c\u{2028}b: 2\n", 1, "b"),
        ("a: 1 # c\u{2028}b: 2\nc: 3\n", 1, "b"),
        ("# harmless\u{0085}\nx: 1\na: 1 # c\u{2028}b: 2\n", 3, "b"),
        ("x: 1\r\na: 1 # c\u{2028}b: 2\r\nz: 3\r\n", 2, "b"),
    ] {
        let doc = unmarshal(source.as_bytes()).unwrap();
        assert!(yaml_path(&doc.content[0], path).is_some());
    }
}

#[test]
fn review_r11_opus_go_byte_fixtures() {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/config-save-r11-opus-go.json")).unwrap();
    let mut failures = vec![];
    for row in rows {
        let source = row["source"].as_str().unwrap();
        let op = row["op"].as_str().unwrap();
        let out = if op == "nested" {
            operation(source, op).1
        } else {
            match super::review_r10_tests::outputs(source, op) {
                Ok(mut out) => out.pop().unwrap(),
                Err(error) => {
                    failures.push(
                        serde_json::json!({"case":row["case"],"op":op,"error":error.to_string()}),
                    );
                    continue;
                }
            }
        };
        if super::owner_legacy_tests::main_for_assertion("r11", source, op, &out)
            != row["expected"].as_str().unwrap()
        {
            failures.push(serde_json::json!({"case":row["case"],"op":op,"out":out,"expected":row["expected"]}));
        }
        if row["go_equal"] == true {
            assert_eq!(
                super::owner_legacy_tests::main_for_assertion("r11", source, op, &out),
                row["go"].as_str().unwrap(),
                "{} {op}",
                row["case"]
            );
        }
    }
    eprintln!("opus byte changes: {failures:#?}");
    assert!(failures.is_empty());
}

#[test]
fn review_r11_opus_first_key_head_equivalence_is_named_and_narrow() {
    let s = "other: 1\n\n# one\n\n# two\nfirst: 2\n";
    let g = "# one\n\n# two\nfirst: 2\nother: 1\n";
    let analysis = parity::comment_ownership_analysis(s, Some("first: 2\nother: 1\n"), g, &[]);
    assert!(analysis.accepted(), "{:?}", analysis.failure);
    assert_eq!(analysis.rules, ["first-root-key-head-document-split"]);
    let value = "config-version:\n  # one\n\n  # two\n  8\nmodels: {}\n";
    let kept = "# one\n\n# two\nconfig-version: 8\nmodels: {}\n";
    let dropped = "config-version: 8\nmodels: {}\n";
    let analysis = parity::comment_ownership_analysis(value, Some(dropped), kept, &[]);
    assert!(analysis.accepted(), "{:?}", analysis.failure);
    assert_eq!(
        analysis.rules,
        [
            "canonical-version-value-head-to-key-head",
            "first-root-key-head-document-split"
        ]
    );
    for (source, wrong, reason, key, side, slot) in [
        (
            s,
            "other: 1\n# one\n\n# two\nfirst: 2\n",
            "owner_budget",
            Some("other"),
            "key",
            "foot",
        ),
        (
            "parent:\n  # one\n\n  # two\n  first: 2\n",
            "# one\n\nparent:\n  # two\n  first: 2\n",
            "owner_budget",
            None,
            "document",
            "head",
        ),
        (
            s,
            "# two\n\n# one\nfirst: 2\nother: 1\n",
            "owner_budget",
            None,
            "document",
            "head",
        ),
        (
            s,
            "# changed\n\n# two\nfirst: 2\nother: 1\n",
            "source_budget",
            None,
            "document",
            "head",
        ),
        (
            "# one\n\nother: 1\n# two\nfirst: 2\n",
            "---\n# one\n\n# two\nfirst: 2\nother: 1\n",
            "owner_budget",
            Some("first"),
            "key",
            "head",
        ),
    ] {
        let analysis =
            parity::comment_ownership_analysis(source, Some("first: 2\nother: 1\n"), wrong, &[]);
        assert!(!analysis.accepted(), "{source:?} -> {wrong:?}");
        let failure = analysis.failure.unwrap();
        eprintln!("first-head negative {source:?} -> {wrong:?}: {failure}");
        assert_eq!(failure["reason"], reason);
        assert_eq!(
            failure["owner"]["path"].as_array().unwrap().len(),
            usize::from(key.is_some())
        );
        if let Some(key) = key {
            assert_eq!(failure["owner"]["path"][0]["key"], key);
        }
        assert_eq!(failure["owner"]["node"], side);
        assert_eq!(failure["owner"]["slot"], slot);
    }
}

#[test]
fn review_r11_unicode_empty_block_eof_matches_go() {
    let source = "# harmless\u{0085}\nx: |\n";
    for op in ["save", "nested"] {
        let (result, out) = operation(source, op);
        result.unwrap();
        Config::load_bytes(out.as_bytes()).unwrap();
    }
    assert!(operation("# harmless\nx: |\n", "nested").0.is_ok());
}

#[test]
fn review_r11_opus_c5_keeps_bytes_without_licensing_go_relocation() {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/config-save-r11-opus-go.json")).unwrap();
    for row in rows
        .iter()
        .filter(|r| r["case"] == "C5" && (r["op"] == "save" || r["op"] == "raw"))
    {
        let result = parity::comment_ownership_analysis(
            row["source"].as_str().unwrap(),
            Some(row["go"].as_str().unwrap()),
            row["expected"].as_str().unwrap(),
            &[],
        );
        if row["op"] == "raw" {
            assert!(result.accepted(), "{:?}", result.failure);
        } else {
            let failure = result.failure.unwrap();
            eprintln!("C5 save: {failure}");
            assert_eq!(failure["reason"], "go_owner_witness");
            assert_eq!(
                failure["owner"]["path"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["key"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["routing", "retry", "request-retry"]
            );
            assert_eq!(failure["owner"]["node"], "key");
            assert_eq!(failure["owner"]["slot"], "foot");
        }
    }
}

#[test]
fn review_r11_opus_balanced_owner_negatives() {
    for (name, s, g, r, reason, key, side, slot) in [
        (
            "bal_line",
            "a: 1 # x\nb: 2 # x\n",
            "a: 1\nb: 2 # x\n",
            "a: 1 # x\nb: 2\n",
            "go_owner_witness",
            "b",
            "value",
            "line",
        ),
        (
            "bal_head",
            "# x\na: 1\n# x\nb: 2\nc: 3\n",
            "a: 1\n# x\nb: 2\n# x\nc: 3\n",
            "# x\na: 1\nb: 2\n# x\nc: 3\n",
            "go_owner_witness",
            "b",
            "key",
            "head",
        ),
        (
            "bal_swap",
            "a: 1 # x\nb: 2\nc: 3 # x\n",
            "a: 1\nb: 2 # x\nc: 3 # x\n",
            "a: 1 # x\nb: 2 # x\nc: 3\n",
            "go_owner_witness",
            "c",
            "value",
            "line",
        ),
        (
            "control_line",
            "a: 1 # x\nb: 2 # y\n",
            "a: 1\nb: 2 # y\n",
            "a: 1 # x\nb: 2\n",
            "go_budget",
            "b",
            "value",
            "line",
        ),
        (
            "control_head",
            "# x\na: 1\n# y\nb: 2\nc: 3\n",
            "a: 1\n# y\nb: 2\n# x\nc: 3\n",
            "# x\na: 1\nb: 2\n# y\nc: 3\n",
            "owner_budget",
            "c",
            "key",
            "head",
        ),
    ] {
        let failure = parity::comment_ownership_analysis(s, Some(g), r, &[])
            .failure
            .expect(name);
        eprintln!("{name}: {failure}");
        assert_eq!(failure["reason"], reason, "{name}");
        assert_eq!(failure["owner"]["path"][0]["key"], key, "{name}");
        assert_eq!(failure["owner"]["node"], side, "{name}");
        assert_eq!(failure["owner"]["slot"], slot, "{name}");
    }
}

#[test]
fn review_r11_01_unicode_comment_exposes_the_go_document() {
    let source = "# c\u{2028}debug: true\n";
    assert!(Config::load_bytes(source.as_bytes()).unwrap().debug);
    for op in ["nested", "put", "raw"] {
        let (result, output) = operation(source, op);
        result.unwrap();
        assert!(Config::load_bytes(output.as_bytes()).unwrap().debug);
    }
}

#[test]
fn review_r11_01_agreeing_empty_and_comment_only_inputs_stay_writable() {
    for source in [
        "",
        "# comment only\n",
        "# comment\u{2028}\n",
        "# comment\u{0085}\n",
    ] {
        assert!(unmarshal(source.as_bytes()).is_ok(), "{source:?}");
        let (result, output) = operation(source, "raw");
        assert!(result.is_ok(), "raw {source:?}: {result:?}");
        assert!(Config::load_bytes(output.as_bytes()).is_ok());
    }
}

#[test]
fn review_r11_02_rebound_anchor_tag_must_not_change_alias_type() {
    let source = "x: &a !!str 1\ny: &a\u{2028}  1\nz: *a\nauth-dir: /old\n";
    let before = crate::config::AnyValue::parse_yaml(source).unwrap();
    let (result, output) = operation(source, "nested");
    let after = crate::config::AnyValue::parse_yaml(&output).unwrap();
    let z_type = |value: &crate::config::AnyValue| match value {
        crate::config::AnyValue::Map(map) => match map.get("z") {
            Some(crate::config::AnyValue::Str(_)) => "string",
            Some(crate::config::AnyValue::Int(_)) => "integer",
            _ => "other",
        },
        _ => "not a mapping",
    };
    eprintln!(
        "finding 2 z type: {} -> {}",
        z_type(&before),
        z_type(&after)
    );
    eprintln!("finding 2 result={result:?}; before={before:?}; after={after:?}; output={output:?}");
    result.unwrap();
    assert_eq!(z_type(&before), "integer");
    assert_eq!(z_type(&after), "integer");
    if let (crate::config::AnyValue::Map(mut before), crate::config::AnyValue::Map(mut after)) =
        (before, after)
    {
        before.remove("auth-dir");
        after.remove("auth-dir");
        assert_eq!(before, after);
    } else {
        panic!("expected mappings");
    }
}

#[test]
fn review_r11_02_agreeing_tagged_inputs_stay_writable() {
    for value in [
        "!!str 1",
        "!<tag:yaml.org,2002:str> 1",
        "!!int 1",
        "!<tag:yaml.org,2002:int> 1",
        "!!bool true",
        "!!float 1.5",
        "!!seq [a, b]",
        "!!map {k: v}",
        "!custom value",
        "!!binary Zm9v",
        "!!timestamp 2020-01-02",
        "!!str \"a\u{2028}b\"",
    ] {
        for prefix in ["", "# harmless\u{2028}\n"] {
            let source = format!("{prefix}x: {value}\nauth-dir: /old\n");
            let (result, output) = operation(&source, "nested");
            assert!(result.is_ok(), "{value}: {result:?}");
            assert_eq!(
                Config::load_bytes(output.as_bytes()).unwrap().auth_dir,
                "/x"
            );
        }
    }
}

#[test]
fn review_r11_04_identical_text_cannot_replace_a_kept_go_owner() {
    let source = "a: 1 # c\nb: 2 # c\n";
    let go = "a: 1 # c\nb: 2\n";
    let wrong = "a: 1\nb: 2 # c\n";
    let failure = parity::comment_ownership_analysis(source, Some(go), wrong, &[])
        .failure
        .unwrap();
    assert_eq!(failure["reason"], "go_owner_witness");
    assert_eq!(
        failure["owner"],
        serde_json::json!({"path":[{"key":"a","tag":"!!str"}],"node":"value","slot":"line"})
    );
    assert!(parity::retained_comment_ownership(source, go, source));
}

#[test]
fn review_r11_05_later_documents_cannot_hide_values_or_comments() {
    let source = "auth-dir: /x\n---\ntail: 1 # keep\n";
    let wrong = "auth-dir: /x\n---\ntail: 2 # invented\n";
    let values = parity::same_values(source, wrong);
    let comments = parity::retained_comment_ownership(source, source, wrong);
    let faithful = parity::faithful_without_comments(source);
    eprintln!("finding 5 values={values:?}; comments={comments}; faithful={faithful:?}");
    assert_eq!(values, None);
    assert!(!comments);
    assert!(faithful.is_none());
    for text in [
        "auth-dir: # c\n{}\n---\ntail: 1\n",
        "---\nauth-dir: # c\n{}\n...\n\u{feff}---\n",
        "auth-dir: # c\n{}\n...\ntail: 1\n",
    ] {
        assert!(!parity::single_document(text), "{text:?}");
    }
    for text in [
        "auth-dir: # c\n{}\n",
        "x: |\n  ---\n",
        "x: 'first\n  ---\n  last'\n",
        "---\nx: 1\n...\n",
    ] {
        assert!(parity::single_document(text), "{text:?}");
    }
}

#[test]
fn review_r11_07_changed_sequences_cannot_reuse_index_source_budgets() {
    let source = "xs:\n  - old # belongs-to-old\n  - survivor\n";
    for (go, wrong) in [
        (
            "xs:\n  - survivor\n",
            "xs:\n  - survivor # belongs-to-old\n",
        ),
        (
            "xs:\n  - survivor\n  - old\n",
            "xs:\n  - survivor # belongs-to-old\n  - old\n",
        ),
    ] {
        let failure = parity::comment_ownership_analysis(source, Some(go), wrong, &[])
            .failure
            .unwrap();
        assert_eq!(failure["reason"], "owner_budget");
        assert_eq!(
            failure["owner"],
            serde_json::json!({"path":[{"key":"xs","tag":"!!str"},{"index":0}],"node":"item","slot":"line"})
        );
        assert!(parity::retained_comment_ownership(source, wrong, wrong));
    }
    assert!(parity::retained_comment_ownership(
        source,
        "xs:\n  - old\n  - survivor\n",
        source
    ));
}

#[test]
fn review_r13_hidden_documents_at_unicode_breaks_are_outside_witnesses() {
    for sep in ['\u{2028}', '\u{2029}', '\u{0085}', '\n'] {
        let source = format!("auth-dir: /x\n# c{sep}---{sep}tail: 1 # keep\n");
        let protected = source.replace("1 # keep", "2 # invented");
        let analysis = parity::comment_ownership_analysis(&source, Some(&source), &protected, &[]);
        assert!(!analysis.accepted(), "{sep:?}");
        assert_eq!(
            analysis.failure.unwrap()["reason"],
            "source_comments_unparseable"
        );
        assert!(!parity::single_document(&source));
        assert!(!parity::single_document(&protected));
        assert_eq!(parity::same_values(&source, &protected), None);
        assert!(parity::faithful_without_comments(&source).is_none());
        assert!(parity::single_document(&format!(
            "auth-dir: /x\n# c{sep}tail: 1\n"
        )));
    }
}

#[test]
fn review_r13_first_head_projection_cannot_start_on_a_later_key() {
    let source = "other: 1\n\n# one\n\n# two\nfirst: 2\n";
    let go = "other: 1\nfirst: 2\n";
    let wrong = "# one\n\nother: 1\n# two\nfirst: 2\n";
    let failure = parity::comment_ownership_analysis(source, Some(go), wrong, &[])
        .failure
        .unwrap();
    assert_eq!(failure["reason"], "owner_budget");
    assert_eq!(
        failure["owner"],
        serde_json::json!({"path":[],"node":"document","slot":"head"})
    );
    let positive = "# one\n\n# two\nfirst: 2\nother: 1\n";
    assert!(parity::comment_ownership_analysis(source, Some(go), positive, &[]).accepted());
}
