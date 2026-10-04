// WP4-C's blank import: internal/translator/openai/interactions/responses
// registers openai-response -> interactions and interactions ->
// openai-response in sdk/translator's default registry, which the registry/*
// entries run. The Rust registry holds the same pairs, in
// crates/open-ferry-translate/src/registry/builtin/interactions/responses.rs,
// and tools/parity/src/interactions/responses/request.rs and response.rs map
// them to their suites.
package main

import (
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/openai/interactions/responses"
)
