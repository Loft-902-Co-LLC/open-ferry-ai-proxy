// Ported from CLIProxyAPI internal/signature/gemini_sanitize.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini replay policy for thought signatures.

use std::borrow::Cow;

use serde_json::Value;

use super::{
    Action, BlockKind, GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, Provider,
    decide_signature_compatibility, is_gemini_thought_signature_bypass,
    signature_payload_without_provider_prefix,
};
use crate::json::{self, str_of};

/// Where a Gemini part may carry its thought signature. The first is canonical.
const THOUGHT_SIGNATURE_PATHS: [&str; 7] = [
    "thoughtSignature",
    "thought_signature",
    "functionCall.thoughtSignature",
    "functionCall.thought_signature",
    "functionResponse.thoughtSignature",
    "functionResponse.thought_signature",
    "extra_content.google.thought_signature",
];

/// Server-side tool blocks, which Gemini requires echoed back untouched.
const SERVER_TOOL_KEYS: [&str; 4] = ["toolCall", "tool_call", "toolResponse", "tool_response"];

/// `GeminiReplaySignatureOrBypass`: the thought signature to replay to Gemini.
/// A compatible signature is kept in its normalized form; a missing, unknown or
/// foreign one becomes the bypass sentinel.
pub fn gemini_replay_signature_or_bypass(raw: &str, block_kind: BlockKind) -> String {
    let decision = decide_signature_compatibility(Provider::Gemini, raw, block_kind);
    if decision.compatible && !decision.normalized_signature.is_empty() {
        return decision.normalized_signature;
    }
    if decision.action == Action::ReplaceWithGeminiBypass
        && !decision.replacement_signature.is_empty()
    {
        return decision.replacement_signature;
    }
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_owned()
}

/// `SanitizeGeminiRequestThoughtSignatures`: applies Gemini's replay policy to
/// the contents at `contents_path` (default `contents`), returning whether
/// anything changed.
///
/// Provider signatures stay on their model parts, moved to the canonical
/// `thoughtSignature` field. Server-side tool blocks are left untouched. Only a
/// missing or foreign signature on a turn's first `functionCall` becomes the
/// bypass sentinel; other parts with one lose it, so parallel calls stay
/// unsigned as Gemini returns them. `functionResponse` parts never keep one.
///
/// Upstream first scans for whether anything needs changing, to avoid
/// rewriting the request. That is the same as whether this changes anything,
/// so it isn't ported.
pub fn sanitize_gemini_request_thought_signatures(
    payload: &mut Value,
    contents_path: &str,
) -> bool {
    let contents_path = match contents_path.trim() {
        "" => "contents",
        path => path,
    };
    let Some(Value::Array(contents)) = json::path_mut(payload, contents_path) else {
        return false;
    };
    let mut changed = false;
    for content in contents {
        let is_model_turn = str_of(content.get("role")) == "model";
        let Some(Value::Array(parts)) = content.get_mut("parts") else {
            continue;
        };
        let mut first_function_call_seen = false;
        for part in parts {
            changed |= sanitize_part(part, is_model_turn, &mut first_function_call_seen);
        }
    }
    changed
}

/// Applies the replay policy to one part, returning whether it changed.
fn sanitize_part(
    part: &mut Value,
    is_model_turn: bool,
    first_function_call_seen: &mut bool,
) -> bool {
    let signature = part_thought_signature(part).map(Cow::into_owned);
    if part.get("functionResponse").is_some() {
        return signature.is_some() && delete_part_thought_signature_fields(part);
    }
    if !is_model_turn || SERVER_TOOL_KEYS.iter().any(|key| part.get(key).is_some()) {
        return false;
    }

    let has_function_call = part.get("functionCall").is_some();
    let is_first_function_call = has_function_call && !*first_function_call_seen;
    *first_function_call_seen |= has_function_call;
    let replay = if is_first_function_call {
        gemini_replay_signature_or_bypass(
            signature.as_deref().unwrap_or_default(),
            BlockKind::GeminiFunctionCall,
        )
    } else {
        let Some(raw) = &signature else {
            return false;
        };
        let block_kind = if has_function_call {
            BlockKind::GeminiFunctionCall
        } else {
            BlockKind::GeminiModelPart
        };
        let decision = decide_signature_compatibility(Provider::Gemini, raw, block_kind);
        // A sentinel is only kept on a first call; sibling calls and text parts
        // drop it.
        if decision.action == Action::Preserve
            && !is_gemini_thought_signature_bypass(signature_payload_without_provider_prefix(raw))
        {
            decision.normalized_signature
        } else {
            String::new()
        }
    };

    if replay.is_empty() {
        return signature.is_some() && delete_part_thought_signature_fields(part);
    }
    if has_normalized_part_thought_signature(part, &replay) {
        return false;
    }
    delete_part_thought_signature_fields(part);
    if let Some(object) = part.as_object_mut() {
        object.insert("thoughtSignature".to_owned(), Value::String(replay));
    }
    true
}

/// The thought signature a Gemini part carries, from the first field that has
/// one.
pub(super) fn part_thought_signature(part: &Value) -> Option<Cow<'_, str>> {
    THOUGHT_SIGNATURE_PATHS
        .iter()
        .find_map(|path| json::path(part, path))
        .map(|value| str_of(Some(value)))
}

/// Whether `replay` is the part's only thought signature, as a string in the
/// canonical field. Upstream also rejects a duplicated `thoughtSignature` key,
/// which a parsed [`Value`] can't hold.
pub(super) fn has_normalized_part_thought_signature(part: &Value, replay: &str) -> bool {
    matches!(part.get("thoughtSignature"), Some(Value::String(s)) if s == replay)
        && THOUGHT_SIGNATURE_PATHS[1..]
            .iter()
            .all(|path| json::path(part, path).is_none())
}

/// Removes every thought signature field, returning whether there was one.
fn delete_part_thought_signature_fields(part: &mut Value) -> bool {
    THOUGHT_SIGNATURE_PATHS.iter().fold(false, |deleted, path| {
        json::delete_path(part, path) | deleted
    })
}
