// Ported from CLIProxyAPI internal/signature/grok_validation.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! xAI Grok reasoning and compaction `encrypted_content`.

use super::{
    ClaudeValidationOptions, Error, GeminiValidationOptions, first_invalid_base64_char,
    inspect_gemini_thought_signature, is_valid_claude_cais_signature,
    is_valid_claude_thinking_signature, is_valid_kimi_thinking_signature,
    maybe_self_describing_envelope, split_signature_provider_prefix,
};
use crate::go::{self, base64::RAW_STD};

/// A transport cap on the opaque blob.
pub const MAX_GROK_ENCRYPTED_CONTENT_LEN: usize = 8 * 1024 * 1024;
/// A deliberately loose floor. The shortest payload upstream has observed keeps
/// moving down (50 bytes, then 43), so entropy does the real filtering.
pub const MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN: usize = 32;
/// Rejects payloads that aren't ciphertext. Real ones are at 0.892 or above.
pub const MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO: f64 = 0.85;

/// The sizes of a well-formed Grok `encrypted_content`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrokEncryptedContentInfo {
    pub raw_len: usize,
    pub decoded_len: usize,
    pub router_suffix: String,
}

/// `IsValidGrokEncryptedContent`.
pub fn is_valid_grok_encrypted_content(raw: &str) -> bool {
    inspect_grok_encrypted_content(raw).is_ok()
}

/// `InspectGrokEncryptedContent`: checks the transport shape of xAI reasoning
/// or compaction `encrypted_content`. This does not prove it decrypts.
///
/// This is not a provider classifier. xAI's payloads have no envelope and look
/// like uniform random bytes, so any high-entropy unpadded base64 passes. Know
/// the value came from xAI first, from a cache prefix or a confirmed xAI target
/// model, and treat the result as a replay-safety check.
pub fn inspect_grok_encrypted_content(raw: &str) -> Result<GrokEncryptedContentInfo, Error> {
    let sig = raw.trim();
    if sig.is_empty() {
        return Err(error!("empty Grok encrypted_content"));
    }
    if sig.len() > MAX_GROK_ENCRYPTED_CONTENT_LEN {
        return Err(error!(
            "Grok encrypted_content exceeds maximum length ({MAX_GROK_ENCRYPTED_CONTENT_LEN} bytes)"
        ));
    }
    if sig != raw {
        return Err(error!(
            "Grok encrypted_content has leading or trailing whitespace"
        ));
    }
    if sig.contains('=') {
        return Err(error!(
            "invalid Grok encrypted_content: expected unpadded standard base64"
        ));
    }
    if split_signature_provider_prefix(sig).is_some() {
        return Err(error!(
            "invalid Grok encrypted_content: carries another provider's cache prefix"
        ));
    }
    let full_len = sig.len();
    let (sig, router_suffix) = match sig.split_once('.') {
        Some((head, tail)) => {
            if !super::meta_validation::valid_router_suffix(tail) {
                return Err(error!(
                    "invalid Grok encrypted_content: router suffix must be 1-128 alphanumeric characters"
                ));
            }
            (head, tail)
        }
        None => (sig, ""),
    };
    if let Some((index, c)) = first_invalid_base64_char(sig, b"+/") {
        return Err(error!(
            "invalid Grok encrypted_content: contains non-base64 character U+{:04X} at byte {index}",
            u32::from(c)
        ));
    }
    // Only a few first characters can start a self-describing envelope, which
    // skips these probes for most real xAI ciphertext. Claude CAIS drops its
    // padding when its length is a multiple of 3, so the padding check above
    // doesn't exclude it.
    if maybe_self_describing_envelope(sig) {
        if sig.starts_with("gAAAA") {
            return Err(error!(
                "Grok encrypted_content looks like GPT/Codex reasoning signature"
            ));
        }
        if super::is_valid_meta_reasoning_signature(sig) {
            return Err(error!(
                "Grok encrypted_content looks like Meta reasoning signature"
            ));
        }
        if is_valid_claude_thinking_signature(sig, ClaudeValidationOptions::STRICT) {
            return Err(error!(
                "Grok encrypted_content looks like Claude thinking signature"
            ));
        }
        if is_valid_claude_cais_signature(sig) {
            return Err(error!(
                "Grok encrypted_content looks like Claude CAIS thinking signature"
            ));
        }
        let known_envelope = GeminiValidationOptions {
            require_known_envelope: true,
            ..GeminiValidationOptions::default()
        };
        if inspect_gemini_thought_signature(sig, known_envelope).is_ok() {
            return Err(error!(
                "Grok encrypted_content looks like Gemini thoughtSignature"
            ));
        }
    }
    // Kimi has no envelope either, so length is the only separator. xAI's
    // length varies byte by byte; Kimi's is one of two constants.
    if is_valid_kimi_thinking_signature(sig) {
        return Err(error!(
            "Grok encrypted_content has a Kimi thinking signature length"
        ));
    }

    let decoded = RAW_STD
        .decode(sig)
        .map_err(|err| error!("invalid Grok encrypted_content: base64 decode failed: {err}"))?;
    if decoded.starts_with(b"cpa-codex-gemini-carrier-v1:") {
        return Err(error!(
            "invalid Grok encrypted_content: carries Gemini Codex thoughtSignature carrier"
        ));
    }
    if decoded.len() < MIN_GROK_ENCRYPTED_CONTENT_DECODED_LEN {
        return Err(error!(
            "invalid Grok encrypted_content: decoded payload too short ({} bytes)",
            decoded.len()
        ));
    }
    let entropy_ratio = byte_entropy_ratio(&decoded);
    if entropy_ratio < MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO {
        return Err(error!(
            "invalid Grok encrypted_content: decoded payload entropy ratio {entropy_ratio:.3} below {MIN_GROK_ENCRYPTED_CONTENT_ENTROPY_RATIO:.3}"
        ));
    }
    Ok(GrokEncryptedContentInfo {
        raw_len: full_len,
        router_suffix: router_suffix.to_owned(),
        decoded_len: decoded.len(),
    })
}

/// The Shannon entropy of `buf`'s bytes, as a fraction of the most that many
/// bytes could have.
pub(super) fn byte_entropy_ratio(buf: &[u8]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &b in buf {
        counts[usize::from(b)] += 1;
    }
    let n = buf.len() as f64;
    let entropy = counts
        .iter()
        .filter(|&&count| count != 0)
        .fold(0.0, |entropy, &count| {
            let p = count as f64 / n;
            entropy - p * go::log2(p)
        });
    let max_symbols = buf.len().min(256);
    if max_symbols <= 1 {
        return 0.0;
    }
    entropy / go::log2(max_symbols as f64)
}

#[cfg(test)]
mod router_tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
    #[test]
    fn validates_native_entropy_before_reporting_suffix() {
        let native = STANDARD_NO_PAD.encode((0..255).collect::<Vec<u8>>());
        let info = inspect_grok_encrypted_content(&format!("{native}.Router123")).unwrap();
        assert_eq!(info.router_suffix, "Router123");
        assert_eq!(info.raw_len, native.len() + 10);
        assert!(
            inspect_grok_encrypted_content(&format!(
                "{}.Router123",
                STANDARD_NO_PAD.encode([0; 255])
            ))
            .is_err()
        );
    }
}
