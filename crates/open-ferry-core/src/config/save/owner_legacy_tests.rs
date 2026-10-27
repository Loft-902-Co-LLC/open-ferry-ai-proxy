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
fn independent_layout_paths() -> Vec<(&'static str, &'static str)> {
    let mut paths: Vec<_> = super::super::v8::V8_PATHS
        .iter()
        .chain(super::super::layout::V8_STRUCT_PATHS)
        .chain(super::super::v8::V8_SHARED_STRUCT_PATHS)
        .chain(v8::v8_aliases())
        .copied()
        .collect();
    paths.sort_by_key(|(old, _)| std::cmp::Reverse(old.split('.').count()));
    paths
}
fn independent_field<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
    node.pairs()
        .find(|(key, _)| key.value == name)
        .map(|(_, value)| value)
}
fn independent_canonical_path(
    mut path: Vec<String>,
    paths: &[(&str, &str)],
    legacy_access: bool,
) -> Vec<String> {
    let Some(root) = path.get(1).and_then(|key| key.strip_prefix("!!str:")) else {
        return path;
    };
    for (old, family) in super::super::v8::V8_KEY_FAMILIES {
        if root == *old {
            path.splice(
                1..2,
                ["!!str:api-keys".to_owned(), format!("!!str:{family}")],
            );
            return path;
        }
    }
    for (old, current) in paths {
        if old.split('.').next() != Some(root) {
            continue;
        }
        // The same root name is a legacy access-key sequence or a v8 provider
        // mapping. Its parsed shape, rather than comments, determines the move.
        if *old == "api-keys" && !legacy_access {
            continue;
        }
        let old: Vec<_> = old.split('.').map(|s| format!("!!str:{s}")).collect();
        if path.get(1..).is_some_and(|p| p.starts_with(&old)) {
            path.splice(
                1..1 + old.len(),
                current.split('.').map(|s| format!("!!str:{s}")),
            );
            break;
        }
    }
    path
}
pub(super) fn independent_canonical_owners(data: &[u8]) -> super::owner_tests::OwnerInventory {
    let doc = unmarshal(data).unwrap();
    let legacy_access = doc
        .content
        .first()
        .and_then(|root| independent_field(root, "api-keys"))
        .is_some_and(|node| node.kind == Kind::Sequence);
    let paths = independent_layout_paths();
    let mut result = std::collections::BTreeMap::new();
    for ((path, side, slot, part, hash), count) in super::owner_tests::independent_owners(data) {
        let path = independent_canonical_path(path, &paths, legacy_access);
        *result.entry((path, side, slot, part, hash)).or_insert(0) += count;
    }
    result
}
fn independent_expected_owners(data: &[u8], rendered: &[u8]) -> super::owner_tests::OwnerInventory {
    fn parse(data: &[u8]) -> Node {
        let mut doc = unmarshal(data).unwrap();
        if let Some(root) = doc.content.first_mut() {
            *root = expand_config_aliases(root).unwrap();
        }
        doc
    }
    fn nodes<'a>(
        node: &'a Node,
        path: &mut Vec<String>,
        side: &'static str,
        out: &mut Vec<(Vec<String>, &'static str, &'a Node)>,
    ) {
        out.push((path.clone(), side, node));
        if node.kind == Kind::Mapping {
            for (key, value) in node.pairs() {
                path.push(format!("{}:{}", key.short_tag(), key.value));
                nodes(key, path, "key", out);
                nodes(value, path, "value", out);
                path.pop();
            }
        } else {
            for (index, child) in node.content.iter().enumerate() {
                path.push(index.to_string());
                nodes(
                    child,
                    path,
                    if node.kind == Kind::Document {
                        "root"
                    } else {
                        "item"
                    },
                    out,
                );
                path.pop();
            }
        }
    }
    fn indexed(doc: &Node) -> Vec<(Vec<String>, &'static str, &Node)> {
        let mut result = Vec::new();
        nodes(doc, &mut Vec::new(), "document", &mut result);
        let paths = independent_layout_paths();
        let legacy_access = doc
            .content
            .first()
            .and_then(|root| independent_field(root, "api-keys"))
            .is_some_and(|node| node.kind == Kind::Sequence);
        for (path, _, _) in &mut result {
            *path = independent_canonical_path(std::mem::take(path), &paths, legacy_access);
        }
        result
    }
    let source = parse(data);
    let target = parse(rendered);
    let source_nodes = indexed(&source);
    let target_nodes = indexed(&target);
    let credentials = |nodes: &[(Vec<String>, &str, &Node)]| {
        let mut entries = std::collections::BTreeMap::<_, Vec<Vec<String>>>::new();
        for (path, side, node) in nodes {
            if *side != "item"
                || path.get(1).is_none_or(|root| root != "!!str:api-keys")
                || !(path.len() == 4 || (path.len() == 6 && path[4] == "!!str:keys"))
            {
                continue;
            }
            if let Some(key) = independent_field(node, "api-key")
                && key.kind == Kind::Scalar
                && !key.value.trim().is_empty()
            {
                entries
                    .entry((path[..3].to_vec(), key.value.trim().to_owned()))
                    .or_default()
                    .push(path.clone());
            }
        }
        entries
    };
    let target_credentials = credentials(&target_nodes);
    let mut credential_moves = Vec::new();
    for (identity, old_paths) in credentials(&source_nodes) {
        if let Some(new_paths) = target_credentials.get(&identity) {
            // Layout conversion keeps key occurrences in order, including
            // repeated credentials. Parsed values identify them independently
            // of comments and of the gate's receipt bindings.
            credential_moves.extend(old_paths.into_iter().zip(new_paths.iter().cloned()));
        }
    }
    let project = |path: &[String], side: &str| {
        let Some((old, new)) = credential_moves
            .iter()
            .find(|(old, _)| path.starts_with(old))
        else {
            return path.to_vec();
        };
        let tail = &path[old.len()..];
        let key_path: Vec<_> = new.iter().chain(tail).cloned().collect();
        if new.len() == 4 || tail.is_empty() {
            return key_path;
        }
        let group_path: Vec<_> = new[..4].iter().chain(tail).cloned().collect();
        let exists =
            |path: &Vec<String>| target_nodes.iter().any(|(p, s, _)| p == path && *s == side);
        match (exists(&key_path), exists(&group_path)) {
            (true, false) => key_path,
            (false, true) => group_path,
            _ => path.to_vec(),
        }
    };
    let mut moves = Vec::new();
    for (source_path, source_side, source) in &source_nodes {
        if source.kind != Kind::Sequence {
            continue;
        }
        let target_path = project(source_path, source_side);
        let Some((_, _, target)) = target_nodes
            .iter()
            .find(|(path, side, _)| *path == target_path && side == source_side)
        else {
            continue;
        };
        if target.kind != Kind::Sequence {
            continue;
        }
        // Scalar list edits retain occurrences in their original order within
        // each non-empty trimmed value. Pairing parsed values never consults
        // comment text, receipt identity, or the repair gate.
        let mut old_values = std::collections::BTreeMap::<&str, Vec<usize>>::new();
        let mut new_values = std::collections::BTreeMap::<&str, Vec<usize>>::new();
        for (index, item) in source.content.iter().enumerate() {
            if item.kind == Kind::Scalar && !item.value.trim().is_empty() {
                old_values.entry(item.value.trim()).or_default().push(index);
            }
        }
        for (index, item) in target.content.iter().enumerate() {
            if item.kind == Kind::Scalar && !item.value.trim().is_empty() {
                new_values.entry(item.value.trim()).or_default().push(index);
            }
        }
        for (value, old_indices) in old_values {
            if let Some(new_indices) = new_values.get(value) {
                for (index, next) in old_indices.into_iter().zip(new_indices) {
                    let mut old = source_path.clone();
                    old.push(index.to_string());
                    let mut new = target_path.clone();
                    new.push(next.to_string());
                    moves.push((old, new));
                }
            }
        }
    }

    let mut expected = std::collections::BTreeMap::new();
    for ((mut path, side, slot, part, hash), count) in independent_canonical_owners(data) {
        if let Some((old, new)) = moves.iter().find(|(old, _)| path.starts_with(old)) {
            path.splice(..old.len(), new.iter().cloned());
        } else {
            path = project(&path, side);
        }
        *expected.entry((path, side, slot, part, hash)).or_insert(0) += count;
    }
    expected
}
fn independent_source_owners_held(data: &[u8], rendered: &[u8]) -> bool {
    let actual = independent_canonical_owners(rendered);
    independent_expected_owners(data, rendered)
        .iter()
        .all(|(owner, count)| actual.get(owner) == Some(count))
}

#[test]
fn owner_independent_inventory_tracks_retained_keys_without_licensing_comment_moves() {
    let source = "access:\n  api-keys:\n    # source key\n    - retained\n";
    let correct = "access:\n  api-keys:\n    - inserted\n    # source key\n    - retained\n";
    let wrong = "access:\n  api-keys:\n    # source key\n    - inserted\n    - retained\n";
    assert!(independent_source_owners_held(
        source.as_bytes(),
        correct.as_bytes()
    ));
    assert!(!independent_source_owners_held(
        source.as_bytes(),
        wrong.as_bytes()
    ));
    let repeated_source = "access:\n  api-keys:\n    # first source\n    - ' retained '\n    # second source\n    - retained\n";
    let repeated_correct = "access:\n  api-keys:\n    - inserted\n    # first source\n    - retained\n    # second source\n    - ' retained '\n";
    let repeated_wrong = "access:\n  api-keys:\n    - inserted\n    # second source\n    - retained\n    # first source\n    - ' retained '\n";
    assert!(independent_source_owners_held(
        repeated_source.as_bytes(),
        repeated_correct.as_bytes()
    ));
    assert!(!independent_source_owners_held(
        repeated_source.as_bytes(),
        repeated_wrong.as_bytes()
    ));
    let scalar_source =
        "oauth-excluded-models:\n  antigravity: [first, empty, retained # source item\n  ]\n";
    let scalar_correct =
        "oauth:\n  excluded-models:\n    antigravity: [first, retained # source item\n    ]\n";
    let scalar_wrong =
        "oauth:\n  excluded-models:\n    antigravity: [first # source item\n    , retained]\n";
    assert!(independent_source_owners_held(
        scalar_source.as_bytes(),
        scalar_correct.as_bytes()
    ));
    assert!(!independent_source_owners_held(
        scalar_source.as_bytes(),
        scalar_wrong.as_bytes()
    ));
    let source = "claude-api-key:\n  - api-key: retained\n    cloak:\n      sensitive-words:\n        # source child\n        - word\n";
    let correct = "api-keys:\n  claude:\n    - keys:\n        - api-key: retained\n          cloak:\n            sensitive-words:\n              # source child\n              - word\n        - api-key: other\n          cloak:\n            sensitive-words:\n              - word\n";
    let wrong = "api-keys:\n  claude:\n    - keys:\n        - api-key: retained\n          cloak:\n            sensitive-words:\n              - word\n        - api-key: other\n          cloak:\n            sensitive-words:\n              # source child\n              - word\n";
    assert!(independent_source_owners_held(
        source.as_bytes(),
        correct.as_bytes()
    ));
    assert!(!independent_source_owners_held(
        source.as_bytes(),
        wrong.as_bytes()
    ));
    let entry_source =
        "meta-api-key:\n  # source entry\n  - api-key: retained\n    base-url: endpoint\n";
    let entry_correct = "api-keys:\n  meta:\n    - base-url: endpoint\n      keys:\n        # source entry\n        - api-key: retained\n";
    let entry_wrong_group = "api-keys:\n  meta:\n    # source entry\n    - base-url: endpoint\n      keys:\n        - api-key: retained\n";
    let entry_wrong_provider = "api-keys:\n  meta:\n    - base-url: endpoint\n      keys:\n        - api-key: retained\n  xai:\n    - keys:\n        # source entry\n        - api-key: retained\n";
    assert!(independent_source_owners_held(
        entry_source.as_bytes(),
        entry_correct.as_bytes()
    ));
    assert!(!independent_source_owners_held(
        entry_source.as_bytes(),
        entry_wrong_group.as_bytes()
    ));
    assert!(!independent_source_owners_held(
        entry_source.as_bytes(),
        entry_wrong_provider.as_bytes()
    ));
}

#[test]
fn owner_independent_inventory_tracks_provider_occurrences_in_both_layouts() {
    let source = "xai-api-key:\n  - api-key: removed\n  - api-key: retained\n    request-retry: 1 # retained field\n";
    let correct = "xai-api-key:\n  - api-key: retained\n    request-retry: 1 # retained field\n";
    let wrong = "xai-api-key:\n  - api-key: other\n    request-retry: 1 # retained field\n  - api-key: retained\n    request-retry: 1\n";
    assert!(independent_source_owners_held(
        source.as_bytes(),
        correct.as_bytes()
    ));
    assert!(!independent_source_owners_held(
        source.as_bytes(),
        wrong.as_bytes()
    ));
    let source = "api-keys:\n  gemini:\n    - api-key: repeated # first occurrence\n    - api-key: repeated # second occurrence\n";
    let correct = "api-keys:\n  gemini:\n    - keys:\n        - api-key: repeated # first occurrence\n    - keys:\n        - api-key: repeated # second occurrence\n";
    let wrong = "api-keys:\n  gemini:\n    - keys:\n        - api-key: repeated # second occurrence\n    - keys:\n        - api-key: repeated # first occurrence\n";
    assert!(independent_source_owners_held(
        source.as_bytes(),
        correct.as_bytes()
    ));
    assert!(!independent_source_owners_held(
        source.as_bytes(),
        wrong.as_bytes()
    ));
    let source = "claude-api-key:\n  - api-key: removed\n  - api-key: retained\n    cloak:\n      sensitive-words:\n        - removed\n        # retained child\n        - word\n";
    let correct = "api-keys:\n  claude:\n    - keys:\n        - api-key: retained\n          cloak:\n            sensitive-words:\n              # retained child\n              - word\n        - api-key: other\n          cloak:\n            sensitive-words:\n              - word\n";
    let wrong = "api-keys:\n  claude:\n    - keys:\n        - api-key: retained\n          cloak:\n            sensitive-words:\n              - word\n        - api-key: other\n          cloak:\n            sensitive-words:\n              # retained child\n              - word\n";
    assert!(independent_source_owners_held(
        source.as_bytes(),
        correct.as_bytes()
    ));
    assert!(!independent_source_owners_held(
        source.as_bytes(),
        wrong.as_bytes()
    ));
}

fn independent_step_proof(input: &[u8], main: &[u8], output: &[u8]) -> Value {
    let source = independent_expected_owners(input, main);
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
    if super::owner_tests::round9::recorded_refusal("legacy", id, "", "", 0) {
        assert_eq!(result["on"], result["off"]);
        return;
    }
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
    assert_eq!(count, if cfg!(feature = "parity") { 37 } else { 35 });
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
            let source_owners = independent_expected_owners(input, &candidate.0.bytes);
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

#[test]
#[ignore = "offline independent parse/path recheck of cached writer evidence"]
fn owner_recheck_cached_semantic_owners() {
    let input = std::env::var_os("FU_OWNERS_CACHED_INPUT").unwrap();
    let output = std::env::var_os("FU_OWNERS_CACHED_OUTPUT").unwrap();
    let mut rows: Vec<Value> = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    let mut failed = 0;
    for row in &mut rows {
        for step in row["steps"].as_array_mut().unwrap() {
            if step["call_changed"] != true {
                continue;
            }
            let source = step["input"].as_str().unwrap().as_bytes();
            let main = step["main_candidate"].as_str().unwrap().as_bytes();
            let actual = step["actual"].as_str().unwrap().as_bytes();
            let proof = independent_step_proof(source, main, actual);
            let expected = independent_expected_owners(source, actual)
                .into_iter()
                .collect::<Vec<_>>();
            let good = step["proof"]["faithful_main"] == false
                && [
                    "complete",
                    "new_holds",
                    "new_equals_actual",
                    "same_values",
                    "stable",
                ]
                .iter()
                .all(|key| step["proof"][*key] == true)
                && ["main_drift", "source_owners_held", "multiplicity"]
                    .iter()
                    .all(|key| proof[*key] == true);
            step["original_index_inventory"] = step["independent"].clone();
            step["original_index_proved"] = step["proved"].clone();
            step["independent"] = proof;
            step["independent_expected_source_owners"] = json!(expected);
            step["independent_rust_source_owners_held"] =
                step["independent"]["source_owners_held"].clone();
            step["proved"] = json!(good);
            failed += usize::from(!good);
        }
    }
    std::fs::write(output, serde_json::to_vec(&rows).unwrap()).unwrap();
    assert_eq!(failed, 0, "cached independent semantic owner proof failed");
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
        let source_data = row["source"].as_str().unwrap().as_bytes();
        let rust_data = row["rust"].as_str().unwrap().as_bytes();
        let source = independent_expected_owners(source_data, rust_data);
        let rust = independent_canonical_owners(rust_data);
        assert!(
            independent_source_owners_held(source_data, rust_data),
            "same-input witness lost a Rust source owner"
        );
        let go_file = row["go"]["files"]
            .get(0)
            .and_then(Value::as_str)
            .filter(|text| {
                Config::load_bytes(text.as_bytes()).is_ok() && parity::single_document(text)
            });
        let go = go_file.map(|text| independent_canonical_owners(text.as_bytes()));
        let go_holds =
            go_file.map(|text| independent_source_owners_held(source_data, text.as_bytes()));
        result.push(json!({"id":row["id"],"rust_source_owners_held":true,
            "go_source_owners_held":go_holds,
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

#[test]
#[ignore = "offline independent raw-write then save/migrate history audit"]
fn owner_round5_raw_history_audit() {
    let rows: Vec<Value> = serde_json::from_slice(
        &std::fs::read(std::env::var_os("FU_OWNERS_HISTORY_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    let mut results = Vec::new();
    for row in rows {
        let source = row["source"].as_str().unwrap().as_bytes();
        let main = row["main"].as_str().unwrap().as_bytes();
        let protected = row["protected"].as_str().unwrap().as_bytes();
        for migrate in [false, true] {
            let save = |data: &[u8]| -> Result<Vec<u8>, String> {
                let cfg = Config::parse(data).map_err(|_| "config parse".to_owned())?;
                render_preserving_comments(data, &cfg, migrate)
                    .map_err(|_| "save refusal".to_owned())
            };
            let before = tree::without_owner_protection(|| save(main));
            let after = save(protected);
            let mut result = json!({"id":row["id"],"migrate":migrate,"base_ok":before.is_ok(),"head_ok":after.is_ok()});
            if let (Ok(before), Ok(after)) = (before, after) {
                let expected = independent_expected_owners(source, &before);
                let old = independent_canonical_owners(&before);
                let new = independent_canonical_owners(&after);
                let hashes: std::collections::BTreeSet<_> =
                    expected.keys().map(|(_, _, _, _, hash)| hash).collect();
                let mut worse = Vec::new();
                for hash in hashes {
                    let group = |inventory: &super::owner_tests::OwnerInventory| {
                        inventory
                            .iter()
                            .filter(|((_, _, _, _, h), _)| h == hash)
                            .map(|(owner, count)| (owner.clone(), *count))
                            .collect::<Vec<_>>()
                    };
                    let source_group = group(&expected);
                    let new_group = group(&new);
                    if new_group != source_group && new_group != group(&old) {
                        worse.push(hash);
                    }
                }
                result["worse_hashes"] = json!(worse);
                result["same_values_styles_tags"] = json!(
                    values(&unmarshal(&before).unwrap()) == values(&unmarshal(&after).unwrap())
                );
                result["source_owners"] = json!(expected.into_iter().collect::<Vec<_>>());
                result["main_owners"] = json!(old.into_iter().collect::<Vec<_>>());
                result["new_owners"] = json!(new.into_iter().collect::<Vec<_>>());
                result["main"] = json!(String::from_utf8(before).unwrap());
                result["new"] = json!(String::from_utf8(after).unwrap());
            }
            results.push(result);
        }
    }
    std::fs::write(
        std::env::var_os("FU_OWNERS_HISTORY_OUTPUT").unwrap(),
        serde_json::to_vec(&results).unwrap(),
    )
    .unwrap();
}
