//! Per-step source identities from the independent pinned Go tree oracle.
use super::*;

pub(super) fn bind(
    case: &Case,
    protected: &Value,
    run_go: &mut impl FnMut(&Case) -> Result<GoResult, Box<dyn Error>>,
) -> Result<Case, Box<dyn Error>> {
    let mut bound = case.clone();
    let mut evidence = Vec::new();
    let Some(mut source) = case.options["file"].as_str() else {
        return Ok(bound);
    };
    let (Some(steps), Some(files)) = (
        case.options["steps"].as_array(),
        protected["files"].as_array(),
    ) else {
        return Ok(bound);
    };
    for (step, file) in steps.iter().zip(files) {
        if step["op"] == "write" {
            source = step["body"].as_str().unwrap_or_default();
        }
        let mut request = case.clone();
        request.options = json!({"_ownership_request":{"source":source,"operation":step,"source_owner":file.as_str().is_some_and(|r|core::source_owner_candidate(source,r)),"protected":file}});
        let mut witness = output(run_go(&request)?).unwrap_or(Value::Null);
        if let Some(frame) = witness.get_mut("retained_source_owner")
            && let Some(protected) = file.as_str()
        {
            frame["protected"] = protected.into();
            frame["repeat_input"] = protected.into();
            frame["repeat_operation"] = step.clone();
            let repeat = |operation: &Value| {
                if Config::load_bytes(protected.as_bytes()).is_err() {
                    return None;
                }
                let dir = TempDir::new().ok()?;
                let path = dir.0.join("config.yaml");
                fs::write(&path, protected).ok()?;
                if run_step(&path, operation).ok()?.is_some() {
                    return None;
                }
                fs::read_to_string(path).ok()
            };
            let repeated = repeat(step);
            frame["repeat_ok"] = repeated.is_some().into();
            frame["repeated"] = json!(repeated);
            // A raw write must also be a normalization fixed point when its
            // output becomes the next body, in addition to repeating the exact
            // original operation on that output.
            if step["op"] == "write" {
                let mut normalize = step.clone();
                normalize["body"] = protected.into();
                frame["normalized"] = json!(repeat(&normalize));
            }
        }
        evidence.push(witness);
        let Some(next) = file.as_str() else { break };
        source = next;
    }
    bound.options["_ownership_evidence"] = json!(evidence);
    Ok(bound)
}

pub(super) fn analyze(
    case: &Case,
    index: usize,
    source: &str,
    go: Option<&str>,
    protected: &str,
    written: &[String],
    observed: bool,
) -> core::CommentOwnership {
    let strict = if let Some(evidence) = case.options["_ownership_evidence"].get(index) {
        core::comment_ownership_with_provenance(
            source,
            go,
            protected,
            written,
            &case.options["steps"][index],
            evidence,
            observed && go.is_none(),
        )
    } else if go.is_none() && !observed {
        core::unobserved_comment_ownership(source, protected)
    } else {
        core::comment_ownership_analysis(source, go, protected, written)
    };
    if strict.accepted() {
        return strict;
    }
    if source_owner(case, index, source, protected).is_some_and(|proof| proof.accepted()) {
        let mut rules = vec!["retained-source-owner"];
        if case.options["_ownership_evidence"][index]["archives"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            rules.push("upstream-generated-archive")
        }
        core::CommentOwnership {
            failure: None,
            rules,
        }
    } else {
        strict
    }
}

pub(super) fn source_owner(
    case: &Case,
    index: usize,
    source: &str,
    protected: &str,
) -> Option<core::SourceOwnerProof> {
    let evidence = case.options["_ownership_evidence"].get(index)?;
    Some(core::retained_source_owner(
        source,
        protected,
        &case.options["steps"][index],
        evidence,
    ))
}

pub(super) fn source_owner_evidence(case: &Case, protected: &Value) -> Vec<Value> {
    let Some(mut source) = case.options["file"].as_str() else {
        return vec![];
    };
    let (Some(steps), Some(files)) = (
        case.options["steps"].as_array(),
        protected["files"].as_array(),
    ) else {
        return vec![];
    };
    let mut result = vec![];
    for (index, (step, file)) in steps.iter().zip(files).enumerate() {
        if step["op"] == "write" {
            source = step["body"].as_str().unwrap_or_default()
        }
        let Some(file) = file.as_str() else { break };
        if let Some(proof) = source_owner(case, index, source, file) {
            result.push(json!({"step":index,"rule":"retained-source-owner","checks":proof.conditions,"accepted":proof.accepted()}));
        }
        source = file;
    }
    result
}
