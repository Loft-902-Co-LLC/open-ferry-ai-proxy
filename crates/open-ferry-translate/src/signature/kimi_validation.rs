// Ported from CLIProxyAPI internal/signature/kimi_validation.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Kimi thinking signatures.
//!
//! Kimi's signatures have no envelope: no magic prefix, version byte or key id,
//! and every byte looks uniformly random. What they do have is a fixed length
//! per protocol mode, 12946 characters for non-streaming responses and 4340 for
//! streaming, whatever the thinking text. Upstream observed exactly those two
//! across every Kimi model in both modes, and no other provider's signatures
//! land on either length.
//!
//! The lengths are an observed regularity rather than a contract, and Kimi
//! never reads the signature back, so a gateway change could move them without
//! any visible error. That's why Kimi is probed after every self-describing
//! envelope: a drift can only cost Kimi its own identification.

use super::{
    ClaudeValidationOptions, Error, GeminiValidationOptions, first_invalid_base64_char,
    grok_validation::byte_entropy_ratio, is_valid_claude_cais_signature,
    is_valid_claude_thinking_signature, is_valid_gemini_thought_signature,
    maybe_self_describing_envelope, split_signature_provider_prefix,
};
use crate::go::base64::RAW_STD;

/// The length of a non-streaming Kimi Messages signature.
pub const KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN: usize = 12946;
/// The length of a streaming Kimi `signature_delta`.
pub const KIMI_THINKING_SIGNATURE_STREAMING_LEN: usize = 4340;
/// Keeps same-length filler from passing as Kimi. Real signatures sit at 0.997
/// or above.
pub const MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO: f64 = 0.85;

/// The code path that produced a Kimi signature, known from its length alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KimiSignatureMode {
    NonStreaming,
    Streaming,
}

impl KimiSignatureMode {
    /// Upstream's name for the mode.
    pub fn as_str(self) -> &'static str {
        match self {
            KimiSignatureMode::NonStreaming => "non_streaming",
            KimiSignatureMode::Streaming => "streaming",
        }
    }
}

/// An accepted Kimi thinking signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KimiThinkingSignatureInfo {
    pub raw_len: usize,
    pub decoded_len: usize,
    pub mode: KimiSignatureMode,
}

/// `IsValidKimiThinkingSignature`.
pub fn is_valid_kimi_thinking_signature(raw: &str) -> bool {
    inspect_kimi_thinking_signature(raw).is_ok()
}

/// `InspectKimiThinkingSignature`: checks that `raw` has the size and character
/// class Kimi produces. This proves nothing about the payload.
pub fn inspect_kimi_thinking_signature(raw: &str) -> Result<KimiThinkingSignatureInfo, Error> {
    let sig = raw.trim();
    if sig.is_empty() {
        return Err(error!("empty Kimi thinking signature"));
    }
    if sig != raw {
        return Err(error!(
            "Kimi thinking signature has leading or trailing whitespace"
        ));
    }
    let mode = match sig.len() {
        KIMI_THINKING_SIGNATURE_NON_STREAMING_LEN => KimiSignatureMode::NonStreaming,
        KIMI_THINKING_SIGNATURE_STREAMING_LEN => KimiSignatureMode::Streaming,
        len => {
            return Err(error!(
                "invalid Kimi thinking signature: unexpected length {len}"
            ));
        }
    };
    if sig.contains('=') {
        return Err(error!(
            "invalid Kimi thinking signature: expected unpadded standard base64"
        ));
    }
    if let Some((index, c)) = first_invalid_base64_char(sig, b"+/") {
        return Err(error!(
            "invalid Kimi thinking signature: contains non-base64 character U+{:04X} at byte {index}",
            u32::from(c)
        ));
    }
    if split_signature_provider_prefix(sig).is_some() {
        return Err(error!(
            "invalid Kimi thinking signature: carries another provider's cache prefix"
        ));
    }
    // Detection already runs the envelope probes first, but this function is
    // public, so a foreign envelope of matching length is rejected here too.
    if maybe_self_describing_envelope(sig) {
        if sig.starts_with("gAAAA") {
            return Err(error!(
                "Kimi thinking signature looks like GPT/Codex reasoning signature"
            ));
        }
        if is_valid_claude_cais_signature(sig) {
            return Err(error!(
                "Kimi thinking signature looks like Claude CAIS thinking signature"
            ));
        }
        if is_valid_claude_thinking_signature(sig, ClaudeValidationOptions::STRICT) {
            return Err(error!(
                "Kimi thinking signature looks like Claude thinking signature"
            ));
        }
        let known_envelope = GeminiValidationOptions {
            require_known_envelope: true,
            ..GeminiValidationOptions::default()
        };
        if is_valid_gemini_thought_signature(sig, known_envelope) {
            return Err(error!(
                "Kimi thinking signature looks like Gemini thoughtSignature"
            ));
        }
    }
    let decoded = RAW_STD
        .decode(sig)
        .map_err(|err| error!("invalid Kimi thinking signature: base64 decode failed: {err}"))?;
    let entropy_ratio = byte_entropy_ratio(&decoded);
    if entropy_ratio < MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO {
        return Err(error!(
            "invalid Kimi thinking signature: decoded payload entropy ratio {entropy_ratio:.3} below {MIN_KIMI_THINKING_SIGNATURE_ENTROPY_RATIO:.3}"
        ));
    }
    Ok(KimiThinkingSignatureInfo {
        raw_len: sig.len(),
        decoded_len: decoded.len(),
        mode,
    })
}
