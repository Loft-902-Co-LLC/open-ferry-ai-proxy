// Counterexamples keep all other proof obligations satisfied so each guard
// remains independently observable under the reviewer's single mutations.
use super::tests::{
    expanded_owner_fixture, replace_source_owner_output, source_owner_test_snapshot,
};
use super::*;

fn expanded_witness(source: &str, go: &str, protected: &str, moves: Value) -> (Value, Value) {
    let op = json!({"op":"write","body":source});
    let mut witness = json!({"source":source,"operation":op,"moves":moves,"archives":[],
    "retained_source_owner":{
        "expanded_source_inventory":source_owner_test_snapshot(source),
        "go":{"files":[go]},"marked_go":{"files":[go]},
        "exact_go":{"files":[go]},"exact_go_inventory":source_owner_test_snapshot(go),
        "foot_separators":[],"repeat_ok":true
    }});
    replace_source_owner_output(&mut witness, &op, protected);
    (op, witness)
}

#[test]
fn expanded_repaired_comments_keep_their_source_order() {
    let source = "# first\n# second\nauth-dir: fixture\nnext: other\n";
    let go = "auth-dir: fixture\nnext: other\n";
    let reversed = "# second\n# first\nauth-dir: fixture\nnext: other\n";
    let (op, witness) = expanded_witness(source, go, source, json!([]));
    assert!(retained_expanded_source_owner(source, source, &op, &witness).accepted());
    let (op, witness) = expanded_witness(source, go, reversed, json!([]));
    let proof = retained_expanded_source_owner(source, reversed, &op, &witness);
    assert_eq!(proof.conditions, [false, true, true, true, true]);
}

#[test]
fn expanded_untouched_comments_keep_go_order_even_at_a_moved_owner() {
    let source = "# repair\nauth-dir: fixture\n# kept-one\n# kept-two\nnext: other\n";
    let go = "# kept-one\n# kept-two\nauth-dir: fixture\nnext: other\n";
    let right = "# repair\n# kept-one\n# kept-two\nauth-dir: fixture\nnext: other\n";
    let reversed = "# repair\n# kept-two\n# kept-one\nauth-dir: fixture\nnext: other\n";
    let (op, witness) = expanded_witness(source, go, right, json!([]));
    assert!(retained_expanded_source_owner(source, right, &op, &witness).accepted());
    let (op, witness) = expanded_witness(source, go, reversed, json!([]));
    assert_eq!(
        retained_expanded_source_owner(source, reversed, &op, &witness).conditions,
        [false, true, true, true, true]
    );
}

#[test]
fn expanded_source_inventory_must_agree_with_the_independent_parser() {
    let (source, _, op, mut witness) = expanded_owner_fixture();
    witness["retained_source_owner"]["expanded_source_inventory"]["comments"][0]["text"] =
        "# invented source allowance".into();
    assert!(!retained_expanded_source_owner(&source, &source, &op, &witness).accepted());
}

#[test]
fn expanded_projection_must_name_the_real_go_destination_node() {
    let source = "auth-dir: fixture # retained\nnext: other\n";
    let go = "auth-dir: fixture\nnext: other\n";
    let output = "auth-dir: fixture\nnext: other # retained\n";
    let original = source_owner_inventory(source).unwrap()[0].owner.json();
    let destination = source_owner_inventory(output).unwrap()[0].owner.json();
    let moves = json!([{"source":original,"ordinal":0,"text":"# retained","via":"observed","destination":destination}]);
    let (op, witness) = expanded_witness(source, go, output, moves);
    assert!(!retained_expanded_source_owner(source, output, &op, &witness).accepted());
}

#[test]
fn expanded_repair_cannot_choose_a_go_owner_instead_of_its_projection() {
    let source = "auth-dir: fixture # retained\nnext: other\n";
    let go = "auth-dir: fixture\n# retained\nnext: other # retained\n";
    let output = "auth-dir: fixture\nnext: other # retained\n";
    let (op, witness) = expanded_witness(source, go, output, json!([]));
    assert_eq!(
        retained_expanded_source_owner(source, output, &op, &witness).conditions,
        [false, true, true, true, true]
    );
}

#[test]
fn expanded_moves_bind_both_the_comment_text_and_observed_or_fallback_origin() {
    let (source, _, op, witness) = expanded_owner_fixture();
    assert!(retained_expanded_source_owner(&source, &source, &op, &witness).accepted());
    for (field, bad) in [("text", "# another comment"), ("via", "invented")] {
        let mut changed = witness.clone();
        changed["moves"][0][field] = bad.into();
        assert!(
            !retained_expanded_source_owner(&source, &source, &op, &changed).accepted(),
            "{field}"
        );
    }
}

#[test]
fn expanded_indexed_owner_requires_a_move_receipt_even_without_relocation() {
    let source = "items:\n  - fixture # retained\n";
    let go = "items:\n  - fixture\n";
    let owner = source_owner_inventory(source).unwrap()[0].owner.json();
    let moves = json!([{"source":owner,"ordinal":0,"text":"# retained","via":"observed","destination":owner}]);
    let (op, witness) = expanded_witness(source, go, source, moves);
    assert!(retained_expanded_source_owner(source, source, &op, &witness).accepted());
    let (op, witness) = expanded_witness(source, go, source, json!([]));
    assert!(!retained_expanded_source_owner(source, source, &op, &witness).accepted());
}

#[test]
fn foot_separator_license_requires_unique_owner_counts_values_and_exact_removal() {
    let fixtures: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/parity/src/config_save/testdata/owners-repairs.json"
    )))
    .unwrap();
    let f = &fixtures[0];
    let source = f["source"].as_str().unwrap();
    let protected = f["protected"].as_str().unwrap();
    let frame = &f["evidence"]["retained_source_owner"];
    let go = frame["exact_go"]["files"][0].as_str().unwrap();
    let expected = source_owner_expanded_inventory(source).unwrap();
    let native = source_owner_native_inventory(&frame["protected_inventory"], protected).unwrap();
    assert!(source_owner_repair_layout(
        source, go, protected, &expected, &native, frame
    ));
    for mutation in [
        "ambiguous-foot",
        "comment-count",
        "data",
        "without-bytes",
        "nonblank-line",
    ] {
        let mut expected = expected.clone();
        let mut native = native.clone();
        let mut changed = frame.clone();
        let mut protected = protected.to_owned();
        match mutation {
            "ambiguous-foot" => {
                let mut duplicate = expected
                    .iter()
                    .find(|c| c.owner.slot == Slot::Foot)
                    .unwrap()
                    .clone();
                duplicate.owner.path = vec![Part::Key("!!str".into(), "port".into())];
                expected.push(duplicate);
            }
            "comment-count" => {
                let comments = changed["foot_separators"][0]["inventory"]["comments"]
                    .as_array_mut()
                    .unwrap();
                let mut extra = comments.last().unwrap().clone();
                extra["ordinal"] = (extra["ordinal"].as_u64().unwrap() + 1).into();
                extra["text"] = "# unaccounted removal copy".into();
                comments.push(extra);
            }
            "data" => {
                changed["foot_separators"][0]["inventory"]["values"] = json!({"changed":true})
            }
            "without-bytes" => changed["foot_separators"][0]["without"] = "unbound removal".into(),
            "nonblank-line" => {
                let line = changed["foot_separators"][0]["line"].as_u64().unwrap() as usize;
                let mut lines: Vec<_> =
                    protected.split_inclusive('\n').map(str::to_owned).collect();
                assert_eq!(lines[line], "\n");
                lines[line] = " \n".into();
                protected = lines.concat();
                native = source_owner_inventory(&protected).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            !source_owner_repair_layout(source, go, &protected, &expected, &native, &changed),
            "{mutation}"
        );
    }
}

#[test]
fn original_history_budget_caps_go_duplicates_at_source_copies() {
    let source = "auth-dir: fixture # same\n";
    let go = "# same\n# same\nauth-dir: fixture\n";
    assert!(CommentOwnership::go_budget(source, go, source).accepted());
    assert_eq!(
        CommentOwnership::go_budget(source, go, "auth-dir: fixture\n")
            .failure
            .unwrap()["reason"],
        "go_budget"
    );
    assert!(
        CommentOwnership::go_budget("auth-dir: fixture\n", go, "auth-dir: fixture\n").accepted()
    );
}

#[test]
fn original_history_budget_keeps_comments_written_on_alias_nodes() {
    let source = "auth-dir: &a fixture # inherited\nextra: *a # local\n";
    let dropped = source.replace(" # local", "");
    let budget = CommentOwnership::go_budget(source, source, &dropped);
    assert_eq!(budget.failure.unwrap()["text"], "# local");
    assert!(CommentOwnership::go_budget(source, source, source).accepted());
}

#[test]
fn measured_gaps_reach_the_expanded_proof_with_complete_native_inventories() {
    let gaps: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/parity/src/config_save/testdata/source-owner-gaps.json"
    )))
    .unwrap();
    let tracked: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/parity/src/config_save/testdata/source-owner-tracked-gap.json"
    )))
    .unwrap();
    for f in gaps
        .as_array()
        .unwrap()
        .iter()
        .chain(std::iter::once(&tracked))
    {
        let source = f["source"].as_str().unwrap();
        let protected = f["protected"].as_str().unwrap();
        let witness = &f["evidence"];
        let frame = &witness["retained_source_owner"];
        let go = frame["exact_go"]["files"][0].as_str().unwrap();
        for (receipt, text) in [
            ("expanded_source_inventory", source),
            ("exact_go_inventory", go),
            ("protected_inventory", protected),
        ] {
            assert!(
                source_owner_native_inventory(&frame[receipt], text).is_some(),
                "{receipt}"
            );
        }
        assert!(
            !retained_expanded_source_owner(source, protected, &f["operation"], witness).accepted()
        );
    }
}
