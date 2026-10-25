use super::*;
use serde_json::{Value, json};
use std::fs;

// This diagnostic inventory is independent of receipt bookkeeping. It is used
// only by explicitly invoked offline measurements, never to license a repair.
pub(super) fn inventory(text: &str, expand: bool) -> Value {
    let Ok(mut doc) = unmarshal(text.as_bytes()) else {
        return Value::Null;
    };
    if expand && let Some(root) = doc.content.first_mut() {
        let Ok(expanded) = expand_config_aliases(root) else {
            return Value::Null;
        };
        *root = expanded;
    }
    fn visit(
        node: &Node,
        path: &mut Vec<Value>,
        side: &str,
        comments: &mut Vec<Value>,
        nodes: &mut Vec<Value>,
    ) {
        nodes.push(json!({"path":path,"side":side,"kind":format!("{:?}",node.kind),"tag":node.tag,"value":node.value,"children":node.content.len(),"line":node.line}));
        for (role, text) in [
            ("head", &node.head_comment),
            ("line", &node.line_comment),
            ("foot", &node.foot_comment),
        ] {
            for (part, line) in text.lines().enumerate() {
                let line = line.trim_start_matches([' ', '\t']);
                if !line.is_empty() {
                    comments.push(json!({"text":line,"path":path,"role":role,"side":side,"part":part,"node_line":node.line}));
                }
            }
        }
        match node.kind {
            Kind::Mapping => {
                for (key, value) in node.pairs() {
                    path.push(json!({"key":key.value,"tag":key.tag}));
                    visit(key, path, "key", comments, nodes);
                    visit(value, path, "value", comments, nodes);
                    path.pop();
                }
            }
            Kind::Sequence => {
                for (i, value) in node.content.iter().enumerate() {
                    path.push(json!({"index":i}));
                    visit(value, path, "item", comments, nodes);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let (mut comments, mut nodes) = (Vec::new(), Vec::new());
    // The document's own comments and root are distinct owners.
    let children = std::mem::take(&mut doc.content);
    visit(&doc, &mut Vec::new(), "document", &mut comments, &mut nodes);
    if let Some(root) = children.first() {
        visit(root, &mut Vec::new(), "root", &mut comments, &mut nodes);
    }
    for i in 0..comments.len() {
        comments[i]["occurrence"] = json!(
            comments[..i]
                .iter()
                .filter(|c| c["text"] == comments[i]["text"])
                .count()
                + 1
        );
    }
    json!({"comments":comments,"nodes":nodes})
}

#[test]
#[ignore = "offline fresh sweep diagnosis; paths supplied through FU_OWNERS_DIAG_INPUTS and FU_OWNERS_DIAG_OUTPUT"]
fn owner_diagnose_sweeps() {
    let runs: Vec<Value> = serde_json::from_slice(
        &fs::read(std::env::var_os("FU_OWNERS_DIAG_INPUTS").unwrap()).unwrap(),
    )
    .unwrap();
    let out = std::path::PathBuf::from(std::env::var_os("FU_OWNERS_DIAG_OUTPUT").unwrap());
    fs::create_dir_all(&out).unwrap();
    for run in runs {
        let mut rows = Vec::new();
        for entry in run["cases"].as_array().unwrap() {
            let fixture: Value =
                serde_json::from_slice(&fs::read(entry["failure"].as_str().unwrap()).unwrap())
                    .unwrap();
            let proof: Value = fs::read(entry["proof"].as_str().unwrap())
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null);
            let go: Value =
                serde_json::from_str(fixture["upstream_output_raw"].as_str().unwrap()).unwrap();
            let protected = &fixture["open_ferry_output"];
            let mut source = fixture["options"]["file"].as_str().unwrap();
            let mut steps = Vec::new();
            for (i, (operation, rust)) in fixture["options"]["steps"]
                .as_array()
                .unwrap()
                .iter()
                .zip(protected["files"].as_array().unwrap())
                .enumerate()
            {
                if operation["op"] == "write" {
                    source = operation["body"].as_str().unwrap();
                }
                let rust = rust.as_str().unwrap();
                let original = go["files"].get(i).and_then(Value::as_str);
                let loadable = original.is_some_and(|g| Config::load_bytes(g.as_bytes()).is_ok());
                let owner_good = if loadable {
                    parity::retained_comment_ownership(source, original.unwrap(), rust)
                } else {
                    parity::retained_unloadable_comments(source, rust, &[])
                };
                let mut trace_comments = Vec::new();
                let mut trace_bound = false;
                if !loadable {
                    let trace = proof["ablated_traces"].get(i);
                    let target = original.map(|s| s.as_bytes().to_vec()).or_else(|| {
                        trace
                            .and_then(|t| t["rejected"].as_array()?.last())
                            .and_then(|r| {
                                serde_json::from_value::<Vec<u8>>(r["bytes"].clone()).ok()
                            })
                    });
                    if let (Some(trace), Some(target)) = (trace, target)
                        && let Some(emissions) = trace["emissions"].as_array()
                    {
                        for emission in emissions.iter().rev() {
                            let bytes =
                                serde_json::from_value::<Vec<u8>>(emission["bytes"].clone())
                                    .unwrap();
                            if bytes != target {
                                continue;
                            }
                            let (mut end, mut good, mut comments) = (0, true, Vec::new());
                            for c in emission["comments"].as_array().unwrap() {
                                let at = c["offset"].as_u64().unwrap() as usize;
                                let bytes =
                                    serde_json::from_value::<Vec<u8>>(c["bytes"].clone()).unwrap();
                                if at < end || target.get(at..at + bytes.len()) != Some(&bytes) {
                                    good = false;
                                    break;
                                }
                                end = at + bytes.len();
                                comments.push(String::from_utf8(bytes).unwrap());
                            }
                            if good {
                                trace_comments = comments;
                                trace_bound = true;
                                break;
                            }
                        }
                    }
                }
                let source_inventory = inventory(source, true);
                let rust_inventory = inventory(rust, false);
                let counts_good = loadable
                    || (source_inventory != Value::Null
                        && trace_comments.iter().all(|text| {
                            trace_comments.iter().filter(|other| *other == text).count()
                                <= rust_inventory["comments"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter(|c| c["text"].as_str() == Some(text))
                                    .count()
                        }));
                if !owner_good || !counts_good {
                    steps.push(json!({"step":i,"operation":operation,"go_loadable":loadable,"owner_check_passed":owner_good,"trace_bound":trace_bound,"trace_comments":trace_comments,"source":source,"go":original,"rust":rust,"source_inventory":source_inventory,"go_inventory":if loadable{original.map(|g|inventory(g,false))}else{None},"rust_inventory":rust_inventory}));
                }
                source = rust;
            }
            rows.push(json!({"case":fixture["case"],"failure":entry["failure"],"proof":entry["proof"],"reason":proof.get("reason"),"checks":proof["checks"],"subset":proof["subset"],"steps":steps}));
        }
        fs::write(
            out.join(format!("{}.json", run["run"].as_str().unwrap())),
            serde_json::to_vec(
                &json!({"run":run["run"],"seed":run["seed"],"mode":"config","cases":rows}),
            )
            .unwrap(),
        )
        .unwrap();
    }
}

pub(super) fn replay_steps(options: &Value) -> Value {
    let dir = crate::config::testing::TempDir::new();
    let path = dir.join("config.yaml");
    fs::write(&path, options["file"].as_str().unwrap_or_default()).unwrap();
    let mut files = Vec::new();
    for step in options["steps"].as_array().into_iter().flatten() {
        let text = |name: &str| step[name].as_str().unwrap_or_default();
        let before = fs::read(&path).unwrap();
        let result = match text("op") {
            "save" => {
                let config = step["config"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| String::from_utf8(before.clone()).unwrap());
                match Config::parse(&config) {
                    Ok(cfg) => save_preserving_comments(
                        &path,
                        &cfg,
                        step["migrate"].as_bool().unwrap_or(false),
                    ),
                    Err(error) => return json!({"error":format!("config: {error}"),"files":files}),
                }
            }
            "nested" => {
                let keys: Vec<_> = step["keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|key| key.as_str().unwrap_or_default())
                    .collect();
                update_nested_scalar(&path, &keys, text("value"))
            }
            "write" => write_file(&path, text("body").as_bytes()),
            _ => panic!("unsupported saved operation"),
        };
        if let Err(error) = result {
            assert!(fs::read(&path).unwrap() == before);
            return json!({"error":error.to_string(),"files":files});
        }
        files.push(fs::read_to_string(&path).unwrap());
    }
    json!({"files":files})
}

#[test]
#[ignore = "offline persisted-output ablation audit; descriptors supplied in FU_OWNERS_ABLATION_INPUT"]
fn owner_disabled_matches_persisted_base_outputs() {
    let input: Vec<Value> = serde_json::from_slice(
        &fs::read(std::env::var_os("FU_OWNERS_ABLATION_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    let mut rows = Vec::new();
    let mut mismatches = 0;
    for entry in input {
        let fixture: Value =
            serde_json::from_slice(&fs::read(entry["fixture"].as_str().unwrap()).unwrap()).unwrap();
        let actual = parity::ablation::with_disabled(
            &[parity::ablation::Protection::DeferredCommentOwners],
            || {
                use parity::ablation::{Protection, enabled};
                assert!(
                    [
                        Protection::EmptyMappingPendingComment,
                        Protection::CanonicalVersionComment,
                        Protection::DroppedSingleKeyComment,
                        Protection::InlineEmptyNullComment,
                        Protection::EffectiveBlockScalarComment,
                        Protection::BlockScalarCommentIndentation,
                        Protection::MultipleFlowItemFeet
                    ]
                    .into_iter()
                    .all(enabled)
                );
                replay_steps(&fixture["options"])
            },
        );
        let expected = fixture
            .get("open_ferry_output")
            .or_else(|| fixture.get("protected_rust"))
            .unwrap();
        let same = actual == *expected;
        mismatches += usize::from(!same);
        rows.push(json!({"phase":entry["phase"],"case":fixture["case"],"same":same,"before_sha256":sha256_hex(&serde_json::to_vec(expected).unwrap()),"disabled_sha256":sha256_hex(&serde_json::to_vec(&actual).unwrap())}));
    }
    fs::write(
        std::env::var_os("FU_OWNERS_ABLATION_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
    assert_eq!(mismatches, 0, "persisted disabled-path mismatches");
}
