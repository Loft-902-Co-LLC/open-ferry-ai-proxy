//! A failed upstream run is accepted only with a complete counterfactual.
//! Neither failure text nor partial files are repaired or discarded.

use crate::cases::Case;
use serde_json::{Value, json};

/// The exact source-only rewrite implemented beside the writer's protection.
/// Configs supplied to individual steps and whole-file writes are untouched.
pub fn rewritten(case: &Case) -> Option<Case> {
    let source = case.options["file"].as_str()?;
    let source = open_ferry_core::config::save::parity::quoted_source(source).ok()??;
    let mut candidate = case.clone();
    candidate.options["file"] = source.into();
    Some(candidate)
}

/// Failure shape, an exact permitted rewrite, successful complete Go output,
/// equal Go/Rust outputs under the existing rules, and identical Rust runs.
pub fn proves(
    case: &Case,
    candidate: &Case,
    original_go: &Value,
    counterfactual_go: &Value,
    go_matches: bool,
    rust_original: &Value,
    rust_rewritten: &Value,
) -> bool {
    let yaml_failure = ["error", "unloadable"].iter().any(|key| {
        original_go[*key]
            .as_str()
            .is_some_and(|error| error.contains("yaml:"))
    });
    yaml_failure
        && rewritten(case).is_some_and(|expected| expected.options == candidate.options)
        && counterfactual_go
            .as_object()
            .is_some_and(|fields| fields.len() == 1)
        && counterfactual_go["files"].as_array().is_some_and(|files| {
            case.options["steps"]
                .as_array()
                .is_some_and(|steps| files.len() == steps.len())
        })
        && go_matches
        && rust_original == rust_rewritten
}

pub fn evidence(
    case: &Case,
    candidate: &Case,
    original_go: &Value,
    counterfactual_go: &Value,
    rust_original: &Value,
    rust_rewritten: &Value,
) -> Value {
    json!({"case":case.name,"options":case.options,"rewritten_options":candidate.options,
        "original_go":original_go,"counterfactual_go":counterfactual_go,
        "rust_original":rust_original,"rust_rewritten":rust_rewritten})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translator::Translator;
    use crate::upstream::GoResult;

    #[test]
    fn null_plugin_refusal_without_exact_counterfactual_stays_different() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-null-generated-refusal.json"
        ))
        .unwrap();
        let case = Case::new("null-plugin-generated-refusal", "", "")
            .with_options(fixture["options"].clone());
        let rust = Translator::ConfigSave.run_rust(&case).unwrap();
        assert_eq!(rust, fixture["open_ferry_output"]);
        assert!(rewritten(&case).is_none());
        let evaluated = crate::evaluate(
            Translator::ConfigSave,
            &case,
            &GoResult::Output(fixture["original_go"].to_string().into_bytes()),
        );
        assert!(matches!(evaluated.outcome, crate::Outcome::Different(_)));
    }

    #[test]
    fn three_saved_whole_cases_require_every_counterfactual_check() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-counterfactual.json"
        ))
        .unwrap();
        let pending: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-pending-key-counterfactual.json"
        ))
        .unwrap();
        for fixture in fixtures
            .as_array()
            .unwrap()
            .iter()
            .chain(pending.as_array().unwrap())
        {
            let case = Case::new(fixture["case"].as_str().unwrap(), "", "")
                .with_options(fixture["options"].clone());
            let candidate = rewritten(&case).unwrap();
            assert_eq!(candidate.options, fixture["rewritten_options"]);
            let rust_original = Translator::ConfigSave.run_rust(&case).unwrap();
            let rust_rewritten = Translator::ConfigSave.run_rust(&candidate).unwrap();
            assert_eq!(rust_original, fixture["open_ferry_output"]);
            let evaluated = crate::evaluate(
                Translator::ConfigSave,
                &candidate,
                &GoResult::Output(fixture["counterfactual_go"].to_string().into_bytes()),
            );
            let matching = matches!(
                evaluated.outcome,
                crate::Outcome::Identical | crate::Outcome::Equivalent(_)
            );
            assert!(proves(
                &case,
                &candidate,
                &fixture["original_go"],
                &fixture["counterfactual_go"],
                matching,
                &rust_original,
                &rust_rewritten
            ));
            let mut changed = candidate.clone();
            changed.options["file"] =
                format!("{}\n", changed.options["file"].as_str().unwrap()).into();
            assert!(!proves(
                &case,
                &changed,
                &fixture["original_go"],
                &fixture["counterfactual_go"],
                matching,
                &rust_original,
                &rust_rewritten
            ));
            let mut failed = fixture["counterfactual_go"].clone();
            failed["error"] = "yaml: still fails".into();
            assert!(!proves(
                &case,
                &candidate,
                &fixture["original_go"],
                &failed,
                matching,
                &rust_original,
                &rust_rewritten
            ));
            assert!(!proves(
                &case,
                &candidate,
                &fixture["original_go"],
                &fixture["counterfactual_go"],
                false,
                &rust_original,
                &rust_rewritten
            ));
            let mut changed_rust = rust_rewritten.clone();
            changed_rust["files"][0] =
                format!("{}\n", changed_rust["files"][0].as_str().unwrap()).into();
            assert!(!proves(
                &case,
                &candidate,
                &fixture["original_go"],
                &fixture["counterfactual_go"],
                matching,
                &rust_original,
                &changed_rust
            ));
            assert!(!proves(
                &case,
                &candidate,
                &json!({"files":[]}),
                &fixture["counterfactual_go"],
                matching,
                &rust_original,
                &rust_rewritten
            ));
        }
    }
}
