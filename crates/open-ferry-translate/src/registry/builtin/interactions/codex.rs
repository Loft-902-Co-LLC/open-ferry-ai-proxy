// Ported from CLIProxyAPI internal/translator/codex/interactions/init.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Interactions to Codex translator's registration: Interactions clients
//! to a Codex upstream (`interactions` → `codex`).
//!
//! Not ported yet: WP4-D registers the pair here, in the same commit as the
//! parity harness's Go import in `tools/parity/go/parity_registry_codex.go`.
//!
//! Deviations from upstream: none.

use crate::registry::Registry;

pub(super) fn register(registry: &Registry) {
    let _ = registry;
}
