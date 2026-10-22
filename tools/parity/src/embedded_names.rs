//! Collection-valued Interactions function names: replay both sanitizer inputs.
use crate::cases::Case;
use crate::raw_json::{self, Raw};
use serde_json::Value;

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

pub fn repair(case: &Case, go: &mut Value, rust: &Value) -> bool {
    let Some(request) = raw_json::parse(&case.request) else {
        return false;
    };
    let Some(Raw::Array(tools, _)) = request.get("tools") else {
        return false;
    };
    // Match the translator's declaration flattening and name fallback, keeping
    // a slot for every emitted tool even when its name is already a string.
    fn name(tool: &Raw<'_>) -> Option<Option<(String, String)>> {
        fn nonempty(n: &&Raw<'_>) -> bool {
            let v = n.value();
            !v.is_null() && v.as_str().is_none_or(|s| !s.is_empty())
        }
        let n = tool
            .get("name")
            .filter(nonempty)
            .or_else(|| tool.get("function")?.get("name").filter(nonempty))?;
        Some(
            matches!(n, Raw::Object(..) | Raw::Array(..))
                .then(|| (sanitize(n.text()), sanitize(&n.value().to_string()))),
        )
    }
    let mut pairs = Vec::new();
    for tool in tools {
        let declarations = tool
            .get("function_declarations")
            .filter(|v| matches!(v, Raw::Array(..)))
            .or_else(|| {
                tool.get("functionDeclarations")
                    .filter(|v| matches!(v, Raw::Array(..)))
            });
        if let Some(Raw::Array(items, _)) = declarations {
            pairs.extend(items.iter().filter_map(name));
        } else if let Some(pair) = name(tool) {
            pairs.push(pair);
        }
    }
    let Some(go) = go.get_mut("tools").and_then(Value::as_array_mut) else {
        return false;
    };
    let Some(rust) = rust.get("tools").and_then(Value::as_array) else {
        return false;
    };
    if go.len() != rust.len() || go.len() != pairs.len() {
        return false;
    }
    let mut changed = false;
    for ((g, r), pair) in go.iter_mut().zip(rust).zip(pairs) {
        if let (Some(gname), Some(rname)) = (g["name"].as_str(), r["name"].as_str())
            && gname != rname
            && pair
                .as_ref()
                .is_some_and(|(raw, compact)| raw == gname && compact == rname)
        {
            g["name"] = r["name"].clone();
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn another_tools_name_cannot_justify_this_tools_mismatch() {
        let case = Case::new(
            "position",
            "m",
            r#"{"tools":[{"type":"search"},{"type":"function","name":{ "a": 1 }},{"type":"function","name":"ordinary"}]}"#,
        );
        let raw = sanitize(r#"{ "a": 1 }"#);
        let compact = sanitize(r#"{"a":1}"#);
        let mut go = json!({"tools":[{"name":raw},{"name":raw}]});
        let rust = json!({"tools":[{"name":raw},{"name":compact}]});
        assert!(!repair(&case, &mut go, &rust));
        assert_ne!(go, rust);
        let mut go = json!({"tools":[{"name":raw},{"name":"ordinary"}]});
        let rust = json!({"tools":[{"name":compact},{"name":"ordinary"}]});
        assert!(repair(&case, &mut go, &rust));
        assert_eq!(go, rust);
    }
}
