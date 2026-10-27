use super::*;
use std::sync::{Barrier, Mutex};
use std::time::Duration;

#[test]
fn jobs_default_to_available_processors_and_require_a_positive_count() {
    let parse = |extra: &[&str]| {
        Args::parse(
            ["--upstream", "."]
                .into_iter()
                .chain(extra.iter().copied())
                .map(str::to_owned),
        )
    };
    assert_eq!(
        parse(&[]).unwrap().jobs,
        std::thread::available_parallelism().map_or(1, usize::from)
    );
    assert_eq!(parse(&["--jobs", "1"]).unwrap().jobs, 1);
    assert_eq!(parse(&["--jobs", "4"]).unwrap().jobs, 4);
    for flags in [
        &["--jobs"][..],
        &["--jobs", "0"],
        &["--jobs", "-1"],
        &["--jobs", "many"],
    ] {
        assert!(parse(flags).is_err());
    }
}

#[test]
fn parallel_fixture_records_and_artifacts_match_sequential_in_case_order() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("cases/testdata/config-save-r8-certifier.json")).unwrap();
    let rows = &rows[..4];
    let barrier = Barrier::new(4);
    let run = |jobs| {
        let dir = std::env::temp_dir().join(format!(
            "parity-parallel-fixtures-{}-{jobs}",
            std::process::id()
        ));
        fresh_dir(&dir).unwrap();
        let completed = Mutex::new(Vec::new());
        let mut records = Vec::new();
        let mut tally = Tally::default();
        parallel::for_each(rows.len(), jobs, |index| {
            let row = &rows[index];
            let case = Case::new(row["case"].as_str().unwrap(), "", "").with_options(row["options"].clone());
            let go = &row["probes"].as_array().unwrap().iter().find(|p| p["kind"] == "options").unwrap()["go"];
            let mut evaluated = evaluate(Translator::ConfigSave, &case, &GoResult::Output(serde_json::to_vec(go).unwrap()));
            let certify = || {
                config_save::ablation::certify(&case, go, evaluated.rust_output.as_ref().unwrap(), |candidate| {
                    if candidate.options.get("_ownership_request").is_some() {
                        return Ok(GoResult::Panic("identity fixture unavailable".into()));
                    }
                    let probe = row["probes"].as_array().unwrap().iter().find(|p| p["options"] == candidate.options).unwrap();
                    Ok(GoResult::Output(serde_json::to_vec(&probe["go"])?))
                }).unwrap()
            };
            let proof = if jobs > 1 { config_save::ablation::without_parallel_replay(certify) } else { certify() };
            if let Some(deviations) = proof.deviations {
                evaluated.outcome = Outcome::Equivalent(deviations);
            } else {
                evaluated.certification_failure = baseline::difference_kinds(&case.options, &proof.evidence, go, evaluated.rust_output.as_ref().ok());
            }
            if jobs > 1 {
                barrier.wait();
                if index == 0 {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            completed.lock().unwrap().push(index);
            (case, evaluated, proof.evidence)
        }, |_, (case, evaluated, proof)| -> Result<(), Box<dyn Error>> {
            let outcome = match &evaluated.outcome {
                Outcome::Identical => json!(["identical"]),
                Outcome::Equivalent(d) => json!(["equivalent", d.iter().map(|d| d.describe()).collect::<Vec<_>>()]),
                Outcome::Different(d) => json!(["different", d.iter().map(|d| (&d.path, &d.go, &d.rust)).collect::<Vec<_>>()]),
            };
            records.push(json!({"case":case.name,"outcome":outcome,"kinds":evaluated.certification_failure,"proof":proof,"rust":evaluated.rust_output}));
            tally.record(&case, evaluated, &dir)
        }).unwrap();
        if jobs > 1 {
            assert_ne!(
                completed.lock().unwrap()[0],
                0,
                "force completion order to differ"
            );
        }
        let artifacts: BTreeMap<_, _> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect();
        fs::remove_dir_all(dir).unwrap();
        (
            records,
            artifacts,
            tally.identical,
            tally.equivalent,
            tally.different,
            tally.certification_failures,
        )
    };
    let sequential = run(1);
    let parallel = run(4);
    assert_eq!(sequential, parallel);
    assert_eq!(
        parallel
            .0
            .iter()
            .map(|r| r["case"].as_str().unwrap())
            .collect::<Vec<_>>(),
        rows.iter()
            .map(|r| r["case"].as_str().unwrap())
            .collect::<Vec<_>>()
    );
}

#[test]
fn parallel_errors_stop_recording_at_the_first_case_order_error() {
    for jobs in [1, 4] {
        let mut recorded = Vec::new();
        let result = parallel::for_each(
            12,
            jobs,
            |index| {
                if index == 1 {
                    std::thread::sleep(Duration::from_millis(30));
                }
                if index == 1 || index == 2 {
                    Err(format!("case {index}"))
                } else {
                    Ok(index)
                }
            },
            |_, result| {
                recorded.push(result?);
                Ok::<_, String>(())
            },
        );
        assert_eq!(result, Err("case 1".into()));
        assert_eq!(recorded, [0]);
    }
    parallel::for_each(
        0,
        4,
        |_| unreachable!(),
        |_, _: ()| -> Result<(), ()> { unreachable!() },
    )
    .unwrap();
}

#[test]
fn workers_restore_protection_scopes_after_each_case_and_panic() {
    use open_ferry_core::config::save::parity::{
        self as core,
        ablation::{with_disabled, with_trace},
    };
    let all = config_save::ablation::ALL;
    parallel::for_each(
        16,
        4,
        |_| {
            assert_eq!(core::disabled_protections(&all), 0);
            let _ = panic::catch_unwind(|| {
                with_trace(|| with_disabled(&all, || panic!("case diagnostic")))
            });
            assert_eq!(core::disabled_protections(&all), 0);
        },
        |_, ()| Ok::<_, ()>(()),
    )
    .unwrap();
}

#[test]
fn staged_artifacts_publish_duplicate_names_in_record_order() {
    let root = std::env::temp_dir().join(format!("parity-publish-order-{}", std::process::id()));
    fresh_dir(&root).unwrap();
    let case = Case::new("same-name", "", "");
    for value in ["first", "last"] {
        let staged = root.join(value);
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("input.jsonl"), value).unwrap();
        save_counterfactual(&staged, &case, &json!({"value":value})).unwrap();
        publish_case(&staged, &root, &case).unwrap();
        assert_eq!(
            fs::read(root.join("input.jsonl")).unwrap(),
            value.as_bytes()
        );
        assert_eq!(
            fs::read(root.join("counterfactual/same-name.json")).unwrap(),
            serde_json::to_vec_pretty(&json!({"value":value})).unwrap()
        );
    }
    fs::remove_dir_all(root).unwrap();
}
