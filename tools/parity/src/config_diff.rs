//! Our side of the harness's `config-diff/details` entry: the change
//! details `open_ferry_core::config::diff::build_change_details` gives
//! between the case's two configs (see `go/parity_config_diff.go`).

use open_ferry_core::config::Config;
use open_ferry_core::config::diff::build_change_details;
use serde_json::{Value, json};

use crate::cases::Case;

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
