// Ported from CLIProxyAPI internal/signature/claude_antigravity_validation.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI

//! Antigravity's Claude thinking signatures in Q form.
//!
//! Antigravity's Claude 5.5 returns a CAQS signature inside a second base64
//! layer. The outer layer decodes to base64 text starting with `C`, which
//! encodes to a leading `Q`. That text decodes to an envelope version 4
//! protobuf whose channel block names channel 18, Google infrastructure
//! (field 2 set to 2) and a `thinking` block, with the signature bytes in
//! container field 5.
//!
//! The check is structural. It cannot prove that a signature verifies, or that
//! another account can replay it. Only the observed layout is accepted, both
//! layers must be canonical base64, deeper wrapping is rejected, and so is a
//! second container or channel block.
//!
//! Deviations from upstream: none.

use super::Error;
use super::claude_validation::{
    ClaudeCaisSignatureInfo, MAX_CLAUDE_THINKING_SIGNATURE_LEN, inspect_claude_cais_payload,
    strip_claude_signature_prefix,
};
use crate::go::base64::{Encoding, STD};

/// `base64.StdEncoding.Strict()`.
const STRICT_STD: Encoding = STD.strict();

/// `InspectAntigravityClaudeCAQSSignature`: checks a Q-form signature, after
/// an optional cache prefix.
pub fn inspect_antigravity_claude_caqs_signature(
    raw: &str,
) -> Result<ClaudeCaisSignatureInfo, Error> {
    inspect_unprefixed_antigravity_caqs(strip_claude_signature_prefix(raw))
}

/// Upstream's checks after the cache prefix is stripped. The Claude validators
/// call this on a signature they have already stripped.
pub(super) fn inspect_unprefixed_antigravity_caqs(
    sig: &str,
) -> Result<ClaudeCaisSignatureInfo, Error> {
    if sig.is_empty() || sig.len() > MAX_CLAUDE_THINKING_SIGNATURE_LEN {
        return Err(error!("invalid Antigravity CAQS signature length"));
    }
    if !sig.starts_with('Q') || sig.contains(['\r', '\n']) {
        return Err(error!("invalid Antigravity CAQS wrapper"));
    }
    let inner = STRICT_STD
        .decode(sig)
        .map_err(|err| error!("invalid Antigravity CAQS encoding: {err}"))?;
    if inner.first() != Some(&b'C') || inner.iter().any(|b| b" \t\r\n#".contains(b)) {
        return Err(error!("invalid Antigravity CAQS inner encoding"));
    }
    let decoded = STRICT_STD
        .decode(&inner)
        .map_err(|err| error!("invalid Antigravity CAQS inner encoding: {err}"))?;
    let info = inspect_claude_cais_payload(&decoded, true)
        .map_err(|err| error!("invalid Antigravity CAQS payload: {err}"))?;
    // Only the observed Google thinking channel. Other versions and channels
    // need their own capture and replay checks.
    if info.envelope_version != 4
        || info.channel_id != 18
        || info.infrastructure != Some(2)
        || info.block_kind != "thinking"
        || !info.signature_in_container
    {
        return Err(error!(
            "unsupported Antigravity CAQS envelope or channel schema"
        ));
    }
    Ok(info)
}
