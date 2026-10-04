// Ported from CLIProxyAPI internal/translator/interactions/claude/init.go and
// internal/translator/claude/interactions/init.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Claude and Interactions translators' registrations: Claude Messages
//! clients to an Interactions upstream (`claude` → `interactions`), and
//! Interactions clients to a Claude upstream (`interactions` → `claude`).
//!
//! Not ported yet: WP4-A registers both pairs here, in the same commit as
//! the parity harness's Go import in `tools/parity/go/parity_registry_claude.go`.
//!
//! Deviations from upstream: none.

use crate::registry::Registry;

pub(super) fn register(registry: &Registry) {
    let _ = registry;
}
