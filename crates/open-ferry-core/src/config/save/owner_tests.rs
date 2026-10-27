use super::*;

#[path = "owner_r10_tests.rs"]
pub(super) mod round10;
#[path = "owner_r5_tests.rs"]
mod round5;
#[path = "owner_r6_tests.rs"]
mod round6;
#[path = "owner_r7_tests.rs"]
mod round7;
#[path = "owner_r9_tests.rs"]
pub(super) mod round9;

fn probes() -> Vec<serde_json::Value> {
    serde_json::from_str(include_str!("../testdata/writer/owner_probes.json")).unwrap()
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
fn owner_raw_write_catalog_includes_expanded_alias_comments() {
    let data = b"oauth: {}\nplugins:\n    configs:\n        example:\n            anchor: &value\n                # scalar head\n                null\n            copy: *value\n";
    let (doc, original) = tree::read_comment_source(data).unwrap();
    assert_eq!(original.comments.len(), 1);
    let mut expanded_source = original;
    expanded_source.include_copies(&expand_config_aliases(&doc).unwrap());
    assert_eq!(expanded_source.comments.len(), 2);
    let (candidate, source) = candidate_write_file(data).unwrap();
    assert_eq!(
        source.comments.len(),
        expanded_source.comments.len(),
        "raw write discarded an expanded alias comment"
    );
    let main = tree::without_owner_protection(|| render_write_file(data)).unwrap();
    assert!(main == candidate.bytes);
    let actual = render_write_file(data).unwrap();
    assert!(
        actual == main || independent_owners(data) == independent_owners(&actual),
        "raw alias output changed a source owner"
    );
    assert!(Config::load_bytes(&actual).is_ok());
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
fn owner_many_comment_parts_keep_their_physical_receipts() {
    let mut data = String::new();
    for i in 0..256 {
        data.push_str(&format!("# part {i} π\n"));
        if i % 4 == 0 {
            data.push_str("\n\n\n");
        }
    }
    data.push_str("auth-dir: /x # tail\n");
    let (node, source) = tree::read_comment_source(data.as_bytes()).unwrap();
    assert!(source.complete);
    assert_eq!(source.comments.len(), 257);
    for emitted in [
        tree::source_yaml(data.as_bytes(), &node),
        tree::marshal_config_receipted(&node).unwrap(),
    ] {
        assert_eq!(emitted.receipts.comments.len(), 257);
        assert!(source.holds(&tree::read_receipted(&emitted).unwrap()));
    }
}

#[test]
fn owner_a2_probe_refuses_a_head_repair_lost_by_a_later_save() {
    let probe = probes()
        .into_iter()
        .find(|p| p["case"] == "A-sequence-head")
        .unwrap();
    let source = probe["options"]["file"].as_str().unwrap().as_bytes();
    round9::unsafe_nested_repair_is_refused(source);
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
        serde_json::from_str(include_str!("../testdata/writer/owner_examples.json")).unwrap();
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
        }
        .into();
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
pub(super) type OwnerInventory =
    std::collections::BTreeMap<(Vec<String>, &'static str, &'static str, usize, String), usize>;
pub(super) fn independent_owners(data: &[u8]) -> OwnerInventory {
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
        serde_json::from_str(include_str!("../testdata/writer/owner_before.json")).unwrap();
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_cases.json")).unwrap();
    let rows: Vec<_> = before.iter().filter(|row| row["case"]=="flow_first_foot" && ["nested","raw"].contains(&row["op"].as_str().unwrap())).map(|row| {
        let case=cases.iter().find(|c| c["case"]==row["case"]).unwrap();
        let source=case["source"].as_str().unwrap().as_bytes();
        let expected=independent_owners(source);
        let main=tree::without_owner_protection(||if row["op"]=="nested" {render_nested_scalar(source,&["auth-dir"],"/x")} else {render_write_file(source)}).unwrap();let actual=independent_owners(&main);
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
        serde_json::from_str(include_str!("../testdata/writer/owner_examples.json")).unwrap();
    let (mut a2, mut a3) = (
        std::collections::BTreeSet::new(),
        std::collections::BTreeSet::new(),
    );
    for case in cases {
        let data = case["options"]["file"].as_str().unwrap().as_bytes();
        let expected = independent_owners(data);
        let (prior, source) = candidate_nested_scalar(data, &["auth-dir"], "/x").unwrap();
        let output = render_nested_scalar(data, &["auth-dir"], "/x").unwrap();
        if let Some(physical) = case.get("physical_source") {
            assert!(sha256_hex(data) == physical["source_sha256"]);
            assert!(sha256_hex(&prior.bytes) == physical["main_sha256"]);
            assert!(expected.values().sum::<usize>() == physical["parsed_comments"]);
            for span in physical["unparsed"].as_array().unwrap() {
                let offset = span["offset"].as_u64().unwrap() as usize;
                let length = span["length"].as_u64().unwrap() as usize;
                assert!(sha256_hex(&data[offset..offset + length]) == span["sha256"]);
                assert!(expected.keys().all(|key| key.4 != span["sha256"]));
            }
            assert!(expected != independent_owners(&prior.bytes));
            assert!(
                output == prior.bytes,
                "unparsed source must keep writer bytes"
            );
            continue;
        }
        if case["case"] == "random-30-2345-step-0" {
            round9::unsafe_nested_repair_is_refused(data);
        } else {
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
        }
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

#[test]
fn owner_bookkeeping_work_scales_with_nodes_and_occurrences() {
    fn work(n: usize, save: bool) -> usize {
        let mut text = String::from("plugins:\n  configs:\n    example:\n      items:\n");
        for i in 0..n {
            text.push_str(&format!(
                "        # head {i}\n        - item{i} # line {i}\n"
            ));
        }
        let (result, work) = tree::count_comment_work(|| {
            let (doc, source) = tree::read_comment_source(text.as_bytes()).unwrap();
            let emitted = tree::marshal_config_receipted(&doc).unwrap();
            let rebound = tree::read_receipted(&emitted).unwrap();
            assert!(
                source.holds(&rebound),
                "cost fixture changed its source owners"
            );
            if save {
                let mut cfg = Config::parse(text.as_bytes()).unwrap();
                cfg.auth_dir = "/x".into();
                render_preserving_comments(text.as_bytes(), &cfg, false)
            } else {
                render_nested_scalar(text.as_bytes(), &["auth-dir"], "/x")
            }
        });
        result.unwrap();
        work
    }
    for save in [false, true] {
        let small = work(256, save);
        let large = work(1024, save);
        assert!(
            large <= small * 6,
            "comment bookkeeping grew faster than linearly: {small} -> {large}"
        );
        assert!(
            large <= 2000 * 1024,
            "comment bookkeeping exceeded its linear work budget: {large}"
        );
    }
    fn alias_work(n: usize) -> usize {
        let mut text = String::from("plugins:\n  configs:\n    example:\n      pool: &pool\n");
        for i in 0..16 {
            text.push_str(&format!("        - item{i} # pool {i}\n"));
        }
        text.push_str("      copies:\n");
        for _ in 0..n {
            text.push_str("        - *pool\n");
        }
        let (output, work) = tree::count_comment_work(|| {
            let mut cfg = Config::parse(text.as_bytes()).unwrap();
            cfg.auth_dir = "/x".into();
            render_preserving_comments(text.as_bytes(), &cfg, false).unwrap()
        });
        assert!(Config::load_bytes(&output).is_ok());
        work
    }
    let small = alias_work(32);
    let large = alias_work(128);
    assert!(
        large <= small * 6,
        "alias-copy save bookkeeping grew faster than linearly: {small} -> {large}"
    );
}

#[test]
fn owner_scalar_header_repairs_keep_the_writer_scalar_style_and_tag() {
    fn scalars(node: &Node, out: &mut Vec<(String, super::super::yaml3::Style, String)>) {
        if node.kind == Kind::Scalar {
            out.push((node.value.clone(), node.style, node.tag.clone()));
        }
        for child in &node.content {
            scalars(child, out);
        }
    }
    for (id, item, save) in [
        ("plain", "x", false),
        ("single", "'x  y'", false),
        ("double-escape", "\"x\\ny\"", false),
        ("leading-space", "\" x\"", false),
        ("tab", "\"x\\ty\"", false),
        ("explicit-str", "!!str x", false),
        ("literal-keep", "|+\n          x\n", false),
        ("integer-save", "7", true),
    ] {
        let source = format!(
            "oauth-request-scoped-errors:\n  aistudio:\n    - status: 400\n      action: continue\n      match: &m # own match\n        - {item} # own item\n        - y\n"
        );
        // Literal header comments belong before its body.
        let source = source.replace(
            "|+\n          x\n # own item",
            "|+ # own item\n          x\n",
        );
        let run = |bytes: &[u8]| {
            if save {
                let mut cfg = Config::parse(bytes).unwrap();
                cfg.auth_dir = "/x".into();
                render_preserving_comments(bytes, &cfg, false).unwrap()
            } else {
                render_nested_scalar(bytes, &["auth-dir"], "/x").unwrap()
            }
        };
        let main = tree::without_owner_protection(|| run(source.as_bytes()));
        let output = run(source.as_bytes());
        let mut before = Vec::new();
        scalars(&unmarshal(&main).unwrap(), &mut before);
        let mut after = Vec::new();
        scalars(&unmarshal(&output).unwrap(), &mut after);
        assert_eq!(
            before, after,
            "{id}: scalar style, explicit tag or value changed"
        );
        assert_eq!(output, run(&output), "{id}: repeated edit changed bytes");
        let doc = unmarshal(&output).unwrap();
        let items = yaml_path(&doc.content[0], "oauth-request-scoped-errors.aistudio").unwrap();
        let items = yaml_path(&items.content[0], "match").unwrap();
        assert_eq!(
            items.content[0].line_comment, "# own match\n# own item",
            "{id}: both source owners must hold"
        );
    }
}

#[test]
fn owner_multiple_flow_item_feet_keep_receipts_through_the_separator_and_normalizer() {
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_cases.json")).unwrap();
    let case = cases
        .iter()
        .find(|row| row["case"] == "flow_two_feet")
        .unwrap();
    let data = case["source"].as_str().unwrap().as_bytes();
    let (node, source) = tree::read_comment_source(data).unwrap();
    let rendered = tree::marshal_config_receipted(&node).unwrap();
    let before = rendered.receipts.comments.len();
    let normalized = tree::normalize_receipted(rendered);
    assert_eq!(
        before,
        normalized.receipts.comments.len(),
        "flow-foot indentation dropped receipts"
    );
    let parsed = tree::read_receipted(&normalized).unwrap();
    assert!(
        source.holds(&parsed),
        "flow feet or incoming key head changed their source owners"
    );
}

#[test]
fn owner_alias_receipts_reuse_each_shared_anchor_snapshot() {
    fn check(n: usize) -> usize {
        let mut text = String::from(
            "plugins:\n  configs:\n    example:\n      pool: &pool\n        - first # own\n        - second\n      refs:\n",
        );
        for _ in 0..n {
            text.push_str("        - *pool\n");
        }
        let ((), work) = tree::count_comment_work(|| {
            let (doc, mut source) = tree::read_comment_source(text.as_bytes()).unwrap();
            assert!(source.complete);
            let refs = yaml_path(&doc.content[0], "plugins.configs.example.refs").unwrap();
            let super::super::yaml3::AliasTarget::Node(first) =
                refs.content[0].alias.as_ref().unwrap()
            else {
                panic!("missing anchor snapshot")
            };
            for alias in &refs.content[1..] {
                let super::super::yaml3::AliasTarget::Node(next) = alias.alias.as_ref().unwrap()
                else {
                    panic!("missing anchor snapshot")
                };
                assert!(
                    std::sync::Arc::ptr_eq(first, next),
                    "alias receipt annotation copied a shared target"
                );
            }
            let expanded = expand_config_aliases(&doc).unwrap();
            source.include_copies(&expanded);
            assert!(source.holds(&expanded));
        });
        work
    }
    let small = check(64);
    let large = check(256);
    assert!(
        large <= small * 6,
        "alias bookkeeping grew faster than linearly: {small} -> {large}"
    );
}

#[test]
fn owner_mapping_key_lines_use_a_header_free_form_and_repeat_the_same_edit() {
    let source = b"debug: false\nmodels: &m # header\n  catalog: '' # value\nconfig-version: 8\n";
    for normalize in [false, true] {
        let run = |data: &[u8]| {
            if normalize {
                normalize_config_layout(data, true).unwrap().0
            } else {
                render_nested_scalar(data, &["auth-dir"], "/x").unwrap()
            }
        };
        let main = tree::without_owner_protection(|| run(source));
        let output = run(source);
        assert_ne!(output, main, "mapping-key source owner was not repaired");
        assert!(
            !output.windows(3).any(|w| w == b" ! "),
            "mapping-key repair used an avoidable tag header"
        );
        assert_eq!(
            independent_owners(source),
            independent_owners(&output),
            "mapping-key source owners changed"
        );
        assert_eq!(
            output,
            run(&output),
            "header-free mapping-key form did not settle"
        );
    }
}

#[test]
fn owner_repairing_many_mapping_headers_has_linear_bookkeeping() {
    fn work(n: usize) -> usize {
        let mut source = String::from("plugins:\n  configs:\n    example:\n");
        for i in 0..n {
            source.push_str(&format!(
                "      field{i}: &a{i} # header{i}\n        child: value # value{i}\n"
            ));
        }
        let (output, work) = tree::count_comment_work(|| {
            render_nested_scalar(source.as_bytes(), &["auth-dir"], "/x").unwrap()
        });
        assert!(
            !output.windows(3).any(|w| w == b" ! "),
            "mapping header used an avoidable tag"
        );
        assert_eq!(
            independent_owners(source.as_bytes()),
            independent_owners(&output)
        );
        assert_eq!(
            output,
            render_nested_scalar(&output, &["auth-dir"], "/x").unwrap()
        );
        work
    }
    let small = work(32);
    let large = work(128);
    assert!(
        large <= small * 6,
        "mapping repair bookkeeping grew faster than linearly: {small} -> {large}"
    );
}

#[test]
fn owner_repairing_many_scalar_headers_has_linear_bookkeeping() {
    fn work(n: usize) -> usize {
        let mut source = String::from("plugins:\n  configs:\n    example:\n");
        for i in 0..n {
            source.push_str(&format!(
                "      field{i}: &a{i} # header{i}\n        - x # item{i}\n        - y\n"
            ));
        }
        let (output, work) = tree::count_comment_work(|| {
            render_nested_scalar(source.as_bytes(), &["auth-dir"], "/x").unwrap()
        });
        assert_eq!(
            independent_owners(source.as_bytes()),
            independent_owners(&output)
        );
        assert_eq!(
            output,
            render_nested_scalar(&output, &["auth-dir"], "/x").unwrap()
        );
        work
    }
    let small = work(16);
    let large = work(64);
    assert!(
        large <= small * 6,
        "scalar repair bookkeeping grew faster than linearly: {small} -> {large}"
    );
}

#[test]
fn owner_scalar_header_groups_commit_only_after_their_headers_validate() {
    let data=b"plugins:\n  configs:\n    example:\n      bad: &bad # bad header\n        - x # bad item\n        - y\n      good: &good # good header\n        - x # good item\n        - y\n";
    let (mut candidate, source) =
        tree::with_owner_locations(|| candidate_nested_scalar(data, &["auth-dir"], "/x")).unwrap();
    // Exercise a header the scalar proposal must reject, without changing any
    // source receipt. The unrelated valid group must still be repaired.
    let marker = b"      bad:";
    let at = candidate
        .bytes
        .windows(marker.len())
        .position(|w| w == marker)
        .unwrap()
        + marker.len();
    candidate.bytes.splice(at..at, b" !!seq".iter().copied());
    for emission in &mut candidate.receipts.comments {
        if emission.offset >= at {
            emission.offset += 6;
        }
    }
    let bad: std::collections::HashSet<_> = source
        .comments
        .iter()
        .filter(|c| {
            source
                .original_locations
                .get(&c.receipt.owner)
                .is_some_and(|p| p.ends_with(".bad[0]"))
        })
        .map(|c| c.receipt.id.clone())
        .collect();
    assert_eq!(bad.len(), 2);
    let before: Vec<_> = candidate
        .receipts
        .comments
        .iter()
        .filter(|e| bad.contains(&e.receipt.id))
        .map(|e| (e.offset, e.length, e.receipt.id.clone()))
        .collect();
    tree::repair_scalar_headers(&source, &mut candidate);
    let after: Vec<_> = candidate
        .receipts
        .comments
        .iter()
        .filter(|e| bad.contains(&e.receipt.id))
        .map(|e| (e.offset, e.length, e.receipt.id.clone()))
        .collect();
    assert_eq!(before, after, "a rejected group changed its emissions");
    let parsed = tree::read_receipted(&candidate).unwrap();
    let mut good = source.clone();
    good.comments.retain(|c| {
        source
            .original_locations
            .get(&c.receipt.owner)
            .is_some_and(|p| p.ends_with(".good[0]"))
    });
    assert_eq!(good.comments.len(), 2);
    assert!(
        good.holds(&parsed),
        "a failed header group discarded another group's good repair"
    );
    assert!(
        !source.holds(&parsed),
        "the incomplete group must remain outside the complete gate"
    );
}

#[test]
fn owner_receipt_cannot_retarget_its_source_owner() {
    use super::super::yaml3::CommentId;
    let (mut document, source) =
        tree::read_comment_source(b"first: one # keep\nsecond: two\n").unwrap();
    assert!(source.holds(&document));
    let root = document.content.first_mut().unwrap();
    let text = std::mem::take(&mut root.content[1].line_comment);
    let mut spans = std::mem::take(&mut root.content[1].comment_receipts.line);
    let other = CommentId::fresh();
    root.content[3].comment_receipts.owner = Some(other.clone());
    for span in &mut spans {
        span.receipt.owner = other.clone();
    }
    root.content[3].line_comment = text;
    root.content[3].comment_receipts.line = spans;
    assert!(
        !source.holds(&document),
        "a rewritten receipt replaced its original source owner"
    );
}

#[test]
fn owner_lookup_hints_cannot_change_receipt_identity() {
    let (document, mut source) =
        tree::read_comment_source(b"first: one # first\nsecond: two # second\n").unwrap();
    assert!(source.holds(&document));
    let identities: std::collections::HashMap<_, _> = source
        .comments
        .iter()
        .enumerate()
        .map(|(index, comment)| (comment.receipt.id.clone(), index))
        .collect();
    // Cloned sources and subsets can retain stale hints. They must still use
    // the original identities, including when two hints name the same slot.
    for comment in &source.comments {
        comment.receipt.id.assign_comment_index(0);
    }
    for (index, comment) in source.comments.iter().enumerate() {
        assert_eq!(identities.get(&comment.receipt.id), Some(&index));
    }
    assert!(source.holds(&document));
    let mut subset = source.clone();
    subset.comments.remove(0);
    assert!(subset.holds(&document));
    source.comments.push(source.comments[0].clone());
    assert!(!source.holds(&document));
}

#[test]
fn owner_byte_only_rendering_skips_unused_receipt_walks() {
    let mut text = String::from("items:\n");
    for _ in 0..1024 {
        text.push_str("  - item\n");
    }
    let node = unmarshal(text.as_bytes()).unwrap();
    let (plain, plain_work) = tree::count_comment_work(|| encode::faithful_node(&node, 2).unwrap());
    let (tracked, tracked_work) =
        tree::count_comment_work(|| encode::faithful_node_receipted(&node, 2).unwrap());
    assert!(plain == tracked.0);
    assert!(tracked.1.owners.is_empty() && tracked.1.comments.is_empty());
    assert!(
        tracked_work >= plain_work + 1024,
        "byte-only rendering scanned receipt paths it never returns"
    );

    let (mut node, source) = tree::read_comment_source(b"items:\n  - item # keep\n").unwrap();
    let line_id = super::super::yaml3::CommentId::fresh();
    yaml_path_mut(&mut node.content[0], "items")
        .unwrap()
        .content[0]
        .line_comment_id = Some(line_id.clone());
    let plain = encode::protected_node(&node, 2).unwrap();
    let tracked = tree::marshal_receipted(&node, 2).unwrap();
    assert!(plain.0 == tracked.bytes);
    assert!(source.holds(&tree::read_receipted(&tracked).unwrap()));
    let value = encode::Value::Node(node);
    let plain = encode::marshal_with_comment_ids(&value).unwrap();
    let tracked = encode::marshal_with_receipts(&value).unwrap();
    assert!(plain.0 == tracked.0);
    assert!(plain.1.len() == 1 && tracked.1.len() == 1);
    assert!(plain.1[0].id == line_id && tracked.1[0].id == line_id);
    assert!(!tracked.2.owners.is_empty() && !tracked.2.comments.is_empty());
}
