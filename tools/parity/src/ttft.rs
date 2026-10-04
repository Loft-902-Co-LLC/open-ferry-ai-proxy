//! Our side of the harness's `ttft/token-event` entry: whether an
//! upstream's stream event carries the first token, as
//! `open_ferry_core::observe::usage` decides it for the time to first
//! token (see `go/parity_ttft.go`). Not ported yet (P3 WP-C): every case
//! gives null, and no case is generated.

use serde_json::Value;

use crate::cases::Case;

/// `ttft/token-event`: whether the case's event carries a token, for
/// its format. For now, null.
pub fn token_event(case: &Case) -> Result<Value, String> {
    let _ = case;
    Ok(Value::Null)
}
