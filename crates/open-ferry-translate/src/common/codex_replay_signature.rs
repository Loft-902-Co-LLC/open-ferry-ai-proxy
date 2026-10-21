// Ported from CLIProxyAPI internal/translator/common/codex_replay_signature.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI
use crate::signature::{Provider, compatible_signature_for_provider, detect_signature_provider, is_valid_grok_encrypted_content};
use crate::thinking::base_model_name;

pub(crate) fn for_target(model: &str, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() { return None; }
    let target = Provider::from_model_name(base_model_name(model));
    for_provider(target, raw)
}

pub(crate) fn for_provider(target: Provider, raw: &str) -> Option<String> {
    let effective = if target == Provider::Unknown { Provider::Gpt } else { target };
    let detected = detect_signature_provider(&raw);
    if detected != Provider::Unknown {
        return (detected == effective).then(|| compatible_signature_for_provider(effective, &raw)).flatten();
    }
    (target == Provider::Grok && is_valid_grok_encrypted_content(&raw)).then(|| raw.to_owned())
}

pub(crate) fn resolve_target<'a>(model: &'a str, request: &'a serde_json::Value) -> &'a str {
    if Provider::from_model_name(base_model_name(model)) != Provider::Unknown { return model; }
    let raw = request.get("model").and_then(serde_json::Value::as_str).unwrap_or_default().trim();
    if matches!(Provider::from_model_name(base_model_name(raw)), Provider::Meta | Provider::Grok | Provider::Gpt) { raw } else { model }
}
