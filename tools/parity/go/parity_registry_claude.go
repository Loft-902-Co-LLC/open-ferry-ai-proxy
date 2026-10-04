// WP4-A imports internal/translator/interactions/claude and
// internal/translator/claude/interactions here, as blank imports, so that
// sdk/translator's default registry, which the registry/* entries run, holds
// their pairs. It does so in the same commit as the Rust registry gains
// them, in
// crates/open-ferry-translate/src/registry/builtin/interactions/claude.rs,
// and maps them to its suites in tools/parity/src/interactions/claude.rs.
// Not ported yet: nothing is imported.
package main
