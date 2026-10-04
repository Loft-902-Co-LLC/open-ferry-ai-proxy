// Ported from CLIProxyAPI internal/translator/gemini/interactions/init.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Gemini and Interactions translators' registrations: Interactions
//! clients to a Gemini upstream (`interactions` → `gemini`), Gemini clients
//! to an Interactions upstream (`gemini` → `interactions`), and Interactions
//! passed through (`interactions` → `interactions`).
//!
//! Not ported yet: WP4-E registers the three pairs here, in the same commit
//! as the parity harness's Go import in
//! `tools/parity/go/parity_registry_gemini.go`.
//!
//! Deviations from upstream: none.

use crate::registry::Registry;

pub(super) fn register(registry: &Registry) {
    let _ = registry;
}
