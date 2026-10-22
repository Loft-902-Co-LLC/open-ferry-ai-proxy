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
                let request = match route {
                    "claude" => json!({"messages":[{"role":"user","content":"hi"}],
                        "tools":[{"name":"T","input_schema":schema}]}),
                    "chat" => json!({"messages":[{"role":"user","content":"hi"}],
                        "tools":[{"type":"function","function":{"name":"T","parameters":schema}}]}),
                    "responses" => json!({"input":"hi",
                        "tools":[{"type":"function","name":"T","parameters":schema}]}),
                    _ => unreachable!(),
                };
                let request: Value =
                    serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
                let (out, error) = match route {
                    "claude" => open_ferry_translate::gemini::claude::convert_claude_request_to_gemini(
                        "gemini-2.5-pro", &request, false, ModelCatalog::embedded()),
                    "chat" => open_ferry_translate::gemini::openai::chat_completions::convert_openai_request_to_gemini(
                        "gemini-2.5-pro", &request, false),
                    "responses" => open_ferry_translate::gemini::openai::responses::convert_openai_responses_request_to_gemini(
                        "gemini-2.5-pro", &request, false),
                    _ => unreachable!(),
                };
                assert!(error.is_none());
                let schema = &out["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
                assert!(schema.is_object());
                assert!(structural_depth(schema) <= 128, "{route}/{keyword}/{pairs}");
                assert_eq!(schema["properties"], json!({"a":{"type":"string"}}));
                assert!(structural_depth(&out) <= 128);
                assert!(!serde_json::to_vec(&out).unwrap().is_empty());
                drop(out);
                drop(request);
            }
        })
        .unwrap()
        .join()
        .unwrap();
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
