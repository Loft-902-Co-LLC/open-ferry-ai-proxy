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
