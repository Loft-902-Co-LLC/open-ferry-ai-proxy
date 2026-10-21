// Ported from CLIProxyAPI internal/translator/codex/gemini (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

const MAGIC: &str = "cpa-codex-gemini-carrier-v1:";

pub(super) fn encode(raw: &str) -> String {
    STANDARD.encode(format!("{MAGIC}{}", raw.trim()))
}

pub(super) fn decode(raw: &str) -> String {
    let raw = raw.trim();
    if let Ok(bytes) = crate::go::base64::STD.decode(raw)
        && let Ok(text) = String::from_utf8(bytes)
        && let Some(sig) = text.strip_prefix(MAGIC) {
            return sig.trim().to_owned();
        }
    raw.to_owned()
}

pub(super) fn for_target(model: &str, raw: &str) -> Option<String> {
    let raw = decode(raw);
    crate::common::codex_replay_signature::for_target(model, &raw)
}

pub(super) fn set(part: &mut Value, raw: &str) {
    if !raw.trim().is_empty() {
        part["thoughtSignature"] = encode(raw).into();
    }
}
