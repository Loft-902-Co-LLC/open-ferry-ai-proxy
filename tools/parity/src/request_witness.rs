//! Bind a complete lexical request span to its actual translated output path.
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
        let Ok(output) = translator.run_rust(&changed) else {
            continue;
        };
        let Some(output) = output.pointer(path).and_then(Value::as_str) else {
            continue;
        };
        if text.replace(&compact, &probe) == output {
            return true;
        }
        for (i, _) in text.match_indices(&compact) {
            let mut expected = text.to_owned();
            expected.replace_range(i..i + compact.len(), &probe);
            if expected == output {
                return true;
            }
        }
    }
    false
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
            r#"{"a":1}"#
        ));
        assert!(!bound_span(
            translator,
            &case,
            &baseline,
            path,
            r#"{ "a": 1 }"#,
            text
        ));
        let mut compacted = baseline.clone();
        compacted
            .pointer_mut(path)
            .unwrap()
            .clone_from(&json!(r#"{"a":1}"#));
        let witness =
            |p: &str, s: &str, t: &str| bound_span(translator, &case, &compacted, p, s, t);
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
            r#"{"a":1}"#
        ));
    }
}
