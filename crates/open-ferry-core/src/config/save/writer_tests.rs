use super::super::*;
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

fn operation(source: &str, op: &str) -> Value {
    operation_with_updates(source, op, false)
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

#[test]
fn writer_byte_controls_stay_identical_on_every_operation() {
    let before: Vec<Value> = serde_json::from_str(include_str!("writer_before.json")).unwrap();
    let go: Vec<Value> = serde_json::from_str(include_str!("writer_go.json")).unwrap();
    for case in cases()
        .iter()
        .filter(|case| case["case"] != "flow_two_feet")
    {
        for op in OPS {
            let expected = if case["case"] == "C8_crlf" {
                &go
            } else {
                &before
            };
            let expected = &expected
                .iter()
                .find(|row| row["case"] == case["case"] && row["op"] == op)
                .unwrap()["result"];
            let result = review_operation(case["source"].as_str().unwrap(), op);
            assert_eq!(result, *expected, "{} {op}", case["case"]);
            if ["C8_dashes_comment", "C8_crlf"].contains(&case["case"].as_str().unwrap())
                && result["files"].as_array().unwrap().len() == 2
            {
                let result = operation(case["source"].as_str().unwrap(), op);
                assert_eq!(
                    result["files"][0], result["files"][1],
                    "{} {op}",
                    case["case"]
                );
            }
        }
    }
}

#[test]
fn writer_document_marker_paragraph_protection_keeps_the_version_owner() {
    let source = cases()
        .into_iter()
        .find(|case| case["case"] == "C8_dashes_comment")
        .unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let go: Vec<Value> = serde_json::from_str(include_str!("writer_go.json")).unwrap();
    for op in ["save", "migrate", "raw", "normalize", "patch", "put"] {
        let result = operation(&source, op);
        let output = result["files"][0].as_str().unwrap();
        let doc = unmarshal(output.as_bytes()).unwrap();
        let root = &doc.content[0];
        let i = find_map_key_index(root, "config-version").unwrap();
        assert_eq!(root.content[i].head_comment.trim(), "# k1\n\n# k2");
        let go = go
            .iter()
            .find(|row| row["case"] == "C8_dashes_comment" && row["op"] == op)
            .unwrap()["result"]["files"][0]
            .as_str()
            .unwrap();
        let doc = unmarshal(go.as_bytes()).unwrap();
        let root = &doc.content[0];
        let i = find_map_key_index(root, "config-version").unwrap();
        assert!(!root.content[i].head_comment.contains("# k1"));
        assert!(
            root.content[..i]
                .iter()
                .any(|n| n.foot_comment.contains("# k1"))
        );
        assert_ne!(output, go);
    }
}

#[test]
fn writer_two_flow_feet_save_reload_at_each_item_and_stay_stable() {
    let source = cases()
        .into_iter()
        .find(|case| case["case"] == "flow_two_feet")
        .unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned();
    for op in OPS {
        let result = operation(&source, op);
        if [
            "save", "save2", "migrate", "migrate2", "patch", "patch2", "put", "put2",
        ]
        .contains(&op)
        {
            let before: Vec<Value> =
                serde_json::from_str(include_str!("writer_before.json")).unwrap();
            let expected = &before
                .iter()
                .find(|r| r["case"] == "flow_two_feet" && r["op"] == op)
                .unwrap()["result"];
            assert_eq!(&result, expected, "{op} retains the original refusal");
            continue;
        }
        assert!(result.get("refused").is_none(), "{op}");
        let files = result["files"].as_array().unwrap();
        for file in files {
            let output = file.as_str().unwrap();
            Config::load_bytes(output.as_bytes()).unwrap();
            let doc = unmarshal(output.as_bytes()).unwrap();
            let items = yaml_path(&doc.content[0], "oauth.model-alias.antigravity").unwrap();
            assert_eq!(items.content.len(), 2, "{op}");
            for (index, item) in items.content.iter().enumerate() {
                assert_eq!(
                    item.foot_comment,
                    format!("# f{index}"),
                    "{op} item {index}"
                );
                assert!(item.head_comment.is_empty(), "{op} item {index}");
                let values: Vec<_> = item
                    .pairs()
                    .map(|(k, v)| (k.value.as_str(), v.value.as_str()))
                    .collect();
                assert_eq!(
                    values,
                    if index == 0 {
                        vec![("name", "a"), ("alias", "b")]
                    } else {
                        vec![("name", "c"), ("alias", "d")]
                    },
                    "{op}"
                );
            }
            for comment in ["# f0", "# f1"] {
                assert_eq!(
                    parity::comment_occurrences(output, comment, false),
                    Some(1),
                    "{op}"
                );
            }
        }
        if files.len() == 2 {
            assert_eq!(files[0], files[1], "{op}");
        }
    }
}

#[test]
#[ignore = "offline operation-matrix capture; set FU_WRITER_CAPTURE"]
fn writer_capture_operations() {
    let rows: Vec<_> = cases().iter().flat_map(|case| OPS.map(|op| {
            json!({"case":case["case"], "op":op, "result":review_operation(case["source"].as_str().unwrap(), op)})
    })).collect();
    let destination = std::env::var_os("FU_WRITER_CAPTURE").unwrap();
    std::fs::write(destination, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
}

#[cfg(feature = "parity")]
#[test]
fn writer_flow_feet_counterfactual_has_exact_go_outcomes_and_bytes() {
    let source = cases()
        .into_iter()
        .find(|case| case["case"] == "flow_two_feet")
        .unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let go: Vec<Value> = serde_json::from_str(include_str!("writer_go.json")).unwrap();
    for op in OPS {
        let expected = &go
            .iter()
            .find(|row| row["case"] == "flow_two_feet" && row["op"] == op)
            .unwrap()["result"];
        let result = parity::ablation::with_disabled(
            &[parity::ablation::Protection::MultipleFlowItemFeet],
            || review_operation(&source, op),
        );
        assert_eq!(result, *expected, "{op}");
    }
}

#[test]
fn writer_parent_foot_does_not_gain_an_item_owner() {
    let fixture: Value = serde_json::from_str(include_str!("writer_parent_foot.json")).unwrap();
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, fixture["options"]["file"].as_str().unwrap()).unwrap();
    for (i, step) in fixture["options"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let source = std::fs::read_to_string(&path).unwrap();
        match step["op"].as_str().unwrap() {
            "nested" => {
                let keys: Vec<_> = step["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| k.as_str().unwrap())
                    .collect();
                update_nested_scalar(&path, &keys, step["value"].as_str().unwrap()).unwrap();
            }
            "save" => {
                let config = Config::parse(step["config"].as_str().unwrap_or(&source)).unwrap();
                save_preserving_comments(
                    &path,
                    &config,
                    step["migrate"].as_bool().unwrap_or(false),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let output = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            sha256_hex(output.as_bytes()),
            sha256_hex(fixture["go"]["files"][i].as_str().unwrap().as_bytes()),
            "step {i} keeps the original bytes"
        );
        let analysis = parity::comment_ownership_analysis(
            &source,
            Some(fixture["go"]["files"][i].as_str().unwrap()),
            &output,
            &[],
        );
        assert!(
            analysis.accepted(),
            "step {i}: {:?}",
            analysis.failure.as_ref().map(|f| &f["reason"])
        );
    }
}
