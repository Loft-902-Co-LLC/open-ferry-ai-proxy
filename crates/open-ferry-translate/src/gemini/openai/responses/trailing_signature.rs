// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/trailing_signature.go
// (cacheGeminiResponsesTextSignatures, restoreGeminiResponsesTextSignatures)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Signatures Gemini sends after a message's text, kept out of the client's
//! reasoning timeline.
//!
//! A signature that trails text belongs to that text. Rather than send it to
//! the client as a carrier after the message, the response translator keeps
//! it in the replay cache under the message's ID with a hash of its text.
//! When the message comes back unchanged in a later request, the signatures
//! are put back after it as carriers, in their original order, and any
//! carrier the client sent for the same signature is dropped.
//!
//! Deviations from upstream: none.

use std::collections::HashSet;
use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::replay_cache;
use super::request::assistant_visible_text;
use super::signature_carrier::{PREVIOUS, TEXT, compatible, decode, encode, is_detached_carrier};
use crate::json::{object, str_of};
use crate::thinking::base_model_name;

/// The replay cache session for a message's text signatures.
fn session(message_id: &str) -> String {
    format!("gemini-responses-text:{message_id}")
}

/// The SHA-256 of `text`, in lower-case hex.
fn text_hash(text: &str) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(text.as_bytes()) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// `cacheGeminiResponsesTextSignatures`: keeps `signatures` for the text of
/// message `message_id`. Returns whether they were kept; nothing is kept if
/// any isn't a signature Gemini takes for text.
pub(super) fn cache_text_signatures(
    model: &str,
    message_id: &str,
    text: &str,
    signatures: &[String],
) -> bool {
    if message_id.is_empty() || text.is_empty() {
        return false;
    }
    let hash = text_hash(text);
    let mut items = Vec::with_capacity(signatures.len());
    for signature in signatures {
        if compatible(signature, TEXT).is_none() {
            return false;
        }
        items.push(object([
            ("type", "thought_signature".into()),
            ("targetKind", "text".into()),
            ("thoughtSignature", Value::String(signature.clone())),
            ("targetHash", Value::String(hash.clone())),
        ]));
    }
    replay_cache::cache_items(base_model_name(model), &session(message_id), &items)
}

/// `restoreGeminiResponsesTextSignatures`: the items with the signatures
/// kept for each assistant message put back after it as carriers.
pub(super) fn restore_text_signatures(model: &str, items: &[Value]) -> Vec<Value> {
    let mut restored = Vec::with_capacity(items.len());
    let mut skip = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        if skip.contains(&index) {
            continue;
        }
        restored.push(item.clone());
        let Some(text) = assistant_visible_text(item) else {
            continue;
        };
        let message_id = str_of(item.get("id"));
        let message_id = message_id.trim();
        if message_id.is_empty() {
            continue;
        }
        let Some(cached) = replay_cache::get_items(base_model_name(model), &session(message_id))
        else {
            continue;
        };
        // Replay the cached signatures in their order, then keep the
        // client's carriers for any others.
        let mut replayed = HashSet::new();
        let hash = text_hash(&text);
        for entry in &cached {
            let signature = str_of(entry.get("thoughtSignature"));
            if str_of(entry.get("targetHash")) != hash {
                continue;
            }
            restored.push(object([
                ("type", "reasoning".into()),
                ("summary", Value::Array(Vec::new())),
                (
                    "encrypted_content",
                    Value::String(encode(&signature, PREVIOUS, TEXT)),
                ),
            ]));
            replayed.insert(signature.into_owned());
        }
        for (adjacent, next) in items.iter().enumerate().skip(index + 1) {
            if !is_detached_carrier(next) {
                break;
            }
            let decoded = decode(&str_of(next.get("encrypted_content")));
            if decoded.ok
                && decoded.direction == PREVIOUS
                && decoded.target == TEXT
                && replayed.contains(&decoded.signature)
            {
                skip.insert(adjacent);
            }
        }
    }
    restored
}

#[cfg(test)]
mod tests;
