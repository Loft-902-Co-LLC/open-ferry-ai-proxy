//! The translators to and from Gemini Interactions, registered as the
//! `init` functions of upstream's six Interactions packages register them
//! (`internal/translator/antigravity/interactions` isn't ported). Each family
//! of translators registers its pairs in a file of its own, called from
//! here.

mod chat;
mod claude;
mod codex;
mod gemini;
mod responses;

use serde_json::Value;

use crate::json::exact;
use crate::registry::Registry;

pub(super) fn register(registry: &Registry) {
    claude::register(registry);
    chat::register(registry);
    responses::register(registry);
    codex::register(registry);
    gemini::register(registry);
}

/// A whole response's body as JSON, or `null` if it isn't, with each
/// number's text kept for the translators that copy it as upstream does.
fn parse(body: &[u8]) -> Value {
    exact::from_slice(body).unwrap_or(Value::Null)
}
