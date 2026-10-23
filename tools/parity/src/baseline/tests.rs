use super::*;
use serde_json::json;

mod closure;
mod followups;
mod relabel;
mod review_r13;

fn failed_go() -> Value {
    json!({"files":[],"error":"upstream refused"})
}

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
        random_case_probes: None,
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
fn lowering_compares_the_recorded_seed_before_the_profile_minimum() {
    let mut b = baseline();
    b.calibration.push(Calibration {
        seed: 31,
        different: 2,
        ..b.calibration[0].clone()
    });
    let mut o = observation();
    o.seed = 31;
    let result = b.assess(&o, Path::new("b.json")).unwrap();
    assert!(result.passed);
    assert!(result.markdown.contains("Baseline can be lowered"));
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
    assert!(config_output(&json!({"files":[],"error":"write refused"})));
    for value in [
        json!({"panic":"failed"}),
        json!({"files":[],"panic":"failed"}),
        json!({"files":[null]}),
        json!({"files":[],"error":"a","unloadable":"b"}),
    ] {
        assert!(!config_output(&value));
    }
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
        assert!(difference_kinds(&options, &proof, &failed_go(), None).is_none());
    }
}
#[test]
fn kinds_ignore_content_paths_and_offsets_but_keep_check_reason_and_operation() {
    let options = json!({"steps":[{"op":"nested"},{"op":"save"}]});
    let mut p = json!({"kind":"uncertified-protection-ablation", "failures":[{"check":3,"step":0,"reason":"owner_budget","text":"# private","owner":{"path":"secret"}}]});
    let before = difference_kinds(&options, &p, &failed_go(), None).unwrap();
    assert_eq!(
        before,
        BTreeSet::from(["check-3/owner_budget/nested".into()])
    );
    p["failures"][0]["text"] = "other".into();
    p["failures"][0]["owner"] = Value::Null;
    assert_eq!(
        difference_kinds(&options, &p, &failed_go(), None).unwrap(),
        before
    );
    p["failures"][0]["step"] = 1.into();
    assert_ne!(
        difference_kinds(&options, &p, &failed_go(), None).unwrap(),
        before
    );
    p["failures"][0]["reason"] = "owner_order".into();
    assert_ne!(
        difference_kinds(&options, &p, &failed_go(), None).unwrap(),
        before
    );
    p["failures"][0]["check"] = 4.into();
    assert_ne!(
        difference_kinds(&options, &p, &failed_go(), None).unwrap(),
        before
    );
}
#[test]
fn no_subset_uses_all_failed_trials_not_the_first_only() {
    let p = json!({"kind":"uncertified-protection-ablation", "reason":"check 1: no reproducing subset", "trials":[{"failures":[{"check":1,"step":0,"reason":"snapshot_bytes"}]},{"failures":[{"check":1,"step":1,"reason":"missing_snapshot"}]}]});
    assert_eq!(
        difference_kinds(
            &json!({"steps":[{"op":"write"},{"op":"save"}]}),
            &p,
            &failed_go(),
            None
        )
        .unwrap(),
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

#[test]
fn protected_replay_mismatch_is_fatal_except_the_recorded_yaml_refusal_boundary() {
    let options = json!({"steps":[{"op":"save"}]});
    let mut proof = json!({"kind":"uncertified-protection-ablation","reason":"protected replay mismatch","failures":[{"stage":"protected_replay","reason":"missing_snapshot","step":0}]});
    assert!(difference_kinds(&options, &proof, &failed_go(), None).is_none());
    assert!(
        difference_kinds(
            &options,
            &proof,
            &failed_go(),
            Some(&json!({"files":[],"error":"harness failure"}))
        )
        .is_none()
    );
    let refused =
        json!({"files":[],"error":"failed to parse config file: yaml: invalid attempted file"});
    assert!(difference_kinds(&options, &proof, &failed_go(), Some(&refused)).is_some());
    proof["failures"][0]["reason"] = "snapshot_bytes".into();
    assert!(difference_kinds(&options, &proof, &failed_go(), Some(&refused)).is_none());
}

fn fingerprint_fixture(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("parity-fingerprint-{name}-{}", std::process::id()));
    for path in GENERATOR_PATHS {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "generator fixture\n").unwrap();
    }
    root
}

#[test]
fn fingerprint_paths_exclude_manifests_product_harness_and_certifier() {
    let root = fingerprint_fixture("paths");
    let paths = fingerprint_paths(&root).unwrap();
    assert_eq!(paths.len(), GENERATOR_PATHS.len());
    for path in paths {
        let path = path.to_string_lossy().replace('\\', "/");
        assert!(!path.ends_with("Cargo.lock"));
        assert!(!path.ends_with("Cargo.toml"));
        assert!(!path.ends_with("rust-toolchain.toml"));
        assert!(!path.starts_with("crates/"));
        assert!(!path.starts_with("tools/parity/go"));
        assert!(
            path.starts_with("tools/parity/src/generate") || path == "tools/parity/src/cases.rs"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn certifier_and_product_edits_do_not_change_the_fingerprint() {
    let root = fingerprint_fixture("changes");
    let before = fingerprint_at(&root, "pin").unwrap();
    assert_eq!(
        before.keys().map(String::as_str).collect::<Vec<_>>(),
        ["classifier_version", "generator_sha256", "upstream_pin"]
    );
    for path in [
        "Cargo.lock",
        "Cargo.toml",
        "rust-toolchain.toml",
        "tools/parity/Cargo.toml",
        "crates/open-ferry-core/src/config/save/mod.rs",
        "tools/parity/go/parity_config_save.go",
        "tools/parity/src/config_save.rs",
        "tools/parity/src/config_save/counterfactual.rs",
        "tools/parity/src/main.rs",
        "tools/parity/src/compare.rs",
        "tools/parity/src/baseline.rs",
        "tools/parity/src/cases/config_save.rs",
        "tools/parity/src/cases/config_diff.rs",
        "tools/parity/src/cases/testdata/config-save-ablation-replays.json",
        "config.example.yaml",
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "changed certifier or dependency\n").unwrap();
        assert_eq!(before, fingerprint_at(&root, "pin").unwrap());
    }
    fs::write(root.join(GENERATOR_PATHS[0]), "changed generator\n").unwrap();
    assert_ne!(before, fingerprint_at(&root, "pin").unwrap());
    assert_ne!(before, fingerprint_at(&root, "other pin").unwrap());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn incremental_unions_kinds_and_calibration_without_ten_new_seeds() {
    let b = baseline();
    let mut o = observation();
    o.seed = 40;
    o.case_kinds = BTreeMap::from([(
        "new-case".into(),
        BTreeSet::from(["check-3/owner_order/save".into()]),
    )]);
    let result = refresh_incremental(&b, &[o], &b.fingerprint).unwrap();
    assert_eq!(
        result.kinds,
        BTreeSet::from([
            "check-3/owner_budget/save".into(),
            "check-3/owner_order/save".into()
        ])
    );
    assert_eq!(result.calibration.len(), 2);
    assert_eq!(result.pr_different_cases, b.pr_different_cases);
}

#[test]
fn incremental_bound_never_decreases_and_can_increase_with_the_same_margin() {
    let mut b = baseline();
    b.max_different_per_1000 = 75;
    let low = observation();
    let result = refresh_incremental(&b, &[low], &b.fingerprint).unwrap();
    assert_eq!(result.max_different_per_1000, 75);
    let high = Observation {
        cases: 4,
        ..observation()
    };
    let result = refresh_incremental(&b, &[high], &b.fingerprint).unwrap();
    assert_eq!(result.max_different_per_1000, 300); // 1/4 * 1000 * 1.2
}

#[test]
fn incremental_selected_observations_add_kinds_but_not_rate_or_pr_ids() {
    let b = baseline();
    let mut o = Observation {
        selected: true,
        cases: 1,
        seed: 1,
        random: 25,
        ..observation()
    };
    o.case_kinds = BTreeMap::from([(
        "new-selected".into(),
        BTreeSet::from(["check-3/owner_order/save".into()]),
    )]);
    let result = refresh_incremental(&b, &[o], &b.fingerprint).unwrap();
    assert!(result.kinds.contains("check-3/owner_order/save"));
    assert_eq!(result.max_different_per_1000, b.max_different_per_1000);
    assert_eq!(result.pr_different_cases, b.pr_different_cases);
}

#[test]
fn incremental_rejects_observation_or_existing_fingerprint_mismatch() {
    let b = baseline();
    let mut o = observation();
    o.fingerprint.insert("test".into(), "changed".into());
    for (observations, fingerprint) in [
        (vec![o.clone()], b.fingerprint.clone()),
        (vec![o.clone()], o.fingerprint.clone()),
    ] {
        let error = refresh_incremental(&b, &observations, &fingerprint)
            .unwrap_err()
            .to_string();
        assert!(error.contains("fingerprint mismatch"));
    }
}

#[test]
fn incremental_rejects_non_certification() {
    let b = baseline();
    let o = Observation {
        non_certification: 1,
        case_kinds: BTreeMap::new(),
        ..observation()
    };
    let error = refresh_incremental(&b, &[o], &b.fingerprint)
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot baseline non-certification failures"));
}

#[test]
fn incremental_preserves_pr_set_and_refuses_even_a_lower_pr_set() {
    let b = baseline();
    let mut o = Observation {
        seed: 1,
        random: 25,
        ..observation()
    };
    let result = refresh_incremental(&b, &[o.clone()], &b.fingerprint).unwrap();
    assert_eq!(result.pr_different_cases, b.pr_different_cases);
    o.case_kinds = BTreeMap::from([(
        "new-pr-case".into(),
        BTreeSet::from(["check-3/owner_budget/save".into()]),
    )]);
    for conflict in [
        o.clone(),
        Observation {
            different: 0,
            case_kinds: BTreeMap::new(),
            ..o
        },
    ] {
        let error = refresh_incremental(&b, &[conflict], &b.fingerprint)
            .unwrap_err()
            .to_string();
        assert!(error.contains("PR observation conflicts with existing baseline"));
    }
}

#[test]
fn incremental_is_order_independent_and_deduplicates_calibration_rows() {
    let b = baseline();
    let mut observations = vec![
        observation(), // already in the existing calibration
        Observation {
            seed: 41,
            ..observation()
        },
        Observation {
            seed: 40,
            ..observation()
        },
        observation(),
    ];
    let forward = refresh_incremental(&b, &observations, &b.fingerprint).unwrap();
    observations.reverse();
    let backward = refresh_incremental(&b, &observations, &b.fingerprint).unwrap();
    assert_eq!(forward.calibration.len(), 3);
    assert_eq!(
        serde_json::to_vec_pretty(&forward).unwrap(),
        serde_json::to_vec_pretty(&backward).unwrap()
    );
}

#[test]
fn full_refresh_still_requires_ten_seeds_and_the_pr_profile() {
    let o = observation();
    let mut observations: Vec<_> = (30..39)
        .map(|seed| Observation { seed, ..o.clone() })
        .collect();
    observations.push(Observation {
        seed: 1,
        random: 25,
        ..o.clone()
    });
    assert!(
        refresh(&observations, &o.fingerprint)
            .unwrap_err()
            .to_string()
            .contains("at least ten distinct nightly-sized config seeds")
    );
    observations.pop();
    observations.push(Observation { seed: 39, ..o });
    assert!(
        refresh(&observations, &observations[0].fingerprint)
            .unwrap_err()
            .to_string()
            .contains("needs seed 1 / random 25")
    );
}
