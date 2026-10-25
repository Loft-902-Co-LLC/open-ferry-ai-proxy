use super::*;

fn cases() -> Vec<Value> {
    let mut cases: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_r4_cases.json")).unwrap();
    cases.extend(
        serde_json::from_str::<Vec<Value>>(include_str!("../testdata/writer/writer_r5_cases.json"))
            .unwrap(),
    );
    cases
}

fn rows() -> Vec<Value> {
    serde_json::from_str(include_str!("../testdata/writer/writer_r4_main.json")).unwrap()
}

fn input(case: &str) -> String {
    cases().into_iter().find(|c| c["case"] == case).unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn check_owners_values_and_repeat(case: &str, op: &str, result: &Value) {
    let source = input(case);
    assert!(
        !result["refused"].as_bool().unwrap_or(false),
        "{case} {op} changed a refusal"
    );
    for (i, file) in result["files"].as_array().unwrap().iter().enumerate() {
        let output = file.as_str().unwrap();
        assert!(
            Config::parse(output).is_ok(),
            "{case} {op} snapshot {i} cannot load"
        );
        assert!(
            inventory(output) == inventory(&source),
            "{case} {op} snapshot {i} moved a comment"
        );
        let mut expected = Config::parse(&source).unwrap();
        if ["save", "migrate", "nested", "patch", "put"].contains(&op.trim_end_matches('2')) {
            expected.auth_dir = if i > 0 && !["patch", "put"].contains(&op.trim_end_matches('2')) {
                "/y"
            } else {
                "/x"
            }
            .into();
        }
        assert!(
            config_values(&expected) == config_values(&Config::parse(output).unwrap()),
            "{case} {op} snapshot {i} changed a value"
        );
    }
    let repeated = op_run(&source, &format!("{}2", op.trim_end_matches('2')), false);
    assert!(
        !repeated["refused"].as_bool().unwrap_or(false),
        "{case} {op} repeat refuses"
    );
    assert_eq!(
        repeated["files"].as_array().unwrap().len(),
        2,
        "{case} {op}"
    );
    assert!(
        repeated["files"][0] == repeated["files"][1],
        "{case} {op} did not settle"
    );
}

#[test]
fn writer_r4_root_key_foot_stays_a_foot_after_normalization() {
    let source = input("s3_head_col8")
        .lines()
        .skip(3)
        .map(|line| line.strip_prefix("      ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let legacy = input("legacy_root_col1");
    for (op, source) in [
        ("nested2", source),
        ("raw", legacy.clone()),
        ("raw2", legacy),
    ] {
        let doc = unmarshal(source.as_bytes()).unwrap();
        assert!(
            yaml_path(&doc.content[0], "config-version").is_none()
                && !is_v8_config_layout(&doc.content[0]).unwrap(),
            "root foot {op} must exercise a legacy file"
        );
        let result = op_run(&source, op, false);
        assert!(
            !result["refused"].as_bool().unwrap_or(false),
            "root foot {op} refuses"
        );
        assert_eq!(
            result["files"].as_array().unwrap().len(),
            if op.ends_with('2') { 2 } else { 1 }
        );
        let mut expected = values(&doc);
        if op == "nested2" {
            expected
                .as_object_mut()
                .unwrap()
                .insert("auth-dir".into(), json!("/x"));
        }
        for file in result["files"].as_array().unwrap() {
            assert!(
                values(&unmarshal(file.as_str().unwrap().as_bytes()).unwrap()) == expected,
                "root foot {op} changed a value"
            );
            let mut expected_config = Config::parse(&source).unwrap();
            if op == "nested2" {
                expected_config.auth_dir = "/x".into();
            }
            assert!(
                config_values(&Config::parse(file.as_str().unwrap()).unwrap())
                    == config_values(&expected_config),
                "root foot {op} changed a config value"
            );
            assert!(
                inventory(file.as_str().unwrap()) == inventory(&source),
                "root foot {op} moved a comment"
            );
        }
        let repeat = op_run(&source, &format!("{}2", op.trim_end_matches('2')), false);
        assert!(
            result["files"][0] == repeat["files"][0] && repeat["files"][0] == repeat["files"][1],
            "root foot {op} did not settle"
        );
    }
}

#[test]
fn writer_r4_source_foot_does_not_take_the_real_following_head() {
    let source = input("s3_head_col8").replace(
        "      empty: {}",
        "      # actual head one\n\n      # actual head two\n      empty: {}",
    ) + "config-version: 8\n";
    for source in [source.clone(), source.replace('\n', "\r\n")] {
        let original = owners(&source);
        for op in ["nested2", "raw2", "normalize2"] {
            let result = op_run(&source, op, false);
            assert!(
                !result["refused"].as_bool().unwrap_or(false),
                "source foot {op} refuses"
            );
            assert_eq!(result["files"].as_array().unwrap().len(), 2);
            for file in result["files"].as_array().unwrap() {
                let output = file.as_str().unwrap();
                assert!(
                    inventory(output) == inventory(&source),
                    "source foot {op} moved a comment"
                );
                let actual = owners(output);
                for owner in original["owners"].as_array().unwrap() {
                    let current = actual["owners"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|r| r["path"] == owner["path"])
                        .unwrap();
                    for slot in ["head", "foot"] {
                        assert_eq!(
                            current[slot].as_str().unwrap().trim(),
                            owner[slot].as_str().unwrap().trim(),
                            "source foot {op} changed a paragraph"
                        );
                    }
                }
            }
            assert!(
                result["files"][0] == result["files"][1],
                "source foot {op} did not settle"
            );
        }
    }
}

#[test]
fn writer_r4_source_head_witness_requires_the_same_key() {
    let source = unmarshal(input("s3_head_col8").as_bytes()).unwrap();
    let emitted =
        String::from_utf8(crate::config::yaml3::encode::marshal_node(&source, 2).unwrap()).unwrap();
    for (from, to) in [("empty: {}", "other-empty: {}"), ("items:", "other-items:")] {
        let emitted = emitted.replace(from, to);
        let actual =
            super::super::super::tree::source_flow_item_heads(&source, emitted.as_bytes().to_vec());
        assert!(
            inventory(std::str::from_utf8(&actual).unwrap()) == inventory(&emitted),
            "unmatched source key changed a comment owner"
        );
    }
}

#[test]
fn writer_r4_physical_head_witness_does_not_cross_other_yaml_breaks() {
    let source = input("s3_head_col8") + "\n      # r1\n      # r2\n      last-empty: {}\n";
    let source = unmarshal(source.as_bytes()).unwrap();
    let emitted =
        String::from_utf8(crate::config::yaml3::encode::marshal_node(&source, 2).unwrap()).unwrap();
    for separator in ["\u{85}", "\u{2028}", "\u{2029}", "\r"] {
        let data = format!("{emitted}# guard{separator}# next\n");
        let actual =
            super::super::super::tree::source_flow_item_heads(&source, data.as_bytes().to_vec());
        assert!(
            actual == data.as_bytes(),
            "non-LF source repair changed the previous bytes"
        );
        for count in 1..=12 {
            let data = format!("# guard{}\n{emitted}", separator.repeat(count));
            let actual = super::super::super::tree::source_flow_item_heads(
                &source,
                data.as_bytes().to_vec(),
            );
            let doc = unmarshal(&actual).unwrap();
            let demo = yaml_path(&doc.content[0], "plugins.configs.demo").unwrap();
            let key = find_map_key_index(demo, "last-empty").unwrap();
            assert_eq!(
                demo.content[key].head_comment.trim(),
                "# r1\n# r2",
                "logical break {count} moved the later key's head"
            );
        }
    }
}

#[test]
fn writer_r4_separator_leaves_the_previous_key_foot_in_place() {
    for row in rows()
        .into_iter()
        .filter(|r| r.as_array().unwrap().len() == 4)
    {
        let case = row[0].as_str().unwrap();
        let op = row[1].as_str().unwrap();
        let result = op_run(&input(case), op, true);
        check_owners_values_and_repeat(case, op, &result);
        assert_eq!(
            receipt(&result),
            row[3],
            "{case} {op} faithful tail-only repair bytes"
        );
    }
    for (case, op) in [("z_go_output_v8", "normalize"), ("y_ls_head", "normalize2")] {
        let row = rows()
            .into_iter()
            .find(|r| r[0] == case && r[1] == op)
            .unwrap();
        assert_eq!(
            receipt(&op_run(&input(case), op, true)),
            row[2],
            "{case} {op} must keep main's bytes"
        );
    }
}

#[test]
fn writer_r4_split_key_feet_keep_owners_or_main_bytes() {
    for row in rows() {
        let case = row[0].as_str().unwrap();
        let op = row[1].as_str().unwrap();
        // This legacy raw root-key foot is an existing partial repair. The
        // 192-input comparison checks its prior bytes; don't make its
        // misplaced comment an expected owner in this test.
        if case == "x_alias_split_legacy" && op.starts_with("raw") {
            continue;
        }
        let result = op_run(&input(case), op, true);
        if receipt(&result) != row[2] {
            check_owners_values_and_repeat(case, op, &result);
        }
    }
}

#[cfg(feature = "parity")]
#[test]
fn writer_r4_tail_repair_keeps_each_switch_and_faithful_emission_separate() {
    use parity::ablation::{Protection, with_disabled};
    for row in rows() {
        let case = row[0].as_str().unwrap();
        let op = row[1].as_str().unwrap();
        for protection in [
            Protection::MultipleFlowItemFeet,
            Protection::EmptyMappingPendingComment,
        ] {
            assert_eq!(
                receipt(&with_disabled(&[protection], || op_run(
                    &input(case),
                    op,
                    true
                ))),
                row[2],
                "{case} {op} disabled {protection:?} must keep main's bytes"
            );
        }
    }
    let doc = unmarshal(input("s3_head_col8").as_bytes()).unwrap();
    let faithful = || crate::config::yaml3::encode::faithful_node(&doc, 2).unwrap();
    let expected = sha256_hex(&faithful());
    for protection in [
        Protection::MultipleFlowItemFeet,
        Protection::EmptyMappingPendingComment,
    ] {
        assert_eq!(
            sha256_hex(&with_disabled(&[protection], faithful)),
            expected,
            "config-only tail repair changed faithful YAML"
        );
    }
}

#[test]
fn writer_r4_non_lf_normalization_keeps_the_emitted_bytes() {
    for separator in ["\u{85}", "\u{2028}", "\u{2029}", "\r"] {
        let source = format!("# guard{separator}{separator}\n{}", input("s3_head_col8"));
        let (doc, _) = super::super::normalize_config_layout_doc(source.as_bytes(), true).unwrap();
        let previous = super::super::super::tree::marshal(&doc).unwrap();
        let (normal, _) = super::super::normalize_config_layout(source.as_bytes(), true).unwrap();
        assert!(
            normal == previous,
            "non-LF normalization changed the previous emitted bytes"
        );
    }
}

#[path = "writer_r4_scope_tests.rs"]
mod writer_r4_scope_tests;

#[path = "writer_r5_tests.rs"]
mod writer_r5_tests;
