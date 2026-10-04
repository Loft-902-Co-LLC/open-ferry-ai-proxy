// Imports internal/translator/codex/interactions, as a blank import, so
// that sdk/translator's default registry, which the registry/* entries run,
// holds its pair: interactions to codex. The Rust registry registers it in
// crates/open-ferry-translate/src/registry/builtin/interactions/codex.rs,
// and tools/parity/src/interactions/codex.rs maps it to its suites.
package main

import (
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/interactions"
)
