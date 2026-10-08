// Ported from CLIProxyAPI internal/runtime/executor/helps/payload_helpers.go
// (buildPayloadPath, payloadRuleTargetsPath, resolvePayloadRulePaths,
// splitPayloadRulePath, parsePayloadQueryPathPart, findPayloadQueryClose,
// appendPayloadPathPart, payloadValueAtPath) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A rule's path under the call's root, and the paths it stands for in a
//! body: a `#(query)` key is replaced by the index of the first array item
//! the query matches, and a `#(query)#` key by the index of each.
//!
//! Deviations from upstream: none.

use serde_json::Value;

use super::gjson::{self, Found, Loc};
use super::query;

/// `path` under `root`, both trimmed, without a leading `.` on `path`
/// (`buildPayloadPath`).
pub(super) fn build_path(root: &str, path: &str) -> String {
    let root = root.trim();
    let path = path.trim();
    if root.is_empty() {
        return path.to_owned();
    }
    if path.is_empty() {
        return root.to_owned();
    }
    let path = path.strip_prefix('.').unwrap_or(path);
    format!("{root}.{path}")
}

/// Whether writing `path` touches `tracked`: the same path, one inside it,
/// or one it is inside (`payloadRuleTargetsPath`).
pub(super) fn targets_path(path: &str, tracked: &str) -> bool {
    if tracked.is_empty() || path.is_empty() {
        return false;
    }
    let below = |inner: &str, outer: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('.'))
    };
    path == tracked || below(path, tracked) || below(tracked, path)
}

/// The paths `path` stands for in `doc`, each `#(query)` key replaced by
/// the indexes of the items it matches (`resolvePayloadRulePaths`). Empty
/// when a query matches nothing.
pub(super) fn resolve(doc: &Value, path: &str) -> Vec<String> {
    let path = path.trim();
    if path.is_empty() {
        return Vec::new();
    }
    if !path.contains("#(") {
        return vec![path.to_owned()];
    }
    let mut paths = vec![String::new()];
    for part in split_rule_path(path) {
        let Some((query, all)) = parse_query_part(part) else {
            for path in &mut paths {
                *path = append_part(path, part);
            }
            continue;
        };
        let mut next = Vec::with_capacity(paths.len());
        for base in &paths {
            let Some(found) = value_at(doc, base) else {
                continue;
            };
            let Some(items) = found.array_items() else {
                continue;
            };
            for (index, item) in items.iter().enumerate() {
                if !query::matches(item, query) {
                    continue;
                }
                next.push(append_part(base, &index.to_string()));
                if !all {
                    break;
                }
            }
        }
        paths = next;
        if paths.is_empty() {
            return Vec::new();
        }
    }
    paths
}

/// What `path` finds in `doc`, the whole body for an empty path
/// (`payloadValueAtPath`).
fn value_at<'a>(doc: &'a Value, path: &str) -> Option<Found<'a>> {
    if path.is_empty() {
        return Some(Found::At(doc, Loc::new()));
    }
    gjson::get(doc, path)
}

/// `path` split at the `.`s outside quotes and parentheses
/// (`splitPayloadRulePath`).
fn split_rule_path(path: &str) -> Vec<&str> {
    let bytes = path.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => quote = Some(b),
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'.' if depth == 0 => {
                parts.push(path.get(start..i).unwrap_or_default());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(path.get(start..).unwrap_or_default());
    parts
}

/// The query of a `#(query)` or `#(query)#` key, trimmed, and whether it
/// takes every match (`parsePayloadQueryPathPart`).
fn parse_query_part(part: &str) -> Option<(&str, bool)> {
    if !part.starts_with("#(") {
        return None;
    }
    let close = find_close(part)?;
    let suffix = part.get(close + 1..).unwrap_or_default();
    if !suffix.is_empty() && suffix != "#" {
        return None;
    }
    Some((part.get(2..close).unwrap_or_default().trim(), suffix == "#"))
}

/// Where the `)` closing the `#(` at the start of `part` is
/// (`findPayloadQueryClose`).
fn find_close(part: &str) -> Option<usize> {
    let bytes = part.as_bytes();
    let mut quote = None;
    let mut escaped = false;
    let mut depth = 1usize;
    for (i, &b) in bytes.iter().enumerate().skip(2) {
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => quote = Some(b),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `part` after `path`, with a `.` between them when both have something
/// (`appendPayloadPathPart`).
fn append_part(path: &str, part: &str) -> String {
    if path.is_empty() {
        return part.to_owned();
    }
    if part.is_empty() {
        return path.to_owned();
    }
    format!("{path}.{part}")
}
