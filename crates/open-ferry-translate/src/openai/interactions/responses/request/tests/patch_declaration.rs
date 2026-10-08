// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/apply_patch_test.go
// (TestInteractionsApplyPatchDeclarationAndHistory,
// TestInteractionsApplyPatchWinningDeclarationAndNegativeCompatibility) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The client's `apply_patch` custom tool as the request translator
//! declares it to Interactions, with its patch contract, and its calls in
//! the history. The response translator's tests from the same file are in
//! `response/tests/apply_patch.rs`.
//!
//! Changed tests:
//! - TestInteractionsApplyPatchWinningDeclarationAndNegativeCompatibility
//!   keeps only its request half here: whether the declaration carries the
//!   patch contract. Its response half is with the response tests.

use super::*;

/// Upstream's `patchRequest`.
const PATCH_REQUEST: &str = r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}]}"#;

/// Upstream's `patchText`.
const PATCH_TEXT: &str = "  *** Begin Patch\n*** Add File: 中.txt\n+😀\n*** End Patch\n ";

/// The model upstream's tests translate for.
const MODEL: &str = "devin/swe-2";

/// TestInteractionsApplyPatchDeclarationAndHistory
#[test]
fn declaration_and_history() {
    let mut request: Value = serde_json::from_str(PATCH_REQUEST).expect("valid JSON");
    request["input"] = json!([
        {
            "type": "custom_tool_call",
            "name": "apply_patch",
            "namespace": "functions",
            "call_id": "c1",
            "input": PATCH_TEXT,
        },
        { "type": "custom_tool_call_output", "call_id": "c1", "output": "ok" },
    ]);
    let out = to_interactions(MODEL, request);
    let additional = at(&out, "tools.0.parameters.additionalProperties");
    assert!(
        text_at(&out, "tools.0.description").contains("*** Begin Patch")
            && additional.is_some_and(|value| *value == json!(false)),
        "missing patch contract: {out}"
    );
    assert!(
        text_at(&out, "input.0.arguments.input") == PATCH_TEXT
            && text_at(&out, "input.1.name") == "functions__apply_patch",
        "history changed: {out}"
    );
}

/// The request half of
/// TestInteractionsApplyPatchWinningDeclarationAndNegativeCompatibility.
#[test]
fn winning_declaration_carries_the_contract() {
    for (name, request, patch) in [
        (
            "top function beats additional custom",
            r#"{"tools":[{"type":"function","name":"apply_patch","parameters":{"type":"object","properties":{"n":{"type":"number"}}}}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            false,
        ),
        (
            "direct function beats namespace custom",
            r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"functions__apply_patch"}]}"#,
            false,
        ),
        (
            "other custom remains lenient",
            r#"{"tools":[{"type":"custom","name":"exec"}]}"#,
            false,
        ),
        (
            "sanitization collision keeps qualified Interactions name",
            r#"{"tools":[{"type":"namespace","name":"a.b","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"a_b__apply_patch"}]}"#,
            true,
        ),
    ] {
        let request: Value = serde_json::from_str(request).expect("valid JSON");
        let translated = to_interactions(MODEL, request);
        assert_eq!(
            text_at(&translated, "tools.0.description").contains("*** Begin Patch"),
            patch,
            "{name}: wrong contract: {translated}"
        );
    }
}
