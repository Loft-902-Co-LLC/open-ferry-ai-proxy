//! Our side of the harness's `usage/parse` entry: the usage parsed
//! from an upstream's response body or stream line by
//! `open_ferry_core::observe::usage` (see `go/parity_usage.go`). Not
//! ported yet (P3 WP-C): every case gives null, and no case is
//! generated.

use serde_json::Value;

use crate::cases::Case;

/// `usage/parse`: the usage the case's parser reads from its body or
/// line. For now, null.
pub fn parse(case: &Case) -> Result<Value, String> {
    let _ = case;
    Ok(Value::Null)
}
