//! Our side of the harness's `payload/apply` entry: the config's
//! payload rules applied to a translated body by
//! `open_ferry_providers::payload::apply`, and the tracked paths they
//! touched (see `go/parity_payload.go`). Not ported yet (P3 WP-D): every
//! case gives null, and no case is generated.

use serde_json::Value;

use crate::cases::Case;

/// `payload/apply`: the case's body with its rules applied, and the
/// tracked paths they touched. For now, null.
pub fn apply(case: &Case) -> Result<Value, String> {
    let _ = case;
    Ok(Value::Null)
}
