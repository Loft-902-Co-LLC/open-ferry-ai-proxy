// Imports internal/translator/openai/interactions/chat-completions, as a
// blank import, so that sdk/translator's default registry, which the
// registry/* entries run, holds its pairs: openai to interactions and
// interactions to openai. The Rust registry registers them in
// crates/open-ferry-translate/src/registry/builtin/interactions/chat.rs,
// and tools/parity/src/interactions/chat.rs maps them to its suites.
package main

import (
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/openai/interactions/chat-completions"
)
