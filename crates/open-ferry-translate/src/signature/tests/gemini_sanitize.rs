// Ported from CLIProxyAPI internal/signature/gemini_sanitize_test.go (v8.0.10,
// MIT). https://github.com/router-for-me/CLIProxyAPI

use std::borrow::Cow;

use serde_json::Value;

use super::*;

/// gjson `Get`: a dotted path whose numeric segments index arrays.
fn get<'v>(payload: &'v Value, path: &str) -> Option<&'v Value> {
    payload.pointer(&format!("/{}", path.replace('.', "/")))
}

/// gjson `Get(path).String()`.
fn get_string<'v>(payload: &'v Value, path: &str) -> Cow<'v, str> {
    crate::json::str_of(get(payload, path))
}

/// Sanitizes a parsed copy of `input`, returning it and whether it changed.
fn sanitize(input: &str, contents_path: &str) -> (Value, bool) {
    let mut out = json(input);
    let changed = sanitize_gemini_request_thought_signatures(&mut out, contents_path);
    (out, changed)
}

#[test]
fn sanitize_gemini_request_thought_signatures_preserves_gemini_signature() {
    let sig = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39]);
    let input = json(&format!(
        r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"f","args":{{}}}},"thoughtSignature":"{sig}"}}]}}]}}"#
    ));

    let mut out = input.clone();
    let changed = sanitize_gemini_request_thought_signatures(&mut out, "contents");

    assert_eq!(
        get_string(&out, "contents.0.parts.0.thoughtSignature"),
        sig,
        "thoughtSignature. Output: {out}"
    );
    assert!(
        !changed && out == input,
        "compatible canonical signature payload was rewritten: {out}"
    );
}

// NormalizesDuplicateCanonicalField isn't ported: a parsed Value can't hold duplicate keys.

#[test]
fn sanitize_gemini_request_thought_signatures_parallel_synthetic_only_first_gets_bypass() {
    let (out, _) = sanitize(
        r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"first","args":{}}},{"functionCall":{"name":"second","args":{}}}]}]}"#,
        "contents",
    );

    assert_eq!(
        get_string(&out, "contents.0.parts.0.thoughtSignature"),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "first call signature; output={out}"
    );
    assert!(
        get(&out, "contents.0.parts.1.thoughtSignature").is_none(),
        "second parallel call should remain unsigned; output={out}"
    );
}

#[test]
fn sanitize_gemini_request_thought_signatures_native_parallel_preserves_unsigned_sibling() {
    let native_signature = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39]);
    let input = json(&format!(
        r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"first","args":{{}}}},"thoughtSignature":"{native_signature}"}},{{"functionCall":{{"name":"second","args":{{}}}}}}]}}]}}"#
    ));

    let mut out = input.clone();
    let changed = sanitize_gemini_request_thought_signatures(&mut out, "contents");

    assert_eq!(
        get_string(&out, "contents.0.parts.0.thoughtSignature"),
        native_signature,
        "first call signature; output={out}"
    );
    assert!(
        get(&out, "contents.0.parts.1.thoughtSignature").is_none(),
        "native unsigned sibling should remain unsigned; output={out}"
    );
    assert!(
        !changed && out == input,
        "already-native parallel history was rewritten: {out}"
    );
}

#[test]
fn sanitize_gemini_request_thought_signatures_removes_polluted_sibling_bypass() {
    let native_signature = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39]);
    let (out, _) = sanitize(
        &format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"first","args":{{}}}},"thoughtSignature":"{native_signature}"}},{{"functionCall":{{"name":"second","args":{{}}}},"thoughtSignature":"{GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}"}}]}}]}}"#
        ),
        "contents",
    );

    assert_eq!(
        get_string(&out, "contents.0.parts.0.thoughtSignature"),
        native_signature,
        "first call signature; output={out}"
    );
    assert!(
        get(&out, "contents.0.parts.1.thoughtSignature").is_none(),
        "polluted sibling bypass should be removed; output={out}"
    );
}

#[test]
fn sanitize_gemini_request_thought_signatures_removes_prefixed_sibling_bypass() {
    let native_signature = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39]);
    for prefix in ["gemini", "google"] {
        let (out, _) = sanitize(
            &format!(
                r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"first","args":{{}}}},"thoughtSignature":"{native_signature}"}},{{"functionCall":{{"name":"second","args":{{}}}},"thoughtSignature":"{prefix}#{GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}"}}]}}]}}"#
            ),
            "contents",
        );

        assert!(
            get(&out, "contents.0.parts.1.thoughtSignature").is_none(),
            "{prefix}: prefixed sibling bypass should be removed; output={out}"
        );
    }
}

#[test]
fn sanitize_gemini_request_thought_signatures_leaves_unsigned_thought_unsigned() {
    let input =
        json(r#"{"contents":[{"role":"model","parts":[{"text":"hidden","thought":true}]}]}"#);

    let mut out = input.clone();
    let changed = sanitize_gemini_request_thought_signatures(&mut out, "contents");

    assert!(
        get(&out, "contents.0.parts.0.thoughtSignature").is_none(),
        "unsigned thought should remain unsigned; output={out}"
    );
    assert!(
        !changed && out == input,
        "unsigned thought payload was rewritten: {out}"
    );
}

#[test]
fn sanitize_gemini_request_thought_signatures_reuses_unsigned_function_response_payload() {
    let input = json(
        r#"{"contents":[{"role":"user","parts":[{"functionResponse":{"name":"f","response":{"result":"ok"}}}]}]}"#,
    );

    let mut out = input.clone();
    let changed = sanitize_gemini_request_thought_signatures(&mut out, "contents");

    assert!(!changed, "unsigned function response payload was rewritten");
    assert_eq!(out, input, "payload changed");
}

#[test]
fn sanitize_gemini_request_thought_signatures_replaces_base64_uuid_function_call() {
    let sig = test_gemini_thought_signature(b"e24830a7-5cd6-42fe-998b-ee539e72b9c3");
    let (out, _) = sanitize(
        &format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"f","args":{{}},"thoughtSignature":"{sig}"}}}}]}}]}}"#
        ),
        "contents",
    );

    assert_eq!(
        get_string(&out, "contents.0.parts.0.thoughtSignature"),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "thoughtSignature should be the bypass sentinel. Output: {out}"
    );
    assert!(
        get(&out, "contents.0.parts.0.functionCall.thoughtSignature").is_none(),
        "nested functionCall thoughtSignature should be removed. Output: {out}"
    );
}

// Upstream also asserts the debug log line, which isn't ported.
#[test]
fn sanitize_gemini_request_thought_signatures_logs_bypass_replacement() {
    let sig = test_gemini_thought_signature(b"e24830a7-5cd6-42fe-998b-ee539e72b9c3");
    let (out, _) = sanitize(
        &format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"f","args":{{}},"thoughtSignature":"{sig}"}}}}]}}]}}"#
        ),
        "contents",
    );

    assert_eq!(
        get_string(&out, "contents.0.parts.0.thoughtSignature"),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "thoughtSignature should be the bypass sentinel. Output: {out}"
    );
}

// Upstream also asserts the aggregated debug log, which isn't ported.
#[test]
fn sanitize_gemini_request_thought_signatures_aggregates_repeated_logs() {
    let (out, _) = sanitize(
        r#"{"contents":[
            {"role":"model","parts":[{"text":"a","thoughtSignature":"invalid_sig_1"}]},
            {"role":"model","parts":[{"text":"b","thoughtSignature":"invalid_sig_2"}]},
            {"role":"model","parts":[{"text":"c","thoughtSignature":"invalid_sig_3"}]}
        ]}"#,
        "contents",
    );

    for i in 0..3 {
        assert!(
            get(&out, &format!("contents.{i}.parts.0.thoughtSignature")).is_none(),
            "expected part {i} signature to be dropped, got: {out}"
        );
    }

    let mut again = out.clone();
    let changed = sanitize_gemini_request_thought_signatures(&mut again, "contents");
    assert!(
        !changed && again == out,
        "sanitized payload should be reused unchanged: {again}"
    );
}

// Upstream also asserts one debug log per detected provider, which isn't
// ported.
#[test]
fn sanitize_gemini_request_thought_signatures_distinguishes_detected_providers() {
    let (out, _) = sanitize(
        r#"{"contents":[
            {"role":"model","parts":[{"text":"a","thoughtSignature":"sealed.v1.demo_signature_1"}]},
            {"role":"model","parts":[{"text":"b","thoughtSignature":"sealed.v1.demo_signature_2"}]},
            {"role":"model","parts":[{"text":"c","thoughtSignature":"invalid_sig_unknown_1"}]},
            {"role":"model","parts":[{"text":"d","thoughtSignature":"invalid_sig_unknown_2"}]}
        ]}"#,
        "contents",
    );

    for i in 0..4 {
        assert!(
            get(&out, &format!("contents.{i}.parts.0.thoughtSignature")).is_none(),
            "expected part {i} signature to be dropped, got: {out}"
        );
    }
}

// Upstream also asserts a single aggregated debug log, which isn't ported.
#[test]
fn sanitize_gemini_request_thought_signatures_sibling_bypass_logs_single_aggregate() {
    for prefix in ["", "gemini#", "google#"] {
        let (out, _) = sanitize(
            &format!(
                concat!(
                    r#"{{"contents":[{{"role":"model","parts":["#,
                    r#"{{"functionCall":{{"name":"first","args":{{}}}},"thoughtSignature":"{bypass}"}},"#,
                    r#"{{"functionCall":{{"name":"second","args":{{}}}},"thoughtSignature":"{prefix}{bypass}"}},"#,
                    r#"{{"functionCall":{{"name":"third","args":{{}}}},"thoughtSignature":"{prefix}{bypass}"}}]}}]}}"#,
                ),
                bypass = GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
                prefix = prefix,
            ),
            "contents",
        );

        assert!(
            get(&out, "contents.0.parts.1.thoughtSignature").is_none()
                && get(&out, "contents.0.parts.2.thoughtSignature").is_none(),
            "{prefix:?}: sibling bypass was not removed: {out}"
        );
    }
}

// Upstream also asserts the debug logs, and runs twice to check that a
// process-wide deduplication cache doesn't suppress the second request's logs.
// The run is kept twice here: independent requests are sanitized alike.
#[test]
fn sanitize_gemini_request_thought_signatures_invalid_sibling_still_logs_at_debug() {
    let input = format!(
        concat!(
            r#"{{"request":{{"contents":[{{"role":"model","parts":["#,
            r#"{{"functionCall":{{"name":"first","args":{{}}}},"thoughtSignature":"{bypass}"}},"#,
            r#"{{"functionCall":{{"name":"second","args":{{}}}},"thoughtSignature":"invalid_sibling_signature"}},"#,
            r#"{{"functionCall":{{"name":"third","args":{{}}}},"thoughtSignature":"{bypass}"}}]}}]}}}}"#,
        ),
        bypass = GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
    );

    for run in 0..2 {
        let (out, _) = sanitize(&input, "request.contents");
        assert!(
            get(&out, "request.contents.0.parts.1.thoughtSignature").is_none(),
            "run {run}: invalid sibling signature was not removed: {out}"
        );
    }
}

// Upstream also asserts that each case logs at debug, which isn't ported.
#[test]
fn sanitize_gemini_request_thought_signatures_foreign_prefixed_bypass_logs_at_debug() {
    for prefix in ["claude#", "gpt#", "swe#"] {
        let foreign = format!("{prefix}{GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}");
        let sibling_cases: [&[&str]; 3] = [
            &[&foreign],
            &[&foreign, "invalid_sibling"],
            &["invalid_sibling", &foreign],
        ];
        for siblings in sibling_cases {
            let mut parts = format!(
                r#"{{"functionCall":{{"name":"first","args":{{}}}},"thoughtSignature":"{GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}"}}"#
            );
            for sig in siblings {
                parts += &format!(
                    r#",{{"functionCall":{{"name":"sibling","args":{{}}}},"thoughtSignature":"{sig}"}}"#
                );
            }
            let (out, _) = sanitize(
                &format!(r#"{{"contents":[{{"role":"model","parts":[{parts}]}}]}}"#),
                "contents",
            );
            for i in 0..siblings.len() {
                assert!(
                    get(
                        &out,
                        &format!("contents.0.parts.{}.thoughtSignature", i + 1)
                    )
                    .is_none(),
                    "{prefix:?} {siblings:?}: foreign sibling signature was not removed: {out}"
                );
            }
        }
    }
}

// ReasonGroupsRemainDistinct isn't ported: it asserts only debug logs.

#[test]
fn sanitize_gemini_request_thought_signatures_preserves_field2_wrapped_uuid_function_call() {
    let sig = test_gemini3_thought_signature(b"e24830a7-5cd6-42fe-998b-ee539e72b9c3");
    let (out, _) = sanitize(
        &format!(
            r#"{{"request":{{"contents":[{{"role":"model","parts":[{{"functionCall":{{"name":"f","args":{{}}}},"thoughtSignature":"{sig}"}}]}}]}}}}"#
        ),
        "request.contents",
    );

    assert_eq!(
        get_string(&out, "request.contents.0.parts.0.thoughtSignature"),
        sig,
        "want wrapped UUID signature preserved. Output: {out}"
    );
}

// serde_json keeps the last of the duplicated nested keys, so this checks one
// nested signature rather than upstream's two; the top-level one is unaffected.
#[test]
fn sanitize_gemini_request_thought_signatures_removes_function_response_signature() {
    let (out, _) = sanitize(
        r#"{"contents":[{"role":"user","parts":[{"functionResponse":{"name":"f","response":{"result":"ok"},"thoughtSignature":"bad","thoughtSignature":"worse"},"thoughtSignature":"bad"}]}]}"#,
        "contents",
    );

    assert!(
        get(&out, "contents.0.parts.0.thoughtSignature").is_none(),
        "functionResponse top-level thoughtSignature should be removed. Output: {out}"
    );
    assert!(
        get(&out, "contents.0.parts.0.functionResponse.thoughtSignature").is_none(),
        "functionResponse nested thoughtSignature should be removed. Output: {out}"
    );
}

#[test]
fn sanitize_gemini_request_thought_signatures_preserves_tool_call_and_response_signatures() {
    const LIVE_CAPTURED_TOOL_CALL_SIG: &str = "ErUDCrIDCAISrQMBEU0yD9ECvDhSY1DQJNUGafArdfd2mDfO8VQq7XjLx/91zESuo0QPSdkRFWkLeVIocSQmQULonYMOJcs6XDLV2LTRC9myb3MCCP9CUoWbEeqhAvXKTScyS3nwBDDVJYuDDbY3YvR4V86T/DnU3qufpaVZ3wQOiJVyBVZ515dYTN+XGq7SuUc3RpfAqVU06jgxaCM0WKV4Df5mGMJWb25e/aFG2Jc7upSqpf3n6aElj+4c/eWr4GdKd0TUIElXBZ0HEN/vNcWzD3F0S4MeVbk1LDakL6HG6oyaSS2gocxYNYxqm9mdMHaXYa4mIYqWqmqBEnbgcHp8H4fgqBxc3Cx8C3otV8IarO5OALaVDA3NaXB1zjLet1587kEpkCNr9OvrYOES2nCl/i4EgbPK01nlXo+Wwm5jsZU5nEG4/Z0bErzqC5TKwOsqpJ7afL2sPWI0IGrXhXL+QCumWCS5iUtwybSkL7CYSk9GC+iY+ev6FAmC4V5JEc4OaWOc9+m/29LniN/iPTSxtUQSZT94pUa3/irIIdH7ReAS3cpeM6OTvumR1PwNxXx3XM1mEGc=";
    let sig_model = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39]);

    let (out, _) = sanitize(
        &format!(
            r#"{{"contents":[{{"role":"user","parts":[{{"text":"hello"}}]}},{{"role":"model","parts":[{{"toolCall":{{"toolType":"GOOGLE_SEARCH_WEB","id":"1"}},"thoughtSignature":"{LIVE_CAPTURED_TOOL_CALL_SIG}"}},{{"toolResponse":{{"toolType":"GOOGLE_SEARCH_WEB","id":"1"}},"thoughtSignature":"{LIVE_CAPTURED_TOOL_CALL_SIG}"}},{{"functionCall":{{"name":"f","args":{{}}}},"thoughtSignature":"{sig_model}"}}]}}]}}"#
        ),
        "contents",
    );

    assert_eq!(
        get_string(&out, "contents.1.parts.0.thoughtSignature"),
        LIVE_CAPTURED_TOOL_CALL_SIG,
        "toolCall thoughtSignature. Output: {out}"
    );
    assert_eq!(
        get_string(&out, "contents.1.parts.1.thoughtSignature"),
        LIVE_CAPTURED_TOOL_CALL_SIG,
        "toolResponse thoughtSignature. Output: {out}"
    );
    assert_eq!(
        get_string(&out, "contents.1.parts.2.thoughtSignature"),
        sig_model,
        "functionCall thoughtSignature. Output: {out}"
    );
}

// Server-side tool blocks (toolCall and toolResponse) carry their own
// signatures or envelopes, which must be echoed back to the API untouched.
#[test]
fn sanitize_gemini_request_thought_signatures_skips_tool_call_and_tool_response_parts() {
    const ARBITRARY_SIG: &str = "arbitrary_opaque_tool_sig_value";
    let (out, _) = sanitize(
        &format!(
            r#"{{"contents":[{{"role":"model","parts":[
            {{"toolCall":{{"toolType":"GOOGLE_SEARCH_WEB","args":{{}}}},"thoughtSignature":"{ARBITRARY_SIG}"}},
            {{"toolResponse":{{"toolType":"GOOGLE_SEARCH_WEB","response":{{}}}},"thoughtSignature":"{ARBITRARY_SIG}"}}
        ]}}]}}"#
        ),
        "contents",
    );

    assert_eq!(
        get_string(&out, "contents.0.parts.0.thoughtSignature"),
        ARBITRARY_SIG,
        "toolCall thoughtSignature should be preserved"
    );
    assert_eq!(
        get_string(&out, "contents.0.parts.1.thoughtSignature"),
        ARBITRARY_SIG,
        "toolResponse thoughtSignature should be preserved"
    );
}

// Defensively ensures the snake_case tool_call and tool_response variants are
// skipped too.
#[test]
fn sanitize_gemini_request_thought_signatures_skips_snake_case_tool_call_and_response_parts() {
    const ARBITRARY_SIG: &str = "arbitrary_snake_tool_sig";
    let (out, _) = sanitize(
        &format!(
            r#"{{"contents":[{{"role":"model","parts":[
            {{"tool_call":{{"tool_type":"GOOGLE_SEARCH_WEB","args":{{}}}},"thought_signature":"{ARBITRARY_SIG}"}},
            {{"tool_response":{{"tool_type":"GOOGLE_SEARCH_WEB","response":{{}}}},"thought_signature":"{ARBITRARY_SIG}"}}
        ]}}]}}"#
        ),
        "contents",
    );

    assert_eq!(
        get_string(&out, "contents.0.parts.0.thought_signature"),
        ARBITRARY_SIG,
        "tool_call thought_signature should be preserved"
    );
    assert_eq!(
        get_string(&out, "contents.0.parts.1.thought_signature"),
        ARBITRARY_SIG,
        "tool_response thought_signature should be preserved"
    );
}

// An unknown or foreign signature on a model text part must be dropped, or
// Gemini fails with 400 "Corrupted thought signature".
#[test]
fn sanitize_gemini_request_thought_signatures_drops_foreign_signature_on_text_part() {
    const FOREIGN_SIG: &str = "claude_or_invalid_signature";
    let (out, _) = sanitize(
        &format!(
            r#"{{"contents":[{{"role":"model","parts":[{{"text":"answer","thoughtSignature":"{FOREIGN_SIG}"}}]}}]}}"#
        ),
        "contents",
    );

    assert!(
        get(&out, "contents.0.parts.0.thoughtSignature").is_none(),
        "expected foreign signature on text part to be dropped, got {out}"
    );
}

// In a turn mixing a toolCall with an unsigned functionCall, the toolCall keeps
// its signature and the first functionCall gets the bypass sentinel.
#[test]
fn sanitize_gemini_request_thought_signatures_mixed_tool_call_and_unsigned_function_call() {
    const TOOL_SIG: &str = "ErUDCrIDCAISrQMBEU0yD9ECvDhSY1DQJNUGafArdfd2mDfO8VQq7XjLx/91zESuo0QPSdkRFWkLeVIocSQmQULonYMOJcs6XDLV2LTRC9myb3MCCP9CUoWbEeqhAvXKTScyS3nwBDDVJYuDDbY3YvR4V86T/DnU3qufpaVZ3wQOiJVyBVZ515dYTN+XGq7SuUc3RpfAqVU06jgxaCM0WKV4Df5mGMJWb25e/aFG2Jc7upSqpf3n6aElj+4c/eWr4GdKd0TUIElXBZ0HEN/vNcWzD3F0S4MeVbk1LDakL6HG6oyaSS2gocxYNYxqm9mdMHaXYa4mIYqWqmqBEnbgcHp8H4fgqBxc3Cx8C3otV8IarO5OALaVDA3NaXB1zjLet1587kEpkCNr9OvrYOES2nCl/i4EgbPK01nlXo+Wwm5jsZU5nEG4/Z0bErzqC5TKwOsqpJ7afL2sPWI0IGrXhXL+QCumWCS5iUtwybSkL7CYSk9GC+iY+ev6FAmC4V5JEc4OaWOc9+m/29LniN/iPTSxtUQSZT94pUa3/irIIdH7ReAS3cpeM6OTvumR1PwNxXx3XM1mEGc=";
    let (out, _) = sanitize(
        &format!(
            r#"{{"contents":[{{"role":"model","parts":[
            {{"toolCall":{{"toolType":"GOOGLE_SEARCH_WEB","id":"1"}},"thoughtSignature":"{TOOL_SIG}"}},
            {{"functionCall":{{"name":"my_func","args":{{}}}}}}
        ]}}]}}"#
        ),
        "contents",
    );

    assert_eq!(
        get_string(&out, "contents.0.parts.0.thoughtSignature"),
        TOOL_SIG,
        "toolCall signature should be preserved"
    );
    assert_eq!(
        get_string(&out, "contents.0.parts.1.thoughtSignature"),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "functionCall signature should be the bypass. Output: {out}"
    );
}
