//! Our side of the `session/info` and `session/derive` entries: the session
//! a request names, read by `open_ferry_core::session::extract_session_info`,
//! and the identity `derive_id` derives for it, written as the harness
//! writes upstream's (see `go/parity_session.go` for the options).

use http::{HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::session::{Payload, derive_id, extract_session_info};
use serde_json::{Value, json};

use crate::cases::Case;

/// `session/info`: the session the case's body and headers name, or null
/// when they name none.
pub fn info(case: &Case) -> Result<Value, String> {
    let options = &case.options;
    let mut headers = HeaderMap::new();
    for pair in options["headers"].as_array().into_iter().flatten() {
        let name = pair[0].as_str().unwrap_or_default();
        let value = pair[1].as_str().unwrap_or_default();
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
        let value = HeaderValue::from_str(value).map_err(|error| error.to_string())?;
        headers.append(name, value);
    }
    let execution_id = options["execution_id"].as_str().unwrap_or_default();
    let payload = Payload::parse(case.request.as_bytes());
    Ok(
        extract_session_info(&headers, &payload, execution_id).map_or(Value::Null, |info| {
            json!({
                "session_id": info.session_id,
                "parent_session_id": info.parent_session_id,
                "agent_name": info.agent_name,
                "client_type": info.client_type,
                "is_fork": info.is_fork,
                "is_subagent": info.is_subagent,
            })
        }),
    )
}

/// `session/derive`: the identity derived for the case's body, its client
/// format and caller scope, empty when there is none.
pub fn derive(case: &Case) -> Result<Value, String> {
    let options = &case.options;
    let text = |key: &str| options[key].as_str().unwrap_or_default();
    let payload = Payload::parse(case.request.as_bytes());
    Ok(Value::String(derive_id(
        text("format"),
        &payload,
        text("caller_scope"),
    )))
}
