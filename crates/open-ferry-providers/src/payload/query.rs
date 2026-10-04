// Ported from CLIProxyAPI internal/runtime/executor/helps/payload_helpers.go
// (payloadQueryMatches, payloadQueryAndMatches, splitPayloadLogical,
// payloadQueryTermMatches) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The query of a rule path's `#(query)` key: terms joined by `&&` and
//! `||` (`&&` binding tighter), each a gjson `#(...)` query tested against
//! the one array item.
//!
//! Deviations from upstream: none.

use std::slice;

use serde_json::Value;

use super::gjson;

/// Whether `item` passes `query` (`payloadQueryMatches`).
pub(super) fn matches(item: &Value, query: &str) -> bool {
    split_logical(query, "||").into_iter().any(|either| {
        split_logical(either, "&&")
            .into_iter()
            .all(|term| term_matches(item, term))
    })
}

/// `query` split at each `operator` outside quotes, the parts trimmed
/// (`splitPayloadLogical`).
fn split_logical<'q>(query: &'q str, operator: &str) -> Vec<&'q str> {
    let bytes = query.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            quote = Some(b);
        } else if bytes
            .get(i..)
            .is_some_and(|rest| rest.starts_with(operator.as_bytes()))
        {
            parts.push(query.get(start..i).unwrap_or_default().trim());
            i += operator.len();
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(query.get(start..).unwrap_or_default().trim());
    parts
}

/// Whether gjson's `#(term)` finds `item` (`payloadQueryTermMatches`).
fn term_matches(item: &Value, term: &str) -> bool {
    let term = term.trim();
    if term.is_empty() {
        return false;
    }
    gjson::get_in_items(slice::from_ref(item), &format!("#({term})")).is_some()
}
