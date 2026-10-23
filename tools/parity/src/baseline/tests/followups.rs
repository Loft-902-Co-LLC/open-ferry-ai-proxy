use super::*;
use crate::{cases::Case, config_save::ablation, translator::Translator, upstream::GoResult};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
}

#[test]
fn pr_and_nightly_profiles_keep_all_handwritten_config_save_cases() {
    let upstream = root().join("reference/cliproxyapi");
    let expected = crate::cases::config_save::steps(&upstream);
    assert!(!expected.is_empty());
    for (seed, random) in [(1, 25), (38, 5000)] {
        let args = crate::Args::parse(
            [
                "--upstream".to_owned(),
                upstream.to_string_lossy().into_owned(),
                "--suite".into(),
                "config-save".into(),
                "--seed".into(),
                seed.to_string(),
                "--random".into(),
                random.to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        let (_, handwritten, generated) = crate::selected_suites(&args)
            .unwrap()
            .into_iter()
            .find(|(suite, _, _)| *suite == Translator::ConfigSave)
            .unwrap();
        assert_eq!(generated.len(), random);
        assert_eq!(
            handwritten
                .iter()
                .map(|c| (&c.name, &c.options))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|c| (&c.name, &c.options))
                .collect::<Vec<_>>()
        );
    }
}

fn assert_include_closure(root: &Path, paths: &[&str]) {
    let hashed: BTreeSet<_> = paths
        .iter()
        .map(|path| root.join(path).canonicalize().unwrap())
        .collect();
    for path in paths.iter().filter(|path| path.ends_with(".rs")) {
        let source = fs::read_to_string(root.join(path)).unwrap();
        super::closure::check(root, path, &source, &hashed).unwrap();
    }
}

#[test]
fn random_generator_source_closure_is_fingerprinted() {
    // These are function/type dependencies, invisible to the include scan:
    // step_cases -> Generator::new/Rng, Configs/yaml, Case::new/with_options.
    for path in [
        "tools/parity/src/generate/config_save.rs", // entry point
        "tools/parity/src/generate.rs",             // shared seed and RNG
        "tools/parity/src/generate/config_diff.rs", // configs and YAML
        "tools/parity/src/cases.rs",                // case construction
    ] {
        assert!(
            GENERATOR_PATHS.contains(&path),
            "random dependency is not fingerprinted: {path}"
        );
    }
    assert!(
        GENERATOR_PATHS
            .iter()
            .all(|path| root().join(path).is_file())
    );
    assert_include_closure(root(), GENERATOR_PATHS);
}

#[test]
fn include_closure_scan_rejects_unhashed_text_and_byte_targets() {
    let root = std::env::temp_dir().join(format!("parity-include-closure-{}", std::process::id()));
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("input.txt"), "input\n").unwrap();
    for name in ["include_str", "include_bytes"] {
        for trivia in [" ", " /* nested /* comment */ */ // comment\n"] {
            fs::write(
                root.join("nested/generator.rs"),
                format!("const INPUT: &str = {name}{trivia}!{trivia}(\n\"../input.txt\",\n);\n"),
            )
            .unwrap();
            assert!(
                std::panic::catch_unwind(|| assert_include_closure(
                    &root,
                    &["nested/generator.rs"]
                ))
                .is_err()
            );
            assert_include_closure(&root, &["nested/generator.rs", "input.txt"]);
        }
    }
    fs::remove_dir_all(root).unwrap();
}

fn check_generator_label(root: &Path, baseline_path: &Path) -> std::result::Result<(), String> {
    let baseline: Baseline = read(baseline_path).unwrap();
    let actual = fingerprint_at(root, "pin").unwrap();
    if actual["generator_sha256"] != baseline.fingerprint["generator_sha256"] {
        return Err(format!(
            "generator_sha256 differs from the committed baseline; run {RELABEL} for matching recorded random probes; changed probes or classifier versions need reclassification/full refresh: {REFRESH}"
        ));
    }
    Ok(())
}

#[test]
fn committed_generator_fingerprint_matches_real_tree() {
    check_generator_label(
        root(),
        &root().join("tools/parity/baseline/config-save.json"),
    )
    .unwrap();

    // Exercise the same comparison after a real source edit, so replacing its
    // computed hash with a constant cannot turn this into a vacuous check.
    let copy = fingerprint_fixture("real-tree");
    for path in GENERATOR_PATHS
        .iter()
        .copied()
        .chain(["tools/parity/baseline/config-save.json"])
    {
        let target = copy.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(root().join(path), target).unwrap();
    }
    check_generator_label(&copy, &copy.join("tools/parity/baseline/config-save.json")).unwrap();
    fs::write(copy.join(GENERATOR_PATHS[0]), "changed random generator\n").unwrap();
    let error = check_generator_label(&copy, &copy.join("tools/parity/baseline/config-save.json"))
        .unwrap_err();
    assert!(
        error.contains(RELABEL) && error.contains(REFRESH),
        "{error}"
    );
    // Independently change the committed-JSON side too. A constant expected
    // hash must fail even when the generator itself is unchanged.
    fs::copy(
        root().join(GENERATOR_PATHS[0]),
        copy.join(GENERATOR_PATHS[0]),
    )
    .unwrap();
    let baseline_path = copy.join("tools/parity/baseline/config-save.json");
    let mut edited: Baseline = read(&baseline_path).unwrap();
    edited
        .fingerprint
        .insert("generator_sha256".into(), "f".repeat(64));
    write(&baseline_path, &edited).unwrap();
    assert!(
        check_generator_label(&copy, &baseline_path)
            .unwrap_err()
            .contains(RELABEL)
    );
    fs::remove_dir_all(copy).unwrap();
}

fn oracle_fixture() -> Value {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/review-r13.json")).unwrap();
    rows.into_iter()
        .find(|row| row["case"] == "random-32-1072")
        .unwrap()
}

#[test]
fn comment_free_oracle_cannot_stop_before_completed_go() {
    let fixture = oracle_fixture();
    let case = Case::new("random-32-1072", "", "").with_options(fixture["options"].clone());
    let go = &fixture["go"];
    assert_eq!(successful_prefix(go), Some(3));
    assert!(go.get("error").is_none() && go.get("unloadable").is_none());
    let protected = Translator::ConfigSave.run_rust(&case).unwrap();
    let saved: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/comment-free-prefix.json")).unwrap();
    let oracle = &saved[0]["comment_free_go"];
    for oracle in [
        json!({"files":[],"error":"x"}),
        json!({"files":[go["files"][0]],"error":"x"}),
        json!({"files":go["files"],"unloadable":"x"}),
    ] {
        assert!(oracle_stops_early(go, &oracle));
        let proof = ablation::certify(&case, go, &protected, |_| {
            Ok(GoResult::Output(serde_json::to_vec(&oracle).unwrap()))
        })
        .unwrap();
        assert!(proof.deviations.is_none());
        assert_eq!(proof.evidence["original_go"], *go);
        assert_eq!(proof.evidence["comment_free_go"], oracle);
        assert_eq!(
            proof.evidence["failures"][0]["reason"],
            "comment_free_go_early"
        );
        assert!(difference_kinds(&case.options, &proof.evidence, go, Some(&protected)).is_none());
    }
    let proof = ablation::certify(&case, go, &protected, |_| {
        Ok(GoResult::Output(serde_json::to_vec(oracle).unwrap()))
    })
    .unwrap();
    assert_eq!(proof.evidence["comment_free_go"], *oracle);
    assert_eq!(
        difference_kinds(&case.options, &proof.evidence, go, Some(&protected)).unwrap(),
        BTreeSet::from(["check-3/owner_budget/save".into()])
    );
}

#[test]
fn comment_free_completion_after_original_go_failure_keeps_kinds() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/comment-free-prefix.json")).unwrap();
    for row in rows.into_iter().skip(1) {
        let case =
            Case::new(row["case"].as_str().unwrap(), "", "").with_options(row["options"].clone());
        let go = &row["original_go"];
        let protected = Translator::ConfigSave.run_rust(&case).unwrap();
        assert_eq!(protected, row["protected_rust"]);
        assert!(successful_prefix(go) < successful_prefix(&row["comment_free_go"]));
        assert_eq!(successful_prefix(go), successful_prefix(&protected));
        for early in [false, true] {
            let proof = ablation::certify(&case, go, &protected, |candidate| {
                let output = if candidate.options["_ablation_trace"] == true {
                    row["go_parse_trace"].clone()
                } else if early {
                    json!({"files":[],"error":"x"})
                } else {
                    row["comment_free_go"].clone()
                };
                Ok(GoResult::Output(serde_json::to_vec(&output).unwrap()))
            })
            .unwrap();
            assert!(proof.deviations.is_none());
            let kinds = difference_kinds(&case.options, &proof.evidence, go, Some(&protected));
            if early {
                assert_eq!(
                    proof.evidence["failures"][0]["reason"],
                    "comment_free_go_early"
                );
                assert!(kinds.is_none());
            } else {
                assert_eq!(proof.evidence["failures"], row["failures"]);
                assert_eq!(
                    kinds.unwrap(),
                    serde_json::from_value::<BTreeSet<String>>(row["kinds"].clone()).unwrap()
                );
            }
        }
    }
}

#[test]
fn import_early_oracle_is_fatal_but_missing_legacy_wrappers_keep_behavior() {
    let fixture = oracle_fixture();
    let case = Case::new("random-32-1072", "", "").with_options(fixture["options"].clone());
    let go = &fixture["go"];
    let protected = Translator::ConfigSave.run_rust(&case).unwrap();
    let saved: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/comment-free-prefix.json")).unwrap();
    let full = ablation::certify(&case, go, &protected, |_| {
        Ok(GoResult::Output(
            serde_json::to_vec(&saved[0]["comment_free_go"]).unwrap(),
        ))
    })
    .unwrap();
    let directory =
        std::env::temp_dir().join(format!("parity-import-early-{}", std::process::id()));
    let failures = directory.join("failures");
    let proofs = directory.join("proofs");
    fs::create_dir_all(&failures).unwrap();
    fs::create_dir_all(&proofs).unwrap();
    write(&failures.join("random-32-1072.json"), &json!({"case":case.name,"options":case.options,"upstream_output_raw":serde_json::to_string(go).unwrap(),"open_ferry_output":protected})).unwrap();
    for (early, missing) in [
        (false, None),
        (true, None),
        (true, Some("original_go")),
        (true, Some("comment_free_go")),
    ] {
        let mut proof = full.evidence.clone();
        if early {
            proof["comment_free_go"] = json!({"files":[],"error":"x"});
        }
        if let Some(key) = missing {
            proof.as_object_mut().unwrap().remove(key);
        }
        write(&proofs.join("random-32-1072.json"), &proof).unwrap();
        let imported = import(
            &failures,
            &proofs,
            Observation {
                fingerprint: fingerprint("pin").unwrap(),
                seed: 32,
                random: 5000,
                cases: 1,
                selected: true,
                different: 0,
                non_certification: 0,
                case_kinds: BTreeMap::new(),
            },
        )
        .unwrap();
        assert_eq!(imported.different, 1);
        if early && missing.is_none() {
            assert_eq!(imported.non_certification, 1);
            assert!(imported.case_kinds.is_empty());
        } else {
            assert_eq!(imported.non_certification, 0);
            assert_eq!(
                imported.case_kinds["random-32-1072"],
                BTreeSet::from(["check-3/owner_budget/save".into()])
            );
        }
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn early_oracle_reason_is_fatal_in_top_level_failures_and_trials() {
    let options = json!({"steps":[{"op":"save"}]});
    let go = json!({"files":["debug: true\n"]});
    let ordinary = json!({"check":3,"step":0,"reason":"owner_budget"});
    let early = json!({"check":2,"step":0,"reason":"comment_free_go_early"});
    for proof in [
        json!({"kind":"uncertified-protection-ablation","reason":"check 2: Go oracle stopped early","failures":[ordinary]}),
        json!({"kind":"uncertified-protection-ablation","failures":[early]}),
        json!({"kind":"uncertified-protection-ablation","failures":[ordinary],"trials":[{"failures":[early]}]}),
    ] {
        assert!(difference_kinds(&options, &proof, &go, Some(&go)).is_none());
    }
    let baseline: Baseline = read(&root().join("tools/parity/baseline/config-save.json")).unwrap();
    assert!(
        baseline
            .kinds
            .iter()
            .all(|kind| !kind.contains("comment_free_go_early"))
    );
}
