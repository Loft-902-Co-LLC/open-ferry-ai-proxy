// Ported from CLIProxyAPI internal/signature/claude_antigravity_validation_test.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI

use base64::engine::general_purpose::STANDARD;

use super::signaturetest::antigravity_caqs;
use super::*;
use crate::signature::claude_validation::extract_bytes_field;

/// The four validation modes upstream's tests try.
pub(super) fn validation_modes() -> [ClaudeValidationOptions; 4] {
    [
        ClaudeValidationOptions::default(),
        ClaudeValidationOptions::STRICT,
        ClaudeValidationOptions {
            prefix_only: true,
            ..ClaudeValidationOptions::default()
        },
        ClaudeValidationOptions {
            base64_only: true,
            ..ClaudeValidationOptions::default()
        },
    ]
}

// Ports TestAntigravityCAQSRecognitionAndReplay.
#[test]
fn antigravity_caqs_recognition_and_replay() {
    let sig = antigravity_caqs();
    let info = inspect_antigravity_claude_caqs_signature(&sig).expect("valid Q signature");
    assert!(
        info.envelope_version == 4
            && info.channel_id == 18
            && info.infrastructure == Some(2)
            && info.block_kind == "thinking"
            && info.model_text.is_empty()
            && info.signature_len == 1020
            && info.signature_in_container,
        "unexpected envelope: {info:?}"
    );
    assert!(
        maybe_self_describing_envelope(&sig) && detect_signature_provider(&sig) == Provider::Claude,
        "double CAQS must be detected as Claude"
    );
    for opts in validation_modes() {
        assert!(
            is_valid_claude_thinking_signature(&sig, opts),
            "signature rejected in mode {opts:?}"
        );
        assert_eq!(
            normalize_claude_thinking_signature(&sig, opts)
                .ok()
                .as_deref(),
            Some(sig.as_str()),
            "normalization changed the wrapper in mode {opts:?}"
        );
    }
    assert_eq!(
        compatible_antigravity_claude_thinking_signature(&sig).as_deref(),
        Some(sig.as_str()),
        "Antigravity replay must preserve the original bytes"
    );
    for target in [
        Provider::Claude,
        Provider::Gemini,
        Provider::Gpt,
        Provider::Kimi,
        Provider::Grok,
    ] {
        let decision = decide_signature_compatibility(target, &sig, BlockKind::ClaudeThinking);
        assert!(
            !decision.compatible,
            "Google wrapper must not be replayed directly to {target:?}"
        );
    }
    let inner = String::from_utf8(STANDARD.decode(&sig).expect("outer layer")).expect("ASCII");
    assert!(
        is_valid_claude_cais_signature(&inner),
        "inner CAQS is invalid"
    );
    assert_eq!(
        compatible_antigravity_claude_thinking_signature(&inner),
        None,
        "bare CAQS must not be promoted to Antigravity wire format"
    );
}

// Ports TestAntigravityCAQSRejectsMalformedWrappers.
#[test]
fn antigravity_caqs_rejects_malformed_wrappers() {
    let sig = antigravity_caqs();
    let inner = STANDARD.decode(&sig).expect("outer layer");
    let raw = STANDARD.decode(&inner).expect("inner layer");
    let wrap = |b: &[u8]| STANDARD.encode(STANDARD.encode(b));

    let mut wrong_version = raw.clone();
    wrong_version[1] = 2;
    // Locate the channel infrastructure field without touching the ciphertext.
    let index = raw
        .windows(6)
        .position(|w| w == [8, 18, 16, 2, 24, 2])
        .expect("fixture channel missing");
    let mut wrong_infra = raw.clone();
    wrong_infra[index + 3] = 1;
    let mut bad_field = raw.clone();
    bad_field[index + 2] = 18;
    let mut future_version = raw.clone();
    future_version[1] = 5;
    let mut wrong_channel = raw.clone();
    wrong_channel[index + 1] = 16;

    // An otherwise valid CAQS with its signature in the old channel slot.
    let mut misplaced = ClaudeCaisParts::new("claude-opus-5-5");
    misplaced.top_envelope = 4;
    misplaced.channel_id = 18;
    let misplaced_raw = STANDARD.decode(misplaced.encode()).expect("CAIS fixture");
    let container = extract_bytes_field(&misplaced_raw, 2, "container").expect("container");
    let channel = extract_bytes_field(container, 1, "channel").expect("channel");
    let channel = Pb::new().raw(channel).varint(2, 2).build();
    let new_container = Pb::new().bytes(1, &channel).build();
    let misplaced_payload = Pb::new().raw(&[8, 4]).bytes(2, &new_container).build();

    // A harmless unknown varint makes the inner encoding padded.
    let mut padded_payload = raw.clone();
    padded_payload.extend_from_slice(&[32, 0]);
    let mut bad_padding = STANDARD.encode(&padded_payload).into_bytes();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let pad_index = bad_padding
        .iter()
        .rposition(|&b| b != b'=')
        .expect("not all padding");
    assert_ne!(
        pad_index,
        bad_padding.len() - 1,
        "fixture must have padding"
    );
    let value = alphabet
        .iter()
        .position(|&c| c == bad_padding[pad_index])
        .expect("base64 character");
    bad_padding[pad_index] = alphabet[value | 1];

    let original_container = extract_bytes_field(&raw, 2, "container").expect("container");
    let original_channel = extract_bytes_field(original_container, 1, "channel").expect("channel");
    let thinking = b"\x42\x08thinking";
    let at = original_channel
        .windows(thinking.len())
        .position(|w| w == thinking)
        .expect("thinking block kind");
    let narration_channel = [
        &original_channel[..at],
        b"\x42\x09narration",
        &original_channel[at + thinking.len()..],
    ]
    .concat();
    let narration_container = Pb::new()
        .bytes(1, &narration_channel)
        .raw(&original_container[2 + original_channel.len()..])
        .build();
    let narration_payload = Pb::new()
        .raw(&[8, 4])
        .bytes(2, &narration_container)
        .build();

    let cases = [
        ("empty", String::new()),
        ("prefix only", "Q0FRUw==".to_owned()),
        ("truncated", sig[..sig.len() - 8].to_owned()),
        ("triple encoding", STANDARD.encode(&sig)),
        (
            "wrapped native CAQS",
            STANDARD.encode(OBSERVED_FABLE51_CAQS_SAMPLE),
        ),
        (
            "wrapped native CAIS",
            STANDARD.encode(test_claude_cais_signature("claude-opus-5")),
        ),
        ("version", wrap(&wrong_version)),
        ("infra", wrap(&wrong_infra)),
        ("infra wire type", wrap(&bad_field)),
        ("future version", wrap(&future_version)),
        ("channel", wrap(&wrong_channel)),
        ("narration", wrap(&narration_payload)),
        ("signature location", wrap(&misplaced_payload)),
        (
            "inner trailing space",
            STANDARD.encode([inner.as_slice(), b" "].concat()),
        ),
        (
            "inner trailing tab",
            STANDARD.encode([inner.as_slice(), b"\t"].concat()),
        ),
        ("inner padding bits", STANDARD.encode(&bad_padding)),
        ("outer newline", format!("{}\n{}", &sig[..8], &sig[8..])),
        (
            "inner newline",
            STANDARD.encode([b"CAQS\n".as_slice(), &inner[4..]].concat()),
        ),
        (
            "inner label",
            STANDARD.encode([b"Claude#".as_slice(), &inner].concat()),
        ),
        (
            "inner whitespace",
            STANDARD.encode([b" ".as_slice(), &inner].concat()),
        ),
        (
            "oversize",
            format!("Q{}", "A".repeat(MAX_CLAUDE_THINKING_SIGNATURE_LEN)),
        ),
    ];
    for (name, value) in cases {
        assert!(
            inspect_antigravity_claude_caqs_signature(&value).is_err(),
            "{name}: malformed envelope accepted"
        );
        for opts in validation_modes() {
            assert!(
                !is_valid_claude_thinking_signature(&value, opts),
                "{name}: invalid wrapper accepted in mode {opts:?}"
            );
        }
        assert_eq!(
            compatible_antigravity_claude_thinking_signature(&value),
            None,
            "{name}: invalid wrapper replayable"
        );
    }
}

// Not upstream's: the validators check a Q signature as it is once its cache
// prefix is stripped. Upstream's strip it a second time, so they accept, and
// normalize to, text that isn't a valid signature.
#[test]
fn antigravity_caqs_validators_strip_one_cache_prefix() {
    let sig = antigravity_caqs();
    let raw = format!("claude#Qjunk#{sig}");
    for opts in validation_modes() {
        assert!(
            !is_valid_claude_thinking_signature(&raw, opts),
            "a second cache prefix accepted in mode {opts:?}"
        );
        assert!(
            normalize_claude_thinking_signature(&raw, opts).is_err(),
            "a second cache prefix normalized in mode {opts:?}"
        );
    }
    assert!(!has_claude_thinking_signature_prefix(&raw));
    assert!(!has_decodable_claude_thinking_signature(&raw));
    let payload = json(&format!(
        r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"t","signature":"{raw}"}}]}}]}}"#
    ));
    assert!(
        validate_claude_thinking_signatures(&payload, ClaudeValidationOptions::default()).is_err()
    );
    // The public check strips one prefix, as upstream's does.
    assert!(inspect_antigravity_claude_caqs_signature(&format!("claude#{sig}")).is_ok());
    assert!(inspect_antigravity_claude_caqs_signature(&raw).is_err());
}

// Not upstream's: Claude's own endpoints never take a Q signature, and its
// error names the forms they do take.
#[test]
fn antigravity_caqs_is_not_claude_native() {
    let sig = antigravity_caqs();
    assert_eq!(
        normalize_claude_provider_native_thinking_signature(&sig, ClaudeValidationOptions::STRICT)
            .map_err(|err| err.to_string()),
        Err("invalid signature: expected 'E' or 'R' prefix, got \"Q\"".to_owned())
    );
    assert_eq!(
        normalize_claude_thinking_signature("Xabc", ClaudeValidationOptions::default())
            .map_err(|err| err.to_string()),
        Err("invalid signature: expected 'E', 'R' or 'Q' prefix, got \"X\"".to_owned())
    );
}
