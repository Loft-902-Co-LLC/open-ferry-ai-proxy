//! A named known difference, independent of equivalences and certification.
use crate::cases::Case;
use serde_json::{Value, json};

pub const REASON: &str =
    "upstream can add trailing newlines; Open Ferry preserves the source value";

pub fn evidence(case: &Case, go: &Value, rust: &Value) -> Option<Value> {
    if go == rust || go.as_object()?.len() != 1 || rust.as_object()?.len() != 1 {
        return None;
    }
    let (gs, rs, steps) = (
        go["files"].as_array()?,
        rust["files"].as_array()?,
        case.options["steps"].as_array()?,
    );
    if gs.len() != steps.len() || rs.len() != steps.len() {
        return None;
    }
    let mut source = case.options["file"].as_str()?;
    let mut witnesses = Vec::new();
    for (index, ((g, r), step)) in gs.iter().zip(rs).zip(steps).enumerate() {
        let mut config = None;
        let mut nested = None;
        match step["op"].as_str()? {
            "write" => source = step["body"].as_str()?,
            "save" => {
                if let Some(value) = step.get("config") {
                    config = Some(value.as_str()?);
                }
            }
            "nested" => {
                let keys = step["keys"]
                    .as_array()?
                    .iter()
                    .map(|key| key.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()?;
                nested = Some((keys, step["value"].as_str()?));
            }
            _ => return None,
        }
        let (g, r) = (g.as_str()?, r.as_str()?);
        if g != r {
            if !open_ferry_core::config::save::parity::extra_keep_chomped_newlines(
                source,
                g,
                r,
                config,
                nested
                    .as_ref()
                    .map(|(keys, value)| (keys.as_slice(), *value)),
            ) {
                return None;
            }
            witnesses.push(json!({"step":index,"source":source,"go":g,"rust":r}));
        }
        source = r;
    }
    (!witnesses.is_empty()).then(|| {
        json!({
            "kind":"known-upstream-keep-chomped-newlines", "case":case.name,
            "options":case.options, "original_go":go, "protected_rust":rust,
            "witnesses":witnesses
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST_SOURCE: &str =
        "plugins:\n  configs:\n    example:\n      text: |+\n        note\n\n";

    #[test]
    fn requested_nested_change_cannot_be_known_newline_growth() {
        let case = Case::new("requested-nested-change", "", "").with_options(json!({
            "file":REQUEST_SOURCE,"steps":[{"op":"nested",
                "keys":["plugins","configs","example","text"],"value":"note\n\n\n"}]
        }));
        let go = json!({"files":[format!("{REQUEST_SOURCE}\n")]});
        let rust = json!({"files":[REQUEST_SOURCE]});
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn requested_config_change_cannot_be_known_newline_growth() {
        let requested = format!("{REQUEST_SOURCE}\n\n");
        let case = Case::new("requested-config-change", "", "").with_options(json!({
            "file":REQUEST_SOURCE,"steps":[{"op":"save","config":requested}]
        }));
        let go = json!({"files":[requested]});
        let rust = json!({"files":[REQUEST_SOURCE]});
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn saved_seed_12_history_preserves_all_four_snapshots() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-keep-chomped-history.json"
        ))
        .unwrap();
        let case = Case::new("random-12-2229", "", "").with_options(fixture["options"].clone());
        let rust = crate::config_save::steps(&case).unwrap();
        assert_eq!(rust, fixture["open_ferry_output"]);
        let go = &fixture["upstream_output"];
        let proof = evidence(&case, go, &rust).expect("strict known difference");
        assert_eq!(proof["witnesses"].as_array().unwrap().len(), 4);
        for index in 0..4 {
            let mut near = go.clone();
            near["files"][index] =
                format!("{}# unrelated\n", go["files"][index].as_str().unwrap()).into();
            assert!(evidence(&case, &near, &rust).is_none());
        }
    }

    #[test]
    fn complete_unchanged_history_can_be_a_known_difference() {
        let source =
            "plugins:\n  configs:\n    example:\n      text: >+\n        fictional note\n\n\n";
        let case = Case::new("keep-chomped-history", "", "").with_options(json!({
            "file":"", "steps":[{"op":"write","body":source},{"op":"save"}]
        }));
        let go = json!({"files":[format!("{source}\n"),format!("{source}\n\n")]});
        let rust = json!({"files":[source,source]});
        assert!(evidence(&case, &go, &rust).is_some());
    }

    fn literal_case() -> (Case, Value, Value) {
        (
            Case::new("literal-newline-growth", "", "").with_options(json!({
                "file":REQUEST_SOURCE,"steps":[{"op":"save"}]
            })),
            json!({"files":[format!("{REQUEST_SOURCE}\n")]}),
            json!({"files":[REQUEST_SOURCE]}),
        )
    }

    #[test]
    fn unchanged_or_absent_requested_values_allow_the_proof() {
        for step in [
            json!({"op":"save","config":REQUEST_SOURCE}),
            json!({"op":"save","config":"debug: false\n"}),
            json!({"op":"nested","keys":["plugins","configs","example","text"],"value":"note\n\n"}),
            json!({"op":"nested","keys":["unrelated"],"value":"requested"}),
        ] {
            let (mut case, go, rust) = literal_case();
            case.options["steps"][0] = step;
            assert!(evidence(&case, &go, &rust).is_some());
        }
    }

    #[test]
    fn requested_parent_edit_cannot_hide_a_dropped_edit() {
        let (mut case, go, rust) = literal_case();
        case.options["steps"][0] = json!({"op":"nested",
            "keys":["plugins","configs","example"],"value":"note\n\n"});
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn requested_child_edit_cannot_hide_a_dropped_edit() {
        let (mut case, go, rust) = literal_case();
        case.options["steps"][0] = json!({"op":"nested",
            "keys":["plugins","configs","example","text","child"],"value":"note\n\n"});
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn requested_config_parent_replacement_cannot_hide_a_dropped_edit() {
        let (mut case, go, rust) = literal_case();
        case.options["steps"][0] = json!({"op":"save","config":"plugins: null\n"});
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn requested_nested_v8_path_is_compared_after_alias_resolution() {
        let source = "config-version: 8\nmanagement:\n  panel-github-repository: |+\n    note\n\n";
        let case = Case::new("v8-nested-request", "", "").with_options(json!({
            "file":source,"steps":[{"op":"nested",
                "keys":["management","panel-github-repository"],"value":"note\n\n\n"}]
        }));
        assert!(
            evidence(
                &case,
                &json!({"files":[format!("{source}\n")]}),
                &json!({"files":[source]})
            )
            .is_none()
        );
    }

    #[test]
    fn unrelated_output_comment_alone_prevents_the_proof() {
        let (case, mut go, rust) = literal_case();
        go["files"][0] = format!("{REQUEST_SOURCE}\n# unrelated\n").into();
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn same_source_value_at_another_path_is_not_a_witness() {
        let (mut case, go, rust) = literal_case();
        case.options["file"] = REQUEST_SOURCE.replace("example:", "other:").into();
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn changed_rust_value_alone_prevents_the_proof() {
        let (case, _, _) = literal_case();
        let go = json!({"files":[format!("{REQUEST_SOURCE}\n\n")]});
        let rust = json!({"files":[format!("{REQUEST_SOURCE}\n")]});
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn missing_go_snapshot_alone_prevents_the_proof() {
        let (mut case, go, mut rust) = literal_case();
        case.options["steps"]
            .as_array_mut()
            .unwrap()
            .push(json!({"op":"save"}));
        rust["files"]
            .as_array_mut()
            .unwrap()
            .push(json!(REQUEST_SOURCE));
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn missing_rust_snapshot_alone_prevents_the_proof() {
        let (mut case, mut go, rust) = literal_case();
        case.options["steps"]
            .as_array_mut()
            .unwrap()
            .push(json!({"op":"save"}));
        go["files"]
            .as_array_mut()
            .unwrap()
            .push(json!(format!("{REQUEST_SOURCE}\n\n")));
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn go_error_alone_prevents_the_proof() {
        let (case, mut go, rust) = literal_case();
        go["error"] = "refused".into();
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn rust_error_alone_prevents_the_proof() {
        let (case, go, mut rust) = literal_case();
        rust["error"] = "refused".into();
        assert!(evidence(&case, &go, &rust).is_none());
    }

    #[test]
    fn single_save_folded_proof_keeps_its_equivalent_bucket() {
        let fixtures: Vec<Value> = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-folded-emission.json"
        ))
        .unwrap();
        let fixture = fixtures
            .iter()
            .find(|f| f["case"] == "styles-resave")
            .unwrap();
        let case = Case::new("styles-resave", "", "").with_options(fixture["options"].clone());
        let (go, rust) = (&fixture["upstream_output"], &fixture["open_ferry_output"]);
        assert!(evidence(&case, go, rust).is_some());
        assert!(super::super::folded::evidence(&case, go, rust).is_some());
        let (outcome, _) = crate::scalar_reencode_proof(&case, go, rust).unwrap();
        assert!(
            matches!(outcome, crate::Outcome::Equivalent(d) if d.contains(&crate::Deviation::UpstreamFoldedScalarReencode))
        );
    }
}
