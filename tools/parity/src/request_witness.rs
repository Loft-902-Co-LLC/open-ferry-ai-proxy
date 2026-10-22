//! Bind a complete lexical request span to its translated path and byte position.
use crate::{
    cases::Case,
    raw_json::{self, Raw},
    translator::Translator,
};
use serde_json::{Value, json};

pub fn bound_span(
    translator: Translator,
    case: &Case,
    baseline: &Value,
    path: &str,
    span: &str,
    text: &str,
    position: usize,
) -> bool {
    bound_span_replaying(case, baseline, path, span, text, position, |changed| {
        translator.run_rust(changed)
    })
}

fn bound_span_replaying(
    case: &Case,
    baseline: &Value,
    path: &str,
    span: &str,
    text: &str,
    position: usize,
    mut replay: impl FnMut(&Case) -> Result<Value, String>,
) -> bool {
    if baseline.pointer(path).and_then(Value::as_str) != Some(text) {
        return false;
    }
    let Some(root) = raw_json::parse(&case.request) else {
        return false;
    };
    fn candidates<'a>(n: &'a Raw<'a>, span: &str, out: &mut Vec<&'a Raw<'a>>) {
        if n.text() == span {
            out.push(n);
        }
        match n {
            Raw::Object(fields, _) => {
                for (_, v) in fields {
                    candidates(v, span, out);
                }
            }
            Raw::Array(items, _) => {
                for v in items {
                    candidates(v, span, out);
                }
            }
            Raw::Scalar(_) => {}
        }
    }
    let mut found = Vec::new();
    candidates(&root, span, &mut found);
    for node in found {
        let at = node.text().as_ptr() as usize - case.request.as_ptr() as usize;
        let marker = format!("__parity_request_origin_{at}__");
        if case.request.contains(&marker) {
            continue;
        }
        let probe = match node {
            Raw::Object(..) => json!({"__parity_request_origin__":marker}),
            Raw::Array(..) => json!([marker]),
            Raw::Scalar(_) if node.value().is_string() => json!(marker),
            _ => continue,
        }
        .to_string();
        let compact = node.value().to_string();
        if !text.contains(&compact) {
            continue;
        }
        let mut changed = case.clone();
        changed.request.replace_range(at..at + span.len(), &probe);
        let Ok(output) = replay(&changed) else {
            continue;
        };
        let Some(output) = output.pointer(path).and_then(Value::as_str) else {
            continue;
        };
        if changed_occurrences(text, output, &compact, &probe)
            .is_some_and(|positions| positions.contains(&position))
        {
            return true;
        }
    }
    false
}
/// Exact replacements only, with positions in the unreplayed Rust string.
/// A copied source node may witness several positions, but never an unchanged
/// equal occurrence. Marker uniqueness makes this byte alignment unambiguous.
fn changed_occurrences(
    text: &str,
    replayed: &str,
    compact: &str,
    probe: &str,
) -> Option<std::collections::BTreeSet<usize>> {
    if compact.is_empty() || compact == probe || text.contains(probe) {
        return None;
    }
    let (a, b, c, p) = (
        text.as_bytes(),
        replayed.as_bytes(),
        compact.as_bytes(),
        probe.as_bytes(),
    );
    let (mut i, mut j) = (0, 0);
    let mut changed = std::collections::BTreeSet::new();
    while i < a.len() {
        if a[i..].starts_with(c) && b.get(j..).is_some_and(|s| s.starts_with(p)) {
            changed.insert(i);
            i += c.len();
            j += p.len();
        } else if a.get(i) == b.get(j) {
            i += 1;
            j += 1;
        } else {
            return None;
        }
    }
    (j == b.len() && !changed.is_empty()).then_some(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_equal_structured_value_elsewhere_cannot_license_a_client_string() {
        let translator = Translator::RegistryRequest;
        let case=Case::new("bound", "m",r#"{"input":[{"type":"user_input","content":[{"type":"text","text":"{ \"a\": 1 }"}]}],"unused":{ "a": 1 }}"#)
            .with_options(json!({"from":"interactions","to":"openai"}));
        let baseline = translator.run_rust(&case).unwrap();
        let path = "/messages/0/content";
        let text = baseline.pointer(path).and_then(Value::as_str).unwrap();
        let structured = Case::new(
            "structured",
            "m",
            r#"{"input":[{"type":"user_input","content":[{"type":"text","text":{ "a": 1 }}]}]}"#,
        )
        .with_options(case.options.clone());
        let output = translator.run_rust(&structured).unwrap();
        assert!(bound_span(
            translator,
            &structured,
            &output,
            path,
            r#"{ "a": 1 }"#,
            r#"{"a":1}"#,
            0
        ));
        assert!(!bound_span(
            translator,
            &case,
            &baseline,
            path,
            r#"{ "a": 1 }"#,
            text,
            0
        ));
        let mut compacted = baseline.clone();
        compacted
            .pointer_mut(path)
            .unwrap()
            .clone_from(&json!(r#"{"a":1}"#));
        let witness = |p: &str, s: &str, t: &str, at: usize| {
            bound_span(translator, &case, &compacted, p, s, t, at)
        };
        let cmp = crate::compare::compare_numbers_with_witness(
            &baseline,
            &compacted,
            translator.embedded_json(&case),
            translator.numbers(),
            Some(&case.request),
            Some(&witness),
        );
        assert!(!cmp.differences.is_empty());
        // Even an already-compacted client string cannot borrow that source.
        let compact_case = Case {
            request: case.request.replace("{ \\\"a\\\": 1 }", "{\\\"a\\\":1}"),
            ..case
        };
        let baseline = translator.run_rust(&compact_case).unwrap();
        assert!(!bound_span(
            translator,
            &compact_case,
            &baseline,
            path,
            r#"{ "a": 1 }"#,
            r#"{"a":1}"#,
            0
        ));
    }
}

#[cfg(test)]
mod occurrence_tests;
