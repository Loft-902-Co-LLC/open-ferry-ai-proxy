// Ported from CLIProxyAPI internal/translator/openai/interactions/responses/init.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The OpenAI Responses and Interactions translators' registrations:
//! Responses clients to an Interactions upstream (`openai-response` →
//! `interactions`), and Interactions clients to a Responses upstream
//! (`interactions` → `openai-response`).
//!
//! Not ported yet: WP4-C (WP4-C2, if it is split) registers both pairs here,
//! in the same commit as the parity harness's Go import in
//! `tools/parity/go/parity_registry_responses.go`.
//!
//! Deviations from upstream: none.

use crate::registry::Registry;

pub(super) fn register(registry: &Registry) {
    let _ = registry;
}
