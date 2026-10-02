// Ported from CLIProxyAPI internal/signature/claude_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::Value;

use super::*;

/// `messages.0.content`, or nothing.
fn first_content(payload: &Value) -> &[Value] {
    payload
        .pointer("/messages/0/content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// `messages.0.content.0.signature`.
fn first_signature(payload: &Value) -> Option<&str> {
    payload
        .pointer("/messages/0/content/0/signature")
        .and_then(Value::as_str)
}

/// An assistant message whose only block is a thinking block signed with `sig`.
fn thinking_request(sig: &str) -> Value {
    serde_json::json!({"messages":[{"role":"assistant","content":[
        {"type":"thinking","thinking":"keep","signature":sig}
    ]}]})
}

/// `defaultClaudeCAISParts("claude-opus-5")` with `edit` applied.
fn opus5_parts(edit: impl FnOnce(&mut ClaudeCaisParts)) -> ClaudeCaisParts {
    let mut parts = ClaudeCaisParts::new("claude-opus-5");
    edit(&mut parts);
    parts
}

/// The observed CAQS sample with the first `thinking` in its decoded payload
/// replaced by `with`.
fn caqs_sample_with_block_kind(with: &[u8; 8]) -> String {
    let mut decoded = STANDARD
        .decode(OBSERVED_FABLE51_CAQS_SAMPLE)
        .expect("decode sample");
    let at = decoded
        .windows(8)
        .position(|window| window == b"thinking")
        .expect("block kind in sample");
    decoded[at..at + 8].copy_from_slice(with);
    STANDARD.encode(decoded)
}

#[test]
fn strip_invalid_claude_thinking_blocks_removes_gpt_encrypted_content() {
    let mut payload = json(
        r#"{
            "messages": [
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"codex reasoning","signature":"gAAAABopenai-encrypted-content"},
                    {"type":"text","text":"Answer"}
                ]},
                {"role":"user","content":[{"type":"text","text":"next"}]}
            ]
        }"#,
    );
    strip_invalid_claude_thinking_blocks(&mut payload, ClaudeValidationOptions::default());
    let out = payload.to_string();
    let content = first_content(&payload);
    assert_eq!(content.len(), 1, "messages.0.content length: {out}");
    assert_eq!(content[0]["text"], "Answer", "remaining content text");
    assert!(
        !out.contains("gAAAABopenai-encrypted-content") && !out.contains("codex reasoning"),
        "invalid thinking block was preserved: {out}"
    );
}

#[test]
fn strip_invalid_claude_thinking_blocks_and_empty_messages_drops_messages_left_empty() {
    let mut payload = json(
        r#"{
            "messages": [
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"codex reasoning","signature":"gAAAABopenai-encrypted-content"}
                ]},
                {"role":"user","content":[{"type":"text","text":"next"}]}
            ]
        }"#,
    );
    strip_invalid_claude_thinking_blocks_and_empty_messages(
        &mut payload,
        ClaudeValidationOptions::default(),
    );
    let out = payload.to_string();
    let messages = payload["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    assert_eq!(messages.len(), 1, "messages length: {out}");
    assert_eq!(messages[0]["role"], "user", "remaining role");
    assert!(
        !out.contains("gAAAABopenai-encrypted-content") && !out.contains("codex reasoning"),
        "invalid thinking block was preserved: {out}"
    );
}

#[test]
fn strip_invalid_claude_thinking_blocks_removes_malformed_e_prefix() {
    let mut payload = json(
        r#"{
            "messages": [{"role":"assistant","content":[
                {"type":"thinking","thinking":"bad","signature":"Ebad"},
                {"type":"text","text":"Answer"}
            ]}]
        }"#,
    );
    strip_invalid_claude_thinking_blocks(&mut payload, ClaudeValidationOptions::default());
    let out = payload.to_string();
    assert_eq!(first_content(&payload).len(), 1, "content length: {out}");
    assert!(
        !out.contains("Ebad") && !out.contains("bad"),
        "malformed E-prefix thinking block was preserved: {out}"
    );
}

#[test]
fn strip_invalid_claude_thinking_blocks_base64_only_keeps_decodable_e_prefix() {
    let mut payload = json(
        r#"{
            "messages": [{"role":"assistant","content":[
                {"type":"thinking","thinking":"bad","signature":"Ebad"},
                {"type":"text","text":"Answer"}
            ]}]
        }"#,
    );
    strip_invalid_claude_thinking_blocks(
        &mut payload,
        ClaudeValidationOptions {
            base64_only: true,
            ..Default::default()
        },
    );
    assert_eq!(
        first_content(&payload).len(),
        2,
        "content length: {payload}"
    );
}

#[test]
fn strip_invalid_claude_thinking_blocks_base64_only_removes_invalid_base64() {
    let mut payload = json(
        r#"{
            "messages": [{"role":"assistant","content":[
                {"type":"thinking","thinking":"bad","signature":"E!!!invalid!!!"},
                {"type":"text","text":"Answer"}
            ]}]
        }"#,
    );
    strip_invalid_claude_thinking_blocks(
        &mut payload,
        ClaudeValidationOptions {
            base64_only: true,
            ..Default::default()
        },
    );
    let out = payload.to_string();
    assert_eq!(first_content(&payload).len(), 1, "content length: {out}");
    assert!(
        !out.contains("E!!!invalid!!!") && !out.contains("bad"),
        "invalid-base64 thinking block was preserved: {out}"
    );
}

#[test]
fn strip_invalid_claude_thinking_blocks_allows_empty_signature_empty_text_placeholder() {
    let mut payload = json(
        r#"{
            "messages": [{"role":"assistant","content":[
                {"type":"thinking","text":"","signature":""},
                {"type":"text","text":"Answer"}
            ]}]
        }"#,
    );
    strip_invalid_claude_thinking_blocks(
        &mut payload,
        ClaudeValidationOptions {
            base64_only: true,
            allow_empty_signature_with_empty_text: true,
            ..Default::default()
        },
    );
    assert_eq!(
        first_content(&payload).len(),
        2,
        "content length: {payload}"
    );
}

#[test]
fn strip_invalid_claude_thinking_blocks_strict_removes_malformed_claude_tree() {
    let sig = STANDARD.encode([0x12, 0xff, 0xfe, 0xfd]);
    let mut payload = serde_json::json!({"messages":[{"role":"assistant","content":[
        {"type":"thinking","thinking":"bad","signature":sig},
        {"type":"text","text":"Answer"}
    ]}]});
    strip_invalid_claude_thinking_blocks(&mut payload, ClaudeValidationOptions::STRICT);
    let out = payload.to_string();
    assert_eq!(first_content(&payload).len(), 1, "content length: {out}");
    assert!(
        !out.contains(&sig) && !out.contains("bad"),
        "strict-invalid thinking block was preserved: {out}"
    );
}

#[test]
fn strip_invalid_claude_thinking_blocks_keeps_claude_signature_prefixes() {
    let single_layer = STANDARD.encode([0x12, 0x34]);
    let double_layer = STANDARD.encode(&single_layer);
    let mut payload = serde_json::json!({"messages":[{"role":"assistant","content":[
        {"type":"thinking","thinking":"one","signature":single_layer},
        {"type":"thinking","thinking":"two","signature":format!("modelGroup#{double_layer}")}
    ]}]});
    strip_invalid_claude_thinking_blocks(&mut payload, ClaudeValidationOptions::default());
    assert_eq!(
        first_content(&payload).len(),
        2,
        "content length: {payload}"
    );
}

#[test]
fn claude_cais_signature_observed_fable5_sample() {
    assert!(
        is_valid_claude_cais_signature(OBSERVED_FABLE5_SAMPLE),
        "is_valid_claude_cais_signature(OBSERVED_FABLE5_SAMPLE) = false, want true"
    );
    let info = inspect_claude_cais_signature(OBSERVED_FABLE5_SAMPLE)
        .unwrap_or_else(|err| panic!("inspect_claude_cais_signature failed: {err}"));
    assert_eq!(info.model_text, "claude-fable-5", "model_text");
    assert_eq!(info.block_kind, "thinking", "block_kind");
    assert_eq!(
        info.context_id, "d9743975-4bb0-4936-9e28-d5b0d21bdc48",
        "context_id"
    );
    assert_eq!(info.first_byte, 0x08, "first_byte");
}

#[test]
fn claude_caqs_signature_observed_fable51_sample() {
    assert!(
        is_valid_claude_cais_signature(OBSERVED_FABLE51_CAQS_SAMPLE),
        "is_valid_claude_cais_signature(OBSERVED_FABLE51_CAQS_SAMPLE) = false, want true"
    );
    let info = inspect_claude_cais_signature(OBSERVED_FABLE51_CAQS_SAMPLE)
        .unwrap_or_else(|err| panic!("inspect_claude_cais_signature failed: {err}"));
    assert_eq!(info.envelope_version, 4, "envelope_version");
    assert_eq!(info.channel_id, 17, "channel_id");
    assert_eq!(info.block_kind, "thinking", "block_kind");
    assert_eq!(info.first_byte, 0x08, "first_byte");
    assert_eq!(info.signature_len, 4064, "signature_len");
    assert_eq!(
        detect_signature_provider(OBSERVED_FABLE51_CAQS_SAMPLE),
        Provider::Claude,
        "detect_signature_provider(OBSERVED_FABLE51_CAQS_SAMPLE)"
    );
}

#[test]
fn claude_cais_signature_observed_fable51_caqs_narration_sample() {
    assert!(
        is_valid_claude_cais_signature(OBSERVED_FABLE51_CAQS_NARRATION_SAMPLE),
        "is_valid_claude_cais_signature(OBSERVED_FABLE51_CAQS_NARRATION_SAMPLE) = false, want true"
    );
    let info = inspect_claude_cais_signature(OBSERVED_FABLE51_CAQS_NARRATION_SAMPLE)
        .unwrap_or_else(|err| panic!("inspect_claude_cais_signature failed: {err}"));
    assert_eq!(info.envelope_version, 4, "envelope_version");
    assert_eq!(info.channel_id, 17, "channel_id");
    assert_eq!(info.block_kind, "narration", "block_kind");
    assert_eq!(info.first_byte, 0x08, "first_byte");
    assert_eq!(info.signature_len, 883, "signature_len");
    assert_eq!(
        detect_signature_provider(OBSERVED_FABLE51_CAQS_NARRATION_SAMPLE),
        Provider::Claude,
        "detect_signature_provider(OBSERVED_FABLE51_CAQS_NARRATION_SAMPLE)"
    );
}

#[test]
fn claude_caqs_signature_rejects_malformed_payloads() {
    let cases = [
        (
            "invalid block kind",
            caqs_sample_with_block_kind(b"unknown!"),
        ),
        (
            "unrecognized block kind",
            caqs_sample_with_block_kind(b"redacted"),
        ),
    ];
    for (name, signature) in &cases {
        assert!(
            !is_valid_claude_cais_signature(signature),
            "is_valid_claude_cais_signature({name}) = true, want false"
        );
        assert!(
            inspect_claude_cais_signature(signature).is_err(),
            "inspect_claude_cais_signature({name}) succeeded, want error"
        );
    }
}

#[test]
fn claude_cais_signature_detect_signature_provider() {
    let prefixes = [
        "",
        "ccmax#",
        "claude-code-max#",
        "claude_code_max#",
        "cais#",
        "claude-cais#",
        "claude_cais#",
        "claude#",
    ];
    for prefix in prefixes {
        let sig = format!("{prefix}{OBSERVED_FABLE5_SAMPLE}");
        assert_eq!(
            detect_signature_provider(&sig),
            Provider::Claude,
            "detect_signature_provider with prefix {prefix:?}"
        );
    }
}

#[test]
fn claude_cais_signature_observed_opus5_layout() {
    let signature = test_claude_cais_signature("claude-opus-5");
    let info = inspect_claude_cais_signature(&signature)
        .unwrap_or_else(|err| panic!("inspect_claude_cais_signature failed: {err}"));
    assert_eq!(info.model_text, "claude-opus-5", "model_text");
    let decision = decide_signature_compatibility_for_model(
        Provider::Claude,
        "claude-opus-5",
        &signature,
        BlockKind::ClaudeThinking,
    );
    assert!(
        decision.compatible
            && decision.normalized_signature == signature
            && decision.detected_provider == Provider::Claude,
        "same-model opus-5 decision = {decision:?}, want preserved with detected_provider Claude"
    );
}

#[test]
fn claude_cais_signature_not_compatible_with_gemini() {
    assert_eq!(
        compatible_signature_for_provider(Provider::Gemini, OBSERVED_FABLE5_SAMPLE),
        None,
        "compatible_signature_for_provider(Gemini)"
    );
    assert!(
        !is_signature_compatible_with_provider(Provider::Gemini, OBSERVED_FABLE5_SAMPLE),
        "is_signature_compatible_with_provider(Gemini) = true, want false"
    );
    assert!(
        !is_recognized_gemini_provider_signature(OBSERVED_FABLE5_SAMPLE),
        "is_recognized_gemini_provider_signature = true, want false"
    );
    assert!(
        inspect_gemini_thought_signature(
            OBSERVED_FABLE5_SAMPLE,
            GeminiValidationOptions::default()
        )
        .is_err(),
        "inspect_gemini_thought_signature should fail for a Claude CAIS signature"
    );
}

#[test]
fn claude_cais_signature_compatible_with_all_claude_targets() {
    let decision = decide_signature_compatibility_for_model(
        Provider::Claude,
        "claude-fable-5",
        OBSERVED_FABLE5_SAMPLE,
        BlockKind::ClaudeThinking,
    );
    assert!(
        decision.compatible
            && decision.action == Action::Preserve
            && decision.normalized_signature == OBSERVED_FABLE5_SAMPLE
            && decision.detected_provider == Provider::Claude,
        "decide(Claude, claude-fable-5) = {decision:?}, want compatible and preserved with detected_provider Claude"
    );

    let decision_case = decide_signature_compatibility_for_model(
        Provider::Claude,
        "CLAUDE-FABLE-5",
        OBSERVED_FABLE5_SAMPLE,
        BlockKind::ClaudeThinking,
    );
    assert!(
        decision_case.compatible && decision_case.action == Action::Preserve,
        "case-insensitive decision failed: {decision_case:?}"
    );

    let decision_diff = decide_signature_compatibility_for_model(
        Provider::Claude,
        "claude-opus-5",
        OBSERVED_FABLE5_SAMPLE,
        BlockKind::ClaudeThinking,
    );
    assert!(
        decision_diff.compatible
            && decision_diff.action == Action::Preserve
            && decision_diff.normalized_signature == OBSERVED_FABLE5_SAMPLE,
        "decide(Claude, claude-opus-5) = {decision_diff:?}, want compatible and preserved"
    );

    let opus5_sig = test_claude_cais_signature("claude-opus-5");
    let decision_opus_to_opus48 = decide_signature_compatibility_for_model(
        Provider::Claude,
        "claude-opus-4-8",
        &opus5_sig,
        BlockKind::ClaudeThinking,
    );
    assert!(
        decision_opus_to_opus48.compatible
            && decision_opus_to_opus48.action == Action::Preserve
            && decision_opus_to_opus48.normalized_signature == opus5_sig,
        "decide(Claude, claude-opus-4-8) with an opus-5 signature = {decision_opus_to_opus48:?}, want compatible and preserved"
    );

    assert_eq!(
        compatible_signature_for_provider(Provider::Claude, OBSERVED_FABLE5_SAMPLE),
        Some(OBSERVED_FABLE5_SAMPLE.to_owned()),
        "compatible_signature_for_provider(Claude, OBSERVED_FABLE5_SAMPLE)"
    );

    let decision_gemini = decide_signature_compatibility_for_model(
        Provider::Gemini,
        "claude-fable-5",
        OBSERVED_FABLE5_SAMPLE,
        BlockKind::ClaudeThinking,
    );
    assert!(
        !decision_gemini.compatible,
        "decide(Gemini, claude-fable-5) = {decision_gemini:?}, want incompatible"
    );
}

#[test]
fn sanitize_claude_messages_for_claude_upstream_claude_cais() {
    let mut same = serde_json::json!({"messages":[{"role":"assistant","content":[
        {"type":"thinking","thinking":"keep","signature":OBSERVED_FABLE5_SAMPLE},
        {"type":"text","text":"answer"}
    ]}]});
    let report_same =
        sanitize_claude_messages_for_claude_upstream(&mut same, "claude-fable-5", false);
    assert!(
        report_same.preserved == 1 && report_same.dropped_blocks == 0,
        "unexpected report for same model: {report_same:?}"
    );
    assert_eq!(
        first_signature(&same),
        Some(OBSERVED_FABLE5_SAMPLE),
        "signature, want preserved"
    );

    let mut tool = serde_json::json!({"messages":[{"role":"assistant","content":[
        {"type":"thinking","thinking":"keep","signature":OBSERVED_FABLE5_SAMPLE},
        {"type":"text","text":"answer"},
        {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"pwd"},"signature":OBSERVED_FABLE5_SAMPLE}
    ]}]});
    let report_tool =
        sanitize_claude_messages_for_claude_upstream(&mut tool, "claude-fable-5", false);
    assert_eq!(
        report_tool.preserved, 1,
        "unexpected report for tool input: {report_tool:?}"
    );
    let parts_tool = first_content(&tool);
    assert_eq!(parts_tool.len(), 3, "content length: {tool}");
    assert_eq!(
        parts_tool[0]["signature"], OBSERVED_FABLE5_SAMPLE,
        "thinking block signature lost: {}",
        parts_tool[0]
    );
    assert!(
        parts_tool[2].get("signature").is_none(),
        "tool_use signature should be stripped: {}",
        parts_tool[2]
    );

    let mut diff = serde_json::json!({"messages":[{"role":"assistant","content":[
        {"type":"thinking","thinking":"keep","signature":OBSERVED_FABLE5_SAMPLE},
        {"type":"text","text":"answer"}
    ]}]});
    let report_diff =
        sanitize_claude_messages_for_claude_upstream(&mut diff, "claude-opus-5", false);
    assert!(
        report_diff.preserved == 1 && report_diff.dropped_blocks == 0,
        "unexpected report for cross model: {report_diff:?}"
    );
    let parts_diff = first_content(&diff);
    assert_eq!(parts_diff.len(), 2, "content length: {diff}");
    assert_eq!(
        parts_diff[0]["signature"], OBSERVED_FABLE5_SAMPLE,
        "thinking signature"
    );
}

// Validation is deliberately structural: rejecting a signature drops the whole
// thinking block, so incidental values observed today must not become hard
// requirements.
#[test]
fn claude_cais_signature_tolerates_upstream_field_drift() {
    let cases = [
        ("observed layout", ClaudeCaisParts::new("claude-opus-5")),
        ("new channel id", opus5_parts(|p| p.channel_id = 17)),
        ("new envelope version", opus5_parts(|p| p.top_envelope = 3)),
        (
            "no top-level trailer",
            opus5_parts(|p| p.include_top_trailer = false),
        ),
        (
            "no channel version",
            opus5_parts(|p| p.include_channel_version = false),
        ),
        (
            "longer signature bytes",
            opus5_parts(|p| p.signature_len = 96),
        ),
        ("no field 7", opus5_parts(|p| p.include_field7 = false)),
        (
            "other block kind",
            opus5_parts(|p| p.block_kind = "redacted_thinking".to_owned()),
        ),
        ("no block kind", opus5_parts(|p| p.block_kind.clear())),
        ("no context id", opus5_parts(|p| p.context_id.clear())),
        (
            "unreleased model name",
            ClaudeCaisParts::new("claude-opus-6-preview"),
        ),
    ];

    for (name, parts) in &cases {
        let sig = parts.encode();
        if let Err(err) = inspect_claude_cais_signature(&sig) {
            panic!("{name}: inspect_claude_cais_signature failed: {err}");
        }
        assert_eq!(
            detect_signature_provider_for_block(&sig, BlockKind::ClaudeThinking),
            Provider::Claude,
            "{name}: detect_signature_provider_for_block"
        );

        let mut payload = thinking_request(&sig);
        let report =
            sanitize_claude_messages_for_claude_upstream(&mut payload, "claude-opus-5", false);
        assert!(
            report.preserved == 1 && report.dropped_blocks == 0,
            "{name}: report = {report:?}, want preserved thinking block"
        );
        assert_eq!(
            first_signature(&payload),
            Some(sig.as_str()),
            "{name}: signature, want preserved"
        );
    }
}

#[test]
fn claude_cais_signature_rejects_malformed_payloads() {
    let decoded = STANDARD
        .decode(OBSERVED_FABLE5_SAMPLE)
        .expect("decode observed sample");
    let truncated = STANDARD.encode(&decoded[..decoded.len() / 2]);

    let cases = [
        ("empty", String::new()),
        ("whitespace only", "   ".to_owned()),
        ("not base64", "CAIS!!!not-base64".to_owned()),
        ("truncated payload", truncated),
        // 'E' prefix is the classic Claude form and must not reach CAIS parsing.
        ("classic claude prefix", STANDARD.encode([0x12, 0x00])),
        // A 'C' prefix with a non-0x08 marker byte is the only way to reach the
        // marker check, since 'C' constrains the first byte to 0x08..=0x0b.
        ("wrong marker byte", STANDARD.encode([0x0a, 0x00])),
        (
            "no container",
            opus5_parts(|p| p.include_container = false).encode(),
        ),
        (
            "no channel block",
            opus5_parts(|p| p.include_channel_block = false).encode(),
        ),
        (
            "no channel id",
            opus5_parts(|p| p.include_channel_id = false).encode(),
        ),
        (
            "channel id wrong wire type",
            opus5_parts(|p| p.channel_id_as_bytes = true).encode(),
        ),
        (
            "no signature bytes",
            opus5_parts(|p| p.include_signature = false).encode(),
        ),
        (
            "empty signature bytes",
            opus5_parts(|p| p.signature_len = 0).encode(),
        ),
        (
            "no model text",
            opus5_parts(|p| p.include_model_text = false).encode(),
        ),
        (
            "foreign model text",
            ClaudeCaisParts::new("gemini-3-pro").encode(),
        ),
        (
            "invalid utf-8 model text",
            opus5_parts(|p| p.model_text = b"claude-\xff\xfe".to_vec()).encode(),
        ),
        (
            "non-uuid context id",
            opus5_parts(|p| p.context_id = "not-a-canonical-uuid-value-000000000".to_owned())
                .encode(),
        ),
    ];

    for (name, signature) in &cases {
        assert!(
            !is_valid_claude_cais_signature(signature),
            "{name}: is_valid_claude_cais_signature({signature:?}) = true, want false"
        );
    }
}

// CAIS validation runs before classic Claude validation, so it must not claim
// E/R signatures and change how they are normalized.
#[test]
fn claude_cais_signature_does_not_shadow_classic_claude_signature() {
    let classic = test_claude_thinking_signature();
    assert!(
        !is_valid_claude_cais_signature(&classic),
        "is_valid_claude_cais_signature(classic Claude signature) = true, want false"
    );
    assert_eq!(
        detect_signature_provider_for_block(&classic, BlockKind::ClaudeThinking),
        Provider::Claude,
        "detect_signature_provider_for_block(classic)"
    );

    let mut payload = thinking_request(&classic);
    let report =
        sanitize_claude_messages_for_claude_upstream(&mut payload, "claude-sonnet-4-6", false);
    assert!(
        report.preserved == 1 && report.dropped_blocks == 0,
        "report = {report:?}, want preserved classic thinking block"
    );
    assert_eq!(
        first_signature(&payload),
        Some(classic.as_str()),
        "signature, want the provider-native E form"
    );
}

// The cache collapses every Claude model to the "claude" prefix, so a CAIS
// signature reaches the sanitizer as "claude#..." and must be replayed with the
// prefix stripped instead of being dropped.
#[test]
fn claude_cais_signature_cache_prefix_survives_claude_upstream_sanitize() {
    for prefix in ["claude#", "anthropic#", "cais#", "ccmax#"] {
        let mut payload = thinking_request(&format!("{prefix}{OBSERVED_FABLE5_SAMPLE}"));
        let report =
            sanitize_claude_messages_for_claude_upstream(&mut payload, "claude-fable-5", false);
        assert!(
            report.preserved == 1 && report.dropped_blocks == 0,
            "{prefix}: report = {report:?}, want preserved thinking block"
        );
        assert_eq!(
            first_signature(&payload),
            Some(OBSERVED_FABLE5_SAMPLE),
            "{prefix}: signature, want unprefixed"
        );
    }
}

#[test]
fn compatible_antigravity_claude_thinking_signature_rejects_claude_cais() {
    for prefix in ["", "ccmax#", "cais#", "claude-cais#"] {
        let sig = format!("{prefix}{OBSERVED_FABLE5_SAMPLE}");
        assert_eq!(
            compatible_antigravity_claude_thinking_signature(&sig),
            None,
            "compatible_antigravity_claude_thinking_signature({prefix}ClaudeCAIS)"
        );
    }
}
