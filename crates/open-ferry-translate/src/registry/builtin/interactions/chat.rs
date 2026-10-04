// Ported from CLIProxyAPI internal/translator/openai/interactions/chat-completions/init.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Chat Completions and Interactions translators' registrations: Chat
//! Completions clients to an Interactions upstream (`openai` →
//! `interactions`), and Interactions clients to a Chat Completions upstream
//! (`interactions` → `openai`).
//!
//! Not ported yet: WP4-B registers both pairs here, in the same commit as
//! the parity harness's Go import in `tools/parity/go/parity_registry_chat.go`.
//!
//! Deviations from upstream: none.

use crate::registry::Registry;

pub(super) fn register(registry: &Registry) {
    let _ = registry;
}
