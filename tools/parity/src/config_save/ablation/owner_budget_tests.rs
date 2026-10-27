use super::*;

fn fixtures() -> Value {
    serde_json::from_str(include_str!("../testdata/owner-budget-histories.json")).unwrap()
}

fn case(f: &Value) -> Case {
    Case::new(f["case"].as_str().unwrap(), "", "").with_options(f["options"].clone())
}

fn oracle(f: &Value, request: &Case) -> Result<GoResult, Box<dyn Error>> {
    if request.options == f["comment_free_options"] {
        return Ok(GoResult::Output(serde_json::to_vec(&f["comment_free_go"])?));
    }
    let requested = &request.options["_ownership_request"];
    let witness = f["source_identities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| {
            w["source"] == requested["source"]
                && w["operation"] == requested["operation"]
                && w["retained_source_owner"]["protected"] == requested["protected"]
        })
        .ok_or("unrecorded owner-budget oracle request")?;
    Ok(GoResult::Output(serde_json::to_vec(witness)?))
}

fn saved_case(f: &Value) -> Case {
    let mut case = case(f);
    case.options["_ownership_evidence"] = f["source_identities"].clone();
    case
}

fn certify_saved(f: &Value, go: &Value) -> Option<Attempt> {
    // These native histories predate the current writer. Replay their saved
    // outputs and repeat receipts, while keeping the real certificate checks.
    certify_repair_with_replay(
        &saved_case(f),
        go,
        &f["protected_rust"],
        &mut |r| oracle(f, r),
        |unbound, mask| {
            assert!(unbound.options.get("_ownership_evidence").is_none());
            (mask == 0).then(|| Replay {
                output: f["protected_rust"].clone(),
                traces: vec![],
            })
        },
        |unbound, protected, _| {
            assert!(unbound.options.get("_ownership_evidence").is_none());
            assert_eq!(*protected, f["protected_rust"]);
            let mut bound = unbound.clone();
            bound.options["_ownership_evidence"] = f["source_identities"].clone();
            Ok(bound)
        },
    )
    .unwrap()
}

#[test]
fn exhaustive_search_also_refuses_the_previously_inherited_3152_history() {
    let all = fixtures();
    let f = all
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["case"] == "random-30-3152")
        .unwrap();
    let case = case(f);
    let reproduces_saved_writer = replay(&case, 0).is_some_and(|r| r.output == f["protected_rust"]);
    let result = super::super::certify_full(
        &case,
        &f["original_go"],
        &f["protected_rust"],
        &mut |r: &Case| oracle(f, r),
        false,
    )
    .unwrap();
    assert!(result.deviations.is_none());
    if reproduces_saved_writer {
        assert!(
            result.evidence["failures"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["check"] == 3 && f["reason"] == "go_budget")
        );
    } else {
        // A later writer fix need not reproduce the saved lossy history. The
        // direct inherited-path tests below still enforce its budget refusal.
        assert_eq!(result.evidence["reason"], "protected replay mismatch");
    }
}

#[test]
fn original_go_budget_refuses_saved_histories_in_both_acceptance_paths() {
    for f in fixtures().as_array().unwrap() {
        let case = case(f);
        let go = &f["original_go"];
        let protected = &f["protected_rust"];
        assert!(
            certify_saved(f, go).is_none(),
            "{}: complete history was certified",
            case.name
        );
        let bound = saved_case(f);
        let index = f["loss_step"].as_u64().unwrap() as usize;
        let source = bound.options["_ownership_evidence"][index]["source"]
            .as_str()
            .unwrap();
        let g = go["files"][index].as_str().unwrap();
        let r = protected["files"][index].as_str().unwrap();
        assert!(!inherited_repair(&bound, index, source, Some(g), r));
        let analysis = analyze(&bound, index, source, Some(g), r, &[], true);
        assert_eq!(
            analysis.failure.unwrap()["reason"],
            "go_budget",
            "{}",
            case.name
        );
        let ablated = Replay {
            output: go.clone(),
            traces: vec![],
        };
        let failure = super::super::diagnostics::comments(&bound, protected, &ablated).unwrap_err();
        assert_eq!(failure["check"], 3);
        assert_eq!(failure["step"], index);
        assert_eq!(failure["reason"], "go_budget", "{}", case.name);
    }
}

#[test]
fn current_writer_still_has_a_complete_owner_certificate() {
    let all: Value = serde_json::from_str(include_str!("../testdata/owners-repairs.json")).unwrap();
    let f = &all[0];
    let case = Case::new("current-owner-certificate", "", "")
        .with_options(json!({"file":f["source"],"steps":[f["operation"]]}));
    let go = &f["evidence"]["retained_source_owner"]["exact_go"];
    let protected = json!({"files":[f["protected"]]});
    let proof = certify_repair(&case, go, &protected, &mut |_| {
        Ok(GoResult::Output(serde_json::to_vec(&f["evidence"])?))
    })
    .unwrap()
    .unwrap();
    assert!(
        proof
            .deviations
            .unwrap()
            .contains(&Deviation::RetainedDeferredCommentOwners)
    );
}

#[test]
fn complete_history_go_budget_is_required_after_a_valid_owner_prefix() {
    let all = fixtures();
    let f = all
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["case"] == "random-30-3152")
        .unwrap();
    let mut within_budget = f["original_go"].clone();
    within_budget["files"][1] = f["protected_rust"]["files"][1].clone();
    assert!(
        certify_saved(f, &within_budget).is_some(),
        "budget-safe control must certify"
    );
    assert!(
        certify_saved(f, &f["original_go"]).is_none(),
        "original Go comment budget must refuse the history"
    );
}

#[test]
fn inherited_go_budget_is_required_despite_exact_go_and_stable_repeats() {
    for f in fixtures().as_array().unwrap().iter().filter(|f| {
        matches!(
            f["case"].as_str(),
            Some("random-30-3152" | "random-38-1428")
        )
    }) {
        let bound = saved_case(f);
        let index = f["loss_step"].as_u64().unwrap() as usize;
        let source = bound.options["_ownership_evidence"][index]["source"]
            .as_str()
            .unwrap();
        let protected = f["protected_rust"]["files"][index].as_str().unwrap();
        let go = f["original_go"]["files"][index].as_str().unwrap();
        assert!(
            inherited_repair(&bound, index, source, Some(protected), protected),
            "{}: budget-safe control must inherit",
            bound.name
        );
        assert!(
            !inherited_repair(&bound, index, source, Some(go), protected),
            "{}: original Go comment budget must refuse inheritance",
            bound.name
        );
    }
}

fn inherited_case() -> (Case, String) {
    let f: Value = serde_json::from_str(include_str!("../testdata/owners-repairs.json")).unwrap();
    let f = &f[0];
    let protected = f["protected"].as_str().unwrap().to_owned();
    let step = json!({"op":"save","migrate":false});
    let frame = json!({"source":protected,"retained_source_owner":{
        "protected":protected,"exact_go":{"files":[protected]},"repeat_input":protected,
        "repeat_operation":step,"repeat_ok":true,"repeated":protected
    }});
    let case = Case::new("inherited-independent-conditions", "", "").with_options(json!({
        "file":f["source"],"steps":[f["operation"],step,step],
        "_ownership_evidence":[f["evidence"],frame,frame]
    }));
    (case, protected)
}

#[test]
fn inherited_current_step_requires_its_exact_input_go_bytes() {
    let (mut case, protected) = inherited_case();
    assert!(inherited_repair(
        &case,
        1,
        &protected,
        Some(&protected),
        &protected
    ));
    case.options["_ownership_evidence"][1]["retained_source_owner"]["exact_go"]["files"][0] =
        "port: 9999\n".into();
    assert!(!inherited_repair(
        &case,
        1,
        &protected,
        Some(&protected),
        &protected
    ));
}

#[test]
fn inherited_prefix_requires_each_intermediate_exact_go_and_repeat() {
    let (case, protected) = inherited_case();
    assert!(inherited_repair(
        &case,
        2,
        &protected,
        Some(&protected),
        &protected
    ));
    for field in ["exact_go", "repeated"] {
        let mut changed = case.clone();
        changed.options["_ownership_evidence"][1]["retained_source_owner"][field] =
            "invalid intermediate".into();
        assert!(
            !inherited_repair(&changed, 2, &protected, Some(&protected), &protected),
            "{field}"
        );
    }
}

#[test]
fn complete_certificate_requires_data_agreement_with_original_go_history() {
    let f: Value = serde_json::from_str(include_str!("../testdata/owners-repairs.json")).unwrap();
    let f = &f[0];
    let case = Case::new("original-history-data", "", "")
        .with_options(json!({"file":f["source"],"steps":[f["operation"]]}));
    let protected = json!({"files":[f["protected"]]});
    let mut go = f["evidence"]["retained_source_owner"]["exact_go"].clone();
    let mut oracle = |_: &Case| {
        Ok(GoResult::Output(
            serde_json::to_vec(&f["evidence"]).unwrap(),
        ))
    };
    assert!(
        certify_repair(&case, &go, &protected, &mut oracle)
            .unwrap()
            .is_some()
    );
    go["files"][0] = go["files"][0]
        .as_str()
        .unwrap()
        .replace("port: 8317", "port: 9999")
        .into();
    assert!(
        certify_repair(&case, &go, &protected, &mut oracle)
            .unwrap()
            .is_none()
    );
}

#[test]
fn existing_step_comparisons_cannot_replace_a_whole_history_certificate() {
    let case = Case::new("canonical-only-label", "", "").with_options(json!({
        "file":"config-version: 8 # version\nport: 8317\n",
        "steps":[{"op":"save","migrate":true}]
    }));
    let protected = replay(&case, 0).unwrap().output;
    let go = replay(&case, 1 << 1).unwrap().output;
    assert_ne!(go, protected);
    let witness = json!({"retained_source_owner":{"exact_go":go}});
    let proof = certify_repair(&case, &go, &protected, &mut |_| {
        Ok(GoResult::Output(serde_json::to_vec(&witness).unwrap()))
    })
    .unwrap();
    assert!(
        proof.is_none(),
        "per-step comparisons supplied no owner certificate"
    );
}

#[test]
fn saved_existing_protection_certificates_keep_their_minimal_subset_labels() {
    let all: Value =
        serde_json::from_str(include_str!("../testdata/owners-existing-labels.json")).unwrap();
    for (f, expected) in all.as_array().unwrap().iter().zip([
        Deviation::RetainedCanonicalVersionComment,
        Deviation::RetainedDroppedKeyComment,
    ]) {
        let proof = certify_repair(
            &case(f),
            &f["original_go"],
            &f["protected_rust"],
            &mut |r| oracle(f, r),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            proof.deviations.unwrap(),
            BTreeSet::from([expected]),
            "{}",
            f["case"]
        );
        assert_ne!(proof.evidence["method"], "source-owner-certificate");
    }
}
