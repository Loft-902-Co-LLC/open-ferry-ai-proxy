// Ported from CLIProxyAPI internal/signature/provider_compatibility_test.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI

use super::*;

// Pins the charset check against the alphabets it stands in for. A wrong check
// would silently accept bytes that are not valid base64, or reject a legal
// payload character. Upstream checks its lookup tables over every byte value;
// a `&str` can't hold a lone byte above 0x7f, so the Latin-1 characters
// U+0080..=U+00FF and a few wider ones stand in for those and must be rejected.
#[test]
fn base64_alphabet_set_matches_encoder_alphabets() {
    let cases: [(&str, &[u8], &str); 2] = [
        (
            "grok unpadded std",
            b"+/",
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
        ),
        (
            "gpt base64url",
            b"-_=",
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_=",
        ),
    ];
    for (name, extra, alphabet) in cases {
        for c in 0..=u8::MAX {
            let ch = char::from(c);
            let want = c.is_ascii() && alphabet.as_bytes().contains(&c);
            let got = first_invalid_base64_char(&ch.to_string(), extra);
            assert_eq!(
                got.is_none(),
                want,
                "{name}: byte 0x{c:02x} ({ch:?}) accepted={}, want {want}",
                got.is_none()
            );
            if !want {
                assert_eq!(got, Some((0, ch)), "{name}: byte 0x{c:02x} ({ch:?})");
            }
        }
        for ch in ['\u{2026}', '\u{ff21}', '\u{1f600}'] {
            assert_eq!(
                first_invalid_base64_char(&ch.to_string(), extra),
                Some((0, ch)),
                "{name}: {ch:?} must be rejected"
            );
        }
    }
}

/// `replaySafeEnvelopeFixtures`: one fixture per self-describing provider
/// envelope that carries replayable state. Every entry must survive the
/// structural pre-filter, because losing one would silently reclassify that
/// provider.
fn replay_safe_envelope_fixtures() -> [(&'static str, String, Provider); 6] {
    [
        (
            "claude single-layer E",
            test_claude_thinking_signature(),
            Provider::Claude,
        ),
        (
            "claude double-layer R",
            test_unpadded_antigravity_claude_thinking_signature(),
            Provider::Claude,
        ),
        (
            "claude CAIS",
            test_claude_cais_signature("claude-fable-5"),
            Provider::Claude,
        ),
        (
            "antigravity CAQS",
            signaturetest::antigravity_caqs(),
            Provider::Claude,
        ),
        (
            "gemini protobuf field2",
            test_gemini_thought_signature_envelope(),
            Provider::Gemini,
        ),
        ("gpt fernet", test_gpt_reasoning_signature(), Provider::Gpt),
    ]
}

// Guards the structural pre-filter. Detection skips every provider validator
// when `maybe_self_describing_envelope` returns false, so an envelope missing
// from `SELF_DESCRIBING_FIRST_CHARS` would silently fall through to the
// residual class. Adding a provider envelope without registering its base64
// first character fails here.
#[test]
fn self_describing_signature_first_chars_covers_every_known_envelope() {
    let first_chars = String::from_utf8_lossy(SELF_DESCRIBING_FIRST_CHARS);
    for (name, sig, _) in replay_safe_envelope_fixtures() {
        assert!(
            maybe_self_describing_envelope(&sig),
            "{name}: first char {:?} is not in SELF_DESCRIBING_FIRST_CHARS {first_chars:?}; \
             register it or detection will skip this envelope",
            &sig[..1]
        );
    }

    // The pre-filter must not be so wide that it stops filtering. Opaque xAI
    // ciphertext is the shape it exists to reject.
    for sig in [
        "K1ZAIbzDbO",
        "jQDLUr+fD8RFP8nbkkfI",
        "qcgG7jzxH3D6mlVLBBaKXaG3",
    ] {
        assert!(
            !maybe_self_describing_envelope(sig),
            "opaque ciphertext {sig:?} must not look like a self-describing envelope"
        );
    }
}

// Documents why ascii_uuid is excluded from `SELF_DESCRIBING_FIRST_CHARS`. Its
// first byte is the first hex character of the UUID, so its base64 first
// character spreads over several values, and none need registering: the
// envelope is never replay-safe, so it resolves to `Provider::Unknown` either
// way, and Gemini model parts recover it through the bypass sentinel keyed on
// block kind.
#[test]
fn gemini_ascii_uuid_is_gate_independent() {
    // First hex digit chosen to land on distinct base64 first characters.
    for uuid in [
        "09743975-4bb0-4936-9e28-d5b0d21bdc48",
        "49743975-4bb0-4936-9e28-d5b0d21bdc48",
        "89743975-4bb0-4936-9e28-d5b0d21bdc48",
        "a9743975-4bb0-4936-9e28-d5b0d21bdc48",
        "e9743975-4bb0-4936-9e28-d5b0d21bdc48",
    ] {
        let sig = test_gemini_thought_signature(uuid.as_bytes());
        assert_eq!(
            detect_signature_provider(&sig),
            Provider::Unknown,
            "uuid {:?}: detect_signature_provider must be Unknown regardless of the pre-filter",
            &uuid[..8]
        );
        let decision =
            decide_signature_compatibility(Provider::Gemini, &sig, BlockKind::GeminiFunctionCall);
        assert_eq!(
            decision.action,
            Action::ReplaceWithGeminiBypass,
            "uuid {:?}: action",
            &uuid[..8]
        );
    }
}

// Pins the classification of each envelope so a reordering of the validator
// chain cannot silently reassign one provider's signatures to another.
#[test]
fn detect_signature_provider_for_block_classifies_every_known_envelope() {
    for (name, sig, want) in replay_safe_envelope_fixtures() {
        assert_eq!(
            detect_signature_provider(&sig),
            want,
            "{name}: detect_signature_provider"
        );
    }
}

// Pins the invariant that keeps Claude and Gemini separable regardless of probe
// order in detection. Gemini validates wire shape only and has no literal
// marker, so it is the weakest judge; Claude envelopes survive it solely
// because they carry top-level fields beyond the container and so fail
// Gemini's single-record shape. Loosening the Gemini envelope check would make
// probe order start mattering, and fails here first.
#[test]
fn gemini_envelope_never_claims_claude_signatures() {
    let cases = [
        ("single-layer E", test_claude_thinking_signature()),
        (
            "single-layer E opaque",
            test_claude_thinking_signature_with_opaque_len(64),
        ),
        (
            "double-layer R",
            test_unpadded_antigravity_claude_thinking_signature(),
        ),
        (
            "CAIS synthetic",
            test_claude_cais_signature("claude-opus-5"),
        ),
        ("CAIS observed", OBSERVED_FABLE5_SAMPLE.to_owned()),
    ];
    for (name, sig) in cases {
        // Upstream also passes `SignatureBlockKindUnknown`, which the check ignores.
        assert!(
            !is_recognized_gemini_provider_signature(&sig),
            "claude {name} is claimed by the Gemini envelope check; probe order in detection is now load-bearing"
        );
        assert_eq!(
            detect_signature_provider(&sig),
            Provider::Claude,
            "claude {name}: detect_signature_provider"
        );
        assert_eq!(
            compatible_signature_for_provider(Provider::Gemini, &sig),
            None,
            "claude {name} must not be replayable as a Gemini signature"
        );
    }
}

#[test]
fn detect_signature_provider_uses_provider_prefix() {
    let claude_sig = format!("claude#{}", test_claude_thinking_signature());
    assert_eq!(
        detect_signature_provider(&claude_sig),
        Provider::Claude,
        "detect_signature_provider(claude#...)"
    );

    let gemini_sig = format!(
        "gemini#{}",
        test_gemini3_thought_signature(&[0x01, 0x0c, 0x39])
    );
    assert_eq!(
        detect_signature_provider(&gemini_sig),
        Provider::Gemini,
        "detect_signature_provider(gemini#...)"
    );
}

#[test]
fn detect_signature_provider_rejects_misleading_claude_prefix() {
    let mislabeled_gemini_sig = format!(
        "claude#{}",
        test_gemini3_thought_signature(&[0x01, 0x0c, 0x39])
    );
    assert_eq!(
        detect_signature_provider(&mislabeled_gemini_sig),
        Provider::Unknown,
        "detect_signature_provider(mislabeled claude#Gemini)"
    );
}

#[test]
fn detect_signature_provider_gemini3_e_prefix_does_not_look_claude() {
    // This byte shape base64-encodes with an E prefix but is a Gemini field-2
    // envelope, not a Claude thinking-signature tree.
    let gemini_sig = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39, 0xd6, 0xc7, 0x34]);
    assert!(
        gemini_sig.starts_with('E'),
        "test signature should start with E, got {:?}",
        &gemini_sig[..1]
    );
    assert_eq!(
        detect_signature_provider(&gemini_sig),
        Provider::Gemini,
        "detect_signature_provider(Gemini E-prefix)"
    );
}

#[test]
fn compatible_signature_for_provider_claude_uses_provider_native_e_form() {
    let native_sig = test_claude_thinking_signature();
    let double_encoded = STANDARD.encode(&native_sig);

    assert_eq!(
        compatible_signature_for_provider(Provider::Claude, &double_encoded),
        Some(native_sig),
        "a double-layer Claude signature should be compatible, in the provider-native form"
    );
}

#[test]
fn compatible_antigravity_claude_thinking_signature_uses_double_layer_r_form() {
    let native_sig = test_claude_thinking_signature();
    let expected = STANDARD.encode(&native_sig);

    assert_eq!(
        compatible_antigravity_claude_thinking_signature(&native_sig),
        Some(expected),
        "a Claude signature should be compatible with Antigravity Claude, in the R form"
    );
}

#[test]
fn compatible_antigravity_claude_thinking_signature_rejects_gemini_e_prefix() {
    let gemini_sig = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39, 0xd6, 0xc7, 0x34]);
    assert!(
        gemini_sig.starts_with('E'),
        "test signature should start with E, got {:?}",
        &gemini_sig[..1]
    );
    assert_eq!(
        compatible_antigravity_claude_thinking_signature(&gemini_sig),
        None,
        "a Gemini E-prefix signature must be rejected"
    );
}

#[test]
fn detect_signature_provider_does_not_classify_arbitrary_base64_as_gemini() {
    let opaque = test_gemini_thought_signature(&[0x45, 0x12]);
    assert_eq!(
        detect_signature_provider(&opaque),
        Provider::Unknown,
        "detect_signature_provider(arbitrary base64)"
    );
}

#[test]
fn gemini_ascii_uuid_signature_uses_bypass() {
    let plain_uuid = "e24830a7-5cd6-42fe-998b-ee539e72b9c3";
    let sig = test_gemini_thought_signature(plain_uuid.as_bytes());

    assert_eq!(
        detect_signature_provider(plain_uuid),
        Provider::Unknown,
        "detect_signature_provider(plain UUID)"
    );
    assert_eq!(
        detect_signature_provider(&format!("gemini#{plain_uuid}")),
        Provider::Unknown,
        "detect_signature_provider(gemini#plain UUID)"
    );

    assert_eq!(
        detect_signature_provider(&sig),
        Provider::Unknown,
        "detect_signature_provider(UUID)"
    );
    assert_eq!(
        detect_signature_provider(&format!("gemini#{sig}")),
        Provider::Unknown,
        "detect_signature_provider(gemini#UUID)"
    );
    assert_eq!(
        detect_signature_provider_for_block(&sig, BlockKind::GeminiFunctionCall),
        Provider::Unknown,
        "detect_signature_provider_for_block(UUID tool call)"
    );
    assert_eq!(
        compatible_signature_for_provider(Provider::Gemini, &sig),
        None,
        "UUID signature should not be compatible"
    );
    assert_eq!(
        compatible_signature_for_provider_block(
            Provider::Gemini,
            &sig,
            BlockKind::GeminiFunctionCall
        ),
        None,
        "UUID tool-call signature should not be compatible"
    );
    let decision =
        decide_signature_compatibility(Provider::Gemini, &sig, BlockKind::GeminiFunctionCall);
    assert_eq!(
        decision.action,
        Action::ReplaceWithGeminiBypass,
        "function-call UUID action"
    );
    assert_eq!(
        decision.replacement_signature, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "function-call UUID replacement"
    );
    let decision =
        decide_signature_compatibility(Provider::Gemini, &sig, BlockKind::GeminiModelPart);
    assert_eq!(
        decision.action,
        Action::ReplaceWithGeminiBypass,
        "model-part UUID action"
    );
}

#[test]
fn gemini_wrapped_uuid_function_call_signature_is_compatible() {
    let sig = test_gemini3_thought_signature(b"e24830a7-5cd6-42fe-998b-ee539e72b9c3");

    assert_eq!(
        detect_signature_provider(&sig),
        Provider::Gemini,
        "detect_signature_provider(wrapped UUID)"
    );
    assert_eq!(
        detect_signature_provider_for_block(&sig, BlockKind::GeminiFunctionCall),
        Provider::Gemini,
        "detect_signature_provider_for_block(wrapped UUID tool call)"
    );
    assert_eq!(
        compatible_signature_for_provider_block(
            Provider::Gemini,
            &sig,
            BlockKind::GeminiFunctionCall
        ),
        Some(sig.clone()),
        "wrapped UUID tool-call signature should be kept as is"
    );
    for block_kind in [BlockKind::GeminiFunctionCall, BlockKind::GeminiModelPart] {
        let decision = decide_signature_compatibility(Provider::Gemini, &sig, block_kind);
        assert!(
            decision.compatible
                && decision.action == Action::Preserve
                && decision.normalized_signature == sig,
            "wrapped UUID decision for {} = {decision:?}, want preserved",
            block_kind.as_str()
        );
    }
}

#[test]
fn compatible_signature_for_provider_strips_gemini_prefix() {
    let sig = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39]);
    assert_eq!(
        compatible_signature_for_provider(Provider::Gemini, &format!("gemini#{sig}")),
        Some(sig),
        "a gemini-prefixed signature should be compatible with Gemini, without its prefix"
    );
}

#[test]
fn split_signature_provider_prefix_uses_strict_provider_aliases() {
    let gpt_sig = format!("gpt#{}", test_gpt_reasoning_signature());
    assert_eq!(
        detect_signature_provider(&gpt_sig),
        Provider::Gpt,
        "detect_signature_provider(gpt#...)"
    );

    let mislabeled_prefix = format!("claude-cache#{}", test_claude_thinking_signature());
    assert_eq!(
        split_signature_provider_prefix(&mislabeled_prefix),
        None,
        "claude-cache# should not be accepted as an explicit provider prefix"
    );
    assert_eq!(
        detect_signature_provider(&mislabeled_prefix),
        Provider::Unknown,
        "detect_signature_provider(claude-cache#...)"
    );
}

#[test]
fn decide_signature_compatibility_gemini_function_call_uses_bypass() {
    let decision = decide_signature_compatibility(
        Provider::Gemini,
        &format!("claude#{}", test_claude_thinking_signature()),
        BlockKind::GeminiFunctionCall,
    );
    assert_eq!(decision.action, Action::ReplaceWithGeminiBypass, "action");
    assert_eq!(
        decision.replacement_signature, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
        "replacement_signature"
    );
}

#[test]
fn sanitize_claude_messages_signatures_for_model_normalizes_same_provider_claude() {
    let native_sig = test_claude_thinking_signature();
    let sig = format!("claude#{native_sig}");
    let mut payload = json(&format!(
        r#"{{"model":"claude-sonnet","messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"keep","signature":"{sig}"}},{{"type":"text","text":"answer"}}]}}]}}"#
    ));
    let expected_sig = normalize_claude_provider_native_thinking_signature(
        &native_sig,
        ClaudeValidationOptions::default(),
    )
    .expect("normalize_claude_provider_native_thinking_signature");

    let report = sanitize_claude_messages_signatures_for_model(&mut payload, "claude-sonnet-4-5");
    assert!(
        report.preserved == 1 && report.dropped_blocks == 0,
        "unexpected report: {report:?}"
    );
    assert_eq!(
        payload["messages"][0]["content"][0]["signature"].as_str(),
        Some(expected_sig.as_str()),
        "signature should be normalized"
    );
}

#[test]
fn sanitize_claude_messages_signatures_for_model_drops_claude_thinking_for_gemini() {
    let sig = format!("claude#{}", test_claude_thinking_signature());
    let mut payload = json(&format!(
        r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"drop","signature":"{sig}"}},{{"type":"text","text":"answer"}}]}}]}}"#
    ));

    let report = sanitize_claude_messages_signatures_for_model(&mut payload, "gemini-3.5-flash");
    assert_eq!(
        report.dropped_blocks, 1,
        "dropped_blocks; report={report:?}"
    );
    let content = payload["messages"][0]["content"]
        .as_array()
        .expect("content array");
    assert_eq!(content.len(), 1, "content length: {payload}");
    assert_eq!(
        content[0]["text"].as_str(),
        Some("answer"),
        "remaining text"
    );
}

#[test]
fn sanitize_claude_messages_signatures_for_model_preserves_gemini_thinking_for_gemini() {
    let native_sig = test_gemini3_thought_signature(&[0x01, 0x0c, 0x39]);
    let sig = format!("gemini#{native_sig}");
    let mut payload = json(&format!(
        r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"keep","signature":"{sig}"}},{{"type":"text","text":"answer"}}]}}]}}"#
    ));

    let report = sanitize_claude_messages_signatures_for_model(&mut payload, "gemini-3.5-flash");
    assert!(
        report.preserved == 1 && report.dropped_blocks == 0,
        "unexpected report: {report:?}"
    );
    assert_eq!(
        payload["messages"][0]["content"][0]["signature"].as_str(),
        Some(native_sig.as_str()),
        "signature should be normalized"
    );
}

#[test]
fn sanitize_claude_messages_signatures_for_model_preserves_gpt_for_gpt() {
    let sig = test_gpt_reasoning_signature();
    let mut payload = json(&format!(
        r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"keep","signature":"{sig}"}},{{"type":"text","text":"answer"}}]}}]}}"#
    ));

    let report = sanitize_claude_messages_signatures_for_model(&mut payload, "gpt-5.2");
    assert!(
        report.preserved == 1 && report.dropped_blocks == 0,
        "unexpected report: {report:?}"
    );
    assert_eq!(
        payload["messages"][0]["content"][0]["signature"].as_str(),
        Some(sig.as_str()),
        "signature should be preserved"
    );
}

#[test]
fn sanitize_claude_messages_signatures_for_model_drops_empty_assistant_message() {
    let sig = format!("claude#{}", test_claude_thinking_signature());
    let mut payload = json(&format!(
        r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"drop","signature":"{sig}"}}]}},{{"role":"user","content":[{{"type":"text","text":"next"}}]}}]}}"#
    ));

    let report = sanitize_claude_messages_signatures_for_model(&mut payload, "gpt-5.2");
    assert_eq!(
        report.dropped_blocks, 1,
        "dropped_blocks; report={report:?}"
    );
    let messages = payload["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1, "messages length: {payload}");
    assert_eq!(messages[0]["role"].as_str(), Some("user"), "remaining role");
}

#[test]
fn sanitize_claude_messages_for_claude_upstream_drops_invalid_thinking_and_cleans_tool_use() {
    let mut payload = json(
        r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"drop me","signature":""},{"type":"text","text":"answer"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"git status"},"signature":"bad","thoughtSignature":"bad2","thought_signature":"bad3","model":"claude-sonnet-4-5","extra_content":{"google":{"thought_signature":"bad4"}}}]}]}"#,
    );

    let report =
        sanitize_claude_messages_for_claude_upstream(&mut payload, "claude-sonnet-4-5", false);
    assert_eq!(
        report.dropped_blocks, 1,
        "dropped_blocks; report={report:?}"
    );
    let parts = payload["messages"][0]["content"]
        .as_array()
        .expect("content array");
    assert_eq!(parts.len(), 2, "content length: {payload}");
    assert_eq!(
        parts[0]["type"].as_str(),
        Some("text"),
        "first remaining part = {}, want text",
        parts[0]
    );
    let tool_use = &parts[1];
    assert_eq!(
        tool_use["type"].as_str(),
        Some("tool_use"),
        "second remaining part = {tool_use}, want tool_use"
    );
    assert_eq!(tool_use["id"].as_str(), Some("toolu_1"), "tool_use id");
    for path in [
        "signature",
        "thoughtSignature",
        "thought_signature",
        "model",
        "extra_content",
    ] {
        assert!(
            tool_use.get(path).is_none(),
            "tool_use.{path} should be removed: {tool_use}"
        );
    }
}

#[test]
fn sanitize_claude_messages_for_claude_upstream_normalizes_valid_thinking_and_drops_empty_message()
{
    let native_sig = test_claude_thinking_signature();
    let double_encoded = STANDARD.encode(&native_sig);
    let mut payload = json(&format!(
        r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"keep","signature":"{double_encoded}"}},{{"type":"text","text":"answer"}}]}},{{"role":"assistant","content":[{{"type":"thinking","thinking":"drop"}}]}},{{"role":"user","content":[{{"type":"text","text":"next"}}]}}]}}"#
    ));

    let report =
        sanitize_claude_messages_for_claude_upstream(&mut payload, "claude-sonnet-4-5", false);
    assert!(
        report.preserved == 1 && report.dropped_blocks == 1,
        "unexpected report: {report:?}"
    );
    let messages = payload["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2, "messages length: {payload}");
    assert_eq!(
        messages[0]["content"][0]["signature"].as_str(),
        Some(native_sig.as_str()),
        "signature should be provider-native"
    );
    assert_eq!(
        messages[1]["role"].as_str(),
        Some("user"),
        "remaining second role"
    );
}
