//! Our side of the harness's `config-diff/details` entry: the change
//! details `open_ferry_core::config::diff::build_change_details` gives
//! for two configs (see `go/parity_config_diff.go`). Not ported yet (P3
//! WP-B): every case gives null, and no case is generated.

use serde_json::Value;

use crate::cases::Case;

/// `config-diff/details`: the change details from the case's old
/// config to its new one. For now, null.
pub fn details(case: &Case) -> Result<Value, String> {
    let _ = case;
    Ok(Value::Null)
}
