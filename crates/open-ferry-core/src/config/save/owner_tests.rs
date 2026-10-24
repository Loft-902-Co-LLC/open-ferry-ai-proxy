use super::*;

fn probes() -> Vec<serde_json::Value> {
    serde_json::from_str(include_str!("owner_probes.json")).unwrap()
}

#[test]
fn owner_source_positions_bind_distinct_occurrences() {
    for probe in probes() {
        let name = probe["case"].as_str().unwrap();
        let data = probe["options"]["file"].as_str().unwrap().as_bytes();
        let (doc, source) = tree::read_comment_source(data).unwrap();
        assert!(source.complete, "{name}: source positions incomplete");
        assert!(source.holds(&doc), "{name}: fresh source owners");
        let rendered = tree::source_yaml(data, &doc);
        let parsed = tree::read_receipted(&rendered).unwrap();
        assert!(source.holds(&parsed), "{name}: physical-offset rebind");
        assert_eq!(
            rendered.receipts.comments.len(),
            source.comments.len(),
            "{name}"
        );
    }
    let data = b"x: # repeated\n  - a # repeated\ny: &a [one, two] # alias\nz: *a\n";
    let (doc, mut source) = tree::read_comment_source(data).unwrap();
    assert!(source.complete);
    let expanded = expand_config_aliases(&doc).unwrap();
    source.include_copies(&expanded);
    assert!(source.holds(&expanded));
    for (i, comment) in source.comments.iter().enumerate() {
        assert!(
            source.comments[..i]
                .iter()
                .all(|other| other.receipt.id != comment.receipt.id)
        );
    }
}

#[test]
fn owner_tracking_survives_a_generated_emit_parse() {
    let data = b"plugins:\n  configs:\n    example:\n      x: # repeated\n        - a # repeated\n";
    let (doc, source) = tree::read_comment_source(data).unwrap();
    assert!(source.complete);
    let rendered = tree::marshal_receipted(&doc, 4).unwrap();
    assert_eq!(source.comments.len(), rendered.receipts.comments.len());
    let reparsed = tree::read_receipted(&rendered).unwrap();
    assert!(
        source.holds(&reparsed),
        "intermediate parse discarded source receipts"
    );
}

#[test]
fn owner_a2_probe_keeps_its_sequence_item_head() {
    let probe = probes()
        .into_iter()
        .find(|p| p["case"] == "A-sequence-head")
        .unwrap();
    let source = probe["options"]["file"].as_str().unwrap().as_bytes();
    let output = render_nested_scalar(source, &["auth-dir"], "/x").unwrap();
    let doc = unmarshal(&output).unwrap();
    let list = yaml_path(&doc.content[0], "oauth-model-alias.qwen").unwrap();
    assert!(
        !list.content[1].head_comment.is_empty(),
        "source head drifted"
    );
    let again = render_nested_scalar(&output, &["auth-dir"], "/x").unwrap();
    assert!(output == again, "second write differs");
}

#[test]
fn owner_a3_probe_keeps_both_scalar_line_occurrences() {
    let probe = probes()
        .into_iter()
        .find(|p| p["case"] == "A-scalar-line")
        .unwrap();
    let source = probe["options"]["file"].as_str().unwrap().as_bytes();
    let output = render_nested_scalar(source, &["auth-dir"], "/x").unwrap();
    let doc = unmarshal(&output).unwrap();
    let list = yaml_path(&doc.content[0], "oauth-request-scoped-errors.aistudio").unwrap();
    let items = yaml_path(&list.content[0], "match").unwrap();
    assert_eq!(items.content[0].line_comment.lines().count(), 2);
    assert!(items.content[1].head_comment.is_empty());
    assert!(render_nested_scalar(&output, &["auth-dir"], "/x").unwrap() == output);
}
#[test]
#[ignore = "value-free offline transport diagnostics"]
fn owner_diagnose_probes() {
    use serde_json::json;
    fn visit(
        node: &Node,
        path: &mut Vec<usize>,
        source: &tree::CommentSource,
        rows: &mut Vec<serde_json::Value>,
    ) {
        for (slot, spans) in [
            ("head", &node.comment_receipts.head),
            ("line", &node.comment_receipts.line),
            ("foot", &node.comment_receipts.foot),
        ] {
            for span in spans {
                rows.push(json!({"source_index":source.comments.iter().position(|c| c.receipt.id == span.receipt.id), "slot":slot,"path":path,"owner_matches":node.comment_receipts.owner.as_ref()==Some(&span.receipt.owner),"node_column":node.column,"at":span.at}));
            }
        }
        for (i, child) in node.content.iter().enumerate() {
            path.push(i);
            visit(child, path, source, rows);
            path.pop();
        }
    }
    let rows: Vec<_> = probes().iter().map(|probe| {
        let data = probe["options"]["file"].as_str().unwrap().as_bytes();
        let (candidate, source) = candidate_nested_scalar(data, &["auth-dir"], "/x").unwrap();
        let parsed = tree::read_receipted(&candidate).unwrap();
        let mut before = Vec::new();visit(&parsed,&mut Vec::new(),&source,&mut before);
        let initial = candidate.bytes.clone();
        let proposed = tree::repair_comment_candidate(&source,candidate);
        let parsed = tree::read_receipted(&proposed).unwrap();
        let mut after = Vec::new();visit(&parsed,&mut Vec::new(),&source,&mut after);
        let repeated = candidate_nested_scalar(&proposed.bytes,&["auth-dir"],"/x").unwrap();
        let repeated = tree::finish_comment_owners_replay(&repeated.1,repeated.0);
        json!({"probe":probe["case"],"complete":source.complete,"source_count":source.comments.len(),"source_slots":source.comments.iter().map(|c|json!({"slot":format!("{:?}",c.receipt.slot),"ordinal":c.receipt.ordinal,"source_line":c.source_line,"owner_line":c.owner_line,"column":c.column,"owner_column":c.owner_column})).collect::<Vec<_>>(),"before":before,"after":after,"proposal_changed":initial!=proposed.bytes,"proposal_holds":source.holds(&parsed),"valid":Config::load_bytes(&proposed.bytes).is_ok(),"stable":repeated==proposed.bytes})
    }).collect();
    std::fs::write(
        std::env::var_os("FU_OWNERS_DIAG").unwrap(),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
}
#[test]
#[ignore = "offline example selection; only IDs and owner counts are recorded"]
fn owner_measure_examples() {
    use serde_json::json;
    let input: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("owner_examples.json")).unwrap();
    let rows: Vec<_> = input.iter().map(|case| {
        let data = case["options"]["file"].as_str().unwrap().as_bytes();
        let Ok((candidate, source)) = candidate_nested_scalar(data,&["auth-dir"],"/x") else { return json!({"case":case["case"],"candidate":false}); };
        let before = tree::read_receipted(&candidate).ok();
        let overwritten = candidate.receipts.overwritten.len();
        let original = candidate.bytes.clone();
        let proposed = tree::repair_comment_candidate(&source,candidate);
        let holds = tree::read_receipted(&proposed).ok().is_some_and(|doc| source.holds(&doc));
        let output = render_nested_scalar(data,&["auth-dir"],"/x").unwrap();
        json!({"case":case["case"],"roots":case["roots"],"complete":source.complete,"source_comments":source.comments.len(),"before_holds":before.as_ref().is_some_and(|doc| source.holds(doc)),"proposed_holds":holds,"proposed_changed":proposed.bytes!=original,"changed":output!=original,"overwritten":overwritten,"stable":render_nested_scalar(&output,&["auth-dir"],"/x").is_ok_and(|again| again==output)})
    }).collect();
    std::fs::write(
        std::env::var_os("FU_OWNERS_EXAMPLES").unwrap(),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
}
#[test]
fn owner_deferred_foot_retains_a_displaced_occurrence() {
    use super::super::yaml3::{
        CommentId, CommentReceipt, CommentReceipts, CommentSlot, ReceiptSpan,
    };
    fn foot(node: &mut Node) -> CommentId {
        let owner = CommentId::fresh();
        let id = CommentId::fresh();
        node.foot_comment = "# repeated".into();
        node.comment_receipts = CommentReceipts {
            owner: Some(owner.clone()),
            foot: vec![ReceiptSpan {
                at: 0,
                receipt: CommentReceipt {
                    id: id.clone(),
                    owner,
                    slot: CommentSlot::Foot,
                    ordinal: 0,
                },
            }],
            ..CommentReceipts::default()
        };
        id
    }
    let mut inner = Node::mapping();
    inner.content = vec![
        Node::scalar(STR_TAG, "value"),
        Node::scalar(STR_TAG, "retained"),
    ];
    let pending = foot(&mut inner);
    let mut outer = Node::mapping();
    outer.content = vec![Node::scalar(STR_TAG, "child"), inner];
    let incoming = foot(&mut outer);
    let rendered = tree::marshal_receipted(&outer, 2).unwrap();
    assert!(
        rendered
            .receipts
            .overwritten
            .iter()
            .any(|span| span.receipt.id == pending),
        "pending foot receipt was overwritten"
    );
    assert!(
        rendered
            .receipts
            .comments
            .iter()
            .any(|emission| emission.receipt.id == incoming)
    );
    assert_ne!(pending, incoming);
}
pub(super) fn independent_owners(
    data: &[u8],
) -> std::collections::BTreeMap<(Vec<String>, &'static str, &'static str, usize, String), usize> {
    fn visit(
        node: &Node,
        path: &mut Vec<String>,
        side: &'static str,
        out: &mut std::collections::BTreeMap<
            (Vec<String>, &'static str, &'static str, usize, String),
            usize,
        >,
    ) {
        for (slot, text) in [
            ("head", &node.head_comment),
            ("line", &node.line_comment),
            ("foot", &node.foot_comment),
        ] {
            for (part, line) in text
                .lines()
                .enumerate()
                .filter(|(_, line)| !line.trim().is_empty())
            {
                *out.entry((
                    path.clone(),
                    side,
                    slot,
                    part,
                    sha256_hex(line.trim_start().as_bytes()),
                ))
                .or_default() += 1;
            }
        }
        if node.kind == Kind::Mapping {
            for (key, value) in node.pairs() {
                path.push(format!("{}:{}", key.short_tag(), key.value));
                visit(key, path, "key", out);
                visit(value, path, "value", out);
                path.pop();
            }
        } else {
            for (i, child) in node.content.iter().enumerate() {
                path.push(i.to_string());
                visit(
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
    let mut doc = unmarshal(data).unwrap();
    if let Some(root) = doc.content.first_mut() {
        *root = expand_config_aliases(root).unwrap();
    }
    let mut result = std::collections::BTreeMap::new();
    visit(&doc, &mut Vec::new(), "document", &mut result);
    result
}

#[test]
#[ignore = "independent inventory audit of the supplied control matrix"]
fn owner_audit_matrix_owners() {
    use serde_json::json;
    let before: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("owner_before.json")).unwrap();
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_cases.json")).unwrap();
    let rows: Vec<_> = before.iter().filter(|row| row["case"]=="flow_first_foot" && ["nested","raw"].contains(&row["op"].as_str().unwrap())).map(|row| {
        let case=cases.iter().find(|c| c["case"]==row["case"]).unwrap();
        let source=case["source"].as_str().unwrap().as_bytes();
        let expected=independent_owners(source);
        let actual=independent_owners(row["result"]["files"][0].as_str().unwrap().as_bytes());
        json!({"case":row["case"],"operation":row["op"],"faithful":actual==expected,"source_owners":expected.keys().map(|(path,side,slot,part,_)|json!({"path":path,"side":side,"slot":slot,"part":part})).collect::<Vec<_>>(),"prior_owners":actual.keys().map(|(path,side,slot,part,_)|json!({"path":path,"side":side,"slot":slot,"part":part})).collect::<Vec<_>>()})
    }).collect();
    std::fs::write(
        std::env::var_os("FU_OWNERS_MATRIX_AUDIT").unwrap(),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
}
#[test]
fn owner_diagnosed_a2_and_a3_cases_preserve_each_source_node_and_slot() {
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("owner_examples.json")).unwrap();
    let (mut a2, mut a3) = (
        std::collections::BTreeSet::new(),
        std::collections::BTreeSet::new(),
    );
    for case in cases {
        let data = case["options"]["file"].as_str().unwrap().as_bytes();
        let expected = independent_owners(data);
        let (prior, source) = candidate_nested_scalar(data, &["auth-dir"], "/x").unwrap();
        let output = render_nested_scalar(data, &["auth-dir"], "/x").unwrap();
        assert!(
            output != prior.bytes,
            "{} did not exercise repair",
            case["case"]
        );
        assert!(
            expected == independent_owners(&output),
            "{} changed a source node/slot/paragraph",
            case["case"]
        );
        assert!(source.complete);
        assert!(
            output == render_nested_scalar(&output, &["auth-dir"], "/x").unwrap(),
            "{} second write",
            case["case"]
        );
        for root in case["roots"].as_array().unwrap() {
            if root == "A2" {
                a2.insert(case["original_case"].as_str().unwrap().to_owned());
            }
            if root == "A3" {
                a3.insert(case["original_case"].as_str().unwrap().to_owned());
            }
        }
    }
    assert!(a2.len() >= 4 && a3.len() >= 4);
}

#[test]
fn owner_probes_keep_their_nodes_through_every_writer_entry() {
    use crate::config::v8_edit::{V8Edit, V8Method};
    fn run(data: &[u8], mode: &str) -> Vec<u8> {
        match mode {
            "save" | "migrate" => {
                let mut cfg = Config::load_bytes(data).unwrap();
                cfg.auth_dir = "/x".into();
                render_preserving_comments(data, &cfg, mode == "migrate").unwrap()
            }
            "nested" => render_nested_scalar(data, &["auth-dir"], "/x").unwrap(),
            "raw" => render_write_file(data).unwrap(),
            "normalize" => normalize_config_layout(data, true).unwrap().0,
            "patch" | "put" => {
                crate::config::v8_edit::render(
                    data,
                    &V8Edit {
                        method: if mode == "patch" {
                            V8Method::Patch
                        } else {
                            V8Method::Put
                        },
                        path: if mode == "patch" {
                            vec!["oauth".into()]
                        } else {
                            vec!["oauth".into(), "auth-dir".into()]
                        },
                        body: if mode == "patch" {
                            br#"{"auth-dir":"/x"}"#.to_vec()
                        } else {
                            br#""/x""#.to_vec()
                        },
                        yaml: false,
                    },
                )
                .unwrap()
                .0
            }
            _ => unreachable!(),
        }
    }
    for probe in probes() {
        for mode in [
            "save",
            "migrate",
            "nested",
            "raw",
            "normalize",
            "patch",
            "put",
        ] {
            let data = probe["options"]["file"].as_str().unwrap().as_bytes();
            let output = run(data, mode);
            let baseline = tree::without_owner_protection(|| run(data, mode));
            if output == baseline {
                continue;
            }
            let doc = unmarshal(&output).unwrap();
            let root = &doc.content[0];
            if probe["case"] == "A-sequence-head" {
                let list = yaml_path(root, "oauth-model-alias.qwen")
                    .or_else(|| yaml_path(root, "oauth.model-alias.qwen"))
                    .unwrap();
                assert!(!list.content[1].head_comment.is_empty(), "A2 {mode} owner");
                let rse = yaml_path(root, "oauth-request-scoped-errors")
                    .or_else(|| yaml_path(root, "oauth.request-scoped-errors"))
                    .unwrap();
                let key = find_map_key_index(rse, "aistudio").unwrap();
                assert!(
                    !rse.content[key].line_comment.is_empty(),
                    "A2 {mode} deferred owner"
                );
            } else {
                let list = yaml_path(root, "oauth-request-scoped-errors.aistudio")
                    .or_else(|| yaml_path(root, "oauth.request-scoped-errors.aistudio"))
                    .unwrap();
                let items = yaml_path(&list.content[0], "match").unwrap();
                assert_eq!(
                    items.content[0].line_comment.lines().count(),
                    2,
                    "A3 {mode} scalar owner"
                );
                assert!(
                    items.content[1].head_comment.is_empty(),
                    "A3 {mode} successor head"
                );
            }
            assert!(
                run(&output, mode) == output,
                "{} {mode} second write",
                probe["case"]
            );
        }
    }
}
