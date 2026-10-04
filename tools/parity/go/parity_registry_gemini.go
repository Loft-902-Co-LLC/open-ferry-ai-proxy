// Imports internal/translator/gemini/interactions, as a blank import, so
// that sdk/translator's default registry, which the registry/* entries run,
// holds its pairs: interactions → gemini, gemini → interactions and
// interactions → interactions. The Rust registry registers them in
// crates/open-ferry-translate/src/registry/builtin/interactions/gemini.rs,
// and tools/parity/src/interactions/gemini.rs maps them to its suites.
package main

import (
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/gemini/interactions"
)
