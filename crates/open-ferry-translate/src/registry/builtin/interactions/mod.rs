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

/// The first whole response value, ignoring trailing data as gjson does,
/// or `null` if no complete value is readable. Keep each number's text for
/// the translators that copy it as upstream does.
fn parse(body: &[u8]) -> Value {
    exact::first(body)
        .and_then(Result::ok)
        .unwrap_or(Value::Null)
}
