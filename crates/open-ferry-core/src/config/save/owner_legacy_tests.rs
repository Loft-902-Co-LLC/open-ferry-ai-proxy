//! Re-read physical emission bindings for the narrowly authorized legacy rows.
use super::super::yaml3::{CommentId, CommentSlot};
use super::owner_legacy_ops::{self as ops, Operation};
use super::*;
use serde_json::{Value, json};
fn occurrences(root: &Node, wanted: &CommentId) -> Vec<Value> {
    fn walk(n: &Node, id: &CommentId, path: &str, out: &mut Vec<Value>) {
        for (slot, text, spans) in [
            (CommentSlot::Head, &n.head_comment, &n.comment_receipts.head),
            (CommentSlot::Line, &n.line_comment, &n.comment_receipts.line),
            (CommentSlot::Foot, &n.foot_comment, &n.comment_receipts.foot),
        ] {
            for span in spans.iter().filter(|s| s.receipt.id == *id) {
                let part = text.as_bytes()[..span.at]
                    .iter()
                    .filter(|b| **b == b'\n')
                    .count();
                out.push(json!({"path":path,"slot":format!("{slot:?}"),"part":part,"same_owner":n.comment_receipts.owner.as_ref()==Some(&span.receipt.owner),"same_slot":slot==span.receipt.slot,"same_part":part==span.receipt.ordinal}));
            }
        }
        match n.kind {
            Kind::Mapping => {
                for (k, v) in n.pairs() {
                    let p = format!("{path}.{}", k.value);
                    walk(k, id, &format!("{p}:key"), out);
                    walk(v, id, &p, out);
                }
            }
            Kind::Document => {
                for c in &n.content {
                    walk(c, id, path, out);
                }
            }
            _ => {
                for (i, c) in n.content.iter().enumerate() {
                    walk(c, id, &format!("{path}[{i}]"), out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, wanted, "$", &mut out);
    out
}
fn values(n: &Node) -> Value {
    json!([
        format!("{:?}", n.kind),
        n.short_tag(),
        format!("{:?}", n.style),
        n.value,
        n.content.iter().map(values).collect::<Vec<_>>()
    ])
}
fn prove_step(data: &[u8], op: &Operation, actual: &[u8]) -> Result<Value, String> {
    let (candidate, source) = tree::with_owner_locations(|| op.candidate(data))?;
    let main = tree::without_owner_protection(|| op.run(data))?;
    if candidate.bytes != main {
        return Err("tracked candidate differs from frozen-path output".into());
    }
    let parsed_main = tree::read_receipted(&candidate).map_err(|_| "main parse")?;
    let faithful = source.holds(&parsed_main);
    let proposed = if faithful {
        candidate
    } else {
        tree::repair_comment_candidate(&source, candidate)
    };
    let parsed_new = tree::read_receipted(&proposed).map_err(|_| "new parse")?;
    let holds = source.holds(&parsed_new);
    let emitted = proposed.bytes == actual;
    let same_values = values(&parsed_main) == values(&parsed_new)
        && crate::config::AnyValue::parse_yaml(std::str::from_utf8(&main).unwrap()).ok()
            == crate::config::AnyValue::parse_yaml(std::str::from_utf8(actual).unwrap()).ok();
    let stable = op.run(actual).is_ok_and(|b| b == actual);
    let mut receipts = Vec::new();
    for (index, comment) in source.comments.iter().enumerate() {
        let src = source.original_locations.get(&comment.receipt.owner);
        receipts.push(json!({"occurrence":index,"source":{"path":src,"slot":format!("{:?}",comment.receipt.slot),"part":comment.receipt.ordinal},"main":occurrences(&parsed_main,&comment.receipt.id),"new":occurrences(&parsed_new,&comment.receipt.id)}));
    }
    Ok(
        json!({"faithful_main":faithful,"complete":source.complete,"new_holds":holds,"new_equals_actual":emitted,"same_values":same_values,"stable":stable,"receipts":receipts}),
    )
}
fn audit(row: &Value) -> Value {
    ops::old_scope(row, || {
        let off = tree::without_owner_protection(|| ops::history(row));
        let on = ops::history(row);
        let mut data = row["source"].as_str().unwrap().as_bytes().to_vec();
        let mut proofs = Vec::new();
        let mut independent = Vec::new();
        for (pass, actual) in on["files"].as_array().unwrap().iter().enumerate() {
            let result = ops::operation(&data, row, pass)
                .and_then(|op| prove_step(&data, &op, actual.as_str().unwrap().as_bytes()));
            match result {
                Ok(proof) => proofs.push(proof),
                Err(error) => proofs.push(json!({"audit_error":error})),
            }
            let proof = ops::operation(&data, row, pass).and_then(|op| {
                let main = tree::without_owner_protection(|| op.run(&data))?;
                Ok(if main == actual.as_str().unwrap().as_bytes() {
                    Value::Null
                } else {
                    independent_step_proof(&data, &main, actual.as_str().unwrap().as_bytes())
                })
            });
            independent.push(proof.unwrap_or_else(|error| json!({"audit_error":error})));
            data = actual.as_str().unwrap().as_bytes().to_vec();
        }
        json!({"id":row["id"],"changed":off!=on,"off":off,"on":on,"proofs":proofs,"independent":independent})
    })
}
#[test]
#[ignore = "offline legacy scope audit"]
fn owner_audit_legacy_rows() {
    let rows: Vec<Value> = serde_json::from_slice(
        &std::fs::read(std::env::var_os("FU_OWNERS_LEGACY_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    let result: Vec<_> = rows.iter().map(audit).collect();
    std::fs::write(
        std::env::var_os("FU_OWNERS_LEGACY_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "offline validation against the existing test helpers"]
fn owner_validate_legacy_recipes() {
    let rows: Vec<Value> = serde_json::from_slice(
        &std::fs::read(std::env::var_os("FU_OWNERS_LEGACY_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    let mut failures = Vec::new();
    for row in &rows {
        let group = row["group"].as_str().unwrap();
        let source = row["source"].as_str().unwrap();
        let op = row["op"].as_str().unwrap();
        let actual = match group {
            "r10" | "r11" if op != "nested" => Some(
                super::review_r10_tests::outputs(source, op).map(|files| json!({"files":files})),
            ),
            "r7" => Some(
                super::review_r7_tests::exact_review_output(row)
                    .map(|file| json!({"files":[file]})),
            ),
            _ => None,
        };
        if let Some(Ok(actual)) = actual
            && actual != ops::history(row)
        {
            failures.push(row["id"].clone());
        }
    }
    std::fs::write(
        std::env::var_os("FU_OWNERS_LEGACY_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&failures).unwrap(),
    )
    .unwrap();
}

fn repaired_rows() -> Vec<Value> {
    ops::resolve_sources(
        serde_json::from_str(include_str!("../testdata/writer/owner_legacy.json")).unwrap(),
    )
}
pub(super) fn independent_canonical_owners(data: &[u8]) -> super::owner_tests::OwnerInventory {
    let mut paths: Vec<_> = super::super::v8::V8_PATHS
        .iter()
        .chain(super::super::layout::V8_STRUCT_PATHS)
        .chain(super::super::v8::V8_SHARED_STRUCT_PATHS)
        .chain(v8::v8_aliases())
        .copied()
        .collect();
    paths.sort_by_key(|(old, _)| std::cmp::Reverse(old.split('.').count()));
    let mut result = std::collections::BTreeMap::new();
    for ((mut path, side, slot, part, hash), count) in super::owner_tests::independent_owners(data)
    {
        for (old, current) in &paths {
            let old: Vec<_> = old.split('.').map(|s| format!("!!str:{s}")).collect();
            if path.get(1..).is_some_and(|p| p.starts_with(&old)) {
                path.splice(
                    1..1 + old.len(),
                    current.split('.').map(|s| format!("!!str:{s}")),
                );
                break;
            }
        }
        *result.entry((path, side, slot, part, hash)).or_insert(0) += count;
    }
    result
}

fn independent_step_proof(input: &[u8], main: &[u8], output: &[u8]) -> Value {
    let source = independent_canonical_owners(input);
    let main = independent_canonical_owners(main);
    let actual = independent_canonical_owners(output);
    let drift = source
        .iter()
        .any(|(owner, count)| main.get(owner) != Some(count));
    let held = source
        .iter()
        .all(|(owner, count)| actual.get(owner) == Some(count));
    let totals = |owners: &super::owner_tests::OwnerInventory| {
        let mut counts = std::collections::BTreeMap::new();
        for ((_, _, _, _, hash), count) in owners {
            *counts.entry(hash.clone()).or_insert(0) += count;
        }
        counts
    };
    let source_counts = totals(&source);
    let main_counts = totals(&main);
    let multiplicity = totals(&actual).iter().all(|(hash, count)| {
        *count
            <= source_counts
                .get(hash)
                .copied()
                .unwrap_or_default()
                .max(main_counts.get(hash).copied().unwrap_or_default())
    });
    json!({"main_drift":drift,"source_owners_held":held,"multiplicity":multiplicity})
}

fn verify_repaired_row(row: &Value) {
    let result = audit(&row["recipe"]);
    let id = row["recipe"]["id"].as_str().unwrap();
    assert!(
        super::owner_matrix_tests::receipt(&result["off"]) == row["main"],
        "{id}: frozen main outcome"
    );
    assert!(result["on"] == row["on"], "{id}: repaired bytes/outcome");
    assert!(
        result["proofs"] == row["proofs"],
        "{id}: occurrence owner map"
    );
    // Re-parse source, main and actual output with an independent path walker.
    // Static v8 path moves identify the same setting across layout changes.
    let mut input = row["recipe"]["source"]
        .as_str()
        .unwrap()
        .as_bytes()
        .to_vec();
    for (pass, file) in result["on"]["files"].as_array().unwrap().iter().enumerate() {
        let output = file.as_str().unwrap().as_bytes();
        let source_owners = independent_canonical_owners(&input);
        let main = ops::old_scope(&row["recipe"], || {
            tree::without_owner_protection(|| {
                ops::operation(&input, &row["recipe"], pass)
                    .unwrap()
                    .run(&input)
            })
        })
        .unwrap();
        let main_owners = independent_canonical_owners(&main);
        let actual_owners = independent_canonical_owners(output);
        assert!(
            source_owners
                .iter()
                .any(|(owner, count)| main_owners.get(owner) != Some(count)),
            "{id}: independent main owner check did not demonstrate drift"
        );
        for (owner, count) in &source_owners {
            assert_eq!(
                actual_owners.get(owner),
                Some(count),
                "{id}: independent source owner inventory"
            );
        }
        // Generated config comments are distinct from source occurrences. They
        // may remain where main already writes them, without replacing a source owner.
        fn totals(
            owners: &super::owner_tests::OwnerInventory,
        ) -> std::collections::BTreeMap<&str, usize> {
            let mut out = std::collections::BTreeMap::new();
            for ((_, _, _, _, hash), count) in owners {
                *out.entry(hash.as_str()).or_insert(0) += count;
            }
            out
        }
        let source_counts = totals(&source_owners);
        let main_counts = totals(&main_owners);
        for (hash, count) in totals(&actual_owners) {
            assert!(
                count
                    <= source_counts
                        .get(hash)
                        .copied()
                        .unwrap_or_default()
                        .max(main_counts.get(hash).copied().unwrap_or_default()),
                "{id}: independent comment multiplicity"
            );
        }
        input = output.to_vec();
    }
    for proof in result["proofs"].as_array().unwrap() {
        assert!(proof["faithful_main"] == false, "{id}: main did not drift");
        for check in [
            "complete",
            "new_holds",
            "new_equals_actual",
            "same_values",
            "stable",
        ] {
            assert!(proof[check] == true, "{id}: {check}");
        }
    }
}
/// Static, independently audited drift controls. Each caller explicitly runs
/// its original operation OFF; unchanged rows continue to run ON.
pub(super) fn has_frozen_main_control(group: &str, source: &str, op: &str) -> bool {
    repaired_rows().iter().any(|row| {
        let recipe = &row["recipe"];
        recipe["group"] == group && recipe["source"] == source && recipe["op"] == op
    })
}
#[test]
fn owner_legacy_repairs_have_frozen_main_bytes_and_complete_source_owners() {
    let mut count = 0;
    for row in repaired_rows() {
        if !cfg!(feature = "parity") && row["recipe"]["disable_empty"] == true {
            continue;
        }
        verify_repaired_row(&row);
        count += 1;
    }
    assert_eq!(count, if cfg!(feature = "parity") { 35 } else { 31 });
}
#[cfg(feature = "parity")]
fn sweep_operation(data: &[u8], step: &Value) -> Result<Operation, String> {
    match step["op"].as_str().unwrap() {
        "save" => {
            let text = step["config"]
                .as_str()
                .unwrap_or(std::str::from_utf8(data).unwrap());
            let config = Config::parse(text).map_err(|e| format!("config: {e}"))?;
            Ok(Operation::Save(
                Box::new(config),
                step["migrate"].as_bool().unwrap_or(false),
            ))
        }
        "nested" => Ok(Operation::Nested(
            step["keys"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| k.as_str().unwrap().to_owned())
                .collect(),
            step["value"].as_str().unwrap().to_owned(),
        )),
        "write" => Ok(Operation::Write),
        _ => panic!("unsupported sweep operation"),
    }
}
#[cfg(feature = "parity")]
#[test]
#[ignore = "offline persisted output, value and occurrence-owner audit"]
fn owner_audit_persisted_sweeps() {
    let rows: Vec<Value> = serde_json::from_slice(
        &std::fs::read(std::env::var_os("FU_OWNERS_OUTPUT_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    let mut results = Vec::new();
    let mut mismatches = 0;
    for row in rows {
        let options = &row["options"];
        let off =
            tree::without_owner_protection(|| super::owner_evidence_tests::replay_steps(options));
        let on = super::owner_evidence_tests::replay_steps(options);
        let off_matches = row.get("before").is_none_or(|expected| *expected == off);
        let on_matches = row.get("after").is_none_or(|expected| *expected == on);
        mismatches += usize::from(!off_matches || !on_matches);
        let mut data = options["file"]
            .as_str()
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        let mut steps = Vec::new();
        for (index, (step, actual)) in options["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(on["files"].as_array().unwrap())
            .enumerate()
        {
            let input = if step["op"] == "write" {
                step["body"].as_str().unwrap().as_bytes()
            } else {
                &data
            };
            let actual = actual.as_str().unwrap();
            let operation = sweep_operation(input, step).unwrap();
            let candidate = operation.candidate(input).unwrap();
            let call_changed = candidate.0.bytes != actual.as_bytes();
            let proof = prove_step(input, &operation, actual.as_bytes())
                .unwrap_or_else(|error| json!({"audit_error":error}));
            let independent = if call_changed {
                independent_step_proof(input, &candidate.0.bytes, actual.as_bytes())
            } else {
                Value::Null
            };
            let good = !call_changed
                || (proof["faithful_main"] == false
                    && [
                        "complete",
                        "new_holds",
                        "new_equals_actual",
                        "same_values",
                        "stable",
                    ]
                    .iter()
                    .all(|key| proof[*key] == true)
                    && ["main_drift", "source_owners_held", "multiplicity"]
                        .iter()
                        .all(|key| independent[*key] == true));
            mismatches += usize::from(!good);
            let go = row["go"]["files"].get(index).and_then(Value::as_str);
            let go_loadable = go.is_some_and(|g| {
                Config::load_bytes(g.as_bytes()).is_ok() && parity::single_document(g)
            });
            let source_owners = independent_canonical_owners(input);
            let holds = |owners: &super::owner_tests::OwnerInventory| {
                source_owners
                    .iter()
                    .all(|(owner, count)| owners.get(owner) == Some(count))
            };
            let independent_rust_holds = holds(&independent_canonical_owners(actual.as_bytes()));
            let independent_go_holds = if go_loadable {
                go.map(|bytes| holds(&independent_canonical_owners(bytes.as_bytes())))
            } else {
                None
            };
            let main_inventory = super::owner_evidence_tests::inventory(
                std::str::from_utf8(&candidate.0.bytes).unwrap(),
                false,
            );
            let config_inventory = step["config"]
                .as_str()
                .map(|config| super::owner_evidence_tests::inventory(config, true));
            let certifier = if go_loadable {
                let analysis = parity::comment_ownership_analysis(
                    std::str::from_utf8(input).unwrap(),
                    go,
                    actual,
                    &[],
                );
                json!({"accepted":analysis.accepted(),"failure":analysis.failure,"rules":analysis.rules})
            } else {
                Value::Null
            };
            steps.push(json!({"step":index,"op":step,"main_inventory":main_inventory,"config_inventory":config_inventory,"certifier":certifier,"input":String::from_utf8_lossy(input),"main_candidate":String::from_utf8(candidate.0.bytes).unwrap(),"actual":actual,"call_changed":call_changed,"proved":good,"proof":proof,"independent":independent,"go_loadable":go_loadable,"independent_rust_source_owners_held":independent_rust_holds,"independent_go_source_owners_held":independent_go_holds,"source_inventory":super::owner_evidence_tests::inventory(std::str::from_utf8(input).unwrap(),true),"go_inventory":if go_loadable {go.map(|g|super::owner_evidence_tests::inventory(g,false))}else{None},"rust_inventory":super::owner_evidence_tests::inventory(actual,false)}));
            data = actual.as_bytes().to_vec();
        }
        results.push(json!({"run":row["run"],"case":row["case"],"off_matches":off_matches,"on_matches":on_matches,"changed":off!=on,"off":off,"on":on,"go":row["go"],"steps":steps}));
    }
    std::fs::write(
        std::env::var_os("FU_OWNERS_OUTPUT_OUTPUT").unwrap(),
        serde_json::to_vec(&results).unwrap(),
    )
    .unwrap();
    assert_eq!(
        mismatches, 0,
        "persisted outputs or changed-call owner/value/stability proofs"
    );
}

#[cfg(feature = "parity")]
#[test]
#[ignore = "offline independent same-input Go source-owner witnesses"]
fn owner_audit_same_input_go_witnesses() {
    let rows: Vec<Value> = serde_json::from_slice(
        &std::fs::read(std::env::var_os("FU_OWNERS_GO_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    let mut result = Vec::new();
    for row in rows {
        let source = independent_canonical_owners(row["source"].as_str().unwrap().as_bytes());
        let rust = independent_canonical_owners(row["rust"].as_str().unwrap().as_bytes());
        let holds = |owners: &super::owner_tests::OwnerInventory| {
            source
                .iter()
                .all(|(owner, count)| owners.get(owner) == Some(count))
        };
        assert!(holds(&rust), "same-input witness lost a Rust source owner");
        let go_file = row["go"]["files"].get(0).and_then(Value::as_str);
        let go = go_file
            .filter(|text| {
                Config::load_bytes(text.as_bytes()).is_ok() && parity::single_document(text)
            })
            .map(|text| independent_canonical_owners(text.as_bytes()));
        result.push(json!({"id":row["id"],"rust_source_owners_held":true,
            "go_source_owners_held":go.as_ref().map(holds),
            "source":source.into_iter().collect::<Vec<_>>(),
            "rust":rust.into_iter().collect::<Vec<_>>(),
            "go":go.map(|v|v.into_iter().collect::<Vec<_>>())}));
    }
    std::fs::write(
        std::env::var_os("FU_OWNERS_GO_OUTPUT").unwrap(),
        serde_json::to_vec(&result).unwrap(),
    )
    .unwrap();
}
