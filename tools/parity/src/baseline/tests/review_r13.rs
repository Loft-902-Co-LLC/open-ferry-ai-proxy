use super::*;
use crate::{cases::Case, config_save::ablation, translator::Translator, upstream::GoResult};

fn fixtures() -> Vec<Value> {
    serde_json::from_str(include_str!("../testdata/review-r13.json")).unwrap()
}

fn classify(case: &Case, go: &Value, oracle: Option<&[u8]>) -> (Value, Option<BTreeSet<String>>) {
    let protected = Translator::ConfigSave.run_rust(case).unwrap();
    let proof = ablation::certify(case, go, &protected, |_| {
        Ok(GoResult::Output(
            oracle.expect("unexpected Go oracle call").to_vec(),
        ))
    })
    .unwrap();
    assert!(proof.deviations.is_none());
    assert!(config_output(go) && config_output(&protected));
    let kinds = difference_kinds(&case.options, &proof.evidence, go, Some(&protected));
    (proof.evidence, kinds)
}

#[test]
fn review_r13_rust_only_parse_failure_is_fatal() {
    let case = Case::new("flow-ls", "", "")
        .with_options(json!({"file":"a: [1, # c\u{2028} 2]\n","steps":[{"op":"save"}]}));
    let (proof, kinds) = classify(&case, &json!({"files":["a:\n  - 1 # c\n  - 2\n"]}), None);
    assert_eq!(proof["reason"], "check 1: no reproducing subset");
    assert!(kinds.is_none(), "{kinds:?}");
}

#[test]
fn review_r13_unicode_guard_refusal_has_a_distinct_unbaselined_kind() {
    let case = Case::new("guard-ls", "", "")
        .with_options(json!({"file":"# c\u{2028}debug: true\n","steps":[{"op":"save"}]}));
    let (proof, kinds) = classify(&case, &json!({"files":["# c\ndebug: true\n"]}), None);
    assert_eq!(proof["reason"], "check 1: no reproducing subset");
    assert_eq!(
        kinds.unwrap(),
        BTreeSet::from(["rust-refusal/unicode-guard/save".into()])
    );
    let baseline: Baseline =
        serde_json::from_str(include_str!("../../../baseline/config-save.json")).unwrap();
    assert!(!baseline.kinds.contains("rust-refusal/unicode-guard/save"));
}

#[test]
fn review_r13_protected_replay_rust_only_failure_is_fatal_but_go_first_keeps_kind() {
    let f = fixtures().remove(0);
    let case = Case::new("replay-2687", "", "").with_options(f["options"].clone());
    let (proof, kinds) = classify(&case, &f["go"], None);
    assert_eq!(proof["reason"], "protected replay mismatch");
    assert_eq!(
        kinds.unwrap(),
        BTreeSet::from(["protected-replay/missing_snapshot/save".into()])
    );
    // The saved comparison wrapper has already dropped Go's failed-load
    // snapshot. The real original wrapper also keeps this control in-kind.
    assert_eq!(f["raw_go"]["files"].as_array().unwrap().len(), 2);
    assert!(f["raw_go"]["unloadable"].is_string());
    let (_, kinds) = classify(&case, &f["raw_go"], None);
    assert_eq!(
        kinds.unwrap(),
        BTreeSet::from(["protected-replay/missing_snapshot/save".into()])
    );
    let mut files = f["go"]["files"].as_array().unwrap().clone();
    files.push("disable-cooling: false\n".into()); // exact Opus A1 counterexample
    for _ in 0..2 {
        let (proof, kinds) = classify(&case, &json!({"files":files}), None);
        assert_eq!(proof["reason"], "protected replay mismatch");
        assert!(kinds.is_none(), "{kinds:?}");
        files.push("disable-cooling: false\n".into()); // complete all three steps too
    }
}

#[test]
fn review_r13_original_go_failed_snapshot_uses_the_successful_prefix() {
    let f = fixtures().remove(2);
    let case = Case::new("replay-4480", "", "").with_options(f["options"].clone());
    assert_eq!(f["raw_go"]["files"].as_array().unwrap().len(), 2);
    assert!(f["raw_go"]["unloadable"].is_string());
    assert_eq!(f["go"]["files"].as_array().unwrap().len(), 1);
    assert!(f["go"]["error"].is_string());
    let (proof, kinds) = classify(&case, &f["raw_go"], None);
    assert_eq!(proof["reason"], "protected replay mismatch");
    assert_eq!(
        kinds.unwrap(),
        BTreeSet::from(["protected-replay/missing_snapshot/save".into()])
    );
    let (_, normalized_kinds) = classify(&case, &f["go"], None);
    assert_eq!(
        normalized_kinds.unwrap(),
        BTreeSet::from(["protected-replay/missing_snapshot/save".into()])
    );
    // Go failed after two successful steps, later than Rust's one-step prefix.
    let mut later = f["raw_go"].clone();
    later["files"]
        .as_array_mut()
        .unwrap()
        .insert(1, f["raw_go"]["files"][0].clone());
    assert_eq!(successful_prefix(&later), Some(2));
    let (_, kinds) = classify(&case, &later, None);
    assert!(kinds.is_none());
}

#[test]
fn review_r13b_successful_prefix_is_symmetric_for_error_and_unloadable() {
    let options = json!({"steps":[{"op":"save"},{"op":"save"},{"op":"save"}]});
    let proof = json!({"kind":"uncertified-protection-ablation","reason":"check 1: no reproducing subset","trials":[{"failures":[{"check":1,"reason":"missing_snapshot","step":1}]}]});
    for protected in [
        json!({"files":["x: 1\n"],"error":"refused"}),
        json!({"files":["x: 1\n","bad"],"unloadable":"refused"}),
    ] {
        assert_eq!(successful_prefix(&protected), Some(1));
        for go in [
            json!({"files":["x: 1\n"],"error":"refused"}),
            json!({"files":["x: 1\n","bad"],"unloadable":"refused"}),
        ] {
            assert_eq!(successful_prefix(&go), Some(1));
            assert!(difference_kinds(&options, &proof, &go, Some(&protected)).is_some());
        }
        for go in [
            json!({"files":["x: 1\n","x: 2\n"],"error":"refused"}),
            json!({"files":["x: 1\n","x: 2\n","bad"],"unloadable":"refused"}),
        ] {
            assert_eq!(successful_prefix(&go), Some(2));
            assert!(difference_kinds(&options, &proof, &go, Some(&protected)).is_none());
        }
        let invalid = json!({"files":[],"unloadable":"refused"});
        assert!(difference_kinds(&options, &proof, &invalid, Some(&protected)).is_none());
        assert!(difference_kinds(&options, &proof, &protected, Some(&invalid)).is_none());
    }
}

#[test]
fn review_r13_malformed_and_truncated_comment_free_oracles_are_fatal() {
    let f = fixtures().remove(1);
    let case = Case::new("oracle-1072", "", "").with_options(f["options"].clone());
    for oracle in [
        br#"{"files":[],"unexpected":true}"#.as_slice(),
        br#"{"files":[]}"#,
    ] {
        let (proof, kinds) = classify(&case, &f["go"], Some(oracle));
        assert_eq!(
            proof["failures"][0]["reason"], "comment_free_go_malformed",
            "{}",
            proof["reason"]
        );
        assert!(kinds.is_none(), "{kinds:?}");
    }
}

#[test]
fn review_r13_rust_refusal_requires_go_to_stop_no_later() {
    let options = json!({"steps":[{"op":"save"},{"op":"write"}]});
    let proof = json!({"kind":"uncertified-protection-ablation","reason":"check 1: no reproducing subset","trials":[{"failures":[{"check":1,"reason":"missing_snapshot","step":1}]}]});
    for protected in [
        json!({"files":[],"error":"refused"}),
        json!({"files":["x: 1\n"],"unloadable":"refused"}),
    ] {
        let early = json!({"files":[],"error":"refused"});
        assert!(difference_kinds(&options, &proof, &early, Some(&protected)).is_some());
        let later = json!({"files":["x: 1\n","x: 2\n"],"unloadable":"refused"});
        assert!(difference_kinds(&options, &proof, &later, Some(&protected)).is_none());
        let completed = json!({"files":["x: 1\n","x: 2\n"]});
        assert!(difference_kinds(&options, &proof, &completed, Some(&protected)).is_none());
    }
}

#[test]
fn review_r13_guard_recognition_requires_the_complete_message() {
    let exact = "config value at line 1, path $.debug, is read differently by the saphyr loader and writer in input containing U+2028 (LINE SEPARATOR); write refused, file unchanged";
    assert!(unicode_guard_refusal(exact));
    assert!(unicode_guard_refusal(&exact.replace("value", "scalar")));
    for changed in [
        format!("prefix {exact}"),
        format!("{exact} suffix"),
        exact.replace("line 1", "line 0"),
        exact.replace("U+2028 (LINE SEPARATOR)", "unknown"),
        exact.replace("; write refused, file unchanged", ""),
    ] {
        assert!(!unicode_guard_refusal(&changed), "{changed}");
    }
}
