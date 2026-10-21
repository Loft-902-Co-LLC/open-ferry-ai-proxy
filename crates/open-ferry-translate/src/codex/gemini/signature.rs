// Ported from CLIProxyAPI internal/translator/codex/gemini (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use crate::signature::{Provider, compatible_signature_for_provider, detect_signature_provider, is_valid_grok_encrypted_content};
use crate::thinking::base_model_name;

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
    let target = Provider::from_model_name(base_model_name(model));
    let effective = if target == Provider::Unknown { Provider::Gpt } else { target };
    let detected = detect_signature_provider(&raw);
    if detected != Provider::Unknown {
        return (detected == effective).then(|| compatible_signature_for_provider(effective, &raw)).flatten();
    }
    (target == Provider::Grok && is_valid_grok_encrypted_content(&raw)).then_some(raw)
}

pub(super) fn set(part: &mut Value, raw: &str) {
    if !raw.trim().is_empty() {
        part["thoughtSignature"] = encode(raw).into();
    }
}
