use super::*;
use crate::config::save::owner_legacy_ops as ops;
use serde_json::{Value, json};

fn cases() -> Vec<Value> {
    serde_json::from_str(include_str!("../testdata/writer/owner_round5.json")).unwrap()
}

#[test]
fn owner_round5_unparsed_source_comments_keep_writer_bytes() {
    let base = cases()
        .into_iter()
        .find(|row| row["case"] == "ek-base")
        .unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let boundaries: Vec<Value> = serde_json::from_str(include_str!(
        "../testdata/writer/owner_round5_source_boundaries.json"
    ))
    .unwrap();
    for boundary in boundaries {
        let name = boundary["case"].as_str().unwrap();
        let data = format!("{base}{}", boundary["suffix"].as_str().unwrap());
        let (candidate, source) =
            candidate_nested_scalar(data.as_bytes(), &["auth-dir"], "/x").unwrap();
        let proposed = tree::repair_comment_candidate(&source, candidate.clone());
        assert!(candidate.bytes != proposed.bytes, "{name}: no owner repair");
        assert!(
            source.holds(&tree::read_receipted(&proposed).unwrap()),
            "{name}: parsed comments did not keep their owners"
        );
        assert!(
            super::super::super::yaml3::parser::physical_comment_lines(data.as_bytes())
                .unwrap()
                .len()
                > source.comments.len(),
            "{name}: fixture has no unparsed physical source comment"
        );
        let main = tree::without_owner_protection(|| try_run(data.as_bytes(), "nested"));
        assert!(
            try_run(data.as_bytes(), "nested") == main,
            "{name}: repair lost a physical source comment"
        );
    }
}

fn run(source: &[u8], op: &str) -> Vec<u8> {
    try_run(source, op).unwrap()
}

fn try_run(source: &[u8], op: &str) -> Result<Vec<u8>, String> {
    let row = json!({"op":op,"params":{"save_auth":"/x","nested_keys":["auth-dir"],"nested_values":"/x","paths":{"patch":["oauth"],"put":["oauth","auth-dir"]},"bodies":{"patch":"{\"auth-dir\":\"/x\"}","put":"\"/x\""}}});
    ops::operation(source, &row, 0)?.run(source)
}

fn no_trailing_space(bytes: &[u8], name: &str) {
    assert!(
        bytes.split(|b| *b == b'\n').all(|line| {
            !line
                .strip_suffix(b"\r")
                .unwrap_or(line)
                .last()
                .is_some_and(|b| matches!(b, b' ' | b'\t'))
        }),
        "{name}: repaired line has trailing whitespace"
    );
}

#[test]
fn owner_round5_repairs_have_no_trailing_whitespace() {
    let mut rows = cases();
    let writer: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_cases.json")).unwrap();
    rows.extend(writer);
    for row in rows {
        let source = row["source"].as_str().unwrap().as_bytes();
        for op in [
            "save",
            "migrate",
            "nested",
            "raw",
            "normalize",
            "patch",
            "put",
        ] {
            let operation = || try_run(source, op);
            let main = tree::without_owner_protection(operation);
            let output = operation();
            if output != main
                && let Ok(output) = output
            {
                no_trailing_space(&output, row["case"].as_str().unwrap());
            }
        }
    }
    let legacy = ops::resolve_sources(
        serde_json::from_str(include_str!("../testdata/writer/owner_legacy.json")).unwrap(),
    );
    for row in legacy {
        let recipe = &row["recipe"];
        if !cfg!(feature = "parity")
            && (recipe["disable_empty"] == true || recipe["disable_flow_feet"] == true)
        {
            continue;
        }
        let main = tree::without_owner_protection(|| ops::history(recipe));
        let output = ops::history(recipe);
        if main != output {
            for file in output["files"].as_array().unwrap() {
                no_trailing_space(
                    file.as_str().unwrap().as_bytes(),
                    recipe["id"].as_str().unwrap(),
                );
            }
        }
    }
}

#[test]
fn owner_round5_raw_then_save_or_migrate_keeps_nested_feet() {
    for (id, foot) in [
        ("block", "  # gemini:\n  #   - b\n"),
        ("single", "  # foot\n"),
        ("item-control", "    # - b\n"),
    ] {
        let source = format!("auth-dir: /x\noauth-excluded-models:\n  aistudio:\n    - a\n{foot}");
        let dir = crate::config::testing::TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, b"debug: false\n").unwrap();
        write_file(&path, source.as_bytes()).unwrap();
        let raw = std::fs::read(&path).unwrap();
        let main_raw =
            tree::without_owner_protection(|| render_write_file(source.as_bytes())).unwrap();
        let expected = crate::config::save::owner_legacy_tests::independent_canonical_owners(
            source.as_bytes(),
        );
        let raw_owners =
            crate::config::save::owner_legacy_tests::independent_canonical_owners(&raw);
        assert!(
            raw == main_raw || expected == raw_owners,
            "{id}: raw write moved foot"
        );
        if id == "item-control" {
            assert!(
                expected == raw_owners,
                "item-level raw control lost its owner"
            );
        }
        let config = Config::parse(source.as_bytes()).unwrap();
        let direct_save = render_preserving_comments(source.as_bytes(), &config, false).unwrap();
        assert!(
            expected
                == crate::config::save::owner_legacy_tests::independent_canonical_owners(
                    &direct_save
                ),
            "{id}: save did not repair the key-level foot"
        );
        for migrate in [false, true] {
            // The review's item-level control covers raw then save. Its first
            // migration already loses this owner at the frozen round-4 head.
            if id == "item-control" && migrate {
                continue;
            }
            let cfg = Config::parse(&raw).unwrap();
            std::fs::write(&path, &raw).unwrap();
            save_preserving_comments(&path, &cfg, migrate).unwrap();
            let output = std::fs::read(&path).unwrap();
            let main_cfg = Config::parse(&main_raw).unwrap();
            let main_two = tree::without_owner_protection(|| {
                render_preserving_comments(&main_raw, &main_cfg, migrate)
            })
            .unwrap();
            let actual =
                crate::config::save::owner_legacy_tests::independent_canonical_owners(&output);
            let baseline =
                crate::config::save::owner_legacy_tests::independent_canonical_owners(&main_two);
            assert!(
                actual == expected || actual == baseline,
                "{id}: raw then save/migrate ends worse than main"
            );
            if id == "item-control" {
                assert!(
                    actual == expected,
                    "item-level chain control lost its owner"
                );
            }
            no_trailing_space(&output, id);
        }
    }
}

#[test]
fn owner_round5_mapping_headers_use_tag_free_forms() {
    for id in [
        "ek-base",
        "ek-int-key",
        "ek-flow",
        "ek-long-key",
        "ek-quoted-key",
        "ek-map-in-seq",
    ] {
        let row = cases().into_iter().find(|row| row["case"] == id).unwrap();
        let source = row["source"].as_str().unwrap().as_bytes();
        let output = run(source, "nested");
        assert!(
            !output.windows(3).any(|bytes| bytes == b" ! "),
            "{id}: avoidable collection tag"
        );
        assert_eq!(
            independent_owners(source),
            independent_owners(&output),
            "{id}: tag-free form changed source owners"
        );
        assert_eq!(
            output,
            run(&output, "nested"),
            "{id}: repeated edit changed bytes"
        );
    }
}

#[test]
fn owner_round5_gate_rejects_unreceipted_comment_text() {
    let row = cases()
        .into_iter()
        .find(|row| row["case"] == "ek-base")
        .unwrap();
    let source = row["source"].as_str().unwrap().as_bytes();
    let (mut candidate, source) = candidate_nested_scalar(source, &["auth-dir"], "/x").unwrap();
    candidate
        .bytes
        .extend_from_slice(b"\n# untracked writer text\n");
    let main = candidate.bytes.clone();
    assert!(
        tree::finish_comment_owners(&source, candidate, |bytes| Some(bytes.to_vec())) == main,
        "gate accepted an unequal physical comment multiset"
    );
}

#[test]
fn owner_round5_unsupported_group_keeps_the_whole_writer_outcome() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/owner_round5_cliff.json")).unwrap();
    let row = rows.iter().find(|row| row["case"] == "cliff-abc").unwrap();
    let source = row["source"].as_str().unwrap().as_bytes();
    let main = tree::without_owner_protection(|| run(source, "nested"));
    assert!(
        run(source, "nested") == main,
        "unsupported group licensed a partial file repair"
    );

    // Keep all occurrences emitted and all comment bytes intact. This isolates
    // the owner check from the missing-emission, multiset and repeat guards.
    let row = cases()
        .into_iter()
        .find(|row| row["case"] == "ek-deeper")
        .unwrap();
    let source = row["source"].as_str().unwrap().replace(
        "      models:",
        "      good: &good # good header\n        - 'x  y' # good item\n        - z\n      models:",
    );
    let (candidate, catalog) =
        candidate_nested_scalar(source.as_bytes(), &["auth-dir"], "/x").unwrap();
    assert!(catalog.complete);
    assert!(
        catalog.comments.iter().all(|comment| candidate
            .receipts
            .comments
            .iter()
            .any(|emission| emission.receipt.id == comment.receipt.id)),
        "whole-file fixture must emit every source occurrence"
    );
    let proposal = tree::repair_comment_candidate(&catalog, candidate.clone());
    assert!(
        candidate.bytes != proposal.bytes,
        "whole-file fixture needs a supported repair"
    );
    assert!(
        !catalog.holds(&tree::read_receipted(&proposal).unwrap()),
        "whole-file fixture needs an unsupported owner"
    );
    let comments = |bytes: &[u8]| {
        let mut lines: Vec<_> = super::super::super::yaml3::parser::physical_comment_lines(bytes)
            .unwrap()
            .into_iter()
            .map(|(offset, len)| bytes[offset..offset + len].to_vec())
            .collect();
        lines.sort();
        lines
    };
    assert!(
        comments(source.as_bytes()) == comments(&proposal.bytes),
        "whole-file fixture must keep the full multiset"
    );
    assert!(
        Config::load_bytes(&proposal.bytes).is_ok(),
        "whole-file fixture must stay loadable"
    );
    fn signature(node: &Node) -> Value {
        json!([
            format!("{:?}", node.kind),
            node.tag,
            format!("{:?}", node.style),
            node.value,
            node.content.iter().map(signature).collect::<Vec<_>>()
        ])
    }
    assert!(
        signature(&unmarshal(&candidate.bytes).unwrap())
            == signature(&unmarshal(&proposal.bytes).unwrap()),
        "whole-file fixture must keep values, styles and tags"
    );
    assert!(
        tree::finish_comment_owners(&catalog, candidate.clone(), |bytes| Some(bytes.to_vec()))
            == candidate.bytes,
        "whole-file gate accepted a partial owner repair"
    );
}

#[test]
fn owner_round5_receipt_mismatch_keeps_writer_bytes() {
    let source =
        b"plugins:\n  configs:\n    example:\n      x: &x # key\n        - a # item\n        - b\n";
    let (node, source) = tree::read_comment_source(source).unwrap();
    let mut candidate = tree::marshal_config_receipted(&node).unwrap();
    candidate.receipts.comments[0].length += 1;
    let main = candidate.bytes.clone();
    assert!(
        tree::read_receipted(&candidate).is_err(),
        "receipt mismatch did not fail safely"
    );
    let untracked = tree::read_receipted_for_render(&candidate).unwrap();
    assert!(
        !source.holds(&untracked),
        "mismatch retained partial source bindings"
    );
    let intermediate = tree::marshal_config_receipted(&untracked).unwrap();
    let writer = intermediate.bytes.clone();
    assert!(
        tree::finish_comment_owners(&source, intermediate, |_| panic!(
            "untracked intermediate reached replay"
        )) == writer,
        "intermediate mismatch changed writer bytes"
    );
    assert!(
        tree::finish_comment_owners(&source, candidate, |_| panic!(
            "receipt mismatch reached replay"
        )) == main,
        "receipt mismatch changed writer bytes"
    );
}

#[test]
fn owner_round5_repairs_count_unreceipted_duplicates() {
    for id in [
        "ek-quoted-key",
        "ek-long-key",
        "ek-head-on-child",
        "ek-tagged-key",
        "ek-int-key",
        "ek-two",
    ] {
        let row = cases().into_iter().find(|row| row["case"] == id).unwrap();
        let source = row["source"].as_str().unwrap();
        for source in [source.to_owned(), source.replace("# value", "# header")] {
            let output = run(source.as_bytes(), "save");
            let main = tree::without_owner_protection(|| run(source.as_bytes(), "save"));
            assert!(
                output != main,
                "{id}: duplicate cleanup cancelled the repair"
            );
            assert!(
                independent_owners(source.as_bytes()) == independent_owners(&output),
                "{id}: copied comment changed the complete source inventory"
            );
            assert!(
                output == run(&output, "save"),
                "{id}: duplicate cleanup did not repeat"
            );
        }
    }
    let text = b"x: '# scalar'\ny: |\n  # scalar content\n# actual\n";
    let lines = super::super::super::yaml3::parser::physical_comment_lines(text).unwrap();
    assert_eq!(lines.len(), 1, "scalar contents were counted as comments");
}

#[test]
fn owner_round5_incoming_unparsed_comments_keep_writer_bytes() {
    use crate::config::v8_edit::{V8Edit, V8Method, preview_v8};
    let source = cases()
        .into_iter()
        .find(|row| row["case"] == "ek-base")
        .unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned();
    let inputs: Vec<Value> = serde_json::from_str(include_str!(
        "../testdata/writer/owner_round5_incoming_source.json"
    ))
    .unwrap();
    for input in inputs {
        let edit = V8Edit {
            method: V8Method::Put,
            path: vec!["oauth".into(), "auth-dir".into()],
            body: input["body"].as_str().unwrap().as_bytes().to_vec(),
            yaml: true,
        };
        let main = tree::without_owner_protection(|| preview_v8(source.as_bytes(), &edit)).unwrap();
        let output = preview_v8(source.as_bytes(), &edit).unwrap();
        if input["unparsed"] == true {
            assert!(
                output == main,
                "unparsed incoming source comment was lost by a repair"
            );
        } else {
            assert!(
                output != main,
                "complete incoming source did not exercise repair"
            );
            assert!(
                preview_v8(&output, &edit).unwrap() == output,
                "complete incoming source changed on repeat"
            );
        }
    }
}
