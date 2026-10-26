//! First-failure diagnostics; these do not grant proof exceptions.
use super::*;
use open_ferry_core::config::AnyValue;

fn at(check: usize, index: usize, mut detail: Value) -> Value {
    let object = detail.as_object_mut().unwrap();
    object.insert("check".into(), check.into());
    object.insert("step".into(), index.into());
    object.insert("file".into(), index.into());
    detail
}
fn offset(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b)
        .position(|(a, b)| a != b)
        .unwrap_or(a.len().min(b.len()))
}
fn snapshot_failure(check: usize, expected: &Value, actual: &Value) -> Value {
    let (Some(a), Some(b)) = (expected["files"].as_array(), actual["files"].as_array()) else {
        return at(check, 0, json!({"reason":"snapshot_shape"}));
    };
    for i in 0..a.len().max(b.len()) {
        if a.get(i) == b.get(i) {
            continue;
        }
        return at(
            check,
            i,
            match (
                a.get(i).and_then(Value::as_str),
                b.get(i).and_then(Value::as_str),
            ) {
                (Some(a), Some(b)) => {
                    json!({"reason":"snapshot_bytes","offset":offset(a.as_bytes(),b.as_bytes())})
                }
                _ => json!({"reason":"missing_snapshot"}),
            },
        );
    }
    at(
        check,
        a.len()
            .saturating_sub(usize::from(expected.get("unloadable").is_some())),
        json!({"reason":"failure_boundary"}),
    )
}
fn first_value(a: &AnyValue, b: &AnyValue, path: &mut Vec<Value>) -> bool {
    if a == b {
        return false;
    }
    match (a, b) {
        (AnyValue::Map(a), AnyValue::Map(b)) => {
            let keys: BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys {
                if a.get(key) == b.get(key) {
                    continue;
                }
                path.push(json!(key));
                if let (Some(a), Some(b)) = (a.get(key), b.get(key)) {
                    first_value(a, b, path);
                }
                break;
            }
        }
        (AnyValue::Seq(a), AnyValue::Seq(b)) => {
            for i in 0..a.len().max(b.len()) {
                if a.get(i) == b.get(i) {
                    continue;
                }
                path.push(json!(i));
                if let (Some(a), Some(b)) = (a.get(i), b.get(i)) {
                    first_value(a, b, path);
                }
                break;
            }
        }
        _ => {}
    }
    true
}
fn first_opaque(value: &AnyValue, path: &mut Vec<Value>) -> bool {
    match value {
        AnyValue::AnyMap => return true,
        AnyValue::Map(values) => {
            for (key, value) in values {
                path.push(json!(key));
                if first_opaque(value, path) {
                    return true;
                }
                path.pop();
            }
        }
        AnyValue::Seq(values) => {
            for (i, value) in values.iter().enumerate() {
                path.push(json!(i));
                if first_opaque(value, path) {
                    return true;
                }
                path.pop();
            }
        }
        _ => {}
    }
    false
}
fn value_failure(case: &Case, protected: &Value, expectation: &Value) -> Value {
    let (Some(r), Some(e)) = (
        protected["files"].as_array(),
        expectation["files"].as_array(),
    ) else {
        return at(2, 0, json!({"reason":"value_snapshot_shape"}));
    };
    let length = case.options["steps"].as_array().map_or(0, Vec::len);
    for i in 0..length.max(r.len()).max(e.len()) {
        let (Some(r), Some(e)) = (
            r.get(i).and_then(Value::as_str),
            e.get(i).and_then(Value::as_str),
        ) else {
            return at(2, i, json!({"reason":"missing_value_snapshot"}));
        };
        if Config::load_bytes(r.as_bytes()).is_err() {
            return at(2, i, json!({"reason":"protected_unloadable"}));
        }
        if Config::load_bytes(e.as_bytes()).is_err() {
            return at(2, i, json!({"reason":"oracle_unloadable"}));
        }
        if core::has_binary_values(r) == Some(true) || core::has_binary_values(e) == Some(true) {
            return at(2, i, json!({"reason":"unsupported_binary_values"}));
        }
        if !core::single_document(r) || !core::single_document(e) {
            return at(2, i, json!({"reason":"multiple_documents"}));
        }
        let (Ok(r), Ok(e)) = (AnyValue::parse_yaml(r), AnyValue::parse_yaml(e)) else {
            return at(2, i, json!({"reason":"value_parse"}));
        };
        let mut path = vec![];
        if first_opaque(&r, &mut path) || first_opaque(&e, &mut path) {
            return at(2, i, json!({"reason":"unsupported_value_path","path":path}));
        }
        if first_value(&r, &e, &mut path) {
            return at(2, i, json!({"reason":"value_path","path":path}));
        }
    }
    at(2, 0, json!({"reason":"value_wrapper"}))
}

/// Mirrors check 3, returning its first failure and only the named rules
/// actually required. An invalid ablated emission never supplies owners.
pub(super) fn comments(
    case: &Case,
    protected: &Value,
    replay: &Replay,
) -> Result<BTreeSet<&'static str>, Value> {
    let Some(mut source) = case.options["file"].as_str() else {
        return Err(at(3, 0, json!({"reason":"source_shape"})));
    };
    let (Some(steps), Some(rfiles), Some(gfiles)) = (
        case.options["steps"].as_array(),
        protected["files"].as_array(),
        replay.output["files"].as_array(),
    ) else {
        return Err(at(3, 0, json!({"reason":"comment_snapshot_shape"})));
    };
    let mut rules = BTreeSet::new();
    for (i, (step, r)) in steps.iter().zip(rfiles).enumerate() {
        if step["op"] == "write" {
            source = step["body"]
                .as_str()
                .ok_or_else(|| at(3, i, json!({"reason":"write_body_shape"})))?;
        }
        let r = r
            .as_str()
            .ok_or_else(|| at(3, i, json!({"reason":"protected_snapshot_shape"})))?;
        let g = gfiles.get(i).and_then(Value::as_str);
        let analysis = if let Some(g) = g.filter(|g| Config::load_bytes(g.as_bytes()).is_ok()) {
            ownership::analyze(case, i, source, Some(g), r, &[], true)
        } else if let Some(trace) = replay.traces.get(i) {
            let bytes = if let Some(g) = g {
                g.as_bytes()
            } else {
                &trace
                    .rejected
                    .last()
                    .ok_or_else(|| at(3, i, json!({"reason":"missing_rejected_emission"})))?
                    .bytes
            };
            let written = trace
                .emissions
                .iter()
                .rev()
                .find_map(|e| core::ablation::bind_comments(e, bytes))
                .ok_or_else(|| at(3, i, json!({"reason":"comment_trace_binding"})))?;
            let written = written
                .into_iter()
                .map(|c| String::from_utf8(c.bytes))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| at(3, i, json!({"reason":"comment_trace_utf8"})))?;
            ownership::analyze(case, i, source, None, r, &written, true)
        } else if g.is_none() {
            ownership::analyze(case, i, source, None, r, &[], false)
        } else {
            return Err(at(3, i, json!({"reason":"missing_comment_trace"})));
        };
        if let Some(failure) = analysis.failure {
            return Err(at(3, i, failure));
        }
        // Every accepted ownership rule must also account for added layout.
        // Prefer real Go bytes, including a bound unloadable attempt. Without
        // an observed Go snapshot, only the source can supply a counterpart.
        let layout_go = g.or_else(|| {
            replay
                .traces
                .get(i)?
                .rejected
                .last()
                .and_then(|attempt| std::str::from_utf8(&attempt.bytes).ok())
        });
        if !core::source_owner_layout(source, layout_go.unwrap_or(source), r) {
            return Err(at(3, i, json!({"reason":"physical_layout"})));
        }
        rules.extend(analysis.rules);
        source = r;
    }
    Ok(rules)
}
pub(super) fn failures(proof: Proof<'_>, checks: [bool; 5]) -> Vec<Value> {
    let mut out = vec![];
    if !checks[0] {
        let mut failure = snapshot_failure(1, proof.go, &proof.ablated.output);
        if failure["reason"] == "failure_boundary" && proof.go.get("error").is_some() {
            let i = proof.go["files"].as_array().map_or(0, Vec::len);
            let expected = proof
                .go_trace
                .and_then(|t| t.get(i))
                .and_then(Value::as_array)
                .and_then(|v| v.last())
                .and_then(Value::as_str);
            let actual = proof.ablated.traces.get(i).and_then(|t| t.rejected.last());
            if let (Some(a), Some(b)) = (expected, actual)
                && a.as_bytes() != b.bytes
            {
                failure = at(
                    1,
                    i,
                    json!({"reason":"rejected_bytes","offset":offset(a.as_bytes(),&b.bytes)}),
                );
            }
        }
        out.push(failure);
    }
    if !checks[1] {
        out.push(if !documents_right(proof) {
            at(2, 0, json!({"reason":"multiple_documents"}))
        } else {
            value_failure(proof.case, proof.protected, proof.expectation)
        });
    }
    if !checks[2] {
        out.push(if !documents_right(proof) {
            at(3, 0, json!({"reason":"multiple_documents"}))
        } else {
            comments(proof.case, proof.protected, proof.ablated)
                .err()
                .unwrap_or_else(|| at(3, 0, json!({"reason":"comment_check_inconsistent"})))
        });
    }
    if !checks[3] {
        let mut failure = at(4, 0, json!({"reason":"faithful_snapshot_shape"}));
        if let (Some(r), Some(g)) = (
            proof.protected["files"].as_array(),
            proof.ablated.output["files"].as_array(),
        ) {
            for (i, (r, g)) in r.iter().zip(g).enumerate() {
                let (Some(r), Some(g)) = (r.as_str(), g.as_str()) else {
                    failure = at(4, i, json!({"reason":"faithful_snapshot_shape"}));
                    break;
                };
                if Config::load_bytes(g.as_bytes()).is_err() {
                    continue;
                }
                let (a, b) = (
                    core::faithful_without_comments(r),
                    core::faithful_without_comments(g),
                );
                if a.is_none() || b.is_none() || a != b {
                    failure = at(
                        4,
                        i,
                        match (a, b) {
                            (Some(a), Some(b)) => {
                                json!({"reason":"faithful_bytes","offset":offset(a.as_bytes(),b.as_bytes())})
                            }
                            _ => json!({"reason":"faithful_reparse"}),
                        },
                    );
                    break;
                }
            }
        }
        if !documents_right(proof) {
            failure = at(4, 0, json!({"reason":"multiple_documents"}));
        }
        out.push(failure);
    }
    if !checks[4] {
        out.push(at(
            5,
            0,
            json!({"reason":"nonminimal_subset","mask":proof.mask}),
        ));
    }
    out
}
pub(super) fn no_reproduction(go: &Value, replay: &Replay, go_trace: Option<&Value>) -> Value {
    let empty = Case::new("diagnostic", "", "");
    failures(
        Proof {
            case: &empty,
            go,
            protected: &Value::Null,
            ablated: replay,
            go_trace,
            expectation: &Value::Null,
            mask: 0,
            matches: &[],
        },
        [false, true, true, true, true],
    )
    .remove(0)
}

pub(super) fn comment_free_failure(case: &Case) -> Value {
    let diagnose = |text: &str, index, stage| {
        if !core::single_document(text) {
            return at(
                2,
                index,
                json!({"reason":"multiple_documents","stage":stage}),
            );
        }
        if core::has_binary_values(text) == Some(true) {
            return at(
                2,
                index,
                json!({"reason":"unsupported_binary_values","stage":stage}),
            );
        }
        let mut path = Vec::new();
        if let Some(faithful) = core::faithful_without_comments(text)
            && let (Ok(source), Ok(clean)) =
                (AnyValue::parse_yaml(text), AnyValue::parse_yaml(&faithful))
        {
            if first_opaque(&source, &mut path) || first_opaque(&clean, &mut path) {
                return at(
                    2,
                    index,
                    json!({"reason":"unsupported_value_path","stage":stage,"path":path}),
                );
            }
            if first_value(&source, &clean, &mut path) {
                return at(
                    2,
                    index,
                    json!({"reason":"comment_free_value_path","stage":stage,"path":path}),
                );
            }
        }
        at(
            2,
            index,
            json!({"reason":"comment_free_input_unavailable","stage":stage}),
        )
    };
    if let Some(source) = case.options["file"].as_str()
        && core::comment_free_source(source).is_none()
    {
        return diagnose(source, 0, "initial");
    }
    if let Some(steps) = case.options["steps"].as_array() {
        for (i, step) in steps.iter().enumerate() {
            for field in ["body", "config"] {
                if let Some(text) = step[field].as_str()
                    && core::comment_free_source(text).is_none()
                {
                    return diagnose(text, i, field);
                }
            }
        }
    }
    at(2, 0, json!({"reason":"comment_free_case_unavailable"}))
}
