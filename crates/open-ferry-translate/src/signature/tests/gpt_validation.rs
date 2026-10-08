// Ported from CLIProxyAPI internal/signature/gpt_validation_test.go (v8.0.20,
// MIT). https://github.com/router-for-me/CLIProxyAPI

use super::*;

#[test]
fn detect_signature_provider_gpt_reasoning() {
    assert_eq!(
        detect_signature_provider(&test_gpt_reasoning_signature()),
        Provider::Gpt
    );
}

#[test]
fn inspect_gpt_reasoning_signature_rejects_unicode_ellipsis() {
    let sig = test_gpt_reasoning_signature();
    let polluted = format!("{}{}{}", &sig[..20], '\u{2026}', &sig[20..]);
    let err = inspect_gpt_reasoning_signature(&polluted)
        .unwrap_err()
        .to_string();
    assert!(err.contains("non-base64url character U+2026"), "{err}");
}

#[test]
fn gpt_accepts_padded_and_unpadded_signatures() {
    let padded = valid_codex_reasoning_signature();
    assert!(padded.ends_with('='));
    assert_eq!(
        compatible_signature_for_provider(Provider::Gpt, &padded),
        Some(padded.clone())
    );
    let unpadded = padded.trim_end_matches('=');
    assert_eq!(
        compatible_signature_for_provider(Provider::Gpt, unpadded),
        Some(unpadded.to_owned())
    );
}

#[test]
fn gpt_strips_cache_prefix_and_rejects_others() {
    let sig = valid_codex_reasoning_signature();
    let gpt = |raw: &str| compatible_signature_for_provider(Provider::Gpt, raw);
    assert_eq!(gpt(&format!("codex#{sig}")), Some(sig.clone()));
    assert_eq!(gpt(&format!(" OpenAI # {sig} ")), Some(sig.clone()));
    // Go lowercases İ to i.
    assert_eq!(gpt(&format!("OPENAİ#{sig}")), Some(sig.clone()));
    assert_eq!(gpt(&format!("claude#{sig}")), None);
    assert_eq!(gpt(&format!("unknown#{sig}")), None);
}

#[test]
fn gpt_rejects_malformed_signatures() {
    let err = |raw: &str| {
        inspect_gpt_reasoning_signature(raw)
            .unwrap_err()
            .to_string()
    };
    assert_eq!(err(""), "empty GPT reasoning signature");
    assert_eq!(
        err("Eo8Canthropic-state"),
        "invalid GPT reasoning signature: expected gAAAA prefix"
    );
    assert_eq!(
        err("gAAAA!!!!"),
        "invalid GPT reasoning signature: contains non-base64url character U+0021 at byte 5"
    );
    assert_eq!(
        err("gAAAAé"),
        "invalid GPT reasoning signature: contains non-base64url character U+00E9 at byte 5"
    );
    assert_eq!(
        err("gAAAA="),
        "invalid GPT reasoning signature: base64url decode failed"
    );
    assert_eq!(
        err("gAAAAAAA"),
        "invalid GPT reasoning signature: decoded payload too short"
    );
    // Valid prefix, but the ciphertext is not a whole number of AES blocks.
    let mut raw = [0u8; 1 + 8 + 16 + 17 + 32];
    raw[0] = 0x80;
    assert_eq!(
        err(&URL_SAFE.encode(raw)),
        "invalid GPT reasoning signature: ciphertext length 17 is not a positive AES block multiple"
    );
    // `gAAAA` always decodes to a 0x80 first byte, so the version check can't
    // fail after the prefix check.
    let info = inspect_gpt_reasoning_signature(&valid_codex_reasoning_signature()).unwrap();
    assert_eq!((info.decoded_len, info.ciphertext_len), (73, 16));
}
