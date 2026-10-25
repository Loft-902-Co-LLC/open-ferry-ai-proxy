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

fn op_run(source: &str, op: &str, update_second_pass: bool) -> Value {
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
        let result: Result<(), String> = match op.trim_end_matches('2') {
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
        if let Err(e) = result {
            assert_eq!(std::fs::read(&path).unwrap(), current);
            return json!({"files": files, "refused": true, "error": e});
        }
        files.push(std::fs::read_to_string(&path).unwrap());
    }
    json!({"files": files})
}

fn owners(text: &str) -> Value {
    fn walk(n: &Node, path: String, out: &mut Vec<Value>) {
        if !n.head_comment.is_empty() || !n.line_comment.is_empty() || !n.foot_comment.is_empty() {
            out.push(json!({"path": path, "head": n.head_comment, "line": n.line_comment, "foot": n.foot_comment}));
        }
        match n.kind {
            Kind::Mapping => {
                let mut i = 0;
                while i + 1 < n.content.len() {
                    let k = &n.content[i];
                    let p = format!("{path}.{}", k.value);
                    walk(k, format!("{p}#key"), out);
                    walk(&n.content[i + 1], p, out);
                    i += 2;
                }
            }
            _ => {
                for (i, c) in n.content.iter().enumerate() {
                    walk(c, format!("{path}[{i}]"), out);
                }
            }
        }
    }
    match unmarshal(text.as_bytes()) {
        Ok(doc) => {
            let mut out = Vec::new();
            walk(&doc, String::new(), &mut out);
            json!({"ok": true, "loads": Config::load_bytes(text.as_bytes()).map(|_| true).unwrap_or_else(|e| { let _ = e; false }), "owners": out, "values": values(&doc)})
        }
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

fn values(n: &Node) -> Value {
    match n.kind {
        Kind::Document => n.content.first().map(values).unwrap_or(Value::Null),
        Kind::Mapping => {
            let mut m = serde_json::Map::new();
            let mut i = 0;
            while i + 1 < n.content.len() {
                m.insert(n.content[i].value.clone(), values(&n.content[i + 1]));
                i += 2;
            }
            Value::Object(m)
        }
        Kind::Sequence => Value::Array(n.content.iter().map(values).collect()),
        Kind::Alias => json!({"alias": n.value}),
        _ => Value::String(n.value.clone()),
    }
}

fn annotate(mut v: Value) -> Value {
    let o: Vec<Value> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| owners(f.as_str().unwrap()))
        .collect();
    v["owners"] = Value::Array(o);
    v
}

#[test]
#[ignore]
fn writer_r2_capture() {
    let input = std::env::var_os("FU_R2_IN").unwrap();
    let output = std::env::var_os("FU_R2_OUT").unwrap();
    let cases: Vec<Value> = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    let mut rows = Vec::new();
    for case in &cases {
        let source = case["source"].as_str().unwrap();
        let ops: Vec<String> = case
            .get("ops")
            .and_then(|o| o.as_array())
            .map(|a| a.iter().map(|x| x.as_str().unwrap().to_owned()).collect())
            .unwrap_or_else(|| OPS.iter().map(|s| s.to_string()).collect());
        for op in &ops {
            let review = annotate(op_run(source, op, true));
            let same = if op.ends_with('2') {
                Some(op_run(source, op, false))
            } else {
                None
            };
            #[cfg(feature = "parity")]
            let ablated = Some(parity::ablation::with_disabled(
                &[parity::ablation::Protection::EmptyMappingPendingComment],
                || op_run(source, op, true),
            ));
            #[cfg(not(feature = "parity"))]
            let ablated: Option<Value> = None;
            #[cfg(feature = "parity")]
            let disabled_new = Some(parity::ablation::with_disabled(
                &[parity::ablation::Protection::MultipleFlowItemFeet],
                || op_run(source, op, true),
            ));
            #[cfg(not(feature = "parity"))]
            let disabled_new: Option<Value> = None;
            rows.push(json!({"case": case["case"], "op": op, "source_owners":owners(source), "result": review, "same": same, "ablated": ablated,"disabled_new":disabled_new}));
        }
    }
    std::fs::write(output, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
}

pub(super) fn receipt(result: &Value) -> Value {
    let summary = json!({
        "files": result["files"].as_array().unwrap().iter().map(|s| sha256_hex(s.as_str().unwrap().as_bytes())).collect::<Vec<_>>(),
        "refused": result["refused"].as_bool().unwrap_or(false),
        "error": result["error"].as_str().map(|s| sha256_hex(s.as_bytes())),
    });
    let ordered: std::collections::BTreeMap<_, _> = summary.as_object().unwrap().iter().collect();
    Value::String(sha256_hex(&serde_json::to_vec(&ordered).unwrap())[..16].to_owned())
}

pub(super) fn main_rows() -> Vec<Value> {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_r2_main.json")).unwrap();
    rows.into_iter()
        .map(|r| json!({"case":r[0],"op":r[1],"normal":r[2],"old_off":r.get(3).unwrap_or(&r[2])}))
        .collect()
}

fn source(case: &str) -> String {
    let mut cases: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_r2_cases.json")).unwrap();
    cases.extend(
        serde_json::from_str::<Vec<Value>>(include_str!("../testdata/writer/writer_cases.json"))
            .unwrap(),
    );
    cases.into_iter().find(|r| r["case"] == case).unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn inventory(source: &str) -> std::collections::BTreeMap<(String, String, String), usize> {
    let mut result = std::collections::BTreeMap::new();
    for owner in owners(source)["owners"].as_array().unwrap() {
        for slot in ["head", "line", "foot"] {
            for text in owner[slot]
                .as_str()
                .unwrap()
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                *result
                    .entry((
                        owner["path"].as_str().unwrap().to_owned(),
                        slot.to_owned(),
                        text.to_owned(),
                    ))
                    .or_default() += 1;
            }
        }
    }
    result
}

fn config_values(config: &Config) -> Value {
    fn project(value: &encode::Value) -> Value {
        use encode::Value as Y;
        match value {
            Y::Null => Value::Null,
            Y::Bool(v) => json!(v),
            Y::Int(v) | Y::Duration(v) => json!(v),
            Y::Uint(v) => json!(v),
            Y::Float(v) => json!(v),
            Y::Str(v) => json!(v),
            Y::Seq(items) => Value::Array(items.iter().map(project).collect()),
            Y::Map(pairs) => Value::Object(
                pairs
                    .iter()
                    .map(|(k, v)| (project(k).as_str().unwrap().to_owned(), project(v)))
                    .collect(),
            ),
            Y::Struct(fields) => Value::Object(
                fields
                    .iter()
                    .map(|f| (f.key.clone(), project(&f.value)))
                    .collect(),
            ),
            Y::Node(node) => values(node),
        }
    }
    project(&generate::legacy_config(config).unwrap())
}

fn crlf_exception(case: &str, op: &str) -> bool {
    case == "C8_crlf" && ["raw", "raw2", "normalize", "normalize2"].contains(&op)
}

#[test]
fn writer_r2_review_shapes_preserve_main_or_prove_new_outputs() {
    let golden = main_rows();
    for row in golden {
        let case = row["case"].as_str().unwrap();
        let op = row["op"].as_str().unwrap();
        let source = source(case);
        let result = op_run(&source, op, true);
        if receipt(&result) == row["normal"] || crlf_exception(case, op) {
            continue;
        }
        // A changed refusal or a newly accepted prefix is not an approved
        // substitute for main's refusal. Every changed write is checked.
        assert!(
            !result["refused"].as_bool().unwrap_or(false),
            "{case} {op} changed a refusal"
        );
        let expected_owners = inventory(&source);
        for (i, file) in result["files"].as_array().unwrap().iter().enumerate() {
            let output = file.as_str().unwrap();
            assert!(
                inventory(output) == expected_owners,
                "{case} {op} snapshot {i} moved a comment"
            );
            let mut expected = Config::parse(&source).unwrap();
            if ["save", "migrate", "nested", "patch", "put"].contains(&op.trim_end_matches('2')) {
                expected.auth_dir =
                    if i > 0 && !["patch", "put"].contains(&op.trim_end_matches('2')) {
                        "/y"
                    } else {
                        "/x"
                    }
                    .into();
            }
            let actual = Config::parse(output).unwrap();
            assert!(
                config_values(&expected) == config_values(&actual),
                "{case} {op} snapshot {i} changed a value"
            );
        }
        if !op.ends_with('2') {
            let repeat = op_run(&source, &format!("{op}2"), false);
            assert!(
                !repeat["refused"].as_bool().unwrap_or(false),
                "{case} {op} refused its repeat"
            );
            assert_eq!(repeat["files"].as_array().unwrap().len(), 2, "{case} {op}");
            assert!(
                repeat["files"][0] == repeat["files"][1],
                "{case} {op} did not settle"
            );
        }
    }
}

#[cfg(feature = "parity")]
#[test]
fn writer_r2_each_switch_retains_main_bytes_and_refusals() {
    use parity::ablation::{Protection, with_disabled};
    let golden = main_rows();
    for row in golden {
        let case = row["case"].as_str().unwrap();
        let op = row["op"].as_str().unwrap();
        let expected = if crlf_exception(case, op) {
            let go: Vec<Value> =
                serde_json::from_str(include_str!("../testdata/writer/writer_go.json")).unwrap();
            let row = go
                .iter()
                .find(|r| r["case"] == "C8_crlf" && r["op"] == op)
                .unwrap();
            row["receipt"].clone()
        } else {
            row["normal"].clone()
        };
        let expected_old = if crlf_exception(case, op) {
            expected.clone()
        } else {
            row["old_off"].clone()
        };
        let source = source(case);
        let new_off = with_disabled(&[Protection::MultipleFlowItemFeet], || {
            op_run(&source, op, true)
        });
        assert_eq!(receipt(&new_off), expected, "{case} {op} new switch");
        let old_off = with_disabled(&[Protection::EmptyMappingPendingComment], || {
            op_run(&source, op, true)
        });
        assert_eq!(
            receipt(&old_off),
            expected_old,
            "{case} {op} existing switch"
        );
    }
}

#[test]
fn writer_r2_layout_restore_keeps_legacy_positions() {
    for case in [
        "f2",
        "legacy_two",
        "legacy_two_comments",
        "two_legacy_key_in_v8",
    ] {
        let source = source(case);
        let doc = unmarshal(source.as_bytes()).unwrap();
        let layout = &doc.content[0];
        let mut root = super::flatten_v8(layout).unwrap();
        let cfg = Config::parse(&source).unwrap();
        super::restore_v8_layout(&mut root, layout, source.as_bytes(), &cfg).unwrap();
        for (old, current) in [
            ("auth-dir", "oauth.auth-dir"),
            ("proxy-url", "requests.proxy-url"),
        ] {
            if yaml_path(layout, old).is_some() && yaml_path(layout, current).is_none() {
                assert!(yaml_path(&root, old).is_some(), "{case} moved {old}");
                assert!(
                    yaml_path(&root, current).is_none(),
                    "{case} created {current}"
                );
            }
        }
    }
}

#[test]
fn writer_r2_nonboundary_and_excluded_model_saves_keep_main_bytes() {
    let golden = main_rows();
    for row in golden {
        let case = row["case"].as_str().unwrap();
        let op = row["op"].as_str().unwrap();
        let nonboundary = [
            "two_nonempty_after",
            "two_no_after",
            "two_then_sibling_seq",
            "two_then_plain",
            "nested_plugin",
        ]
        .contains(&case);
        let reordered_save = case == "two_empty_seq_after"
            && ["save", "migrate", "patch", "put"].contains(&op.trim_end_matches('2'));
        if nonboundary || reordered_save {
            assert_eq!(
                receipt(&op_run(&source(case), op, true)),
                row["normal"],
                "{case} {op}"
            );
        }
    }
}

#[test]
fn writer_r2_quoted_scalar_probes_keep_main_refusals() {
    let golden = main_rows();
    for row in golden
        .into_iter()
        .filter(|r| ["f1", "f1b"].contains(&r["case"].as_str().unwrap()))
    {
        let case = row["case"].as_str().unwrap();
        let op = row["op"].as_str().unwrap();
        assert_eq!(
            receipt(&op_run(&source(case), op, true)),
            row["normal"],
            "{case} {op}"
        );
    }
}

#[test]
fn writer_r2_following_empty_key_keeps_its_head() {
    for case in [
        "two_head_before_empty",
        "two_blank_head_before_empty",
        "two_other_comments",
    ] {
        let source = source(case);
        let expected = inventory(&source);
        for op in ["raw2", "nested2", "normalize2"] {
            let output = op_run(&source, op, false);
            assert!(!output["refused"].as_bool().unwrap_or(false), "{case} {op}");
            assert_eq!(output["files"].as_array().unwrap().len(), 2, "{case} {op}");
            for file in output["files"].as_array().unwrap() {
                assert!(
                    inventory(file.as_str().unwrap()) == expected,
                    "{case} {op} changed a head owner"
                );
            }
            assert!(
                output["files"][0] == output["files"][1],
                "{case} {op} did not settle"
            );
        }
    }
}

#[test]
fn writer_r2_already_faithful_normalized_heads_keep_main_bytes() {
    let golden = main_rows();
    for row in golden {
        let case = row["case"].as_str().unwrap();
        let op = row["op"].as_str().unwrap();
        if [
            "two_head_before_empty",
            "two_blank_head_before_empty",
            "two_other_comments",
        ]
        .contains(&case)
            && op.starts_with("normalize")
        {
            assert_eq!(
                receipt(&op_run(&source(case), op, true)),
                row["normal"],
                "{case} {op}"
            );
        }
    }
}

#[test]
fn writer_r3_two_paragraph_head_keeps_its_owner() {
    let source = source("lp2_keyhead_two");
    for op in ["nested2", "raw2"] {
        let result = op_run(&source, op, false);
        assert!(
            !result["refused"].as_bool().unwrap_or(false),
            "{op} refuses"
        );
        assert_eq!(result["files"].as_array().unwrap().len(), 2, "{op}");
        for file in result["files"].as_array().unwrap() {
            let output = file.as_str().unwrap();
            assert!(
                inventory(output) == inventory(&source),
                "{op} moved a comment"
            );
            let doc = unmarshal(output.as_bytes()).unwrap();
            let demo = yaml_path(&doc.content[0], "plugins.configs.demo").unwrap();
            let key = find_map_key_index(demo, "empty").unwrap();
            assert_eq!(
                demo.content[key].head_comment.trim(),
                "# r1\n\n# r2",
                "{op} following head paragraphs"
            );
        }
        assert!(
            result["files"][0] == result["files"][1],
            "{op} does not settle"
        );
    }
}

#[test]
fn writer_r3_separator_requires_the_flow_foot_boundary() {
    for row in main_rows().into_iter().filter(|r| {
        ["r3_multi_head_nonempty", "r3_multi_head_one_foot"].contains(&r["case"].as_str().unwrap())
    }) {
        let case = row["case"].as_str().unwrap();
        let op = row["op"].as_str().unwrap();
        assert_eq!(
            receipt(&op_run(&source(case), op, true)),
            row["normal"],
            "{case} {op} must keep main's bytes"
        );
    }
}

#[cfg(feature = "parity")]
#[test]
fn writer_r3_separator_does_not_change_faithful_yaml() {
    use parity::ablation::{Protection, with_disabled};
    let doc = unmarshal(source("lp2_keyhead_two").as_bytes()).unwrap();
    let faithful = || crate::config::yaml3::encode::faithful_node(&doc, 2).unwrap();
    let expected = sha256_hex(&faithful());
    for protection in [
        Protection::MultipleFlowItemFeet,
        Protection::EmptyMappingPendingComment,
    ] {
        assert_eq!(
            sha256_hex(&with_disabled(&[protection], faithful)),
            expected,
            "config protection leaked into faithful YAML"
        );
    }
}
