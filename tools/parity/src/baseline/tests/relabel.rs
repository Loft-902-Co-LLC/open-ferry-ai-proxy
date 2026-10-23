use super::*;

fn file(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("parity-relabel-{name}-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    directory.join("baseline.json")
}

fn labeled() -> (Baseline, Fingerprint) {
    let current = fingerprint("pin").unwrap();
    let mut baseline = baseline();
    baseline.fingerprint = current.clone();
    baseline
        .fingerprint
        .insert("generator_sha256".into(), "0".repeat(64));
    baseline.random_case_probes = Some(probes::digests().clone());
    (baseline, current)
}

#[test]
fn relabel_changes_only_generator_sha256_when_both_probes_match() {
    let (baseline, current) = labeled();
    let path = file("matching");
    write(&path, &baseline).unwrap();
    let before = fs::read_to_string(&path).unwrap();
    relabel(&path, &current).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        before.replace(&"0".repeat(64), &current["generator_sha256"])
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn relabel_refuses_stale_binary_sources_without_writing() {
    let (baseline, current) = labeled();
    let path = file("stale-binary");
    write(&path, &baseline).unwrap();
    let before = fs::read(&path).unwrap();
    let mut on_disk: BTreeMap<_, _> = sources::EMBEDDED
        .iter()
        .map(|(path, source)| (path.to_string(), source.to_string()))
        .collect();
    on_disk
        .get_mut(GENERATOR_PATHS[1])
        .unwrap()
        .push_str("\n// newer source\n");
    let error = relabel_with_sources(&path, &current, &on_disk)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("binary is older") && error.contains(RELABEL),
        "{error}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);

    // Checkout line endings do not make the embedded binary stale.
    let crlf = sources::EMBEDDED
        .iter()
        .map(|(path, source)| {
            (
                path.replace('/', "\\"),
                source.replace("\r\n", "\n").replace('\n', "\r\n"),
            )
        })
        .collect();
    relabel_with_sources(&path, &current, &crlf).unwrap();
    assert_eq!(
        generator_hash(sources::EMBEDDED.iter().copied()),
        current["generator_sha256"]
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn relabel_changed_probe_refuses_without_writing() {
    let (baseline, current) = labeled();
    let path = file("changed");
    for profile in probes::digests().keys() {
        let mut changed = baseline.clone();
        changed
            .random_case_probes
            .as_mut()
            .unwrap()
            .insert(profile.clone(), "0".repeat(64));
        write(&path, &changed).unwrap();
        let before = fs::read(&path).unwrap();
        let error = relabel(&path, &current).unwrap_err().to_string();
        assert!(
            error.contains(profile) && error.contains(REFRESH),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn relabel_refuses_classifier_and_pin_changes_and_legacy_without_probes() {
    let (baseline, current) = labeled();
    let path = file("legacy");
    for reason in ["classifier_version", "upstream_pin", "random_case_probes"] {
        let mut changed = baseline.clone();
        if reason == "random_case_probes" {
            changed.random_case_probes = None;
        } else {
            changed.fingerprint.insert(
                reason.into(),
                if reason == "classifier_version" {
                    "2"
                } else {
                    "old"
                }
                .into(),
            );
        }
        write(&path, &changed).unwrap();
        // Missing probes remain supported for ordinary baseline validation.
        let decoded: Baseline = read(&path).unwrap();
        decoded.validate(&decoded.fingerprint).unwrap();
        let before = fs::read(&path).unwrap();
        let error = relabel(&path, &current).unwrap_err().to_string();
        assert!(error.contains(reason) && error.contains(REFRESH), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn full_refresh_records_the_real_export_digests() {
    let original = observation();
    let mut observations: Vec<_> = (30..40)
        .map(|seed| Observation {
            seed,
            ..original.clone()
        })
        .collect();
    observations.push(Observation {
        seed: 1,
        random: 25,
        ..original.clone()
    });
    let refreshed = refresh(&observations, &original.fingerprint).unwrap();
    assert_eq!(
        refreshed.random_case_probes.as_ref(),
        Some(probes::digests())
    );
    assert!(probes::valid(
        refreshed.random_case_probes.as_ref().unwrap()
    ));
    for (seed, random) in probes::PROFILES {
        let bytes = probes::export(seed, random).unwrap();
        let rows: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(rows.len(), random);
        assert_eq!(rows[0]["id"], format!("random-{seed}-0"));
        assert_eq!(rows[0].as_object().unwrap().len(), 7);
        let expected: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            probes::digests()[&format!("seed-{seed}-random-{random}")],
            expected
        );
        assert!(!bytes.contains(&b'\r') && !bytes.contains(&b'\n'));
    }
}

#[test]
fn export_and_relabel_cli_are_offline_and_cannot_mix_with_runs() {
    for flag in ["--export-config-cases", "--relabel-baseline"] {
        let args = || [flag, "out.json"].into_iter().map(str::to_owned);
        let parsed = crate::Args::parse(args()).unwrap();
        assert!(parsed.upstream.as_os_str().is_empty());
        for extra in [
            ["--baseline", "existing.json"],
            ["--upstream", "."],
            ["--suite", "config-save"],
        ] {
            assert!(
                crate::Args::parse(args().chain(extra.into_iter().map(str::to_owned))).is_err()
            );
        }
    }
    assert!(
        crate::Args::parse(
            [
                "--relabel-baseline",
                "out.json",
                "--export-config-cases",
                "inputs.json"
            ]
            .into_iter()
            .map(str::to_owned)
        )
        .is_err()
    );
    for extra in [
        ["--seed", "1"],
        ["--random", "5000"],
        ["--show", "10"],
        ["--go", "go"],
    ] {
        let error = crate::Args::parse(
            ["--relabel-baseline", "out.json"]
                .into_iter()
                .chain(extra)
                .map(str::to_owned),
        )
        .err()
        .unwrap();
        assert!(
            error.contains(extra[0]) && error.contains("fixed"),
            "{error}"
        );
    }
    assert!(
        crate::Args::parse(
            [
                "--export-config-cases",
                "out.json",
                "--seed",
                "1",
                "--random",
                "25"
            ]
            .into_iter()
            .map(str::to_owned)
        )
        .is_ok()
    );
}
