//! Per-step source identities from the independent pinned Go tree oracle.
use super::*;

thread_local! {
    static PROOFS: std::cell::RefCell<std::collections::BTreeMap<String, [bool; 5]>> = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

pub(super) fn reset_proofs() {
    PROOFS.with_borrow_mut(|cache| cache.clear());
}

fn prove(source: &str, protected: &str, step: &Value, witness: &Value) -> core::SourceOwnerProof {
    let _timing = super::super::metrics::Timer::start(4);
    if std::env::var_os("OPEN_FERRY_PARITY_FULL_SEARCH").is_some() {
        return core::retained_source_owner(source, protected, step, witness);
    }
    // A pure proof over exact source/output/operation/native receipts. Its key
    // includes every input, including negative receipts. Bound memory and reset
    // it at each record; no serialized writer verdict enters this cache.
    let key = serde_json::to_string(&(source, protected, step, witness)).unwrap();
    if let Some(conditions) = PROOFS.with_borrow(|cache| cache.get(&key).copied()) {
        return core::SourceOwnerProof { conditions };
    }
    let proof = core::retained_source_owner(source, protected, step, witness);
    PROOFS.with_borrow_mut(|cache| {
        if cache.len() == 32 {
            cache.clear();
        }
        cache.insert(key, proof.conditions);
    });
    proof
}

fn stable_repeat(frame: &Value, step: &Value, protected: &str) -> bool {
    frame["repeat_input"] == protected
        && frame["repeat_operation"] == *step
        && frame["repeat_ok"] == true
        && frame["repeated"] == protected
        && (step["op"] != "write" || frame["normalized"] == protected)
}

/// A complete source-owner certificate needs no ablation witness: exact Go
/// replays, both parsers, physical layout, data and Rust repeats independently
/// account for every differing snapshot. A failed candidate changes nothing.
pub(super) fn certify_repair(
    case: &Case,
    go: &Value,
    protected: &Value,
    run_go: &mut impl FnMut(&Case) -> Result<GoResult, Box<dyn Error>>,
) -> Result<Option<Attempt>, Box<dyn Error>> {
    let (Some(steps), Some(gfiles), Some(rfiles)) = (
        case.options["steps"].as_array(),
        go["files"].as_array(),
        protected["files"].as_array(),
    ) else {
        return Ok(None);
    };
    if go.as_object().is_none_or(|o| o.len() != 1)
        || protected.as_object().is_none_or(|o| o.len() != 1)
        || steps.len() != gfiles.len()
        || steps.len() != rfiles.len()
        || go == protected
        || !gfiles.iter().zip(rfiles).all(|(g, r)| {
            g.as_str().zip(r.as_str()).is_some_and(|(g, r)| {
                core::single_document(g)
                    && core::single_document(r)
                    && Config::load_bytes(g.as_bytes()).is_ok()
                    && Config::load_bytes(r.as_bytes()).is_ok()
                    && core::faithful_without_comments(g)
                        .zip(core::faithful_without_comments(r))
                        .is_some_and(|(g, r)| g == r)
            })
        })
    {
        return Ok(None);
    }
    // Serialized evidence is never an oracle receipt.
    let mut unbound = case.clone();
    if let Some(options) = unbound.options.as_object_mut() {
        options.remove("_ownership_evidence");
    }
    if replay(&unbound, 0).is_none_or(|r| r.output != *protected) {
        return Ok(None);
    }
    let bound = bind(&unbound, protected, run_go)?;
    let Some(mut source) = bound.options["file"].as_str() else {
        return Ok(None);
    };
    let mut proofs = Vec::new();
    let mut existing = Vec::new();
    let mut deviations = BTreeSet::from([Deviation::RetainedDeferredCommentOwners]);
    for (index, ((step, g), r)) in steps.iter().zip(gfiles).zip(rfiles).enumerate() {
        if step["op"] == "write" {
            let Some(body) = step["body"].as_str() else {
                return Ok(None);
            };
            source = body;
        }
        let (Some(g), Some(r)) = (g.as_str(), r.as_str()) else {
            return Ok(None);
        };
        let exact =
            &bound.options["_ownership_evidence"][index]["retained_source_owner"]["exact_go"];
        let mut single = unbound.clone();
        single.options = json!({"file":source,"steps":[step]});
        let compared = crate::evaluate(
            crate::Translator::ConfigSave,
            &single,
            &GoResult::Output(serde_json::to_vec(exact)?),
        );
        if compared
            .rust_output
            .as_ref()
            .is_ok_and(|output| *output == json!({"files":[r]}))
            && matches!(
                compared.outcome,
                crate::Outcome::Identical | crate::Outcome::Equivalent(_)
            )
        {
            let mut rules = Vec::new();
            if let crate::Outcome::Equivalent(found) = compared.outcome {
                rules = found.iter().map(|d| d.describe()).collect();
                deviations.extend(found);
            }
            existing.push(json!({"step":index,"kind":"existing-comparison","rules":rules}));
            source = r;
            continue;
        }
        if g != r && !inherited_repair(&bound, index, r) {
            let Some(proof) = source_owner(&bound, index, source, r) else {
                return Ok(None);
            };
            if !proof.accepted() {
                return Ok(None);
            }
            proofs.push(json!({"step":index,"checks":proof.conditions,"accepted":true}));
        }
        source = r;
    }
    if proofs.is_empty() {
        return Ok(None);
    }
    if steps
        .iter()
        .zip(gfiles)
        .zip(rfiles)
        .enumerate()
        .any(|(i, ((step, g), r))| {
            g != r
                && !stable_repeat(
                    &bound.options["_ownership_evidence"][i]["retained_source_owner"],
                    step,
                    r.as_str().unwrap(),
                )
        })
    {
        return Ok(None);
    }
    Ok(Some(Attempt {
        deviations: Some(deviations),
        evidence: json!({"kind":"certified-protection-ablation","method":"source-owner-certificate","case":case.name,"options":unbound.options,"original_go":go,"protected_rust":protected,"owner_rules":["retained-source-owner"],"retained_source_owner":proofs,"existing_steps":existing,"source_identities":bound.options["_ownership_evidence"],"failures":[]}),
    }))
}

pub(super) fn bind(
    case: &Case,
    protected: &Value,
    run_go: &mut impl FnMut(&Case) -> Result<GoResult, Box<dyn Error>>,
) -> Result<Case, Box<dyn Error>> {
    let _timing = super::super::metrics::Timer::start(3);
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
        request.options = json!({"_ownership_request":{"source":source,"operation":step,"source_owner":file.as_str().is_some_and(|r|core::source_owner_candidate(source,r)),"foot_separators":file.as_str().is_some_and(|r| !core::source_owner_layout(source,source,r)),"protected":file,"full":std::env::var_os("OPEN_FERRY_PARITY_FULL_SEARCH").is_some()}});
        let mut witness = output(run_go(&request)?).unwrap_or(Value::Null);
        if let Some(frame) = witness.get_mut("retained_source_owner")
            && let Some(protected) = file.as_str()
        {
            frame["protected"] = protected.into();
            if std::env::var_os("OPEN_FERRY_PARITY_FULL_SEARCH").is_none()
                && frame["exact_go"].as_object().is_some_and(|r| r.len() != 1)
            {
                evidence.push(witness);
                source = protected;
                continue;
            }
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
    if inherited_repair(case, index, protected) {
        return core::CommentOwnership {
            failure: None,
            rules: vec!["retained-source-owner"],
        };
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

/// Once a prefix's owner repair is proved, a subsequent byte-identical Go
/// replay from that repaired prefix needs no new owner exception. A raw write
/// starts a new source and cannot inherit a certificate from the old file.
fn inherited_repair(case: &Case, index: usize, protected: &str) -> bool {
    let Some(evidence) = case.options["_ownership_evidence"].as_array() else {
        return false;
    };
    let Some(steps) = case.options["steps"].as_array() else {
        return false;
    };
    if index == 0 || steps[index]["op"] == "write" {
        return false;
    }
    let frame = &evidence[index]["retained_source_owner"];
    if frame["exact_go"] != json!({"files":[protected]})
        || !stable_repeat(frame, &steps[index], protected)
    {
        return false;
    }
    let mut repaired = false;
    for (i, step) in steps.iter().enumerate().take(index) {
        if step["op"] == "write" {
            repaired = false;
        }
        let witness = &evidence[i];
        let Some(source) = witness["source"].as_str() else {
            return false;
        };
        let Some(output) = witness["retained_source_owner"]["protected"].as_str() else {
            return false;
        };
        if prove(source, output, step, witness).accepted() {
            repaired = true;
        } else if !repaired
            || witness["retained_source_owner"]["exact_go"] != json!({"files":[output]})
            || !stable_repeat(&witness["retained_source_owner"], step, output)
        {
            return false;
        }
    }
    repaired
}

#[cfg(test)]
mod repeat_tests {
    use super::*;

    #[test]
    fn inherited_prefix_requires_the_current_operations_repeat() {
        let fixtures: Value =
            serde_json::from_str(include_str!("../testdata/owners-repairs.json")).unwrap();
        let f = &fixtures[0];
        let protected = f["protected"].as_str().unwrap();
        let step = json!({"op":"save","migrate":false});
        let mut case = Case::new("inherited-repeat", "", "").with_options(json!({
            "file":f["source"],"steps":[f["operation"],step],
            "_ownership_evidence":[f["evidence"],{"retained_source_owner":{
                "exact_go":{"files":[protected]},"repeat_input":protected,"repeat_operation":step,"repeat_ok":true,"repeated":protected
            }}]
        }));
        assert!(inherited_repair(&case, 1, protected));
        case.options["_ownership_evidence"][1]["retained_source_owner"]["repeated"] =
            "changed".into();
        assert!(!inherited_repair(&case, 1, protected));
    }
}

pub(super) fn source_owner(
    case: &Case,
    index: usize,
    source: &str,
    protected: &str,
) -> Option<core::SourceOwnerProof> {
    let evidence = case.options["_ownership_evidence"].get(index)?;
    Some(prove(
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
            // These conditions describe a possible proof, not a rule that
            // was needed. Used rule names come from strict-first analyze().
            result
                .push(json!({"step":index,"checks":proof.conditions,"accepted":proof.accepted()}));
        }
        source = file;
    }
    result
}
