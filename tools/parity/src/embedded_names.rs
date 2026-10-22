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
    let pairs: Vec<_> = tools
        .iter()
        .filter(|tool| tool.get("type").is_some_and(|v| v.value() == "function"))
        .filter_map(|tool| tool.get("name"))
        .filter(|name| matches!(name, Raw::Object(..) | Raw::Array(..)))
        .map(|name| (sanitize(name.text()), sanitize(&name.value().to_string())))
        .collect();
    let Some(go) = go.get_mut("tools").and_then(Value::as_array_mut) else {
        return false;
    };
    let Some(rust) = rust.get("tools").and_then(Value::as_array) else {
        return false;
    };
    if go.len() != rust.len() {
        return false;
    }
    let mut changed = false;
    for (g, r) in go.iter_mut().zip(rust) {
        if let (Some(gname), Some(rname)) = (g["name"].as_str(), r["name"].as_str())
            && gname != rname
            && pairs
                .iter()
                .any(|(raw, compact)| raw == gname && compact == rname)
        {
            g["name"] = r["name"].clone();
            changed = true;
        }
    }
    changed
}
