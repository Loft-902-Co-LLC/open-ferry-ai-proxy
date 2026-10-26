use super::*;

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
