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

fn skip_include_trivia(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix("//") {
            text = rest.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(mut rest) = text.strip_prefix("/*") {
            let mut depth = 1;
            while depth > 0 {
                if let Some(tail) = rest.strip_prefix("/*") {
                    depth += 1;
                    rest = tail;
                } else if let Some(tail) = rest.strip_prefix("*/") {
                    depth -= 1;
                    rest = tail;
                } else {
                    rest = &rest[rest
                        .chars()
                        .next()
                        .expect("unclosed include comment")
                        .len_utf8()..];
                }
            }
            text = rest;
        } else {
            return text;
        }
    }
}

fn assert_include_closure(root: &Path, paths: &[&str]) {
    let hashed: BTreeSet<_> = paths
        .iter()
        .map(|path| root.join(path).canonicalize().unwrap())
        .collect();
    for path in paths.iter().filter(|path| path.ends_with(".rs")) {
        let source = fs::read_to_string(root.join(path)).unwrap();
        for name in ["include_str", "include_bytes"] {
            for tail in source.split(name).skip(1) {
                let Some(tail) = skip_include_trivia(tail).strip_prefix('!') else {
                    continue;
                };
                let tail = skip_include_trivia(tail).strip_prefix('(').unwrap();
                let (literal, _) = tail.split_once(')').unwrap();
                // Reject expressions or unsupported literal spellings rather
                // than silently overlooking an include that needs review.
                let target: String =
                    serde_json::from_str(literal.trim().trim_end_matches(',').trim())
                        .expect("generator includes must use a relative string literal");
                let target = root
                    .join(path)
                    .parent()
                    .unwrap()
                    .join(target)
                    .canonicalize()
                    .unwrap();
                assert!(
                    hashed.contains(&target),
                    "{path}: {name}! target is not fingerprinted: {}",
                    target.display()
                );
            }
        }
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
                format!("const INPUT = {name}{trivia}!{trivia}(\n\"../input.txt\",\n);\n"),
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

fn check_generator_label(root: &Path) -> std::result::Result<(), String> {
    let baseline: Baseline = read(&root.join("tools/parity/baseline/config-save.json")).unwrap();
    let actual = fingerprint_at(root, "pin").unwrap();
    if actual["generator_sha256"] != baseline.fingerprint["generator_sha256"] {
        return Err(format!(
            "generator_sha256 differs from the committed baseline; verify case inputs, relabel matching observations and run a full refresh: {REFRESH}"
        ));
    }
    Ok(())
}

#[test]
fn committed_generator_fingerprint_matches_real_tree() {
    check_generator_label(root()).unwrap();

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
    check_generator_label(&copy).unwrap();
    fs::write(copy.join(GENERATOR_PATHS[0]), "changed random generator\n").unwrap();
    let error = check_generator_label(&copy).unwrap_err();
    assert!(
        error.contains("relabel") && error.contains(REFRESH),
        "{error}"
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
        let proof = ablation::certify(&case, go, &protected, |_| {
            Ok(GoResult::Output(serde_json::to_vec(&oracle).unwrap()))
        })
        .unwrap();
        assert!(proof.deviations.is_none());
        assert_eq!(
            proof.evidence["failures"][0]["reason"],
            "comment_free_go_malformed"
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
                    "comment_free_go_malformed"
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
