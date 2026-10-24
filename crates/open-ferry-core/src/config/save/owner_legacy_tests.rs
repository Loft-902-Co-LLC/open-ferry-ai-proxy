//! Re-read physical emission bindings for the narrowly authorized legacy rows.
use super::super::yaml3::{CommentId, CommentSlot};
use super::owner_legacy_ops::{self as ops, Operation};
use super::*;
use serde_json::{Value, json};
fn location(root: &Node, wanted: &CommentId) -> Option<String> {
    fn walk(n: &Node, id: &CommentId, path: &str) -> Option<String> {
        if n.comment_receipts.owner.as_ref() == Some(id) {
            return Some(path.into());
        }
        match n.kind {
            Kind::Mapping => n.pairs().find_map(|(k, v)| {
                let path = format!("{path}.{}", k.value);
                walk(k, id, &format!("{path}:key")).or_else(|| walk(v, id, &path))
            }),
            Kind::Document => n.content.iter().find_map(|c| walk(c, id, path)),
            _ => n
                .content
                .iter()
                .enumerate()
                .find_map(|(i, c)| walk(c, id, &format!("{path}[{i}]"))),
        }
    }
    walk(root, wanted, "$")
}
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
        n.value,
        n.content.iter().map(values).collect::<Vec<_>>()
    ])
}
fn prove_step(data: &[u8], op: &Operation, actual: &[u8]) -> Result<Value, String> {
    let (candidate, source) = op.candidate(data)?;
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
        let src = source
            .original_sources
            .iter()
            .chain(&source.original_inputs)
            .find_map(|n| location(n, &comment.receipt.owner));
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
        for (pass, actual) in on["files"].as_array().unwrap().iter().enumerate() {
            let result = ops::operation(&data, row, pass)
                .and_then(|op| prove_step(&data, &op, actual.as_str().unwrap().as_bytes()));
            match result {
                Ok(proof) => proofs.push(proof),
                Err(error) => proofs.push(json!({"audit_error":error})),
            }
            data = actual.as_str().unwrap().as_bytes().to_vec();
        }
        json!({"id":row["id"],"changed":off!=on,"off":off,"on":on,"proofs":proofs})
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
    serde_json::from_str(include_str!("owner_legacy.json")).unwrap()
}
fn verify_repaired_row(row: &Value) {
    let result = audit(&row["recipe"]);
    let id = row["recipe"]["id"].as_str().unwrap();
    assert!(result["off"] == row["main"], "{id}: frozen main outcome");
    assert!(result["on"] == row["on"], "{id}: repaired bytes/outcome");
    assert!(
        result["proofs"] == row["proofs"],
        "{id}: occurrence owner map"
    );
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
/// Only these frozen, independently audited rows move their old assertions
/// to OFF. The actual ON bytes still pass all the original value checks.
pub(super) fn main_for_assertion(group: &str, source: &str, op: &str, actual: &str) -> String {
    for row in repaired_rows() {
        let recipe = &row["recipe"];
        if recipe["group"] != group || recipe["source"] != source || recipe["op"] != op {
            continue;
        }
        if let Some(pass) = row["on"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .position(|v| v.as_str() == Some(actual))
        {
            verify_repaired_row(&row);
            return row["main"]["files"][pass].as_str().unwrap().to_owned();
        }
    }
    actual.to_owned()
}
#[cfg(feature = "parity")]
pub(super) fn history_for_assertion(group: &str, source: &str, op: &str, actual: &Value) -> Value {
    for row in repaired_rows() {
        let recipe = &row["recipe"];
        if recipe["group"] == group
            && recipe["source"] == source
            && recipe["op"] == op
            && row["on"] == *actual
        {
            verify_repaired_row(&row);
            return row["main"].clone();
        }
    }
    actual.clone()
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
    assert_eq!(count, if cfg!(feature = "parity") { 33 } else { 29 });
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
                    .all(|key| proof[*key] == true));
            mismatches += usize::from(!good);
            let go = row["go"]["files"].get(index).and_then(Value::as_str);
            let go_loadable = go.is_some_and(|g| {
                Config::load_bytes(g.as_bytes()).is_ok() && parity::single_document(g)
            });
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
            steps.push(json!({"step":index,"op":step,"main_inventory":main_inventory,"config_inventory":config_inventory,"certifier":certifier,"input":String::from_utf8_lossy(input),"main_candidate":String::from_utf8(candidate.0.bytes).unwrap(),"actual":actual,"call_changed":call_changed,"proved":good,"proof":proof,"go_loadable":go_loadable,"source_inventory":super::owner_evidence_tests::inventory(std::str::from_utf8(input).unwrap(),true),"go_inventory":if go_loadable {go.map(|g|super::owner_evidence_tests::inventory(g,false))}else{None},"rust_inventory":super::owner_evidence_tests::inventory(actual,false)}));
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
