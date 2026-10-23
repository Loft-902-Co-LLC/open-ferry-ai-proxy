use super::*;
use serde_json::json;

fn observation() -> Observation {
    Observation {
        fingerprint: BTreeMap::from([("test".into(), "1".into())]),
        seed: 30,
        random: 5000,
        cases: 1000,
        selected: false,
        different: 1,
        non_certification: 0,
        case_kinds: BTreeMap::from([(
            "old".into(),
            BTreeSet::from(["check-3/owner_budget/save".into()]),
        )]),
    }
}
fn baseline() -> Baseline {
    let o = observation();
    Baseline {
        fingerprint: o.fingerprint,
        kinds: o.case_kinds["old"].clone(),
        max_different_per_1000: 2,
        pr_different_cases: BTreeSet::from(["old".into()]),
        calibration: vec![Calibration {
            selected: false,
            seed: 30,
            random: 5000,
            cases: 1000,
            different: 1,
        }],
    }
}
fn assess(o: &Observation) -> Assessment {
    baseline().assess(o, Path::new("baseline.json")).unwrap()
}

#[test]
fn new_kind_fails_even_below_bound() {
    let mut o = observation();
    o.case_kinds
        .get_mut("old")
        .unwrap()
        .insert("check-4/faithful_bytes/save".into());
    let result = assess(&o);
    assert!(!result.passed);
    assert!(result.markdown.contains("new difference kind"));
}
#[test]
fn rate_above_bound_fails_including_fractional_boundary() {
    let mut o = observation();
    o.cases = 499;
    assert!(!assess(&o).passed); // 1/499 > 2/1000; never rounded down.
    o.cases = 500;
    assert!(assess(&o).passed);
}
#[test]
fn new_pr_case_fails_even_with_old_kind_and_low_rate() {
    let mut o = observation();
    o.seed = 1;
    o.random = 25;
    let kinds = o.case_kinds.remove("old").unwrap();
    o.case_kinds.insert("new".into(), kinds);
    let result = assess(&o);
    assert!(!result.passed);
    assert!(result.markdown.contains("new PR-run Different case: new"));
}
#[test]
fn fewer_pass_with_lowering_message_and_refresh_command() {
    let mut o = observation();
    o.different = 0;
    o.case_kinds.clear();
    let result = assess(&o);
    assert!(result.passed);
    assert!(result.markdown.contains("Baseline can be lowered"));
    assert!(result.markdown.contains(REFRESH));
    o.seed = 1;
    o.random = 25;
    assert!(assess(&o).markdown.contains("Baseline can be lowered"));
}
#[test]
fn fingerprint_mismatch_fails_with_refresh_command() {
    let mut o = observation();
    o.fingerprint.insert("test".into(), "changed".into());
    let error = baseline()
        .assess(&o, Path::new("baseline.json"))
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("fingerprint mismatch"));
    assert!(error.contains(REFRESH));
}
#[test]
fn non_certification_failures_never_absorbed_or_refreshed() {
    let mut o = observation();
    o.case_kinds.clear();
    o.non_certification = 1;
    assert!(!assess(&o).passed);
    assert!(assess(&o).markdown.contains("always fatal"));
    assert!(refresh(std::slice::from_ref(&o), &o.fingerprint).is_err());
    let options = json!({"steps":[{"op":"save"}]});
    for proof in [
        json!({}),
        json!({"kind":"uncertified-protection-ablation", "reason":"Go trace mismatch"}),
        json!({"kind":"uncertified-protection-ablation", "reason":"check 2: Go oracle panicked"}),
        json!({"kind":"uncertified-protection-ablation", "reason":"check 1: no reproducing subset", "trials":[{"failures":[{"check":1,"step":0,"reason":"replay_unavailable"}]}]}),
    ] {
        assert!(difference_kinds(&options, &proof).is_none());
    }
}
#[test]
fn kinds_ignore_content_paths_and_offsets_but_keep_check_reason_and_operation() {
    let options = json!({"steps":[{"op":"nested"},{"op":"save"}]});
    let mut p = json!({"kind":"uncertified-protection-ablation", "failures":[{"check":3,"step":0,"reason":"owner_budget","text":"# private","owner":{"path":"secret"}}]});
    let before = difference_kinds(&options, &p).unwrap();
    assert_eq!(
        before,
        BTreeSet::from(["check-3/owner_budget/nested".into()])
    );
    p["failures"][0]["text"] = "other".into();
    p["failures"][0]["owner"] = Value::Null;
    assert_eq!(difference_kinds(&options, &p).unwrap(), before);
    p["failures"][0]["step"] = 1.into();
    assert_ne!(difference_kinds(&options, &p).unwrap(), before);
    p["failures"][0]["reason"] = "owner_order".into();
    assert_ne!(difference_kinds(&options, &p).unwrap(), before);
    p["failures"][0]["check"] = 4.into();
    assert_ne!(difference_kinds(&options, &p).unwrap(), before);
}
#[test]
fn no_subset_uses_all_failed_trials_not_the_first_only() {
    let p = json!({"kind":"uncertified-protection-ablation", "reason":"check 1: no reproducing subset", "trials":[{"failures":[{"check":1,"step":0,"reason":"snapshot_bytes"}]},{"failures":[{"check":1,"step":1,"reason":"missing_snapshot"}]}]});
    assert_eq!(
        difference_kinds(&json!({"steps":[{"op":"write"},{"op":"save"}]}), &p).unwrap(),
        BTreeSet::from([
            "check-1-no-subset/snapshot_bytes/write".into(),
            "check-1-no-subset/missing_snapshot/save".into()
        ])
    );
}
#[test]
fn refresh_is_sorted_deterministic_and_selected_failures_cannot_inflate_rate() {
    let o = observation();
    let mut observations: Vec<_> = (30..40)
        .map(|seed| Observation { seed, ..o.clone() })
        .collect();
    observations.push(Observation {
        seed: 1,
        random: 25,
        ..o.clone()
    });
    observations.push(Observation {
        selected: true,
        cases: 1,
        ..o.clone()
    });
    let b = refresh(&observations, &o.fingerprint).unwrap();
    assert_eq!(b.max_different_per_1000, 5);
    assert!(
        b.assess(observations.last().unwrap(), Path::new("b.json"))
            .unwrap()
            .passed
    );
    let a = serde_json::to_string(&b).unwrap();
    observations.reverse();
    assert_eq!(
        a,
        serde_json::to_string(&refresh(&observations, &o.fingerprint).unwrap()).unwrap()
    );
    let dir = std::env::temp_dir().join(format!("parity-ratchet-test-{}", std::process::id()));
    let path = dir.join("baseline.json");
    write(&path, &b).unwrap();
    let first = fs::read(&path).unwrap();
    write(&path, &b).unwrap();
    assert_eq!(first, fs::read(&path).unwrap());
    let text = String::from_utf8(first).unwrap();
    assert!(text.find("\"calibration\"").unwrap() < text.find("\"fingerprint\"").unwrap());
    fs::remove_file(path).unwrap();
    fs::remove_dir(dir).unwrap();
}
