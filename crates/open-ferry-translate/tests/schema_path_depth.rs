//! Resource regressions for schema writes through upstream-style escaped paths.

use open_ferry_translate::models::ModelCatalog;
use serde_json::{Value, json};

fn structural_depth(value: &Value) -> usize {
    let mut maximum = 0;
    let mut pending = vec![(value, 0)];
    while let Some((value, depth)) = pending.pop() {
        match value {
            Value::Object(fields) => {
                maximum = maximum.max(depth + 1);
                pending.extend(fields.values().map(|value| (value, depth + 1)));
            }
            Value::Array(items) => {
                maximum = maximum.max(depth + 1);
                pending.extend(items.iter().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    maximum
}

fn translate(route: &str, schema: Value) -> Value {
    let request = match route {
        "claude" => json!({"messages":[{"role":"user","content":"hi"}],
            "tools":[{"name":"T","input_schema":schema}]}),
        "chat" => json!({"messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{"name":"T","parameters":schema}}]}),
        "responses" => json!({"input":"hi",
            "tools":[{"type":"function","name":"T","parameters":schema}]}),
        _ => unreachable!(),
    };
    let request: Value = serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
    let (out, error) = match route {
        "claude" => open_ferry_translate::gemini::claude::convert_claude_request_to_gemini(
            "gemini-2.5-pro", &request, false, ModelCatalog::embedded()),
        "chat" => open_ferry_translate::gemini::openai::chat_completions::convert_openai_request_to_gemini(
            "gemini-2.5-pro", &request, false),
        "responses" => open_ferry_translate::gemini::openai::responses::convert_openai_responses_request_to_gemini(
            "gemini-2.5-pro", &request, false),
        _ => unreachable!(),
    };
    assert!(error.is_none(), "{route}");
    assert!(structural_depth(&out) <= 128, "{route}");
    assert!(!serde_json::to_vec(&out).unwrap().is_empty());
    out
}

fn check(route: &'static str, keyword: &'static str) {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            for pairs in [5_000, 20_000, 1_000] {
                let name = r"\.".repeat(pairs);
                let branch = json!({"properties":{name:{"type":"string"}}});
                let mut schema = json!({"type":"object","properties":{"a":{"type":"string"}}});
                if keyword == "then" {
                    schema["if"] = json!({"properties":{"a":{"type":"string"}}});
                    schema["then"] = branch;
                } else {
                    schema[keyword] = json!([branch]);
                }
                let out = translate(route, schema);
                let schema = &out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
                assert!(schema.is_object());
                assert!(structural_depth(schema) <= 128, "{route}/{keyword}/{pairs}");
                assert_eq!(schema["properties"], json!({"a":{"type":"string"}}));
                drop(out);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

fn check_central_gate(property: Value, expected: Value) {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            for route in ["claude", "chat", "responses"] {
                let name = r"\.".repeat(20_000);
                let schema = json!({"type":"object","properties":{
                    "a":{"type":"string"}, name.clone():property}});
                let out = translate(route, schema);
                let schema = &out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
                assert!(schema.is_object(), "{route}");
                assert!(structural_depth(schema) <= 128, "{route}");
                assert_eq!(schema["properties"]["a"], json!({"type":"string"}));
                // The guarded leaf edit must not rebuild the escaped name as
                // thousands of nested keys, or discard the original property.
                assert_eq!(schema["properties"].as_object().unwrap().len(), 2);
                assert_eq!(schema["properties"][&name], expected, "{route}");
                drop(out);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn external_ref_escaped_paths_fit_two_mib_stack() {
    let property = json!({"$ref":"https://example.com/x.json"});
    check_central_gate(property.clone(), property);
}

#[test]
fn array_without_items_escaped_paths_fit_two_mib_stack() {
    check_central_gate(json!({"type":"array"}), json!({"type":"array"}));
}

#[test]
fn claude_all_of_escaped_paths_fit_two_mib_stack() {
    check("claude", "allOf");
}

#[test]
fn claude_then_escaped_paths_fit_two_mib_stack() {
    check("claude", "then");
}

#[test]
fn claude_single_union_escaped_paths_fit_two_mib_stack() {
    check("claude", "anyOf");
}

#[test]
fn chat_all_of_escaped_paths_fit_two_mib_stack() {
    check("chat", "allOf");
}

#[test]
fn responses_all_of_escaped_paths_fit_two_mib_stack() {
    check("responses", "allOf");
}

#[test]
fn ref_sibling_conjunctions_survive_each_gemini_request_route() {
    for route in ["claude", "chat", "responses"] {
        let out = translate(
            route,
            json!({"type":"object","properties":{"x":{
            "$ref":"#/$defs/A","minLength":1}},"$defs":{"A":{"type":"string","minLength":5}}}),
        );
        let schema = &out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
        assert_eq!(schema["properties"]["x"]["minLength"], 1);
        assert_eq!(schema["properties"]["x"]["allOf"][0]["minLength"], 5);
        assert!(schema.get("$defs").is_none());
        assert!(schema["properties"]["x"].get("$ref").is_none());
    }
}

#[test]
fn ref_sibling_conjunction_depth_fits_two_mib_stack() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            for levels in [30, 60, 63] {
                let mut target = json!({"type":"integer"});
                for _ in 0..levels {
                    target = json!({"type":"array","items":target});
                }
                for route in ["claude", "chat", "responses"] {
                    let out = translate(
                        route,
                        json!({"$ref":"#/$defs/A","type":"string","$defs":{"A":target}}),
                    );
                    assert!(structural_depth(&out) <= 128);
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
