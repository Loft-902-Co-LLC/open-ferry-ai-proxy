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
fn check_two_does_not_treat_non_string_keyed_maps_as_equal_values() {
    let case = single("x: {1: old}\nauth-dir: /old # c\n");
    let bad = "x: {1: old}\nauth-dir: # c\n{}\n";
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
    let protected = json!({"files":["x: {1: changed}\nauth-dir: /x # c\n"]});
    let expected = json!({"files":["x: {1: old}\nauth-dir: /x\n"]});
    let checks = evaluate_boundary(
        &case,
        &replay.output,
        &protected,
        &expected,
        &replay,
        1,
        &[(1, true)],
    );
    assert_eq!(checks, [true, false, true, true, true]);
    assert!(!accepted(checks));
    let failures = diagnostics::failures(
        Proof {
            case: &case,
            go: &replay.output,
            protected: &protected,
            expectation: &expected,
            ablated: &replay,
            go_trace: None,
            mask: 1,
            matches: &[(1, true)],
        },
        checks,
    );
    assert_eq!(failures[0]["reason"], "unsupported_value_path");
    assert_eq!(failures[0]["path"], json!(["x"]));
}

fn invalid_review_replay(bad: &str, comments: &[&str]) -> Replay {
    Replay {
        output: json!({"files":[bad],"unloadable":"invalid"}),
        traces: vec![Trace {
            rejected: vec![core::ablation::Rejected {
                stage: "write",
                bytes: bad.as_bytes().to_vec(),
            }],
            emissions: vec![core::ablation::Emission {
                bytes: bad.as_bytes().to_vec(),
                comments: comments
                    .iter()
                    .map(|comment| core::ablation::WrittenComment {
                        offset: bad.find(comment).unwrap(),
                        bytes: comment.as_bytes().to_vec(),
                    })
                    .collect(),
            }],
        }],
    }
}

#[test]
fn review_r11_03_binary_bytes_must_not_compare_lossily() {
    let case = single("x: !!binary /w==\nauth-dir: /old # c\n");
    let replay = invalid_review_replay("x: !!binary /w==\nauth-dir: # c\n{}\n", &["# c"]);
    let protected = json!({"files":["x: !!binary /g==\nauth-dir: /x # c\n"]});
    let oracle = json!({"files":["x: !!binary /w==\nauth-dir: /x\n"]});
    let checks = evaluate_boundary(
        &case,
        &replay.output,
        &protected,
        &oracle,
        &replay,
        1,
        &[(1, true)],
    );
    eprintln!("finding 3 checks={checks:?}");
    assert!(!checks[1]);
    assert!(!accepted(checks));
}

#[test]
fn review_r11_05_multidocument_snapshots_fail_checks_two_through_four() {
    let source = "auth-dir: /x\n---\ntail: 1 # keep\n";
    let wrong = "auth-dir: /x\n---\ntail: 2 # invented\n";
    let case = single(source);
    let replay = replay_output(source);
    let checks = evaluate_boundary(
        &case,
        &replay.output,
        &json!({"files":[wrong]}),
        &replay.output,
        &replay,
        1,
        &[(1, true)],
    );
    eprintln!("finding 5 checks={checks:?}");
    assert_eq!(&checks[1..4], &[false, false, false]);
}

#[test]
fn review_r11_06_unobserved_step_cannot_use_unloadable_line_promotion() {
    let mut case = single("auth-dir: /old # bad\ndebug: false # later\n");
    case.options["steps"]
        .as_array_mut()
        .unwrap()
        .push(json!({"op":"nested","keys":["auth-dir"],"value":"/y"}));
    let replay = invalid_review_replay(
        "auth-dir: # bad\n{}\ndebug: false # later\n",
        &["# bad", "# later"],
    );
    let protected = json!({"files":["auth-dir: /x # bad\ndebug: false # later\n", "auth-dir: /y # bad\n# later\ndebug: false\n"]});
    let oracle = json!({"files":["auth-dir: /x\ndebug: false\n", "auth-dir: /y\ndebug: false\n"]});
    let checks = evaluate_boundary(
        &case,
        &replay.output,
        &protected,
        &oracle,
        &replay,
        1,
        &[(1, true)],
    );
    eprintln!("finding 6 checks={checks:?}");
    assert!(!checks[2]);
}

#[test]
fn review_r11_opus_unobserved_steps_cannot_drop_or_promote() {
    let source = "auth-dir: /old # c\ndebug: false # d\n";
    let mut case = single(source);
    case.options["steps"]
        .as_array_mut()
        .unwrap()
        .push(json!({"op":"nested","keys":["auth-dir"],"value":"/y"}));
    let replay = invalid_review_replay(
        "auth-dir: |\n# c\n  /x\ndebug: false # d\n",
        &["# c", "# d"],
    );
    let oracle = json!({"files":["auth-dir: /x\ndebug: false\n","auth-dir: /y\ndebug: false\n"]});
    let r0 = "# c\nauth-dir: /x\ndebug: false # d\n";
    for (name, first, last, reason, key) in [
        (
            "drop_after",
            r0,
            "auth-dir: /y\ndebug: false\n",
            "source_owner_witness",
            "auth-dir",
        ),
        (
            "rule_a_after",
            r0,
            "# c\nauth-dir: /y\n# d\ndebug: false\n",
            "owner_budget",
            "debug",
        ),
        (
            "drop_step0",
            "auth-dir: /x\ndebug: false\n",
            "auth-dir: /y\ndebug: false\n",
            "go_budget",
            "auth-dir",
        ),
    ] {
        let protected = json!({"files":[first,last]});
        let checks = evaluate_boundary(
            &case,
            &replay.output,
            &protected,
            &oracle,
            &replay,
            1,
            &[(1, true)],
        );
        let failure = diagnostics::comments(&case, &protected, &replay).unwrap_err();
        eprintln!("{name}: {checks:?}; {failure}");
        assert!(!checks[2], "{name}");
        assert_eq!(failure["reason"], reason, "{name}");
        assert_eq!(failure["owner"]["path"][0]["key"], key, "{name}");
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
        if result.deviations.is_some() {
            assert_eq!(
                result.evidence["trials"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()["owner_rules"],
                result.evidence["owner_rules"]
            );
        }
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
            // The former 30-0 positive has a placement on a loadable
            // step that the two narrow owner rules do not license.
            name == "canonical-paragraph-positive",
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

#[test]
fn proof_failures_locate_the_first_snapshot_byte_and_nested_value() {
    let go = json!({"files":["auth-dir: /ok\n","auth-dir: /éx\n"]});
    let replay = Replay {
        output: json!({"files":["auth-dir: /ok\n","auth-dir: /éy\n"]}),
        traces: vec![],
    };
    assert_eq!(
        diagnostics::no_reproduction(&go, &replay, None),
        json!({"check":1,"step":1,"file":1,"reason":"snapshot_bytes","offset":"auth-dir: /éx\n".find('x').unwrap()})
    );
    let case = Case::new("diagnostic", "", "")
        .with_options(json!({"file":"auth-dir: /old\n","steps":[{},{}]}));
    let expected =
        json!({"files":["auth-dir: /ok\n","plugins:\n  configs:\n    demo:\n      text: old\n"]});
    let protected =
        json!({"files":["auth-dir: /ok\n","plugins:\n  configs:\n    demo:\n      text: new\n"]});
    let p = Proof {
        case: &case,
        go: &go,
        protected: &protected,
        ablated: &replay,
        go_trace: None,
        expectation: &expected,
        mask: 1,
        matches: &[(1, true)],
    };
    assert_eq!(
        diagnostics::failures(p, [true, false, true, true, true]),
        vec![
            json!({"check":2,"step":1,"file":1,"reason":"value_path","path":["plugins","configs","demo","text"]})
        ]
    );
}

#[test]
fn proof_comment_failure_names_the_text_node_and_slot() {
    let source = "# own\na: 1\nb: 2\n";
    let case = Case::new("diagnostic", "", "").with_options(json!({"file":source,"steps":[{},{}]}));
    let protected = json!({"files":[source,"a: 1 # own\nb: 2\n"]});
    let replay = Replay {
        output: json!({"files":[source,source]}),
        traces: vec![],
    };
    let failure = diagnostics::comments(&case, &protected, &replay).unwrap_err();
    assert_eq!(failure["check"], 3);
    assert_eq!(failure["step"], 1);
    assert_eq!(failure["file"], 1);
    assert_eq!(failure["reason"], "owner_budget");
    assert_eq!(failure["text"], "# own");
    assert_eq!(failure["owner"]["node"], "value");
    assert_eq!(failure["owner"]["slot"], "line");
    assert_eq!(failure["owner"]["path"][0]["key"], "a");
}
