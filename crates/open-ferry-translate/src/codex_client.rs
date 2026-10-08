// Ported from CLIProxyAPI internal/client/codex/optimize-multi-agent-v2/
// optimize_multi_agent_v2.go (headerValueCaseInsensitive) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Rewrites of requests from Codex clients, upstream's
//! `internal/client/codex/optimize-multi-agent-v2` and `tool-schema`.
//!
//! - [`multi_agent_v2`] readies Codex's multi-agent v2 tools and messages for
//!   other upstreams, and undoes its namespace rename in responses.
//! - [`orphan_delegation`] turns delegation tool outputs without their call
//!   into user messages.
//! - [`tool_integers`] gives Codex's own tools integer parameter types when
//!   the request goes to another provider.
//!
//! They work on parsed JSON and take what they need from the request's
//! headers and the config as plain values, and the models `spawn_agent` may
//! pick as a function that writes their list, so the server and the
//! executors can both call them.
//!
//! Deviations from upstream:
//! - Edits are made on parsed JSON rather than spliced into the bytes, so a
//!   changed body is written again, and of keys repeated in an object only
//!   the last counts (gjson reads the first).

pub mod multi_agent_v2;
pub mod orphan_delegation;
pub mod tool_integers;

use crate::go;

/// The first of a header's values that isn't blank, trimmed, or empty
/// (`headerValueCaseInsensitive`). Pass the values of one header, which a
/// header map already finds regardless of case.
pub fn header_value<'a>(values: impl IntoIterator<Item = &'a [u8]>) -> String {
    values
        .into_iter()
        .map(go::trim_space)
        .find(|value| !value.is_empty())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::header_value;

    #[test]
    fn header_value_is_the_first_value_that_is_not_blank() {
        let values: [&[u8]; 3] = [b"  ", b" codex-tui/0.154.0 ", b"other"];
        assert_eq!(header_value(values), "codex-tui/0.154.0");
        assert_eq!(header_value(std::iter::empty()), "");
        assert_eq!(header_value([b" \t".as_slice()]), "");
    }
}
