// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/signature_carrier.go
// (encodeGeminiResponsesCarrier, decodeGeminiResponsesCarrier,
// compatibleGeminiResponsesCarrierSignature, geminiResponsesCarrierSemanticTarget,
// geminiResponsesCarrierMatchesAdjacent, hasInternalCarrierFields,
// stripGeminiResponsesCarrierMetadata, normalizeGeminiResponsesCarriers,
// geminiResponsesCarrierDirection, geminiResponsesCarrierTarget,
// isOpenAIResponsesDetachedCarrier) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Signature carriers: reasoning items that hold a Gemini thought signature
//! for a neighbouring item.
//!
//! Gemini signs model parts, function calls among them. A Responses client
//! only replays reasoning items, so a signature that belongs to text or to a
//! call is sent to the client in a reasoning item's `encrypted_content`,
//! wrapped as `cpa-gemini-responses-carrier-v1:<direction>:<target>:<base64>`.
//! The direction says which neighbour the signature belongs to (`next`,
//! `previous`, or `standalone` for none) and the target what kind of item it
//! is (`text`, `function` or `any`).
//!
//! When a request comes back, [`normalize`] unwraps each carrier, checks the
//! signature and that a neighbour of the right kind is there, and marks the
//! item with internal fields the request translator reads. A carrier that
//! fails a check is dropped, or loses its `encrypted_content` if it also
//! holds a summary.
//!
//! Deviations from upstream:
//! - A carrier whose decoded signature isn't UTF-8 fails to decode. Go keeps
//!   the bytes in a string, which can't be valid JSON text either.
//! - Taking the internal fields off an item sorts its keys, as Go's
//!   `json.Marshal` of a map does, but by the keys' characters rather than
//!   their bytes after `json.Unmarshal` replaces invalid UTF-8; serde_json
//!   holds only valid UTF-8, so the two orders are the same.

use std::borrow::Cow;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use serde_json::{Map, Value};

use super::at;
use super::request::assistant_visible_text;
use crate::go;
use crate::json::str_of;
use crate::signature::{
    BlockKind, MAX_GEMINI_THOUGHT_SIGNATURE_LEN, Provider, compatible_signature_for_provider_block,
    is_gemini_thought_signature_bypass, signature_payload_without_provider_prefix,
};

pub(super) const PREFIX: &str = "cpa-gemini-responses-carrier-v1:";
pub(super) const NEXT: &str = "next";
pub(super) const PREVIOUS: &str = "previous";
pub(super) const STANDALONE: &str = "standalone";
pub(super) const TEXT: &str = "text";
pub(super) const FUNCTION: &str = "function";
pub(super) const ANY: &str = "any";

pub(super) const DIRECTION_FIELD: &str = "_cpa_reasoning_direction";
pub(super) const TARGET_FIELD: &str = "_cpa_reasoning_target";
pub(super) const SIGNATURE_FIELD: &str = "_cpa_reasoning_signature";
pub(super) const SUMMARY_FIELD: &str = "_cpa_reasoning_summary";

/// `encodeGeminiResponsesCarrier`: `signature` wrapped for the client, or
/// `""` if it is blank.
pub(super) fn encode(signature: &str, direction: &str, target: &str) -> String {
    let signature = signature.trim();
    if signature.is_empty() {
        return String::new();
    }
    format!(
        "{PREFIX}{direction}:{target}:{}",
        STANDARD_NO_PAD.encode(signature)
    )
}

/// What [`decode`] reads from a signature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Decoded {
    /// The signature: the unwrapped one, or the trimmed input if it isn't a
    /// carrier. Empty if a carrier didn't decode.
    pub signature: String,
    pub direction: String,
    pub target: String,
    /// Whether it starts like a carrier.
    pub marked: bool,
    /// Whether it decoded, or wasn't a carrier.
    pub ok: bool,
}

/// `decodeGeminiResponsesCarrier`.
pub(super) fn decode(raw: &str) -> Decoded {
    let raw = raw.trim();
    let Some(rest) = raw.strip_prefix(PREFIX) else {
        return Decoded {
            signature: raw.to_owned(),
            ok: true,
            ..Decoded::default()
        };
    };
    let failed = Decoded {
        marked: true,
        ..Decoded::default()
    };
    if raw.len() > MAX_GEMINI_THOUGHT_SIGNATURE_LEN * 4 / 3 + 1024 {
        return failed;
    }
    let mut fields = rest.splitn(3, ':');
    let (Some(direction), Some(target), Some(payload)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return failed;
    };
    if ![NEXT, PREVIOUS, STANDALONE].contains(&direction)
        || ![TEXT, FUNCTION, ANY].contains(&target)
    {
        return failed;
    }
    let Ok(decoded) = go::base64::RAW_STD.decode(payload) else {
        return failed;
    };
    let Ok(signature) = String::from_utf8(decoded) else {
        return failed;
    };
    if signature.is_empty() || signature.starts_with(PREFIX) {
        return failed;
    }
    Decoded {
        signature,
        direction: direction.to_owned(),
        target: target.to_owned(),
        marked: true,
        ok: true,
    }
}

/// `compatibleGeminiResponsesCarrierSignature`: `signature` as Gemini takes
/// it for the target kind, unless it isn't one or is the bypass sentinel.
pub(super) fn compatible(signature: &str, target: &str) -> Option<String> {
    let kind = if target == FUNCTION {
        BlockKind::GeminiFunctionCall
    } else {
        BlockKind::GeminiModelPart
    };
    let normalized = compatible_signature_for_provider_block(Provider::Gemini, signature, kind)?;
    if is_gemini_thought_signature_bypass(signature_payload_without_provider_prefix(&normalized)) {
        return None;
    }
    Some(normalized)
}

/// `geminiResponsesCarrierSemanticTarget`: the kind of item a carrier can
/// belong to, or `""`.
fn semantic_target(item: &Value) -> &'static str {
    match &*str_of(item.get("type")) {
        "function_call" | "custom_tool_call" => return FUNCTION,
        "reasoning" if !summary_text(item).trim().is_empty() => return TEXT,
        _ => {}
    }
    if assistant_visible_text(item).is_some() {
        return TEXT;
    }
    ""
}

/// `geminiResponsesCarrierMatchesAdjacent`: whether the nearest item in
/// `direction` past other carriers is of the kind `target` names.
fn matches_adjacent(items: &[Value], index: usize, direction: &str, target: &str) -> bool {
    let mut adjacent = index;
    loop {
        let next = if direction == PREVIOUS {
            adjacent.checked_sub(1)
        } else {
            Some(adjacent + 1)
        };
        let Some(next) = next.filter(|&next| next < items.len()) else {
            return false;
        };
        adjacent = next;
        let kind = semantic_target(&items[adjacent]);
        if !kind.is_empty() {
            return target == ANY || target == kind;
        }
        if !is_detached_carrier(&items[adjacent]) {
            return false;
        }
    }
}

/// `hasInternalCarrierFields`.
fn has_internal_fields(item: &Value) -> bool {
    [
        DIRECTION_FIELD,
        TARGET_FIELD,
        SIGNATURE_FIELD,
        SUMMARY_FIELD,
    ]
    .iter()
    .any(|field| item.get(*field).is_some())
}

/// `stripGeminiResponsesCarrierMetadata`: the item without the internal
/// fields, its keys sorted as `json.Marshal` sorts a map's.
fn strip_metadata(item: &Value) -> Option<Value> {
    let Value::Object(fields) = item else {
        return None;
    };
    let mut kept: Vec<(&String, &Value)> = fields
        .iter()
        .filter(|(key, _)| {
            ![
                DIRECTION_FIELD,
                TARGET_FIELD,
                SIGNATURE_FIELD,
                SUMMARY_FIELD,
            ]
            .contains(&key.as_str())
        })
        .collect();
    kept.sort_by(|a, b| a.0.cmp(b.0));
    Some(Value::Object(
        kept.into_iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Map<String, Value>>(),
    ))
}

/// `normalizeGeminiResponsesCarriers`: the items with each carrier unwrapped
/// and marked, or dropped if it fails a check, and whether any carrier, or
/// any plain reasoning signature Gemini takes, was found.
pub(super) fn normalize(items: &[Value]) -> (Vec<Value>, bool) {
    let mut normalized = Vec::with_capacity(items.len());
    let mut has_valid_carrier = false;
    for (index, original) in items.iter().enumerate() {
        let mut item = Cow::Borrowed(original);
        if has_internal_fields(original)
            && let Some(stripped) = strip_metadata(original)
        {
            item = Cow::Owned(stripped);
        }
        if str_of(item.get("type")) != "reasoning" {
            normalized.push(item.into_owned());
            continue;
        }
        let raw_signature = str_of(item.get("encrypted_content")).trim().to_owned();
        let decoded = decode(&raw_signature);
        if !decoded.marked {
            if !raw_signature.is_empty() {
                has_valid_carrier |= compatible(&raw_signature, ANY).is_some();
            }
            normalized.push(item.into_owned());
            continue;
        }
        let mut ok = decoded.ok;
        let mut signature = String::new();
        if ok {
            match compatible(&decoded.signature, &decoded.target) {
                Some(compatible) => signature = compatible,
                None => ok = false,
            }
        }
        if ok && decoded.direction != STANDALONE {
            ok = matches_adjacent(items, index, &decoded.direction, &decoded.target);
        }
        let is_detached = is_detached_carrier(&item);
        let has_summary = !summary_text(&item).trim().is_empty();
        let valid_summary_carrier = has_summary
            && ((decoded.direction == STANDALONE
                && (decoded.target == TEXT || decoded.target == ANY))
                || decoded.direction == NEXT);
        let mut item = item.into_owned();
        if !ok || (!is_detached && !valid_summary_carrier) {
            if !has_summary {
                continue;
            }
            if let Some(fields) = item.as_object_mut() {
                fields.shift_remove("encrypted_content");
            }
            normalized.push(item);
            continue;
        }
        has_valid_carrier = true;
        if let Some(fields) = item.as_object_mut() {
            fields.insert("encrypted_content".to_owned(), Value::String(signature));
            fields.insert(DIRECTION_FIELD.to_owned(), Value::String(decoded.direction));
            fields.insert(TARGET_FIELD.to_owned(), Value::String(decoded.target));
        }
        normalized.push(item);
    }
    (normalized, has_valid_carrier)
}

/// `geminiResponsesCarrierDirection`.
pub(super) fn direction(item: &Value) -> Cow<'_, str> {
    str_of(item.get(DIRECTION_FIELD))
}

/// `geminiResponsesCarrierTarget`.
pub(super) fn target(item: &Value) -> Cow<'_, str> {
    str_of(item.get(TARGET_FIELD))
}

/// `isOpenAIResponsesDetachedCarrier`: a reasoning item with a signature and
/// no summary text.
pub(super) fn is_detached_carrier(item: &Value) -> bool {
    str_of(item.get("type")) == "reasoning"
        && !str_of(item.get("encrypted_content")).trim().is_empty()
        && summary_text(item).trim().is_empty()
}

/// gjson `Get("summary.0.text").String()`.
pub(super) fn summary_text(item: &Value) -> Cow<'_, str> {
    str_of(at(item, "summary.0.text"))
}

#[cfg(test)]
mod tests;
