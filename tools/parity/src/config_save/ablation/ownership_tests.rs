use super::*;

#[test]
fn consumed_replay_worker_panic_keeps_the_sequential_payload() {
    let case = Case::new("consumed-replay-panic", "", "").with_options(json!({
        "file":"auth-dir: /old # source\n",
        "steps":[{"op":"nested","keys":["auth-dir"],"value":"/new"}]
    }));
    let protected = replay(&case, 0).unwrap().output;
    let go = json!({"files":["auth-dir: /different\n"]});
    let run = |parallel| {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            certify_full_with_replay(
                &case,
                &go,
                &protected,
                |_| unreachable!("no reproducing subset"),
                parallel,
                |case, mask| {
                    if mask == 5 {
                        panic!("consumed replay panic");
                    }
                    replay(case, mask)
                },
            )
        }));
        let Err(payload) = result else {
            panic!("the consumed replay must panic")
        };
        crate::panic_message(&*payload)
    };
    assert_eq!(run(false), "panic: consumed replay panic");
    assert_eq!(run(true), run(false));

    let records = |jobs, parallel_tail| {
        let original_go = GoResult::Output(serde_json::to_vec(&go).unwrap());
        let mut records = Vec::new();
        crate::parallel::for_each(
            4,
            jobs,
            |index| {
                if index == 1 {
                    let certify = || {
                        certify_full_with_replay(
                            &case,
                            &go,
                            &protected,
                            |_| unreachable!("no reproducing subset"),
                            parallel_tail,
                            |case, mask| {
                                if mask == 5 {
                                    panic!("consumed replay panic");
                                }
                                replay(case, mask)
                            },
                        )
                    };
                    let result = if jobs > 1 {
                        without_parallel_replay(certify)
                    } else {
                        certify()
                    };
                    result.unwrap();
                }
                crate::evaluate(crate::Translator::ConfigSave, &case, &original_go)
            },
            |index, evaluated| {
                records.push(json!([
                    index,
                    crate::parallel_tests::panic_record(&case, &evaluated)
                ]));
                Ok::<_, ()>(())
            },
            |_, payload| {
                crate::evaluate_panic(crate::Translator::ConfigSave, &case, &original_go, payload)
            },
        )
        .unwrap();
        assert_eq!(records.len(), 4, "record later cases after the panic");
        assert_eq!(
            records[1][1]["rust"],
            json!({"Err":"panic: consumed replay panic"})
        );
        assert!(records[1][1]["kinds"].is_null());
        records
    };
    let sequential = records(1, false);
    assert_eq!(sequential, records(1, true));
    assert_eq!(sequential, records(4, true));
}

#[test]
fn unconsumed_speculative_replay_panic_cannot_change_a_refusal() {
    let case = Case::new("unconsumed-replay-panic", "", "").with_options(json!({
        "file":"auth-dir: /old # source\n",
        "steps":[{"op":"nested","keys":["auth-dir"],"value":"/new"}]
    }));
    let protected = replay(&case, 0).unwrap().output;
    let go = json!({"files":["auth-dir: /different\n"]});
    let run = |parallel| {
        let panicked = std::sync::atomic::AtomicBool::new(false);
        let mut oracle_calls = 0;
        let attempt = certify_full_with_replay(
            &case,
            &go,
            &protected,
            |_| {
                oracle_calls += 1;
                Ok(GoResult::Panic("independent oracle unavailable".into()))
            },
            parallel,
            |case, mask| {
                if mask == 5 {
                    panicked.store(true, std::sync::atomic::Ordering::Relaxed);
                    panic!("unconsumed replay panic");
                }
                let mut replay = replay(case, mask)?;
                // Force the first tail mask to reach the existing oracle refusal.
                // The later panic is outside the sequential execution prefix.
                if mask == 3 {
                    replay.output = go.clone();
                }
                Some(replay)
            },
        )
        .unwrap();
        assert_eq!(oracle_calls, 1);
        assert_eq!(
            panicked.load(std::sync::atomic::Ordering::Relaxed),
            parallel
        );
        assert!(attempt.deviations.is_none());
        assert_eq!(attempt.evidence["reason"], "check 2: Go oracle panicked");
        attempt.evidence
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn replay_tail_workers_receive_the_callers_disabled_protections() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let case = Case::new("inherited-replay-scope", "", "").with_options(json!({
        "file":"auth-dir: /old # source\n",
        "steps":[{"op":"nested","keys":["auth-dir"],"value":"/new"}]
    }));
    let caller = std::thread::current().id();
    let workers = AtomicUsize::new(0);
    with_disabled(&[Protection::DeferredCommentOwners], || {
        let inherited = core::disabled_protections(&ALL);
        assert_eq!(inherited, 128);
        let protected = replay(&case, 0).unwrap().output;
        // No subset reproduces this output, so the threaded tail must run.
        let go = json!({"files":["auth-dir: /different\n"]});
        let run = |parallel| {
            certify_full_with_replay(
                &case,
                &go,
                &protected,
                |_| unreachable!("no reproducing subset"),
                parallel,
                |case, mask| {
                    if std::thread::current().id() != caller {
                        workers.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(core::disabled_protections(&ALL), 0);
                        assert_eq!(mask & inherited, inherited, "worker lost caller scope");
                    }
                    replay(case, mask)
                },
            )
            .unwrap()
        };
        let parallel = run(true);
        let sequential = run(false);
        assert_eq!(parallel.deviations, sequential.deviations);
        assert_eq!(parallel.evidence, sequential.evidence);
        assert_eq!(core::disabled_protections(&ALL), inherited);
    });
    assert!(workers.load(Ordering::Relaxed) > 0);
    assert_eq!(core::disabled_protections(&ALL), 0);
}

#[test]
fn ordered_parallel_search_matches_sequential_search_and_inherits_scope() {
    let case = Case::new("ordered-replays", "", "").with_options(json!({
        "file":"# source\nauth-dir: /old\n",
        "steps":[{"op":"nested","keys":["auth-dir"],"value":"/new"}]
    }));
    with_disabled(&[Protection::DroppedSingleKeyComment], || {
        let protected = replay(&case, 0).unwrap().output;
        for go in [
            protected.clone(),
            json!({"files":["auth-dir: /different\n"]}),
        ] {
            let mut oracle = |_: &Case| Ok(GoResult::Output(serde_json::to_vec(&go).unwrap()));
            let fast = certify_full(&case, &go, &protected, &mut oracle, true).unwrap();
            let full = certify_full(&case, &go, &protected, &mut oracle, false).unwrap();
            assert_eq!(fast.deviations, full.deviations);
            assert_eq!(fast.evidence, full.evidence);
        }
    });
}

#[test]
fn cached_owner_proof_key_includes_all_native_receipts() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../testdata/owners-repairs.json")).unwrap();
    let f = &fixtures[0];
    let mut case = Case::new("receipt-cache", "", "").with_options(json!({
        "file":f["source"],"steps":[f["operation"]],"_ownership_evidence":[f["evidence"]]
    }));
    ownership::reset_proofs();
    assert!(
        ownership::source_owner(
            &case,
            0,
            f["source"].as_str().unwrap(),
            f["protected"].as_str().unwrap()
        )
        .unwrap()
        .accepted()
    );
    case.options["_ownership_evidence"][0]["retained_source_owner"]["protected_inventory"]["comments"]
        [0]["source"]["slot"] = "invalid".into();
    assert!(
        !ownership::source_owner(
            &case,
            0,
            f["source"].as_str().unwrap(),
            f["protected"].as_str().unwrap()
        )
        .unwrap()
        .accepted()
    );
}

#[test]
fn fast_owner_certificate_requires_the_full_proof_and_matches_exhaustive_mode() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../testdata/owners-repairs.json")).unwrap();
    let fixture = &fixtures[0];
    let case = Case::new("native-foot-repair", "", "")
        .with_options(json!({"file":fixture["source"],"steps":[fixture["operation"]]}));
    let go = &fixture["evidence"]["retained_source_owner"]["exact_go"];
    let protected = json!({"files":[fixture["protected"]]});
    for corrupt in [false, true] {
        let mut witness = fixture["evidence"].clone();
        if corrupt {
            witness["retained_source_owner"]["protected_inventory"]["comments"][0]["source"]["slot"] =
                "invalid".into();
        }
        let mut oracle = |_: &Case| Ok(GoResult::Output(serde_json::to_vec(&witness).unwrap()));
        let proof = ownership::certify_repair(&case, go, &protected, &mut oracle).unwrap();
        assert_eq!(
            proof.is_some(),
            !corrupt,
            "fast path skipped a proof condition"
        );
        if !corrupt {
            let fast = certify_mode(&case, go, &protected, &mut oracle, false).unwrap();
            let mut full = certify_mode(&case, go, &protected, &mut oracle, true).unwrap();
            assert_eq!(fast.deviations, full.deviations);
            assert!(
                full.evidence
                    .as_object_mut()
                    .unwrap()
                    .remove("full_search")
                    .is_some()
            );
            assert_eq!(fast.evidence, full.evidence);
        }
    }
}

#[test]
fn oracle_requests_bind_each_protected_source_and_complete_operation() {
    let case = Case::new("binding", "", "").with_options(json!({"file":"a: 1\n","steps":[{"op":"nested","keys":["a"],"value":"2"},{"op":"save","migrate":true},{"op":"write","body":"b: 3\n"}]}));
    let protected = json!({"files":["a: '2'\n","config-version: 8\n","b: 3\n"]});
    let mut requests = Vec::new();
    let bound = ownership::bind(&case, &protected, &mut |request| {
        let witness = request.options["_ownership_request"].clone();
        requests.push(witness.clone());
        Ok(GoResult::Output(
            serde_json::to_vec(
                &json!({"source":witness["source"],"operation":witness["operation"],"moves":[]}),
            )
            .unwrap(),
        ))
    })
    .unwrap();
    assert_eq!(requests.len(), 3);
    for (i, expected) in ["a: 1\n", "a: '2'\n", "b: 3\n"].into_iter().enumerate() {
        assert_eq!(requests[i]["source"], expected);
        assert_eq!(requests[i]["operation"], case.options["steps"][i]);
        assert_eq!(bound.options["_ownership_evidence"][i]["source"], expected);
    }
    assert!(case.options.get("_ownership_evidence").is_none());
}

#[test]
fn unavailable_identity_oracle_preserves_the_strict_failure() {
    let source = "disable-cooling: false # own\n";
    let protected = "routing:\n  cooldown:\n    disable-cooling: false # own\n";
    let case = Case::new("unavailable", "", "")
        .with_options(json!({"file":source,"steps":[{"op":"save","migrate":true}]}));
    let bound = ownership::bind(&case, &json!({"files":[protected]}), &mut |_| {
        Ok(GoResult::Panic("unavailable".into()))
    })
    .unwrap();
    let strict = core::comment_ownership_analysis(source, None, protected, &[]);
    let analyzed = ownership::analyze(&bound, 0, source, None, protected, &[], true);
    assert!(!analyzed.accepted());
    assert_eq!(analyzed.failure, strict.failure);
    assert!(analyzed.rules.is_empty());
}
