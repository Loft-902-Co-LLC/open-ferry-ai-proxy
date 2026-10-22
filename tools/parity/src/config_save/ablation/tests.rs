use super::*;
use crate::translator::Translator;

fn saved() -> Vec<Value> {
    let mut rows: Vec<Value> = serde_json::from_str(include_str!(
        "../../cases/testdata/config-save-ablation-replays.json"
    ))
    .unwrap();
    rows.extend(
        serde_json::from_str::<Vec<Value>>(include_str!(
            "../../cases/testdata/config-save-version-history.json"
        ))
        .unwrap(),
    );
    rows.push(
        serde_json::from_str(include_str!(
            "../../cases/testdata/config-save-unablated-block-scalar.json"
        ))
        .unwrap(),
    );
    rows
}
fn from_fixture(f: &Value) -> Case {
    Case::new(f["case"].as_str().unwrap(), "", "").with_options(f["options"].clone())
}

#[test]
fn export_r8_independent_probe_inputs() {
    let Some(dir) = std::env::var_os("PARITY_R8_PROBES") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    fs::create_dir_all(&dir).unwrap();
    let rows:Vec<_>=saved().iter().map(|f| {
        let case=from_fixture(f);
        let protected=Translator::ConfigSave.run_rust(&case).unwrap();
        let clean=comment_free(&case);
        let mut traced=case.clone();traced.options["_ablation_trace"]=true.into();
        json!({"case":case.name,"options":case.options,"protected":protected,"clean":clean.map(|c|c.options),"traced":traced.options,"replays":(1..64).map(|m|json!({"mask":m,"replay":replay(&case,m).map(|r|json!({"output":r.output,"traces":r.traces}))})).collect::<Vec<_>>()})
    }).collect();
    fs::write(
        dir.join("probe-inputs.json"),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
}

fn single(source: &str) -> Case {
    Case::new("proof-boundary", "", "").with_options(
        json!({"file":source,"steps":[{"op":"nested","keys":["auth-dir"],"value":"/x"}]}),
    )
}
fn replay_output(text: &str) -> Replay {
    Replay {
        output: json!({"files":[text]}),
        traces: vec![],
    }
}
fn evaluate_boundary(
    case: &Case,
    go: &Value,
    protected: &Value,
    oracle: &Value,
    replay: &Replay,
    mask: u8,
    matches: &[(u8, bool)],
) -> [bool; 5] {
    checks(Proof {
        case,
        go,
        protected,
        expectation: oracle,
        ablated: replay,
        go_trace: None,
        mask,
        matches,
    })
}

#[test]
fn check_two_rejects_changed_and_dropped_values_after_invalid_go() {
    let case = single("auth-dir: /old # c\n");
    let bad = "auth-dir: |\n# c\n  /x\n";
    assert!(Config::load_bytes(bad.as_bytes()).is_err());
    let replay = Replay {
        output: json!({"files":[bad],"unloadable":"syntax"}),
        traces: vec![Trace {
            rejected: vec![core::ablation::Rejected {
                stage: "write",
                bytes: bad.as_bytes().to_vec(),
            }],
            emissions: vec![core::ablation::Emission {
                bytes: bad.as_bytes().to_vec(),
                comments: vec![core::ablation::WrittenComment {
                    offset: bad.find("# c").unwrap(),
                    bytes: b"# c".to_vec(),
                }],
            }],
        }],
    };
    let oracle = json!({"files":["auth-dir: /x\n"]});
    for wrong in ["auth-dir: changed # c\n", "# c\nauth-dir: null\n"] {
        let protected = json!({"files":[wrong]});
        let c = evaluate_boundary(
            &case,
            &replay.output,
            &protected,
            &oracle,
            &replay,
            1,
            &[(1, true)],
        );
        assert_eq!(c, [true, false, true, true, true], "{c:?}");
        assert!(!accepted(c));
    }
}

#[test]
fn check_three_rejects_invention_excess_copies_and_w_lost_duplicate() {
    for (source, g, r) in [
        (
            "auth-dir: /old # c\n",
            "auth-dir: /x\n",
            "auth-dir: /x # invented\n",
        ),
        (
            "auth-dir: /old # c\n",
            "auth-dir: /x\n",
            "# c\nauth-dir: /x # c\n",
        ),
        (
            "auth-dir: /old # c\ndebug: false # c\n",
            "auth-dir: /x # c\ndebug: false # c\n",
            "auth-dir: /x # c\ndebug: false\n",
        ),
        (
            "models: {}\nplugins: {}\nconfig-version:\n  # hv1\n\n  # hv2\n  8 # cv\n",
            "models: {}\nplugins: {}\nconfig-version: 8\n",
            "models: {}\nplugins: {}\n# hv1\n\n# hv2\nconfig-version: 8 # cv\n",
        ),
    ] {
        let case = single(source);
        let replay = replay_output(g);
        let protected = json!({"files":[r]});
        let c = evaluate_boundary(
            &case,
            &replay.output,
            &protected,
            &protected,
            &replay,
            1,
            &[(1, true)],
        );
        assert_eq!(c, [true, true, false, true, true], "{r} {c:?}");
        assert!(!accepted(c));
    }
}

#[test]
fn faithful_check_four_rejects_an_unrelated_quote_change() {
    let case = single("auth-dir: /old # c\n");
    let replay = replay_output("auth-dir: '/x'\n");
    let protected = json!({"files":["auth-dir: /x # c\n"]});
    let c = evaluate_boundary(
        &case,
        &replay.output,
        &protected,
        &protected,
        &replay,
        1,
        &[(1, true)],
    );
    assert_eq!(c, [true, true, true, false, true]);
    assert!(!accepted(c));
}

#[test]
fn nonminimal_subsets_and_unrelated_extra_changes_stay_different() {
    let case = single("auth-dir: /old # c\n");
    let replay = replay_output("auth-dir: /x\n");
    let protected = json!({"files":["auth-dir: /x # c\n"]});
    let c = evaluate_boundary(
        &case,
        &replay.output,
        &protected,
        &protected,
        &replay,
        3,
        &[(1, true), (3, true)],
    );
    assert_eq!(c, [true, true, true, true, false]);
    assert!(!accepted(c));
    let mut changed = replay.clone();
    changed.output["files"][0] = "auth-dir: /x\ndebug: false\n".into();
    assert!(!reproduces(&replay.output, &changed, None));
    assert!(!minimal(3, &[(1, true), (3, true)]));
    assert!(minimal(3, &[(1, false), (2, false), (3, true)]));
}

#[test]
fn saved_certifier_cases_use_independent_go_expectations() {
    let rows: Vec<Value> = serde_json::from_str(include_str!(
        "../../cases/testdata/config-save-r8-certifier.json"
    ))
    .unwrap();
    let mut results = Vec::new();
    for row in rows {
        let case = from_fixture(&row);
        let go = &row["probes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["kind"] == "options")
            .unwrap()["go"];
        let traced = &row["probes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["kind"] == "traced")
            .unwrap()["go"];
        assert!(
            trace_output(go, traced).is_some(),
            "instrumentation must preserve the complete uninstrumented result"
        );
        let protected = Translator::ConfigSave.run_rust(&case).unwrap();
        if let Some(expected) = row.get("protected") {
            assert_eq!(&protected, expected);
        }
        let result = certify(&case, go, &protected, |candidate| {
            let p = row["probes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["options"] == candidate.options)
                .expect("every oracle input was independently executed with Go");
            Ok(GoResult::Output(p["go"].to_string().into_bytes()))
        })
        .unwrap();
        if let Some(dir) = std::env::var_os("PARITY_R8_PROBES") {
            fs::write(
                std::path::PathBuf::from(dir).join(format!("certified-{}.json", case.name)),
                serde_json::to_vec_pretty(&result.evidence).unwrap(),
            )
            .unwrap();
        }
        results.push((
            case.name,
            result.deviations.is_some(),
            result.evidence["reason"].clone(),
            result.evidence["checks"].clone(),
        ));
    }
    for (name, yes, reason, checks) in &results {
        assert_eq!(
            *yes,
            matches!(
                name.as_str(),
                "random-30-0" | "canonical-paragraph-positive"
            ),
            "{results:?}"
        );
        assert!(
            reason.is_null(),
            "every saved case must reproduce Go: {results:?}"
        );
        assert_eq!(
            checks,
            &json!([true, true, *yes, true, true]),
            "{results:?}"
        );
    }
}

#[test]
fn unloadable_empty_mapping_bytes_require_bound_counts_and_source_owners() {
    let case = single("auth-dir: /old # c\ndebug: false\n");
    let bad = "auth-dir: # c\n{}\n";
    assert!(Config::load_bytes(bad.as_bytes()).is_err());
    let replay = Replay {
        output: json!({"files":[bad],"unloadable":"invalid"}),
        traces: vec![Trace {
            rejected: vec![core::ablation::Rejected {
                stage: "write",
                bytes: bad.as_bytes().to_vec(),
            }],
            emissions: vec![core::ablation::Emission {
                bytes: bad.as_bytes().to_vec(),
                comments: vec![core::ablation::WrittenComment {
                    offset: bad.find("# c").unwrap(),
                    bytes: b"# c".to_vec(),
                }],
            }],
        }],
    };
    let protected = json!({"files":["auth-dir: /x # c\ndebug: false\n"]});
    let c = evaluate_boundary(
        &case,
        &replay.output,
        &protected,
        &protected,
        &replay,
        1,
        &[(1, true)],
    );
    assert_eq!(c, [true; 5]);
    assert!(accepted(c));
    let mut offset = replay.clone();
    offset.traces[0].emissions[0].comments[0].offset += 1;
    let c = evaluate_boundary(
        &case,
        &replay.output,
        &protected,
        &protected,
        &offset,
        1,
        &[(1, true)],
    );
    assert_eq!(c, [true, true, false, true, true]);
    assert!(!accepted(c));
    let misplaced = json!({"files":["auth-dir: /x\ndebug: false # c\n"]});
    let c = evaluate_boundary(
        &case,
        &replay.output,
        &misplaced,
        &protected,
        &replay,
        1,
        &[(1, true)],
    );
    assert_eq!(c, [true, true, false, true, true]);
    assert!(!accepted(c));
}
