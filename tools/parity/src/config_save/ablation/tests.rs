use super::*;

fn source_owner_byte_gap(name: &str) {
    let fixtures: Value =
        serde_json::from_str(include_str!("../testdata/source-owner-byte-gaps.json")).unwrap();
    let fixture = fixtures
        .as_array()
        .unwrap()
        .iter()
        .find(|fixture| fixture["name"] == name)
        .unwrap();
    let operation = &fixture["operation"];
    let source = if operation["op"] == "write" {
        operation["body"].as_str().unwrap()
    } else {
        fixture["source"].as_str().unwrap()
    };
    let protected = fixture["protected"].as_str().unwrap();
    let mut evidence = fixture["witness"].clone();
    // The source, operation and Go inventories are real pinned-Go receipts.
    // Grant stability to isolate check 4, independently of the current writer.
    let frame = &mut evidence["retained_source_owner"];
    frame["protected"] = protected.into();
    frame["repeat_input"] = protected.into();
    frame["repeat_operation"] = operation.clone();
    frame["repeat_ok"] = true.into();
    frame["repeated"] = protected.into();
    frame["normalized"] = protected.into();
    let conditions =
        core::retained_source_owner(source, protected, operation, &evidence).conditions;
    assert!(
        conditions == [true; 5],
        "{name}: source-owner proof setup failed: {conditions:?}"
    );
    let go = frame_go(&evidence);
    let case = Case::new(name, "", "").with_options(json!({
        "file": source, "steps": [operation], "_ownership_evidence": [evidence]
    }));
    let protected = json!({"files": [protected]});
    let replay = Replay {
        output: go.clone(),
        traces: vec![],
    };
    let matches = [(1, true)];
    let result = checks(Proof {
        case: &case,
        go: &go,
        protected: &protected,
        ablated: &replay,
        go_trace: None,
        expectation: &go,
        mask: 1,
        matches: &matches,
    });
    assert!(!accepted(result), "{name}: unrelated bytes were certified");
    assert_eq!(result, [true, true, true, false, true], "{name}");
}

fn frame_go(evidence: &Value) -> Value {
    evidence["retained_source_owner"]["go"].clone()
}

fn source_owner_layout_gap(name: &str) {
    let fixtures: Value =
        serde_json::from_str(include_str!("../testdata/source-owner-layout-gaps.json")).unwrap();
    let fixture = fixtures
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap();
    assert_source_owner_layout_gap(fixture);
}

fn assert_source_owner_layout_gap(fixture: &Value) {
    let name = fixture["name"].as_str().unwrap();
    let source = fixture["source"].as_str().unwrap();
    let protected = fixture["protected"].as_str().unwrap();
    let operation = &fixture["operation"];
    let evidence = &fixture["evidence"];
    // Go replies and inventories are pinned-Go receipts. The small controls
    // alter layout and grant stability; the two measured cases use the actual
    // owners writer output and repeat. Their provenance distinguishes them.
    let proof = core::retained_source_owner(source, protected, operation, evidence);
    assert!(!proof.accepted(), "{name}: added layout was certified");
    assert_eq!(proof.conditions, [false, true, true, true, true]);
    let case = Case::new(name, "", "").with_options(json!({
        "file":source,"steps":[operation],"_ownership_evidence":[evidence]
    }));
    let go = frame_go(evidence);
    assert!(
        !ownership::analyze(
            &case,
            0,
            source,
            go["files"][0].as_str(),
            protected,
            &[],
            true
        )
        .accepted()
    );
    let protected = json!({"files":[protected]});
    let replay = Replay {
        output: go.clone(),
        traces: vec![],
    };
    let result = checks(Proof {
        case: &case,
        go: &go,
        protected: &protected,
        ablated: &replay,
        go_trace: None,
        expectation: &go,
        mask: 1,
        matches: &[(1, true)],
    });
    assert_eq!(result, [true, true, false, true, true]);
    let failure = diagnostics::comments(&case, &protected, &replay).unwrap_err();
    assert_eq!(failure["reason"], "physical_layout");
    assert_eq!(
        failure["source_owner_checks"],
        json!([false, true, true, true, true])
    );
}

#[test]
fn retained_source_owner_refuses_a_new_trailing_space() {
    source_owner_layout_gap("trailing");
}

#[test]
fn retained_source_owner_refuses_a_new_blank_line() {
    source_owner_layout_gap("blank");
}

#[test]
fn retained_source_owner_refuses_measured_trailing_space() {
    source_owner_layout_gap("trailing-key-region");
}

#[test]
fn retained_source_owner_refuses_measured_blank_inside_a_key_region() {
    source_owner_layout_gap("blank-key-region");
}

#[test]
fn retained_source_owner_records_later_exemptions_after_an_invalid_go_owner() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../testdata/source-owner-wrong-destination.json"
    ))
    .unwrap();
    let source = fixture["source"].as_str().unwrap();
    let protected = fixture["protected"].as_str().unwrap();
    let mut evidence = fixture["evidence"].clone();
    let go = frame_go(&evidence);
    assert!(core::source_owner_layout(
        source,
        go["files"][0].as_str().unwrap(),
        protected
    ));
    // Deliberately invalidate the first Go-owner comparison. The destination
    // check must still see the exemption and record its different-value fault.
    evidence["retained_source_owner"]["go_inventory"]["comments"][0]["source"]["slot"] =
        "line".into();
    let proof = core::retained_source_owner(source, protected, &fixture["operation"], &evidence);
    assert!(!proof.conditions[0]);
    assert!(!proof.conditions[1]);
    assert!(!proof.accepted());
}

#[test]
fn retained_source_owner_accepts_an_edited_value_go_kept_at_its_owner() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../testdata/source-owner-kept-value-edit.json"
    ))
    .unwrap();
    let source = fixture["source"].as_str().unwrap();
    let protected = fixture["protected"].as_str().unwrap();
    let operation = &fixture["operation"];
    let evidence = &fixture["evidence"];
    // These are pinned-Go receipts and an observed writer repeat for
    // r2-040-nested#0. The edited line comment is kept by Go, not excused.
    let proof = core::retained_source_owner(source, protected, operation, evidence);
    assert!(
        proof.accepted(),
        "a Go-kept edited value prevented the source-owner proof: {:?}",
        proof.conditions
    );
    assert_eq!(proof.conditions, [true; 5]);
    let case = Case::new("native-kept-comment-value-edit", "", "").with_options(json!({
        "file":source,"steps":[operation],"_ownership_evidence":[evidence]
    }));
    let go = frame_go(evidence);
    let analysis = ownership::analyze(
        &case,
        0,
        source,
        go["files"][0].as_str(),
        protected,
        &[],
        true,
    );
    assert!(analysis.accepted());
    assert!(analysis.rules.contains(&"retained-source-owner"));
    let protected = json!({"files":[protected]});
    let replay = Replay {
        output: go.clone(),
        traces: vec![],
    };
    let result = checks(Proof {
        case: &case,
        go: &go,
        protected: &protected,
        ablated: &replay,
        go_trace: None,
        expectation: &go,
        mask: 1,
        matches: &[(1, true)],
    });
    assert_eq!(result, [true; 5]);
}

#[test]
fn retained_source_owner_refuses_a_different_real_go_key_value_destination() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../testdata/source-owner-wrong-destination.json"
    ))
    .unwrap();
    let source = fixture["source"].as_str().unwrap();
    let protected = fixture["protected"].as_str().unwrap();
    let operation = &fixture["operation"];
    let evidence = &fixture["evidence"];
    // Go's source, output and inventories are real. The replay destination and
    // protected placement are deliberately redirected; repeat is granted.
    let proof = core::retained_source_owner(source, protected, operation, evidence);
    assert!(
        !proof.accepted(),
        "a replay destination at a different real Go key/value was certified"
    );
    assert_eq!(proof.conditions, [false, true, true, true, true]);
    let case = Case::new("native-wrong-key-destination", "", "").with_options(json!({
        "file":source,"steps":[operation],"_ownership_evidence":[evidence]
    }));
    let go = frame_go(evidence);
    assert!(
        !ownership::analyze(
            &case,
            0,
            source,
            go["files"][0].as_str(),
            protected,
            &[],
            true
        )
        .accepted()
    );
    let protected = json!({"files":[protected]});
    let replay = Replay {
        output: go.clone(),
        traces: vec![],
    };
    let result = checks(Proof {
        case: &case,
        go: &go,
        protected: &protected,
        ablated: &replay,
        go_trace: None,
        expectation: &go,
        mask: 1,
        matches: &[(1, true)],
    });
    assert_eq!(result, [true, true, false, true, true]);
}

#[test]
fn retained_source_owner_byte_gap_b1_reorder() {
    source_owner_byte_gap("B1-reorder");
}
#[test]
fn retained_source_owner_byte_gap_b2_quote() {
    source_owner_byte_gap("B2-quote");
}
#[test]
fn retained_source_owner_byte_gap_bh1_number_spelling() {
    source_owner_byte_gap("BH1-hex-to-dec");
}
#[test]
fn retained_source_owner_byte_gap_ba1_anchor() {
    source_owner_byte_gap("BA1-keep-anchor");
}
#[test]
fn retained_source_owner_byte_gap_w1_write_reorder() {
    source_owner_byte_gap("W1-reorder");
}

#[test]
fn retained_source_owner_accepts_native_replay_and_an_observed_writer_repeat() {
    let fixture: Value =
        serde_json::from_str(include_str!("../testdata/source-owner-retained.json")).unwrap();
    let source = fixture["source"].as_str().unwrap();
    let protected = fixture["protected"].as_str().unwrap();
    let operation = &fixture["operation"];
    let evidence = &fixture["evidence"];
    let go = evidence["retained_source_owner"]["go"]["files"][0]
        .as_str()
        .unwrap();
    assert!(!core::comment_ownership_analysis(source, Some(go), protected, &[]).accepted());
    assert_eq!(
        core::retained_source_owner(source, protected, operation, evidence).conditions,
        [true; 5]
    );
    let case = Case::new("native-retained-source-owner", "", "").with_options(json!({
        "file":source,"steps":[operation],"_ownership_evidence":[evidence]
    }));
    let accepted = ownership::analyze(&case, 0, source, Some(go), protected, &[], true);
    assert!(accepted.accepted());
    assert!(accepted.rules.contains(&"retained-source-owner"));
}

#[test]
fn retained_source_owner_keeps_four_measured_writer_gaps_different() {
    // Eligibility now includes partial and repeated inventories. These measured
    // losses must still fail the complete proof and final ownership decision.
    let fixtures: Value =
        serde_json::from_str(include_str!("../testdata/source-owner-gaps.json")).unwrap();
    let fixtures = fixtures.as_array().unwrap();
    assert_eq!(fixtures.len(), 4);
    for fixture in fixtures {
        let name = fixture["case"].as_str().unwrap();
        let source = fixture["source"].as_str().unwrap();
        let protected = fixture["protected"].as_str().unwrap();
        let operation = &fixture["operation"];
        let evidence = &fixture["evidence"];
        assert!(
            core::source_owner_candidate(source, protected),
            "{name}: the broad eligibility hint excluded a proof candidate"
        );
        assert!(evidence["retained_source_owner"]["identity"]["occurrences"].is_array());
        assert!(evidence["retained_source_owner"]["go_inventory"]["comments"].is_array());
        assert!(
            !core::retained_source_owner(source, protected, operation, evidence).accepted(),
            "{name}: a measured writer gap passed the new proof"
        );
        let case = Case::new(name, "", "").with_options(json!({
            "file":source,"steps":[operation],"_ownership_evidence":[evidence]
        }));
        let go = evidence["retained_source_owner"]["go"]["files"][0].as_str();
        assert!(
            !ownership::analyze(&case, 0, source, go, protected, &[], true).accepted(),
            "{name}: a measured writer gap became certified"
        );
    }
}
#[test]
fn retained_source_owner_measured_tracked_gap_reaches_loss_conditions() {
    // This measured review probe has real pinned-Go receipts for every owner.
    // Its deliberately bad candidate and granted repeat isolate conditions
    // one and five; it is not claimed as an observed product output.
    let fixture: Value =
        serde_json::from_str(include_str!("../testdata/source-owner-tracked-gap.json")).unwrap();
    let source = fixture["source"].as_str().unwrap();
    let protected = fixture["protected"].as_str().unwrap();
    let operation = &fixture["operation"];
    let evidence = &fixture["evidence"];
    for occurrence in evidence["retained_source_owner"]["identity"]["occurrences"]
        .as_array()
        .unwrap()
    {
        assert!(
            evidence["moves"].as_array().unwrap().iter().any(|m| {
                m["source"] == occurrence["source"] && m["ordinal"] == occurrence["ordinal"]
            }),
            "the measured gap has an untracked source owner"
        );
    }
    // This candidate has no surviving protected comments, so even the broad
    // hint can refuse it. The proof assertions below still isolate the loss.
    assert!(!core::source_owner_candidate(source, protected));
    let proof = core::retained_source_owner(source, protected, operation, evidence);
    assert!(
        proof.conditions == [false, true, true, true, false],
        "measured-gap conditions: {:?}",
        proof.conditions
    );
    assert!(!proof.accepted());
    let case = Case::new("measured-tracked-comment-loss", "", "").with_options(json!({
        "file":source,"steps":[operation],"_ownership_evidence":[evidence]
    }));
    let go = evidence["retained_source_owner"]["go"]["files"][0].as_str();
    assert!(!ownership::analyze(&case, 0, source, go, protected, &[], true).accepted());
}

use crate::translator::Translator;

#[test]
fn review_r13_comment_free_oracle_failure_boundaries_are_consistent() {
    let decode = |value: Value, steps| {
        oracle_output(GoResult::Output(serde_json::to_vec(&value).unwrap()), steps)
    };
    for (value, steps) in [
        (json!({"files":["x: 1\n"]}), 1),
        (json!({"files":[]}), 0),
        (json!({"files":[],"error":"refused"}), 1),
        (json!({"files":["x: 1\n"],"error":"refused"}), 2),
        (json!({"files":["x: 1\n"],"unloadable":"refused"}), 1),
    ] {
        assert!(decode(value, steps).is_ok());
    }
    for (value, steps) in [
        (json!({"files":[],"unexpected":true}), 1),
        (json!({"files":["x: 1\n"],"unexpected":true}), 1),
        (json!({"files":[],"error":"x","unloadable":"y"}), 1),
        (json!({"files":[1]}), 1),
        (json!({"files":[]}), 1),
        (json!({"files":["x: 1\n","x: 2\n"]}), 1),
        (json!({"files":["x: 1\n"],"error":"refused"}), 1),
        (json!({"files":[],"unloadable":"refused"}), 1),
        (
            json!({"files":["x: 1\n","x: 2\n"],"unloadable":"refused"}),
            1,
        ),
        (json!({"files":[],"error":""}), 1),
        (json!({"files":["x: 1\n"],"unloadable":""}), 1),
    ] {
        assert_eq!(
            decode(value, steps).unwrap_err(),
            "comment_free_go_malformed"
        );
    }
    assert_eq!(
        oracle_output(GoResult::Output(br#"{"error":"x"}"#.to_vec()), 1).unwrap_err(),
        "comment_free_go_malformed"
    );
    assert_eq!(
        oracle_output(GoResult::Output(b"not JSON".to_vec()), 1).unwrap_err(),
        "comment_free_go_malformed"
    );
    assert_eq!(
        oracle_output(GoResult::Panic("panic".into()), 1).unwrap_err(),
        "comment_free_go_panicked"
    );
}

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
        let mut case = from_fixture(&row);
        // Replay files are inputs, not proof authorities. This receipt must
        // disappear before either oracle request construction or certification.
        case.options["_ownership_evidence"] = json!([{"untrusted-replay-receipt":true}]);
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
            assert!(candidate.options.get("_ownership_evidence").is_none());
            // Historical fixtures contain no independent identity replay.
            // Refuse that optional oracle instead of inventing provenance;
            // all of their original strict negative assertions stay active.
            if candidate.options.get("_ownership_request").is_some() {
                return Ok(GoResult::Panic("identity fixture unavailable".into()));
            }
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

fn neighbour_fixture(name: &str) -> Value {
    let fixtures: Value = serde_json::from_str(include_str!(
        "../testdata/source-owner-blank-neighbours.json"
    ))
    .unwrap();
    fixtures
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap()
        .clone()
}

#[test]
fn retained_source_owner_refuses_a_blank_moved_within_a_region() {
    assert_source_owner_layout_gap(&neighbour_fixture("p2-blank-after-shared-line"));
}

#[test]
fn retained_source_owner_refuses_a_second_blank_paid_for_elsewhere() {
    assert_source_owner_layout_gap(&neighbour_fixture(
        "p4b-source-blank-moved-before-last-item",
    ));
}

#[test]
fn retained_source_owner_refuses_measured_blank_moved_within_a_region() {
    assert_source_owner_layout_gap(&neighbour_fixture("measured-moved-blank"));
}

fn other_rule_layout_checks(name: &str) -> [bool; 5] {
    let f = neighbour_fixture(name);
    let source = f["source"].as_str().unwrap();
    let protected = f["protected"].as_str().unwrap();
    let go = f["go"].as_str().unwrap();
    let mut options = json!({"file":source,"steps":[f["operation"]]});
    if !f["evidence"].is_null() {
        options["_ownership_evidence"] = json!([f["evidence"]]);
    }
    let case = Case::new(name, "", "").with_options(options);
    let strict = ownership::analyze(&case, 0, source, Some(go), protected, &[], true);
    assert!(
        strict.accepted(),
        "fixture needs an accepted other-rule proof"
    );
    assert!(!strict.rules.contains(&"retained-source-owner"));
    let go = json!({"files":[go]});
    let protected = json!({"files":[protected]});
    let replay = Replay {
        output: go.clone(),
        traces: vec![],
    };
    checks(Proof {
        case: &case,
        go: &go,
        protected: &protected,
        ablated: &replay,
        go_trace: None,
        expectation: &go,
        mask: 1,
        matches: &[(1, true)],
    })
}

#[test]
fn other_ownership_rules_refuse_a_measured_added_blank() {
    assert_eq!(
        other_rule_layout_checks("other-rule-added-blank"),
        [true, true, false, true, true]
    );
}

#[test]
fn other_ownership_rules_keep_an_identical_go_v8_migration() {
    let f = neighbour_fixture("identical-go-v8-migration");
    assert_eq!(f["go"], f["protected"]);
    assert_eq!(
        other_rule_layout_checks("identical-go-v8-migration"),
        [true; 5]
    );
}

#[test]
fn retained_source_owner_refuses_a_blank_moved_across_a_comment() {
    assert_source_owner_layout_gap(&neighbour_fixture(
        "x4-blank-before-kept-comment-same-region",
    ));
}

#[test]
fn retained_source_owner_keeps_its_exact_input_go_layout_context() {
    let f: Value = serde_json::from_str(include_str!(
        "../testdata/source-owner-exact-input-context.json"
    ))
    .unwrap();
    let mut options = f["options"].clone();
    options["_ownership_evidence"] = f["source_identities"].clone();
    let case = Case::new("exact-input-context", "", "").with_options(options);
    let go = &f["original_go"];
    let native_go =
        &case.options["_ownership_evidence"][1]["retained_source_owner"]["go"]["files"][0];
    assert!(
        native_go != &go["files"][1],
        "the two Go contexts must differ"
    );
    let replay = Replay {
        output: f["ablated_rust"].clone(),
        traces: vec![],
    };
    let result = checks(Proof {
        case: &case,
        go,
        protected: &f["protected_rust"],
        ablated: &replay,
        go_trace: None,
        expectation: &f["comment_free_go"],
        mask: 1,
        matches: &[(1, true)],
    });
    assert_eq!(result, [true; 5]);
}
