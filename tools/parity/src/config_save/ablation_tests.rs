//! Raw diagnostic replay is necessary, but does not certify a Known result.

use open_ferry_core::config::Config;
use open_ferry_core::config::save::parity::ablation::{Protection, with_disabled};
use serde_json::Value;

use crate::cases::Case;
use crate::translator::Translator;

const ALL: [Protection; 5] = [
    Protection::EmptyMappingPendingComment,
    Protection::CanonicalVersionComment,
    Protection::DroppedSingleKeyComment,
    Protection::InlineEmptyNullComment,
    Protection::MultipleFlowItemFeet,
];

fn case(fixture: &Value) -> Case {
    Case::new(fixture["case"].as_str().unwrap(), "", "").with_options(fixture["options"].clone())
}

fn subsets() -> Vec<Vec<Protection>> {
    let mut masks: Vec<u8> = (1_u8..16).collect();
    masks.sort_by_key(|mask| mask.count_ones());
    masks
        .into_iter()
        .map(|mask| {
            ALL.into_iter()
                .enumerate()
                .filter_map(|(index, p)| (mask & (1 << index) != 0).then_some(p))
                .collect()
        })
        .collect()
}

#[test]
fn all_comment_ablations_leave_the_independent_block_scalar_protection_enabled() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../cases/testdata/config-save-unablated-block-scalar.json"
    ))
    .unwrap();
    let case = case(&fixture);
    let raw_go: Value =
        serde_json::from_str(fixture["upstream_output_raw"].as_str().unwrap()).unwrap();
    let protected = Translator::ConfigSave.run_rust(&case).unwrap();
    assert_eq!(protected, fixture["open_ferry_output"]);
    assert_eq!(raw_go["files"].as_array().unwrap().len(), 1);
    assert!(raw_go["unloadable"].is_string());
    assert!(Config::load_bytes(raw_go["files"][0].as_str().unwrap().as_bytes()).is_err());
    assert!(
        raw_go["files"][0]
            .as_str()
            .unwrap()
            .contains("        - |- #   c50 with words\n#c51\n          <tag>\n")
    );
    assert!(
        protected["files"][0]
            .as_str()
            .unwrap()
            .contains("        - \"<tag>\" #   c50 with words\n#c51\n")
    );
    for file in protected["files"].as_array().unwrap() {
        Config::load_bytes(file.as_str().unwrap().as_bytes()).unwrap();
    }
    let subsets = subsets();
    assert_eq!(subsets.len(), 15);
    let mut attempts = Vec::new();
    for disabled in subsets {
        let ablated = with_disabled(&disabled, || Translator::ConfigSave.run_rust(&case)).unwrap();
        // No subset even reproduces the FIRST snapshot. The scalar-value
        // protection is outside the four permitted comment switches.
        assert_eq!(ablated["files"][0], protected["files"][0], "{disabled:?}");
        assert_ne!(ablated["files"][0], raw_go["files"][0], "{disabled:?}");
        assert_ne!(ablated, raw_go, "{disabled:?}");
        attempts.push(serde_json::json!({
            "disabled": disabled.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>(),
            "ablated_rust": ablated,
            "first_snapshot_equals_raw_go": false,
            "complete_output_equals_raw_go": false,
        }));
    }
    // Explicit test-only export for the investigation report. These are
    // refusals, never accepted counterfactual proof artifacts.
    if let Some(dir) = std::env::var_os("PARITY_ABLATION_EVIDENCE") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("uncertified-random-31-2887.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "kind": "uncertified-protection-ablation",
                "case": case.name,
                "options": case.options,
                "original_go": raw_go,
                "protected_rust": protected,
                "attempts": attempts,
                "check_1": false,
                "checks_2_3_4": "not evaluated: check 1 fails for all 15 subsets",
                "outcome": "Different",
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let evaluated = crate::evaluate(
        Translator::ConfigSave,
        &case,
        &crate::upstream::GoResult::Output(
            fixture["upstream_output_raw"]
                .as_str()
                .unwrap()
                .as_bytes()
                .to_vec(),
        ),
    );
    assert!(matches!(evaluated.outcome, crate::Outcome::Different(_)));
    assert_eq!(Translator::ConfigSave.run_rust(&case).unwrap(), protected);
}

#[test]
fn named_switches_reproduce_saved_raw_outputs_without_certifying_them() {
    let fixtures: Vec<Value> = serde_json::from_str(include_str!(
        "../cases/testdata/config-save-ablation-replays.json"
    ))
    .unwrap();
    for fixture in fixtures {
        let case = case(&fixture);
        let protected = Translator::ConfigSave.run_rust(&case).unwrap();
        assert_eq!(protected, fixture["open_ferry_output"]);
        let expected: Value =
            serde_json::from_str(fixture["upstream_output_raw"].as_str().unwrap()).unwrap();
        let disabled: Vec<_> = fixture["disable"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| match p.as_str().unwrap() {
                "EMPTY" => ALL[0],
                "VERSION" => ALL[1],
                "SINGLE" => ALL[2],
                "INLINE" => ALL[3],
                other => panic!("unknown protection {other}"),
            })
            .collect();
        assert_ne!(protected, expected);
        assert_eq!(
            with_disabled(&disabled, || Translator::ConfigSave.run_rust(&case)).unwrap(),
            expected,
            "{}",
            case.name,
        );
        assert_eq!(Translator::ConfigSave.run_rust(&case).unwrap(), protected);
    }
}

#[test]
fn canonical_switch_replays_both_intermediate_version_histories() {
    let fixtures: Vec<Value> = serde_json::from_str(include_str!(
        "../cases/testdata/config-save-version-history.json"
    ))
    .unwrap();
    for fixture in fixtures {
        let case = case(&fixture);
        let expected: Value =
            serde_json::from_str(fixture["upstream_output_text"].as_str().unwrap()).unwrap();
        assert_eq!(
            with_disabled(&[ALL[1]], || Translator::ConfigSave.run_rust(&case)).unwrap(),
            expected,
        );
        assert_eq!(
            Translator::ConfigSave.run_rust(&case).unwrap(),
            fixture["open_ferry_output"],
        );
    }
}
