// Ported from CLIProxyAPI
// internal/runtime/executor/helps/gemini_content_turns_test.go and
// helps/vertex_payload_helpers_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Dropped:
//! - `TestEnsureGeminiLeadingUserContentReusesLargeValidPayload`,
//!   `TestEnsureGeminiTrailingUserContentReusesLargeValidPayload` and
//!   `BenchmarkStripVertexToolCallIDsLargeNoopPayload`: they measure Go's
//!   allocations; the body here is edited in place.
//!
//! Changed:
//! - `TestStripVertexToolCallIDsReusesPayloadWithoutIDs` checks that the
//!   body is unchanged, rather than that the same bytes come back.

use serde_json::{Value, json};

use super::*;

fn roles(body: &Value, path: &str) -> String {
    match json::get(body, path) {
        Some(Value::Array(contents)) => contents
            .iter()
            .map(|content| json::str_of(content.get("role")))
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    }
}

// TestEnsureGeminiLeadingUserContent.
#[test]
fn leading_user_content() {
    let cases = [
        (
            json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}),
            "contents",
            "user",
            false,
        ),
        (
            json!({"contents":[{"role":"model","parts":[{"functionCall":{"name":"run"}}]},{"role":"user","parts":[{"functionResponse":{"name":"run"}}]}]}),
            "contents",
            "user,model,user",
            true,
        ),
        (
            json!({"contents":[{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"continue"}]}]}),
            "contents",
            "user,model,user",
            true,
        ),
        (
            json!({"request":{"contents":[{"role":"model","parts":[{"text":"answer"}]},{"role":"user","parts":[{"text":"continue"}]}]}}),
            "request.contents",
            "user,model,user",
            true,
        ),
        (json!({"contents":[]}), "contents", "", false),
        (json!({"model":"test"}), "contents", "", false),
    ];
    for (mut body, path, want, leading_empty) in cases {
        ensure_leading_user(&mut body, path);
        assert_eq!(roles(&body, path), want, "{body}");
        if leading_empty {
            assert_eq!(
                json::get(&body, &format!("{path}.0.parts.0.text")),
                Some(&json!("")),
                "{body}"
            );
        }
    }
}

// TestEnsureGeminiTrailingUserContent.
#[test]
fn trailing_user_content() {
    let cases = [
        (
            json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}),
            "contents",
            "user",
            false,
        ),
        (
            json!({"contents":[{"role":"model","parts":[{"functionCall":{"name":"run"}}]},{"role":"model","parts":[{"functionResponse":{"name":"run","response":{"result":"ok"}}}]}]}),
            "contents",
            "model,model",
            false,
        ),
        (
            json!({"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"functionCall":{"name":"run"}}]}]}),
            "contents",
            "user,model,user",
            true,
        ),
        (
            json!({"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"answer"}]}]}),
            "contents",
            "user,model,user",
            true,
        ),
        (
            json!({"request":{"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"answer"}]}]}}),
            "request.contents",
            "user,model,user",
            true,
        ),
        (
            json!({"contents":[{"role":"user","parts":[{"text":"hello"}]},{"role":"assistant","parts":[{"text":"answer"}]}]}),
            "contents",
            "user,assistant,user",
            true,
        ),
        (json!({"contents":[]}), "contents", "", false),
        (json!({"model":"test"}), "contents", "", false),
    ];
    for (mut body, path, want, trailing_empty) in cases {
        ensure_trailing_user(&mut body, path);
        assert_eq!(roles(&body, path), want, "{body}");
        if trailing_empty {
            let last = json::get(&body, path)
                .and_then(Value::as_array)
                .and_then(|contents| contents.last())
                .unwrap();
            assert_eq!(json::get(last, "parts.0.text"), Some(&json!("")), "{body}");
        }
    }
}

// TestEnsureGeminiBoundaryUserContent.
#[test]
fn boundary_user_content() {
    let mut body = json!({"contents":[{"role":"model","parts":[{"text":"single answer"}]}]});
    ensure_boundary_user(&mut body, "contents");
    assert_eq!(
        body,
        json!({"contents":[
            {"role":"user","parts":[{"text":""}]},
            {"role":"model","parts":[{"text":"single answer"}]},
            {"role":"user","parts":[{"text":""}]}
        ]})
    );
}

// TestStripVertexToolCallIDsReusesPayloadWithoutIDs.
#[test]
fn strip_leaves_a_body_without_ids() {
    let input = json!({"contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"id":9_007_199_254_740_993_u64}}}]}]});
    let mut body = input.clone();
    strip_vertex_tool_call_ids(&mut body, "openai-response");
    assert_eq!(body, input);
}

// TestStripVertexToolCallIDsRebuildsContentsOnce.
#[test]
fn strip_removes_call_ids() {
    let input = r#"{"contents":[{"role":"model","parts":[{"functionCall":{"id":"call_1","name":"lookup","args":{"id":9007199254740993}}}]},{"role":"user","parts":[{"functionResponse":{"id":"call_1","name":"lookup","response":{"id":"keep"}}}]}]}"#;
    let mut body: Value = serde_json::from_str(input).unwrap();
    // Only for an OpenAI Responses client.
    let untouched = body.clone();
    strip_vertex_tool_call_ids(&mut body, "openai");
    assert_eq!(body, untouched);

    strip_vertex_tool_call_ids(&mut body, " OpenAI-Response ");
    assert!(!json::exists(&body, "contents.0.parts.0.functionCall.id"));
    assert!(!json::exists(
        &body,
        "contents.1.parts.0.functionResponse.id"
    ));
    assert_eq!(
        json::str_at(&body, "contents.1.parts.0.functionResponse.response.id"),
        "keep"
    );
    assert_eq!(
        json::get(&body, "contents.0.parts.0.functionCall.args.id")
            .unwrap()
            .to_string(),
        "9007199254740993"
    );
}
