// WP4-D imports internal/translator/codex/interactions here, as a blank
// import, so that sdk/translator's default registry, which the registry/*
// entries run, holds its pair (interactions to codex). It does so in the
// same commit as the Rust registry gains it, in
// crates/open-ferry-translate/src/registry/builtin/interactions/codex.rs,
// and maps it to its suites in tools/parity/src/interactions/codex.rs.
package main

import (
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/interactions"
)
