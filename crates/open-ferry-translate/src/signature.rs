// Ported from CLIProxyAPI internal/signature/provider_compatibility.go and
// gpt_validation.go (v8.0.10, MIT). https://github.com/router-for-me/CLIProxyAPI

//! Replay checks for reasoning signatures carried in Claude `thinking` blocks.
//!
//! Only the GPT/Codex check is ported. Upstream also validates Claude, Gemini,
//! Kimi and Grok envelopes. The GPT result never depends on those validators,
//! because a GPT signature is identified by its own `gAAAA` prefix before any
//! other provider is probed.

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

use crate::go;

const MAX_GPT_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

// Go's base64 decoders tolerate non-zero trailing bits, so these do too.
const URL_SAFE_UNPADDED: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::RequireNone)
        .with_decode_allow_trailing_bits(true),
);
const URL_SAFE_PADDED: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical)
        .with_decode_allow_trailing_bits(true),
);

/// Returns the signature to send back to a GPT/Codex upstream as reasoning
/// `encrypted_content`, or `None` if `raw` isn't one. A `gpt#`, `openai#` or
/// `codex#` cache prefix is stripped; any other prefix rules the signature out.
pub(crate) fn compatible_gpt_signature(raw: &str) -> Option<&str> {
    let sig = raw.trim();
    let payload = match sig.split_once('#') {
        Some((prefix, rest)) if is_gpt_cache_prefix(prefix) => rest.trim(),
        // Upstream also recognises Claude, Gemini and SWE cache prefixes, but
        // for GPT those rule the signature out just like an unknown prefix does.
        Some(_) => return None,
        None => sig,
    };
    is_valid_gpt_reasoning_signature(payload).then_some(payload)
}

fn is_gpt_cache_prefix(prefix: &str) -> bool {
    matches!(
        go::to_lower(prefix.trim()).as_str(),
        "openai" | "gpt" | "codex"
    )
}

/// Checks the Fernet-like transport shape of GPT reasoning `encrypted_content`:
/// version byte `0x80`, an 8-byte timestamp, a 16-byte IV, AES-block ciphertext
/// and a 32-byte HMAC. It cannot prove the blob decrypts.
fn is_valid_gpt_reasoning_signature(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() || sig.len() > MAX_GPT_SIGNATURE_LEN || !sig.starts_with("gAAAA") {
        return false;
    }
    if !sig
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'='))
    {
        return false;
    }
    let Ok(decoded) = URL_SAFE_UNPADDED
        .decode(sig)
        .or_else(|_| URL_SAFE_PADDED.decode(sig))
    else {
        return false;
    };
    if decoded.len() < 73 || decoded[0] != 0x80 {
        return false;
    }
    // At least 73 bytes leaves at least one 16-byte block of ciphertext.
    let ciphertext_len = decoded.len() - 1 - 8 - 16 - 32;
    ciphertext_len.is_multiple_of(16)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The smallest well-formed GPT reasoning signature, as built by upstream's tests.
    pub(crate) fn valid_codex_reasoning_signature() -> String {
        let mut raw = [0u8; 1 + 8 + 16 + 16 + 32];
        raw[0] = 0x80;
        raw[8] = 1;
        base64::engine::general_purpose::URL_SAFE.encode(raw)
    }

    #[test]
    fn accepts_padded_and_unpadded_signatures() {
        let padded = valid_codex_reasoning_signature();
        assert!(padded.ends_with('='));
        assert_eq!(compatible_gpt_signature(&padded), Some(padded.as_str()));
        let unpadded = padded.trim_end_matches('=');
        assert_eq!(compatible_gpt_signature(unpadded), Some(unpadded));
    }

    #[test]
    fn strips_gpt_cache_prefix_and_rejects_others() {
        let sig = valid_codex_reasoning_signature();
        assert_eq!(
            compatible_gpt_signature(&format!("codex#{sig}")),
            Some(sig.as_str())
        );
        assert_eq!(
            compatible_gpt_signature(&format!(" OpenAI # {sig} ")),
            Some(sig.as_str())
        );
        // Go lowercases İ to i.
        assert_eq!(
            compatible_gpt_signature(&format!("OPENAİ#{sig}")),
            Some(sig.as_str())
        );
        assert_eq!(compatible_gpt_signature(&format!("claude#{sig}")), None);
        assert_eq!(compatible_gpt_signature(&format!("unknown#{sig}")), None);
    }

    #[test]
    fn rejects_malformed_signatures() {
        assert_eq!(compatible_gpt_signature(""), None);
        assert_eq!(compatible_gpt_signature("Eo8Canthropic-state"), None);
        assert_eq!(compatible_gpt_signature("gAAAA!!!!"), None);
        // Valid prefix, but the ciphertext is not a whole number of AES blocks.
        let mut raw = [0u8; 1 + 8 + 16 + 17 + 32];
        raw[0] = 0x80;
        let odd = base64::engine::general_purpose::URL_SAFE.encode(raw);
        assert_eq!(compatible_gpt_signature(&odd), None);
    }
}
