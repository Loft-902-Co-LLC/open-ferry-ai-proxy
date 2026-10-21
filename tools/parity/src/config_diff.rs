//! Our side of the harness's `config-diff/details` entry: the change
//! details `open_ferry_core::config::diff::build_change_details` gives
//! between the case's two configs (see `go/parity_config_diff.go`).

use open_ferry_core::config::Config;
use open_ferry_core::config::diff::build_change_details;
use serde_json::{Value, json};

use crate::cases::Case;
use crate::compare::Deviation;

/// The settings whose change lines open-ferry gives and upstream doesn't
/// (see the deviations in the port's `config/diff/mod.rs`): the line starts
/// with the name and a colon.
const UNLOGGED: [&str; 12] = [
    "max-request-body-bytes",
    "streaming.send-keepalive-before-ft",
    "routing.session-affinity-rebind-on",
    "codex.signin-with-chatgpt-identifier",
    "ingress-limits.max-connections",
    "ingress-limits.header-timeout-seconds",
    "ingress-limits.tls-handshake-timeout-seconds",
    "ingress-limits.idle-timeout-seconds",
    "ingress-limits.body-idle-timeout-seconds",
    "ingress-limits.body-total-timeout-seconds",
    "ingress-limits.buffered-body-bytes",
    "ingress-limits.write-stall-timeout-seconds",
];

/// `config-diff/details`: `{"details": [...]}` from the case's old config
/// to its new one, or `{"error": "old"}` or `{"error": "new"}` when that
/// config doesn't parse.
pub fn details(case: &Case) -> Result<Value, String> {
    let parse = |side: &str| Config::parse(case.options[side].as_str().unwrap_or_default());
    let Ok(old) = parse("old") else {
        return Ok(json!({ "error": "old" }));
    };
    let Ok(new) = parse("new") else {
        return Ok(json!({ "error": "new" }));
    };
    Ok(json!({ "details": build_change_details(&old, &new) }))
}

/// Whether `line` is a change line for a setting upstream reloads without
/// one: a name from [`UNLOGGED`], or a Vertex entry's `interactions`
/// (`vertex[0].interactions`), at the start of the line.
fn is_unlogged(line: &str) -> bool {
    let Some((name, _)) = line.split_once(':') else {
        return false;
    };
    UNLOGGED.contains(&name)
        || name
            .strip_prefix("vertex[")
            .and_then(|rest| rest.strip_suffix("].interactions"))
            .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
}

/// Our details as upstream's would be, for the settings upstream reloads
/// without a change line: those lines (see [`is_unlogged`]) are dropped.
/// Every other line is kept, so a line either side has alone still shows.
pub fn drop_unlogged(rust: &mut Value) -> Option<Deviation> {
    let details = rust.get_mut("details")?.as_array_mut()?;
    let before = details.len();
    details.retain(|line| !line.as_str().is_some_and(is_unlogged));
    (details.len() < before).then_some(Deviation::UnloggedChangeLine)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{Value, json};

    use super::drop_unlogged;
    use crate::compare::{Deviation, Numbers, compare_numbers};

    /// Compares upstream's details with ours once ours have lost the lines
    /// upstream doesn't log: the differences and the deviations.
    fn compare(go: &Value, mut rust: Value) -> (usize, BTreeSet<Deviation>) {
        let mut deviations: BTreeSet<Deviation> = drop_unlogged(&mut rust).into_iter().collect();
        let comparison = compare_numbers(go, &rust, &[], Numbers::AsWritten);
        deviations.extend(comparison.deviations);
        (comparison.differences.len(), deviations)
    }

    /// Only lines for settings upstream doesn't log differ: equivalent.
    #[test]
    fn extra_lines_are_equivalent() {
        let go = json!({ "details": ["port: 1 -> 2"] });
        let rust = json!({ "details": [
            "max-request-body-bytes: 1 -> 2",
            "port: 1 -> 2",
            "streaming.send-keepalive-before-ft: false -> true",
            "routing.session-affinity-rebind-on:  -> any",
            "vertex[0].interactions: unset -> true",
            "vertex[12].interactions: true -> false",
            "codex.signin-with-chatgpt-identifier: updated",
        ] });
        assert_eq!(
            compare(&go, rust),
            (0, BTreeSet::from([Deviation::UnloggedChangeLine]))
        );
    }

    #[test]
    fn ingress_change_lines_are_open_ferry_additions() {
        for name in super::UNLOGGED
            .iter()
            .filter(|name| name.starts_with("ingress-limits."))
        {
            let go = json!({"details": []});
            let rust = json!({"details": [format!("{name}: 0 -> 7")]});
            assert_eq!(compare(&go, rust).0, 0, "{name}");
        }
    }

    /// Any other line that differs still does, and so do names that only
    /// start or contain one of the settings.
    #[test]
    fn other_lines_still_differ() {
        let go = json!({ "details": ["port: 1 -> 2"] });
        for extra in [
            "debug: false -> true",
            "max-request-body-bytes-x: 1 -> 2",
            "  max-request-body-bytes: 1 -> 2",
            "x.max-request-body-bytes: 1 -> 2",
            "vertex[].interactions: true -> false",
            "vertex[0].interactions-x: true -> false",
            "vertex[0].base-url: https://a -> https://b",
            "max-request-body-bytes 1 -> 2",
            "ingress-limits.unknown: 0 -> 1",
        ] {
            let rust = json!({ "details": [
                "max-request-body-bytes: 1 -> 2",
                "port: 1 -> 2",
                extra,
            ] });
            let (differences, _) = compare(&go, rust);
            assert_eq!(differences, 1, "{extra}");
        }
        // A line upstream has alone is still missing from ours.
        let go = json!({ "details": ["port: 1 -> 2", "max-request-body-bytes: 1 -> 2"] });
        let rust = json!({ "details": ["port: 1 -> 2"] });
        assert_ne!(compare(&go, rust).0, 0);
    }

    /// Nothing is recorded when nothing was dropped.
    #[test]
    fn nothing_dropped_no_deviation() {
        let mut rust = json!({ "details": ["port: 1 -> 2"] });
        assert_eq!(drop_unlogged(&mut rust), None);
        let mut error = json!({ "error": "old" });
        assert_eq!(drop_unlogged(&mut error), None);
    }
}
