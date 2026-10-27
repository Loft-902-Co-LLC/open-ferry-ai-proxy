//! Independent ON checks and explicit frozen OFF controls for writer-base rows.
use super::*;
use serde_json::{Value, json};

fn values(node: &Node) -> Value {
    json!([
        format!("{:?}", node.kind),
        node.tag,
        format!("{:?}", node.style),
        node.value,
        node.content.iter().map(values).collect::<Vec<_>>()
    ])
}

fn cases() -> Vec<Value> {
    let mut rows: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_r2_cases.json")).unwrap();
    rows.extend(
        serde_json::from_str::<Vec<Value>>(include_str!("../testdata/writer/writer_cases.json"))
            .unwrap(),
    );
    rows
}

fn check_control(source: &str, case: &str, op: &str, disable: &str, repaired: Option<&str>) {
    let frozen: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_r2_main.json")).unwrap();
    let row = json!({"id":format!("{case}/{op}"),"source":source,"op":op,
        "disable_empty":disable=="empty","disable_flow_feet":disable=="feet",
        "params":{"save_auth":["/x","/y"],"nested_keys":["auth-dir"],
            "nested_values":["/x","/y"],"path":if op.starts_with("patch") {vec!["oauth"]} else {vec!["oauth","auth-dir"]},
            "body":if op.starts_with("patch") {r#"{"auth-dir":"/x"}"#} else {r#""/x""#}}});
    owner_legacy_ops::old_scope(&row, || {
        let off = tree::without_owner_protection(|| owner_legacy_ops::history(&row));
        let golden = frozen.iter().find(|r| r[0] == case && r[1] == op).unwrap();
        let expected = if disable.is_empty() {
            &golden[2]
        } else {
            golden.get(3).unwrap_or(&golden[2])
        };
        assert!(
            owner_matrix_tests::receipt(&off) == *expected,
            "{case} {op} {disable} frozen OFF bytes"
        );
        let on = owner_legacy_ops::history(&row);
        assert!(
            !on["refused"].as_bool().unwrap_or(false),
            "{case} {op} {disable} newly refused"
        );
        if let Some(expected) = repaired {
            assert!(
                owner_matrix_tests::receipt(&on) == expected,
                "{case} {op} repaired bytes"
            );
        }
        let mut input = source.as_bytes().to_vec();
        for (pass, file) in on["files"].as_array().unwrap().iter().enumerate() {
            let actual = file.as_str().unwrap().as_bytes();
            let operation = owner_legacy_ops::operation(&input, &row, pass).unwrap();
            let main = tree::without_owner_protection(|| operation.run(&input)).unwrap();
            if actual == main {
                // These fixed controls also include raw proposals rejected to
                // keep the next save from losing text the writer retains.
                let raw_fallbacks: Vec<Value> = serde_json::from_str(include_str!(
                    "../testdata/writer/owner_raw_save_fallbacks.json"
                ))
                .unwrap();
                assert!(
                    case == "legacy_one_foot" && op == "migrate2" && pass == 0
                        || owner_tests::round9::recorded_refusal("r2", case, op, disable, pass)
                        || owner_tests::round10::recorded_refusal("r2", case, op, disable, pass)
                        || raw_fallbacks.iter().any(|row| {
                            row[0] == case && row[1] == op && row[2] == disable && row[3] == pass
                        }),
                    "{case} {op} {disable} pass {pass}: unexpected writer fallback"
                );
                assert!(file == &off["files"][pass]);
                input = actual.to_vec();
                continue;
            }
            let source_owners = owner_legacy_tests::independent_canonical_owners(&input);
            let main_owners = owner_legacy_tests::independent_canonical_owners(&main);
            assert!(
                source_owners
                    .iter()
                    .any(|(o, n)| main_owners.get(o) != Some(n)),
                "{case} {op} {disable} main owner drift"
            );
            assert!(
                source_owners == owner_legacy_tests::independent_canonical_owners(actual),
                "{case} {op} {disable} source owners"
            );
            assert!(
                values(&unmarshal(&main).unwrap()) == values(&unmarshal(actual).unwrap()),
                "{case} {op} {disable} values/styles/tags"
            );
            assert!(
                Config::load_bytes(actual).is_ok(),
                "{case} {op} {disable} loadability"
            );
            assert!(
                operation.run(actual).unwrap() == actual,
                "{case} {op} {disable} repeat bytes"
            );
            input = actual.to_vec();
        }
    });
}

#[test]
fn owner_writer_nonboundary_controls_keep_each_source_owner_and_settle() {
    let cases = cases();
    for (case, op, repaired) in [
        ("two_then_plain", "nested", "52c8b2d7b84a8d7f"),
        ("two_then_plain", "nested2", "5020319580ed2cf9"),
        ("two_then_plain", "raw", "acf23c6ffb9fecfd"),
        ("two_then_plain", "raw2", "e816b9594089447d"),
        ("two_nonempty_after", "nested", "249c37ab29f54205"),
        ("two_nonempty_after", "nested2", "0e5c28d7c5168c16"),
        ("two_nonempty_after", "raw", "cabbaf04b398e250"),
        ("two_nonempty_after", "raw2", "4daf2f673c616c5c"),
        ("two_no_after", "nested", "322f138ecbe6f19b"),
        ("two_no_after", "nested2", "ab2bf793778e9e3b"),
        ("two_no_after", "raw", "d9fd4ff520d0c2fc"),
        ("two_no_after", "raw2", "8fe401fc7393cfe2"),
        ("two_then_sibling_seq", "nested", "5c1285da227c41a1"),
        ("two_then_sibling_seq", "nested2", "62a59ee4d8f2e402"),
        ("two_then_sibling_seq", "raw", "b676821700c7b221"),
        ("two_then_sibling_seq", "raw2", "c265643d5f17499a"),
        ("nested_plugin", "nested", "dd8c737be8b346f0"),
        ("nested_plugin", "nested2", "eb27334ab9125779"),
        ("nested_plugin", "raw", "81ec855fdb458d03"),
        ("nested_plugin", "raw2", "3347fd28511efb46"),
    ] {
        let source = cases.iter().find(|r| r["case"] == case).unwrap()["source"]
            .as_str()
            .unwrap();
        check_control(source, case, op, "", Some(repaired));
    }
}

#[test]
fn owner_writer_second_migration_and_nonboundary_heads_keep_changed_call_owners() {
    let cases = cases();
    for (case, op, repaired) in [
        ("r3_multi_head_nonempty", "raw", "794f8c9b5eb8c75c"),
        ("r3_multi_head_nonempty", "raw2", "dfc3f51262c02b01"),
        ("r3_multi_head_one_foot", "raw", "b493c55f523d4cd4"),
        ("r3_multi_head_one_foot", "raw2", "bfc404859adba44c"),
        ("legacy_one_foot", "migrate2", "73820c6ca0f10dcf"),
    ] {
        let source = cases.iter().find(|r| r["case"] == case).unwrap()["source"]
            .as_str()
            .unwrap();
        check_control(source, case, op, "", Some(repaired));
    }
}

// This is an independently audited, fixed row list. It does not inspect the
// gate, its proposed bytes, or the current operation's result.
#[cfg(feature = "parity")]
pub(super) fn diagnostic_control_drifts(case: &str, op: &str) -> bool {
    match case {
        "flow_first_foot"
        | "flow_two_feet"
        | "two"
        | "three"
        | "three_last_two"
        | "three_first_last"
        | "two_then_plain"
        | "two_4sp"
        | "two_flush_dash"
        | "two_nonempty_after"
        | "two_no_after"
        | "two_then_sibling_seq"
        | "two_multiline"
        | "two_same_text"
        | "two_deeper_feet"
        | "two_crlf"
        | "two_doc"
        | "two_doc_comment"
        | "two_head_comments"
        | "two_line_comments"
        | "two_legacy_key_in_v8"
        | "legacy_two"
        | "two_other_comments"
        | "nested_plugin"
        | "plugin_two_feet"
        | "two_empty_seq_after"
        | "legacy_two_comments"
        | "two_ls_scalar"
        | "two_head_before_empty"
        | "two_blank_head_before_empty"
        | "f2"
        | "o1_plain_seq"
        | "lp2_scalar_mid" => matches!(op, "nested" | "nested2" | "raw" | "raw2"),
        "two_no_blank" => matches!(op, "patch" | "patch2" | "put" | "put2"),
        "random-30-1826" => matches!(op, "nested"),
        "random-30-4096" => matches!(op, "raw"),
        "two_nel_scalar" | "f1" | "f1b" | "o1" | "o1_v8" | "o1_first_seq" => {
            matches!(op, "nested" | "nested2")
        }
        "legacy_one_foot" => matches!(
            op,
            "save" | "save2" | "migrate2" | "nested" | "nested2" | "raw" | "raw2"
        ),
        "one_head_before_empty" => matches!(op, "raw" | "raw2" | "patch" | "patch2"),
        "lp2_keyhead_two" | "r3_multi_head_nonempty" | "r3_multi_head_one_foot" => {
            matches!(op, "raw" | "raw2")
        }
        _ => false,
    }
}

#[cfg(feature = "parity")]
#[test]
fn owner_writer_diagnostic_controls_keep_each_source_owner_under_both_switches() {
    let cases = cases();
    let frozen: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_r2_main.json")).unwrap();
    let mut checked = 0;
    for row in &frozen {
        let case = row[0].as_str().unwrap();
        let op = row[1].as_str().unwrap();
        if !diagnostic_control_drifts(case, op) {
            continue;
        }
        let source = cases.iter().find(|r| r["case"] == case).unwrap()["source"]
            .as_str()
            .unwrap();
        for disable in ["empty", "feet"] {
            check_control(source, case, op, disable, None);
            checked += 1;
        }
    }
    assert_eq!(checked, 334);
}

#[cfg(feature = "parity")]
pub(super) fn r4_diagnostic_control_drifts(
    case: &str,
    op: &str,
    protection: parity::ablation::Protection,
) -> bool {
    use parity::ablation::Protection;
    let disabled = match protection {
        Protection::EmptyMappingPendingComment => "empty",
        Protection::MultipleFlowItemFeet => "feet",
        _ => return false,
    };
    let rows: Vec<Value> = serde_json::from_str(include_str!(
        "../testdata/writer/owner_round7_r4_diagnostics.json"
    ))
    .unwrap();
    assert_eq!(rows.len(), 32);
    rows.iter()
        .any(|row| row[0] == case && row[1] == op && row[2] == disabled)
}

#[cfg(feature = "parity")]
#[test]
fn owner_round7_r4_diagnostic_controls_keep_main_bytes_and_source_owners() {
    let rows: Vec<Value> = serde_json::from_str(include_str!(
        "../testdata/writer/owner_round7_r4_diagnostics.json"
    ))
    .unwrap();
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_r4_cases.json")).unwrap();
    assert_eq!(rows.len(), 32);
    for control in rows {
        let case = control[0].as_str().unwrap();
        let op = control[1].as_str().unwrap();
        let disabled = control[2].as_str().unwrap();
        let source = cases.iter().find(|row| row["case"] == case).unwrap()["source"]
            .as_str()
            .unwrap();
        let row = json!({"source":source,"op":op,"disable_empty":disabled=="empty",
            "disable_flow_feet":disabled=="feet","params":{"nested_keys":["auth-dir"],
            "nested_values":["/x","/y"]}});
        owner_legacy_ops::old_scope(&row, || {
            let off = tree::without_owner_protection(|| owner_legacy_ops::history(&row));
            assert_eq!(
                owner_matrix_tests::receipt(&off),
                control[3],
                "{case} {op} {disabled} main"
            );
            let on = owner_legacy_ops::history(&row);
            assert_eq!(
                owner_matrix_tests::receipt(&on),
                control[4],
                "{case} {op} {disabled} repaired"
            );
            let mut input = source.as_bytes().to_vec();
            for (pass, file) in on["files"].as_array().unwrap().iter().enumerate() {
                let actual = file.as_str().unwrap().as_bytes();
                let operation = owner_legacy_ops::operation(&input, &row, pass).unwrap();
                let main = tree::without_owner_protection(|| operation.run(&input)).unwrap();
                if owner_tests::round9::recorded_refusal("r4", case, op, disabled, pass) {
                    assert_eq!(actual, main);
                    assert_eq!(file, &off["files"][pass]);
                    Config::load_bytes(actual).unwrap();
                    input = actual.to_vec();
                    continue;
                }
                let source_owners = owner_legacy_tests::independent_canonical_owners(&input);
                assert_ne!(
                    source_owners,
                    owner_legacy_tests::independent_canonical_owners(&main)
                );
                assert_eq!(
                    source_owners,
                    owner_legacy_tests::independent_canonical_owners(actual)
                );
                assert_eq!(
                    values(&unmarshal(&main).unwrap()),
                    values(&unmarshal(actual).unwrap())
                );
                Config::load_bytes(actual).unwrap();
                assert_eq!(operation.run(actual).unwrap(), actual);
                input = actual.to_vec();
            }
        });
    }
}

#[cfg(feature = "parity")]
#[test]
fn owner_round5_diagnostic_raw_fallback_keeps_two_step_writer_owners() {
    let rows = cases();
    let source = rows
        .iter()
        .find(|row| row["case"] == "random-30-4096")
        .unwrap()["source"]
        .as_str()
        .unwrap()
        .as_bytes();
    for disable in ["empty", "feet"] {
        let scope = json!({"disable_empty":disable=="empty","disable_flow_feet":disable=="feet"});
        owner_legacy_ops::old_scope(&scope, || {
            let raw = render_write_file(source).unwrap();
            let main = tree::without_owner_protection(|| render_write_file(source)).unwrap();
            assert!(
                raw == main,
                "diagnostic raw fallback changed frozen writer bytes"
            );
            let config = Config::parse(&raw).unwrap();
            for migrate in [false, true] {
                let output = render_preserving_comments(&raw, &config, migrate).unwrap();
                let baseline = tree::without_owner_protection(|| {
                    render_preserving_comments(&main, &config, migrate)
                })
                .unwrap();
                assert!(
                    owner_legacy_tests::independent_canonical_owners(&output)
                        == owner_legacy_tests::independent_canonical_owners(&baseline),
                    "diagnostic raw fallback changed two-step writer owners"
                );
                assert!(
                    values(&unmarshal(&output).unwrap()) == values(&unmarshal(&baseline).unwrap()),
                    "diagnostic raw fallback changed values, styles or tags"
                );
            }
        });
    }
}
