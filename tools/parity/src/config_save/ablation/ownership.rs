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
        request.options = json!({"_ownership_request":{"source":source,"operation":step}});
        evidence.push(output(run_go(&request)?).unwrap_or(Value::Null));
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
    if let Some(evidence) = case.options["_ownership_evidence"].get(index) {
        core::comment_ownership_with_provenance(
            source,
            go,
            protected,
            written,
            &case.options["steps"][index],
            evidence,
            observed,
        )
    } else if go.is_none() && !observed {
        core::unobserved_comment_ownership(source, protected)
    } else {
        core::comment_ownership_analysis(source, go, protected, written)
    }
}
