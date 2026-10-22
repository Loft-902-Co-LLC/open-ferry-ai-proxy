//! Complete, minimal protection ablations. No source/output repair is accepted.
use super::{TempDir, run_step};
use crate::{cases::Case, compare::Deviation, upstream::GoResult};
use core::ablation::{Protection, Trace, with_disabled, with_trace};
use open_ferry_core::config::{Config, save::parity as core};
use serde_json::{Value, json};
use std::{collections::BTreeSet, error::Error, fs};

pub const ALL: [Protection; 6] = [
    Protection::EmptyMappingPendingComment,
    Protection::CanonicalVersionComment,
    Protection::DroppedSingleKeyComment,
    Protection::InlineEmptyNullComment,
    Protection::EffectiveBlockScalarComment,
    Protection::BlockScalarCommentIndentation,
];

#[derive(Clone, Debug)]
pub struct Replay {
    pub output: Value,
    pub traces: Vec<Trace>,
}

/// Invalid candidates are observed at the existing refusal boundary, never
/// written. Their exact bytes form the diagnostic attempted snapshot.
pub fn replay(case: &Case, mask: u8) -> Option<Replay> {
    let dir = TempDir::new().ok()?;
    let path = dir.0.join("config.yaml");
    fs::write(&path, case.options["file"].as_str()?).ok()?;
    let disabled: Vec<_> = ALL
        .into_iter()
        .enumerate()
        .filter_map(|(i, p)| (mask & (1 << i) != 0).then_some(p))
        .collect();
    with_disabled(&disabled, || {
        let mut files = Vec::new();
        let mut traces = Vec::new();
        for step in case.options["steps"].as_array()? {
            let (result, trace) = with_trace(|| run_step(&path, step));
            let result = result.ok()?;
            if let Some(error) = result {
                let attempted = trace.rejected.last().filter(|r| r.stage == "write");
                let output = if let Some(attempted) = attempted {
                    files.push(std::str::from_utf8(&attempted.bytes).ok()?.to_owned());
                    json!({"files":files,"unloadable":error})
                } else {
                    json!({"files":files,"error":error})
                };
                traces.push(trace);
                return Some(Replay { output, traces });
            }
            files.push(fs::read_to_string(&path).ok()?);
            traces.push(trace);
        }
        Some(Replay {
            output: json!({"files":files}),
            traces,
        })
    })
}

fn output(result: GoResult) -> Option<Value> {
    match result {
        GoResult::Output(bytes) => serde_json::from_slice(&bytes).ok(),
        GoResult::Panic(_) => None,
    }
}

/// Independent Go trace must reproduce the entire uninstrumented result.
fn trace_output<'a>(raw: &Value, traced: &'a Value) -> Option<&'a Value> {
    let mut comparable = traced.clone();
    let trace = comparable.as_object_mut()?.remove("rejected")?;
    if &comparable != raw {
        return None;
    }
    // Return the original reference, after validating the wrapper.
    let _ = trace;
    traced.get("rejected")
}

/// Check 1, including exact successful prefix, failure step and rejected bytes.
fn reproduces(go: &Value, replay: &Replay, go_trace: Option<&Value>) -> bool {
    let (Some(g), Some(r)) = (go["files"].as_array(), replay.output["files"].as_array()) else {
        return false;
    };
    if g != r {
        return false;
    }
    if go.get("unloadable").is_some() {
        let Some(data) = g.last().and_then(Value::as_str) else {
            return false;
        };
        return replay.output["unloadable"].is_string()
            && replay.traces.len() == g.len()
            && Config::load_bytes(data.as_bytes()).is_err()
            && replay
                .traces
                .last()
                .and_then(|t| t.rejected.last())
                .is_some_and(|bad| bad.stage == "write" && bad.bytes == data.as_bytes());
    }
    if let Some(error) = go.get("error") {
        let Some(trace) = go_trace else {
            return false;
        };
        let index = g.len();
        let Some(bytes) = trace
            .get(index)
            .and_then(Value::as_array)
            .and_then(|v| v.last())
            .and_then(Value::as_str)
        else {
            return false;
        };
        return replay.output.get("error") == Some(error)
            && replay.traces.len() == index + 1
            && replay.traces[index]
                .rejected
                .last()
                .is_some_and(|bad| bad.bytes == bytes.as_bytes())
            && core::faithful_without_comments(bytes).is_none();
    }
    &replay.output == go
}

fn comment_free(case: &Case) -> Option<Case> {
    let mut clean = case.clone();
    clean.options["file"] = core::comment_free_source(case.options["file"].as_str()?)?.into();
    for step in clean.options["steps"].as_array_mut()? {
        for key in ["body", "config"] {
            if let Some(text) = step.get(key).and_then(Value::as_str) {
                step[key] = core::comment_free_source(text)?.into();
            }
        }
    }
    Some(clean)
}

fn values_right(case: &Case, protected: &Value, expectation: &Value) -> bool {
    let Some(steps) = case.options["steps"].as_array() else {
        return false;
    };
    if protected.as_object().is_none_or(|o| o.len() != 1)
        || expectation.as_object().is_none_or(|o| o.len() != 1)
    {
        return false;
    }
    let (Some(r), Some(e)) = (
        protected["files"].as_array(),
        expectation["files"].as_array(),
    ) else {
        return false;
    };
    r.len() == steps.len()
        && r.len() == e.len()
        && r.iter().zip(e).all(|(r, e)| {
            let (Some(r), Some(e)) = (r.as_str(), e.as_str()) else {
                return false;
            };
            Config::load_bytes(r.as_bytes()).is_ok()
                && Config::load_bytes(e.as_bytes()).is_ok()
                && core::same_values(r, e) == Some(true)
        })
}

/// Check 3: parsed owners on loadable output; strict source owners plus an
/// exact, non-overlapping write_comment trace on an unloadable attempted file.
fn comments_right(case: &Case, protected: &Value, replay: &Replay) -> bool {
    let Some(mut source) = case.options["file"].as_str() else {
        return false;
    };
    let (Some(steps), Some(rfiles), Some(gfiles)) = (
        case.options["steps"].as_array(),
        protected["files"].as_array(),
        replay.output["files"].as_array(),
    ) else {
        return false;
    };
    for (i, (step, r)) in steps.iter().zip(rfiles).enumerate() {
        if step["op"] == "write" {
            let Some(body) = step["body"].as_str() else {
                return false;
            };
            source = body;
        }
        let Some(r) = r.as_str() else {
            return false;
        };
        if let Some(g) = gfiles.get(i).and_then(Value::as_str) {
            if Config::load_bytes(g.as_bytes()).is_ok() {
                if !core::retained_comment_ownership(source, g, r) {
                    return false;
                }
            } else {
                let Some(trace) = replay.traces.get(i) else {
                    return false;
                };
                let Some(written) = trace
                    .emissions
                    .iter()
                    .rev()
                    .find_map(|e| core::ablation::bind_comments(e, g.as_bytes()))
                else {
                    return false;
                };
                let Some(comments) = written
                    .iter()
                    .map(|c| String::from_utf8(c.bytes.clone()).ok())
                    .collect::<Option<Vec<_>>>()
                else {
                    return false;
                };
                if !core::retained_unloadable_comments(source, r, &comments) {
                    return false;
                }
            }
        } else {
            // Go failed before writing this step. Its last rejected emission
            // supplies multiplicity only, and never a placement allowance.
            let Some(trace) = replay.traces.get(i) else {
                if !core::retained_unloadable_comments(source, r, &[]) {
                    return false;
                }
                source = r;
                continue;
            };
            let Some(rejected) = trace.rejected.last() else {
                return false;
            };
            let Some(written) = trace
                .emissions
                .iter()
                .rev()
                .find_map(|e| core::ablation::bind_comments(e, &rejected.bytes))
            else {
                return false;
            };
            let Some(comments) = written
                .iter()
                .map(|c| String::from_utf8(c.bytes.clone()).ok())
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            if !core::retained_unloadable_comments(source, r, &comments) {
                return false;
            }
        }
        source = r;
    }
    true
}

/// These owner/budget checks depend only on the fixed source, protected
/// snapshots and exact Go bytes. No alternative reproducing subset can change
/// their result; traces supply counts only on the unloadable steps.
fn placement_right(case: &Case, protected: &Value, replay: &Replay) -> bool {
    let Some(mut source) = case.options["file"].as_str() else {
        return false;
    };
    let (Some(steps), Some(files), Some(gfiles)) = (
        case.options["steps"].as_array(),
        protected["files"].as_array(),
        replay.output["files"].as_array(),
    ) else {
        return false;
    };
    for (i, (step, r)) in steps.iter().zip(files).enumerate() {
        if step["op"] == "write" {
            let Some(body) = step["body"].as_str() else {
                return false;
            };
            source = body;
        }
        let Some(r) = r.as_str() else {
            return false;
        };
        let good = if let Some(g) = gfiles
            .get(i)
            .and_then(Value::as_str)
            .filter(|g| Config::load_bytes(g.as_bytes()).is_ok())
        {
            core::retained_comment_ownership(source, g, r)
        } else {
            core::retained_unloadable_comments(source, r, &[])
        };
        if !good {
            return false;
        }
        source = r;
    }
    true
}

fn nothing_else_changed(protected: &Value, replay: &Replay) -> bool {
    let (Some(r), Some(g)) = (
        protected["files"].as_array(),
        replay.output["files"].as_array(),
    ) else {
        return false;
    };
    g.iter().zip(r).all(|(g, r)| {
        let (Some(g), Some(r)) = (g.as_str(), r.as_str()) else {
            return false;
        };
        if Config::load_bytes(g.as_bytes()).is_err() {
            return true;
        }
        core::faithful_without_comments(g)
            .zip(core::faithful_without_comments(r))
            .is_some_and(|(g, r)| g == r)
    })
}

fn minimal(mask: u8, matches: &[(u8, bool)]) -> bool {
    mask != 0
        && !matches
            .iter()
            .any(|(m, yes)| *yes && *m != mask && *m & mask == *m)
}

fn deviations(mask: u8) -> BTreeSet<Deviation> {
    [
        Deviation::RetainedUpstreamEmptyMappingComment,
        Deviation::RetainedCanonicalVersionComment,
        Deviation::RetainedDroppedKeyComment,
        Deviation::RetainedInlineEmptyNullComment,
        Deviation::RetainedEffectiveBlockScalarComment,
        Deviation::RetainedBlockScalarCommentIndentation,
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(i, d)| (mask & (1 << i) != 0).then_some(d))
    .collect()
}

struct Proof<'a> {
    case: &'a Case,
    go: &'a Value,
    protected: &'a Value,
    ablated: &'a Replay,
    go_trace: Option<&'a Value>,
    expectation: &'a Value,
    mask: u8,
    matches: &'a [(u8, bool)],
}

fn checks(proof: Proof<'_>) -> [bool; 5] {
    let Proof {
        case,
        go,
        protected,
        ablated,
        go_trace,
        expectation,
        mask,
        matches,
    } = proof;
    [
        reproduces(go, ablated, go_trace),
        values_right(case, protected, expectation),
        comments_right(case, protected, ablated),
        nothing_else_changed(protected, ablated),
        minimal(mask, matches),
    ]
}

fn accepted(checks: [bool; 5]) -> bool {
    checks.into_iter().all(|c| c)
}

pub struct Attempt {
    pub deviations: Option<BTreeSet<Deviation>>,
    pub evidence: Value,
}

/// An otherwise Different case receives named deviations only after all checks.
pub fn certify(
    case: &Case,
    go: &Value,
    protected: &Value,
    mut run_go: impl FnMut(&Case) -> Result<GoResult, Box<dyn Error>>,
) -> Result<Attempt, Box<dyn Error>> {
    let refused = |reason: &str| Attempt {
        deviations: None,
        evidence: json!({"kind":"uncertified-protection-ablation","case":case.name,"reason":reason}),
    };
    if replay(case, 0).is_none_or(|r| r.output != *protected) {
        return Ok(refused("protected replay mismatch"));
    }
    let traced = if go.get("error").is_some() {
        let mut c = case.clone();
        c.options["_ablation_trace"] = true.into();
        output(run_go(&c)?)
    } else {
        None
    };
    let go_trace = traced.as_ref().and_then(|t| trace_output(go, t));
    if go.get("error").is_some() && go_trace.is_none() {
        return Ok(refused("Go trace mismatch"));
    }
    let mut masks: Vec<u8> = (1..(1 << ALL.len())).collect();
    masks.sort_by_key(|m| m.count_ones());
    let mut matches = Vec::new();
    let mut dominated = Vec::new();
    let mut trials = Vec::new();
    let mut oracle = None;
    let mut last = None;
    for mask in masks {
        // A proper reproducing subset makes this superset non-minimal,
        // regardless of its bytes. Record that exclusion explicitly.
        if !minimal(mask, &matches) {
            dominated.push(mask);
            continue;
        }
        let Some(ablated) = replay(case, mask) else {
            matches.push((mask, false));
            continue;
        };
        let yes = reproduces(go, &ablated, go_trace);
        matches.push((mask, yes));
        if !yes {
            continue;
        }
        if oracle.is_none() {
            let Some(clean) = comment_free(case) else {
                return Ok(refused("check 2: comment-free source changes values"));
            };
            let Some(expectation) = output(run_go(&clean)?) else {
                return Ok(refused("check 2: Go oracle panicked"));
            };
            oracle = Some((clean, expectation));
        }
        let (clean, expectation) = oracle.as_ref().unwrap();
        let checks = checks(Proof {
            case,
            go,
            protected,
            ablated: &ablated,
            go_trace,
            expectation,
            mask,
            matches: &matches,
        });
        trials.push(json!({"mask":mask,"checks":checks}));
        let accepted = accepted(checks);
        let evidence = json!({"kind":if accepted {"certified-protection-ablation"} else {"uncertified-protection-ablation"},"case":case.name,"options":case.options,"original_go":go,"go_parse_trace":traced,"protected_rust":protected,"ablated_rust":ablated.output,"ablated_traces":ablated.traces,"comment_free_options":clean.options,"comment_free_go":expectation,"subset":ALL.iter().enumerate().filter(|(i,_)|mask&(1<<i)!=0).map(|(_,p)|format!("{p:?}")).collect::<Vec<_>>(),"subset_matches":matches,"nonminimal_supersets":dominated,"trials":trials,"checks":checks});
        if accepted {
            return Ok(Attempt {
                deviations: Some(deviations(mask)),
                evidence,
            });
        }
        let invariant_failure =
            !checks[1] || !checks[3] || !placement_right(case, protected, &ablated);
        last = Some(evidence);
        if invariant_failure {
            // Every reproducing subset has identical Go snapshots; checks 2,
            // 4 and parsed owner/budget checks cannot depend on its switches.
            last.as_mut().unwrap()["remaining_subsets_excluded_by_invariant_check"] = true.into();
            break;
        }
    }
    let Some(mut evidence) = last else {
        return Ok(refused("check 1: no reproducing subset"));
    };
    evidence["subset_matches"] = json!(matches);
    evidence["nonminimal_supersets"] = json!(dominated);
    evidence["trials"] = json!(trials);
    Ok(Attempt {
        deviations: None,
        evidence,
    })
}

#[cfg(test)]
mod tests;
