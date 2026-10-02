// Ported from CLIProxyAPI internal/signature/kimi_validation_test.go (v8.0.10,
// MIT). https://github.com/router-for-me/CLIProxyAPI

use super::*;

// Unpadded base64 of 9709 and 3255 bytes is exactly 12946 and 4340 characters,
// so changing one constant without the other would make it unreachable.
#[test]
fn kimi_thinking_signature_lengths_match_decoded_sizes() {
    for (name, decoded_len, raw_len) in [
        (
            "non streaming",
            9709,
            KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN,
        ),
        ("streaming", 3255, KIMI_THINKING_SIGNATURE_STREAMING_LEN),
    ] {
        let sig = synthesize_kimi_signature(decoded_len, 1);
        assert_eq!(sig.len(), raw_len, "{name}");
        let info =
            inspect_kimi_thinking_signature(&sig).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(info.decoded_len, decoded_len, "{name}");
    }
}

#[test]
fn inspect_kimi_thinking_signature_reports_mode() {
    for (decoded_len, mode) in [
        (9709, KimiSignatureMode::NonStreaming),
        (3255, KimiSignatureMode::Streaming),
    ] {
        let info =
            inspect_kimi_thinking_signature(&synthesize_kimi_signature(decoded_len, 7)).unwrap();
        assert_eq!(info.mode, mode);
    }
}

// The core negative test for a size-only probe: one character either way must
// fall out of the family.
#[test]
fn inspect_kimi_thinking_signature_rejects_neighbouring_lengths() {
    for decoded_len in [9709, 3255] {
        let native = synthesize_kimi_signature(decoded_len, 3);
        let short = &native[..native.len() - 1];
        let long = format!("{native}A");
        for sig in [short, &long] {
            assert!(
                !is_valid_kimi_thinking_signature(sig),
                "length {}",
                sig.len()
            );
        }
    }
}

#[test]
fn inspect_kimi_thinking_signature_rejects_malformed_input() {
    let native = synthesize_kimi_signature(3255, 5);
    let cases = [
        ("empty", String::new()),
        ("whitespace only", "   ".to_owned()),
        ("leading whitespace", format!(" {native}")),
        ("trailing whitespace", format!("{native} ")),
        (
            "padded base64",
            format!("{}==", &native[..native.len() - 2]),
        ),
        (
            "non base64 character",
            format!("{}!", &native[..native.len() - 1]),
        ),
        ("provider cache prefix", format!("claude#{native}")),
    ];
    for (name, sig) in cases {
        assert!(!is_valid_kimi_thinking_signature(&sig), "{name}");
    }
}

// A caller that knows the constants and pads to them with structured bytes.
#[test]
fn inspect_kimi_thinking_signature_rejects_low_entropy_filler() {
    for len in [
        KIMI_THINKING_SIGNATURE_STREAMING_LEN,
        KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN,
    ] {
        assert!(
            !is_valid_kimi_thinking_signature(&"A".repeat(len)),
            "length {len}"
        );
    }
}

// Detection runs the envelope probes first, but callers can reach this
// validator directly, so a foreign envelope must not pass on length alone.
#[test]
fn inspect_kimi_thinking_signature_rejects_self_describing_envelope() {
    assert!(!is_valid_kimi_thinking_signature(OBSERVED_FABLE5_SAMPLE));
}

// About 6% of real Kimi signatures start with an envelope character. They must
// still resolve to Kimi once the envelope probes decline, and a real envelope
// must never be captured by the size probe.
#[test]
fn detect_signature_provider_kimi_runs_after_envelope_probes() {
    assert_eq!(
        detect_signature_provider(OBSERVED_FABLE5_SAMPLE),
        Provider::Claude
    );

    let envelope_prefixed: Vec<String> = (0..200)
        .map(|seed| synthesize_kimi_signature(3255, seed))
        .filter(|sig| maybe_self_describing_envelope(sig))
        .take(3)
        .collect();
    assert!(!envelope_prefixed.is_empty());
    for sig in envelope_prefixed {
        assert_eq!(detect_signature_provider(&sig), Provider::Kimi);
    }
}

#[test]
fn inspect_grok_encrypted_content_rejects_kimi_lengths() {
    for decoded_len in [9709, 3255] {
        let sig = synthesize_kimi_signature(decoded_len, 11);
        assert!(
            !is_valid_grok_encrypted_content(&sig),
            "{decoded_len} bytes"
        );
    }
}

#[test]
fn signature_provider_from_model_name_kimi() {
    for (model, want) in [
        ("kimi-k3", Provider::Kimi),
        ("kimi-k3-256k", Provider::Kimi),
        ("kimi-k2.7-code-highspeed", Provider::Kimi),
        ("k3", Provider::Kimi),
        ("k2-thinking", Provider::Kimi),
        ("moonshot-v1-128k", Provider::Kimi),
        ("claude-opus-5", Provider::Claude),
        ("gemini-3.6-flash", Provider::Gemini),
        ("gpt-5.6-sol", Provider::Gpt),
    ] {
        assert_eq!(Provider::from_model_name(model), want, "{model}");
    }
}

// Kimi returns 200 for a mutated, truncated, non-base64 or absent thinking
// signature, so a foreign signature costs the field rather than the text.
#[test]
fn decide_signature_compatibility_kimi_drops_signature_not_block() {
    let decision = decide_signature_compatibility(
        Provider::Kimi,
        OBSERVED_FABLE5_SAMPLE,
        BlockKind::ClaudeThinking,
    );
    assert!(!decision.compatible);
    assert_eq!(decision.action, Action::DropSignature);
}

#[test]
fn decide_signature_compatibility_kimi_preserves_native_signature() {
    let native = synthesize_kimi_signature(9709, 13);
    let decision =
        decide_signature_compatibility(Provider::Kimi, &native, BlockKind::ClaudeThinking);
    assert!(decision.compatible, "{}", decision.reason);
    assert_eq!(decision.action, Action::Preserve);
    assert_eq!(decision.normalized_signature, native);
}
