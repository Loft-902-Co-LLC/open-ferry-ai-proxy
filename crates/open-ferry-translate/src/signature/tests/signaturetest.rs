// Ported from CLIProxyAPI internal/signature/signaturetest/claude.go
// (AntigravityCAQS) (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI

//! Synthetic envelopes for signature tests.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::Pb;

/// `AntigravityCAQS`: a double-base64 Google Claude thinking envelope, built at
/// test time. Every opaque field is zeros, not captured or replayable state.
pub(crate) fn antigravity_caqs() -> String {
    let channel = Pb::new()
        .varint(1, 18)
        .varint(2, 2)
        .varint(3, 2)
        .varint(7, 1)
        .string(8, "thinking")
        .build();
    let container = Pb::new()
        .bytes(1, &channel)
        .bytes(2, &[0; 12])
        .bytes(3, &[0; 12])
        .bytes(4, &[0; 48])
        .bytes(5, &[0; 1020])
        .build();
    let payload = Pb::new()
        .varint(1, 4)
        .bytes(2, &container)
        .varint(3, 1)
        .build();
    STANDARD.encode(STANDARD.encode(payload))
}
