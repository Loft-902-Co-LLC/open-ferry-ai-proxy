// WP4-A's blank imports: internal/translator/interactions/claude and
// internal/translator/claude/interactions register their pairs in
// sdk/translator's default registry, which the registry/* entries run. The
// Rust registry holds the same pairs, in
// crates/open-ferry-translate/src/registry/builtin/interactions/claude.rs,
// and tools/parity/src/interactions/claude.rs maps them to its suites.
package main

import (
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/claude/interactions"
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/interactions/claude"
)
