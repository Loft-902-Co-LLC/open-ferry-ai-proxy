//! A failed upstream run is accepted only with a complete counterfactual.
//! Neither failure text nor partial files are repaired or discarded.

use crate::cases::Case;
use crate::compare::Deviation;
use serde_json::{Value, json};

/// Quote only the block scalars protected by the writer, in the initial
/// file and in whole-file write bodies. Separate running configs and every
/// other option stay unchanged. A write body starts a new file source.
pub fn rewritten(case: &Case) -> Option<Case> {
    fn quote(input: &mut Value) -> Option<bool> {
        let source = input.as_str()?;
        if let Some(quoted) = open_ferry_core::config::save::parity::quoted_source(source)
            .ok()
            .flatten()
        {
            *input = quoted.into();
            Some(true)
        } else {
            Some(false)
        }
    }
    let mut candidate = case.clone();
    let mut changed = quote(&mut candidate.options["file"])?;
    for step in candidate.options["steps"].as_array_mut()? {
        if step["op"] == "write" {
            changed |= quote(&mut step["body"])?;
        }
    }
    changed.then_some(candidate)
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
    reason(case, original_go, rust_original).is_some()
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

/// Keep successful-but-lossy archives separate from failed YAML writes.
/// Outside precisely matched archived scalar/comment spans, every byte stays equal.
pub fn reason(case: &Case, go: &Value, rust: &Value) -> Option<Deviation> {
    if ["error", "unloadable"]
        .iter()
        .any(|key| go[*key].as_str().is_some_and(|e| e.contains("yaml:")))
    {
        return Some(Deviation::UpstreamBlockScalarCounterfactual);
    }
    fn segments(text: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for line in text.split_inclusive('\n') {
            let archived = line.starts_with("# ");
            if let Some((kind, body)) = out.last_mut()
                && *kind == archived
            {
                body.push_str(if archived { &line[2..] } else { line });
            } else {
                out.push((
                    archived,
                    if archived {
                        line[2..].into()
                    } else {
                        line.into()
                    },
                ));
            }
        }
        out
    }
    if go.as_object()?.len() != 1 || rust.as_object()?.len() != 1 {
        return None;
    }
    let gs = go["files"].as_array()?;
    let rs = rust["files"].as_array()?;
    let steps = case.options["steps"].as_array()?;
    if gs.len() != steps.len() || gs.len() != rs.len() {
        return None;
    }
    let mut sources = vec![case.options["file"].as_str()?];
    let mut changed = false;
    for ((g, r), step) in gs.iter().zip(rs).zip(steps) {
        if step["op"] == "write" {
            sources = vec![step["body"].as_str()?];
        }
        let g = g.as_str()?;
        let r = r.as_str()?;
        if g == r {
            continue;
        }
        let ga = segments(g);
        let ra = segments(r);
        if ga.len() != ra.len() {
            return None;
        }
        for ((archived, g), (r_archived, r)) in ga.iter().zip(&ra) {
            if g == r && archived == r_archived {
                continue;
            }
            if !archived
                || !r_archived
                || !open_ferry_core::config::save::parity::archived_scalar_comment_loss(
                    &sources, g, r,
                )
            {
                return None;
            }
            changed = true;
        }
    }
    changed.then_some(Deviation::UpstreamArchivedScalarCounterfactual)
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

pub struct StepwiseProof {
    pub deviations: std::collections::BTreeSet<Deviation>,
    pub evidence: Value,
}

/// A raw legacy write may leave a safe source unchanged, with the hazard
/// appearing only when a later save re-encodes it. Replay each operation from
/// the preceding certified Go file. Each Rust replay must equal the original
/// whole-case snapshot; each hazardous step needs all four ordinary checks.
/// No earlier output is replaced by a later step's protected encoding.
pub fn stepwise(
    case: &Case,
    original_go: &Value,
    rust_original: &Value,
    mut run_go: impl FnMut(&Case) -> Result<crate::upstream::GoResult, Box<dyn std::error::Error>>,
) -> Result<Option<StepwiseProof>, Box<dyn std::error::Error>> {
    use crate::{Outcome, Translator, evaluate};
    let Some(steps) = case.options["steps"].as_array() else {
        return Ok(None);
    };
    let Some(files) = rust_original["files"].as_array() else {
        return Ok(None);
    };
    if steps.len() < 2
        || steps.len() != files.len()
        || rust_original.as_object().is_none_or(|o| o.len() != 1)
    {
        return Ok(None);
    }
    let mut current = case.options["file"].clone();
    let mut proofs = Vec::new();
    let mut deviations = std::collections::BTreeSet::new();
    let mut protected = false;
    for (index, (step, expected)) in steps.iter().zip(files).enumerate() {
        let mut single = case.clone();
        single.options["file"] = current;
        single.options["steps"] = json!([step]);
        let original = evaluate(Translator::ConfigSave, &single, &run_go(&single)?);
        let Ok(rust) = &original.rust_output else {
            return Ok(None);
        };
        if *rust != json!({"files":[expected]}) {
            return Ok(None);
        }
        let Ok(go) = serde_json::from_str::<Value>(&original.go_output) else {
            return Ok(None);
        };
        // Before the first rewrite, replay must reproduce the original run's
        // successful snapshots, not silently substitute a different prefix.
        if !protected
            && let Some(prior) = original_go["files"].get(index)
            && go["files"].get(0) != Some(prior)
        {
            return Ok(None);
        }
        let (certified_go, proof) = match original.outcome {
            Outcome::Identical => (go.clone(), evidence(&single, &single, &go, &go, rust, rust)),
            Outcome::Equivalent(found) => {
                deviations.extend(found);
                (go.clone(), evidence(&single, &single, &go, &go, rust, rust))
            }
            Outcome::Different(_) => {
                let Some(reason) = reason(&single, &go, rust) else {
                    return Ok(None);
                };
                let Some(candidate) = rewritten(&single) else {
                    return Ok(None);
                };
                let cf = evaluate(Translator::ConfigSave, &candidate, &run_go(&candidate)?);
                let Ok(cf_go) = serde_json::from_str::<Value>(&cf.go_output) else {
                    return Ok(None);
                };
                let Ok(cf_rust) = &cf.rust_output else {
                    return Ok(None);
                };
                if !proves(
                    &single,
                    &candidate,
                    &go,
                    &cf_go,
                    matches!(cf.outcome, Outcome::Identical | Outcome::Equivalent(_)),
                    rust,
                    cf_rust,
                ) {
                    return Ok(None);
                }
                protected = true;
                deviations.extend(crate::proof_deviations(reason, &cf.outcome));
                let proof = evidence(&single, &candidate, &go, &cf_go, rust, cf_rust);
                (cf_go, proof)
            }
        };
        if certified_go.as_object().is_none_or(|o| o.len() != 1)
            || certified_go["files"]
                .as_array()
                .is_none_or(|a| a.len() != 1)
        {
            return Ok(None);
        }
        current = certified_go["files"][0].clone();
        proofs.push(proof);
    }
    Ok(protected.then(|| StepwiseProof {
        deviations,
        evidence: json!({"case":case.name,"options":case.options,
        "original_go":original_go,"rust_original":rust_original,"stepwise":proofs}),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translator::Translator;
    use crate::upstream::GoResult;

    #[test]
    fn stepwise_replay_keeps_prior_raw_write_and_requires_every_proof() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-stepwise-counterfactual.json"
        ))
        .unwrap();
        let f = &fixture["case"];
        let case = Case::new("stepwise", "", "").with_options(f["options"].clone());
        let rust = Translator::ConfigSave.run_rust(&case).unwrap();
        assert_eq!(rust, f["open_ferry_output"]);
        for mutation in [
            "none",
            "go-fails-again",
            "outside-byte",
            "missing-step",
            "wrong-original-snapshot",
            "wrong-rust-snapshot",
        ] {
            let mut original = f["upstream_output"].clone();
            let mut rust = rust.clone();
            let mut case = case.clone();
            if mutation == "missing-step" {
                case.options["steps"].as_array_mut().unwrap().pop();
            }
            if mutation == "wrong-original-snapshot" {
                original["files"][0] = "changed".into();
            }
            if mutation == "wrong-rust-snapshot" {
                rust["files"][0] = "changed".into();
            }
            let result = stepwise(&case, &original, &rust, |single| {
                let row = fixture["steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| {
                        row["options"] == single.options
                            || row["rewritten_options"] == single.options
                    })
                    .expect("each replay uses exactly the independently probed input");
                let rewritten = row["options"] != single.options;
                let mut go = row[if rewritten {
                    "counterfactual_go"
                } else {
                    "original_go"
                }]
                .clone();
                if rewritten && mutation == "go-fails-again" {
                    go["error"] = "yaml: still bad".into();
                }
                if rewritten && mutation == "outside-byte" {
                    go["files"][0] =
                        format!("{}# unrelated\n", go["files"][0].as_str().unwrap()).into();
                }
                Ok(GoResult::Output(go.to_string().into_bytes()))
            })
            .unwrap();
            assert_eq!(result.is_some(), mutation == "none", "{mutation}");
            if let Some(result) = result {
                let deviations = result.deviations;
                let proof = result.evidence;
                assert!(deviations.contains(&Deviation::UpstreamBlockScalarCounterfactual));
                assert_eq!(proof["original_go"], f["upstream_output"]);
                assert_eq!(proof["stepwise"].as_array().unwrap().len(), 4);
                assert_eq!(
                    proof["stepwise"][0]["options"],
                    proof["stepwise"][0]["rewritten_options"]
                );
            }
        }
    }

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
        let writes: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-write-body-counterfactual.json"
        ))
        .unwrap();
        let archived: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-archived-scalar-counterfactual.json"
        ))
        .unwrap();
        for fixture in fixtures
            .as_array()
            .unwrap()
            .iter()
            .chain(pending.as_array().unwrap())
            .chain(writes.as_array().unwrap())
            .chain(archived.as_array().unwrap())
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
            if let Some(index) = candidate.options["steps"]
                .as_array()
                .unwrap()
                .iter()
                .position(|step| step["op"] == "write")
            {
                let mut changed_body = candidate.clone();
                changed_body.options["steps"][index]["body"] = format!(
                    "{}\n",
                    changed_body.options["steps"][index]["body"]
                        .as_str()
                        .unwrap()
                )
                .into();
                assert!(!proves(
                    &case,
                    &changed_body,
                    &fixture["original_go"],
                    &fixture["counterfactual_go"],
                    matching,
                    &rust_original,
                    &rust_rewritten
                ));
            }
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

    #[test]
    fn archived_scalar_loss_keeps_unrelated_bytes_values_and_comments_different() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-archived-scalar-counterfactual.json"
        ))
        .unwrap();
        let f = &fixtures[0];
        let case = Case::new("archive", "", "").with_options(f["options"].clone());
        let go = &f["original_go"];
        let rust = &f["open_ferry_output"];
        assert_eq!(
            reason(&case, go, rust),
            Some(Deviation::UpstreamArchivedScalarCounterfactual)
        );
        for (from, to) in [
            ("management:", "changed-management:"),
            ("#c138", "#other"),
            ("first", "changed-value"),
            ("# c137", "# changed-head"),
        ] {
            let mut changed = go.clone();
            changed["files"][1] = changed["files"][1]
                .as_str()
                .unwrap()
                .replace(from, to)
                .into();
            assert!(changed != *go, "negative must change a byte: {from}");
            assert_eq!(reason(&case, &changed, rust), None, "{from}");
        }
        let mut changed = rust.clone();
        changed["files"][1] = changed["files"][1]
            .as_str()
            .unwrap()
            .replace("#c138\n", "")
            .into();
        assert_eq!(reason(&case, go, &changed), None);
        let mut changed = case.clone();
        changed.options["steps"][1]["body"] = changed.options["steps"][1]["body"]
            .as_str()
            .unwrap()
            .replace("#c138", "#other")
            .into();
        assert_eq!(reason(&changed, go, rust), None);
        assert_eq!(reason(&case, rust, rust), None);
    }
}
