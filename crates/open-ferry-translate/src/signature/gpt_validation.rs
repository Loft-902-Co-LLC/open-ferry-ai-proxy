// Ported from CLIProxyAPI internal/signature/gpt_validation.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! GPT and Codex reasoning `encrypted_content`.

use super::{Error, first_invalid_base64_char};
use crate::go::base64::{RAW_URL, URL};

pub const MAX_GPT_REASONING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;

/// The sizes of a well-formed GPT reasoning signature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GptReasoningSignatureInfo {
    pub decoded_len: usize,
    pub ciphertext_len: usize,
}

/// `IsValidGPTReasoningSignature`.
pub fn is_valid_gpt_reasoning_signature(raw: &str) -> bool {
    inspect_gpt_reasoning_signature(raw).is_ok()
}

/// `InspectGPTReasoningSignature`: checks the Fernet-like envelope of GPT and
/// Codex reasoning `encrypted_content`: version byte `0x80`, an 8-byte
/// timestamp, a 16-byte IV, AES-block ciphertext and a 32-byte HMAC. This does
/// not prove the value decrypts.
pub fn inspect_gpt_reasoning_signature(raw: &str) -> Result<GptReasoningSignatureInfo, Error> {
    let sig = raw.trim();
    if sig.is_empty() {
        return Err(error!("empty GPT reasoning signature"));
    }
    if sig.len() > MAX_GPT_REASONING_SIGNATURE_LEN {
        return Err(error!(
            "GPT reasoning signature exceeds maximum length ({MAX_GPT_REASONING_SIGNATURE_LEN} bytes)"
        ));
    }
    // The literal prefix rejects every other provider's envelope, so it runs
    // before the full character scan.
    if !sig.starts_with("gAAAA") {
        return Err(error!(
            "invalid GPT reasoning signature: expected gAAAA prefix"
        ));
    }
    if let Some((index, c)) = first_invalid_base64_char(sig, b"-_=") {
        return Err(error!(
            "invalid GPT reasoning signature: contains non-base64url character U+{:04X} at byte {index}",
            u32::from(c)
        ));
    }

    let decoded = RAW_URL
        .decode(sig)
        .or_else(|_| URL.decode(sig))
        .map_err(|_| error!("invalid GPT reasoning signature: base64url decode failed"))?;
    if decoded.len() < 73 {
        return Err(error!(
            "invalid GPT reasoning signature: decoded payload too short"
        ));
    }
    if decoded[0] != 0x80 {
        return Err(error!(
            "invalid GPT reasoning signature: expected version 0x80, got 0x{:02x}",
            decoded[0]
        ));
    }
    // At least 73 bytes, so the ciphertext is at least 16.
    let ciphertext_len = decoded.len() - 1 - 8 - 16 - 32;
    if ciphertext_len % 16 != 0 {
        return Err(error!(
            "invalid GPT reasoning signature: ciphertext length {ciphertext_len} is not a positive AES block multiple"
        ));
    }
    Ok(GptReasoningSignatureInfo {
        decoded_len: decoded.len(),
        ciphertext_len,
    })
}
