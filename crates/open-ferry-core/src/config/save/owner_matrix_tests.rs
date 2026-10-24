use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8};
use serde_json::{Value, json};

const OPS: [&str; 14] = [
    "save",
    "save2",
    "migrate",
    "migrate2",
    "nested",
    "nested2",
    "raw",
    "raw2",
    "normalize",
    "normalize2",
    "patch",
    "patch2",
    "put",
    "put2",
];

fn cases() -> Vec<Value> {
    serde_json::from_str(include_str!("writer_cases.json")).unwrap()
}

fn review_operation(source: &str, op: &str) -> Value {
    operation_with_updates(source, op, true)
}

fn operation_with_updates(source: &str, op: &str, update_second_pass: bool) -> Value {
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source).unwrap();
    let mut files = Vec::new();
    for pass in 0..if op.ends_with('2') { 2 } else { 1 } {
        let value = if update_second_pass && pass > 0 {
            "/y"
        } else {
            "/x"
        };
        let current = std::fs::read(&path).unwrap();
        let result = match op.trim_end_matches('2') {
            "save" | "migrate" => {
                Config::load(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|mut config| {
                        config.auth_dir = value.into();
                        save_preserving_comments(&path, &config, op.starts_with("migrate"))
                            .map_err(|e| e.to_string())
                    })
            }
            "nested" => {
                update_nested_scalar(&path, &["auth-dir"], value).map_err(|e| e.to_string())
            }
            "raw" => write_file(&path, &current).map_err(|e| e.to_string()),
            "normalize" => normalize_config_layout(&current, true)
                .map_err(|e| e.to_string())
                .map(|(bytes, _)| std::fs::write(&path, bytes).unwrap()),
            "patch" | "put" => {
                let patch = op.starts_with("patch");
                edit_v8(
                    &path,
                    &V8Edit {
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
                    },
                )
                .map(|_| ())
                .map_err(|e| e.to_string())
            }
            _ => unreachable!(),
        };
        if result.is_err() {
            assert_eq!(std::fs::read(&path).unwrap(), current);
            return json!({"files": files, "refused": true});
        }
        files.push(std::fs::read_to_string(&path).unwrap());
    }
    json!({"files": files})
}

#[cfg(feature = "parity")]
#[test]
fn owner_switch_reproduces_the_base_matrix() {
    let before: Vec<Value> = serde_json::from_str(include_str!("owner_before.json")).unwrap();
    assert_eq!(before.len(), cases().len() * OPS.len());
    for row in before {
        let case = cases()
            .into_iter()
            .find(|c| c["case"] == row["case"])
            .unwrap();
        let result = parity::ablation::with_disabled(
            &[parity::ablation::Protection::DeferredCommentOwners],
            || {
                review_operation(
                    case["source"].as_str().unwrap(),
                    row["op"].as_str().unwrap(),
                )
            },
        );
        assert!(
            result == row["result"],
            "{} {} disabled bytes changed",
            row["case"],
            row["op"]
        );
    }
}

#[test]
fn owner_protection_keeps_all_faithful_writer_control_bytes() {
    let before: Vec<Value> = serde_json::from_str(include_str!("owner_before.json")).unwrap();
    assert_eq!(before.len(), cases().len() * OPS.len());
    for row in before {
        let case = cases()
            .into_iter()
            .find(|c| c["case"] == row["case"])
            .unwrap();
        let result = review_operation(
            case["source"].as_str().unwrap(),
            row["op"].as_str().unwrap(),
        );
        let repaired = row["case"] == "flow_first_foot"
            && matches!(
                row["op"].as_str().unwrap(),
                "nested" | "nested2" | "raw" | "raw2"
            );
        if repaired {
            let after: Vec<Value> = serde_json::from_str(include_str!("owner_after.json")).unwrap();
            let expected = &after
                .iter()
                .find(|r| r["case"] == row["case"] && r["op"] == row["op"])
                .unwrap()["result"];
            assert!(
                result == *expected,
                "{} {} repaired bytes",
                row["case"],
                row["op"]
            );
            let original =
                super::owner_tests::independent_owners(case["source"].as_str().unwrap().as_bytes());
            for file in result["files"].as_array().unwrap() {
                assert!(
                    original
                        == super::owner_tests::independent_owners(
                            file.as_str().unwrap().as_bytes()
                        ),
                    "{} {} item foot owner",
                    row["case"],
                    row["op"]
                );
            }
        } else {
            assert!(
                result == row["result"],
                "{} {} bytes changed",
                row["case"],
                row["op"]
            );
        }
    }
}

#[test]
fn owner_final_gate_returns_faithful_bytes_even_if_a_proposal_differs() {
    let mut exercised = 0;
    for case in cases() {
        let data = case["source"].as_str().unwrap().as_bytes();
        let Ok((candidate, source)) = candidate_nested_scalar(data, &["auth-dir"], "/x") else {
            continue;
        };
        let Ok(parsed) = tree::read_receipted(&candidate) else {
            continue;
        };
        if !source.holds(&parsed) {
            continue;
        }
        let before = candidate.bytes.clone();
        let proposal = tree::repair_comment_candidate(&source, candidate);
        if proposal.bytes == before {
            continue;
        }
        exercised += 1;
        assert!(
            render_nested_scalar(data, &["auth-dir"], "/x").unwrap() == before,
            "faithful gate changed {}",
            case["case"]
        );
    }
    assert!(exercised > 0, "faithful alternative was not exercised");
}

#[test]
fn owner_four_repaired_controls_repeat_without_changes() {
    let source = cases()
        .into_iter()
        .find(|case| case["case"] == "flow_first_foot")
        .unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned();
    for op in ["nested2", "raw2"] {
        let result = operation_with_updates(&source, op, false);
        assert!(
            result["files"][0] == result["files"][1],
            "{op} repeated setting changed bytes"
        );
        assert!(!result["files"][0].as_str().unwrap().is_empty());
    }
}
