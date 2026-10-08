// Ported from CLIProxyAPI internal/signature/grok_validation_test.go (v8.0.20,
// MIT). https://github.com/router-for-me/CLIProxyAPI

use super::*;

fn grok_error(sample: &str) -> String {
    inspect_grok_encrypted_content(sample)
        .expect_err("expected invalid Grok encrypted_content")
        .to_string()
}

#[test]
fn inspect_grok_encrypted_content_rejects_gemini_thought_signature_envelope() {
    let err = grok_error(&test_gemini_thought_signature_envelope());
    assert!(err.contains("Gemini"), "{err}");
}

// The retired Gemini 2.5 envelope is no longer a known Gemini envelope, so it
// falls to the residual class like any other opaque payload.
#[test]
fn inspect_grok_encrypted_content_retired_gemini25_field1_envelope() {
    let sample = test_gemini25_field1_thought_signature_envelope();
    let known = GeminiValidationOptions {
        require_known_envelope: true,
        ..GeminiValidationOptions::default()
    };
    assert!(!is_valid_gemini_thought_signature(&sample, known));
    assert!(inspect_grok_encrypted_content(&sample).is_ok());
}

#[test]
fn inspect_grok_encrypted_content_rejects_claude_thinking_signature() {
    let sample = test_unpadded_claude_thinking_signature();
    assert!(is_valid_claude_thinking_signature(
        &sample,
        ClaudeValidationOptions::STRICT
    ));
    let err = grok_error(&sample);
    assert!(err.contains("Claude"), "{err}");
}

#[test]
fn inspect_grok_encrypted_content_rejects_antigravity_claude_thinking_signature() {
    let sample = test_unpadded_antigravity_claude_thinking_signature();
    assert!(sample.starts_with('R') && !sample.contains('='), "{sample}");
    assert!(is_valid_claude_thinking_signature(
        &sample,
        ClaudeValidationOptions::STRICT
    ));
    let err = grok_error(&sample);
    assert!(err.contains("Claude"), "{err}");
}

// CAIS payloads are high-entropy standard base64 and drop their padding
// whenever the decoded length is a multiple of 3, so neither the padding check
// nor classic Claude validation excludes them on their own.
#[test]
fn inspect_grok_encrypted_content_rejects_claude_cais_signature() {
    for (name, sample) in [
        ("synthetic unpadded", test_unpadded_claude_cais_signature()),
        ("observed fable-5", OBSERVED_FABLE5_SAMPLE.to_owned()),
    ] {
        assert!(!sample.contains('='), "{name}: must be unpadded");
        assert!(is_valid_claude_cais_signature(&sample), "{name}");
        assert!(
            !is_valid_claude_thinking_signature(&sample, ClaudeValidationOptions::STRICT),
            "{name}: must not also pass classic Claude validation"
        );
        let err = grok_error(&sample);
        assert!(err.contains("CAIS"), "{name}: {err}");
    }
}

// A prefixed value belongs to the provider the prefix names, and must never be
// replayed to xAI verbatim.
#[test]
fn inspect_grok_encrypted_content_rejects_provider_cache_prefix() {
    for prefix in ["claude#", "anthropic#", "gemini#", "openai#", "codex#"] {
        let sample = format!("{prefix}{}", test_unpadded_claude_cais_signature());
        assert!(inspect_grok_encrypted_content(&sample).is_err(), "{prefix}");
    }
}

// Neither threshold sits on observed data: the shortest native payload seen is
// 50 decoded bytes and the lowest entropy ratio 0.892.
#[test]
#[allow(clippy::assertions_on_constants)]
fn inspect_grok_encrypted_content_threshold_margins() {
    assert!(MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN < 50);
    assert!(MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO < 0.892);
}

#[test]
fn inspect_grok_encrypted_content_rejects_foreign_shapes() {
    let filler = STANDARD.encode(vec![0xa5; MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN]);
    for sample in [
        "",
        "bad",
        " opaque",
        "gAAAAABinvalid-gpt-shape",
        "abcd_efg",
        &filler,
    ] {
        assert!(
            inspect_grok_encrypted_content(sample).is_err(),
            "{sample:?}"
        );
    }
}

#[test]
fn inspect_grok_encrypted_content_rejects_low_entropy_payload() {
    let sample = STANDARD_NO_PAD.encode(vec![0xa5; MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN]);
    let err = grok_error(&sample);
    assert!(err.contains("entropy ratio"), "{err}");
}

#[test]
fn inspect_grok_encrypted_content_rejects_invalid_base64_length() {
    let err = grok_error("AAAAA");
    assert!(err.contains("base64 decode failed"), "{err}");
}

#[test]
fn byte_entropy_ratio_single_byte_returns_zero() {
    assert_eq!(
        crate::signature::grok_validation::byte_entropy_ratio(&[0xa5]),
        0.0
    );
}

#[test]
fn signature_provider_from_model_name_grok() {
    for model in [
        "grok-4.5",
        "grok-4.5-build",
        "grok-composer-2.5-fast",
        "grok-code-fast-1",
    ] {
        assert_eq!(Provider::from_model_name(model), Provider::Grok, "{model}");
    }
}

// Unlike Kimi, xAI decrypts the blob and answers 400 for foreign or mutated
// input, so an incompatible block can't survive by shedding its signature.
#[test]
fn decide_signature_compatibility_grok_drops_block() {
    let decision =
        decide_signature_compatibility(Provider::Grok, OBSERVED_FABLE5_SAMPLE, BlockKind::Unknown);
    assert!(!decision.compatible);
    assert_eq!(decision.action, Action::DropBlock);
}
