// WP4-E imports internal/translator/gemini/interactions
// here, as blank imports, so that sdk/translator's default registry, which
// the registry/* entries run, holds their pairs. It does so in the same
// commit as the Rust registry gains them, in
// crates/open-ferry-translate/src/registry/builtin/interactions/gemini.rs,
// and maps them to its suites in tools/parity/src/interactions/gemini.rs.
// Not ported yet: nothing is imported.
package main
