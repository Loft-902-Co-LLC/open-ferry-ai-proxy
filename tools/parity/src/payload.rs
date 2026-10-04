//! Our side of the harness's `payload/apply` entry: the config's payload
//! rules applied to a translated body by
//! `open_ferry_providers::payload::apply_call`, and the tracked paths they
//! touched (see `go/parity_payload.go` for the options).

use http::{HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::config::Config;
use open_ferry_providers::payload::{Call, Rules, apply_call};
use serde_json::{Value, json};

use crate::cases::Case;

/// `payload/apply`: `{"body": ..., "touched": [...]}` for the case's body
/// with its rules applied, or `{"config_error": true}` when its config
/// can't be read.
pub fn apply(case: &Case) -> Result<Value, String> {
    let options = &case.options;
    let text = |key: &str| options[key].as_str().unwrap_or_default();
    let rules = if options["no_config"] == true {
        None
    } else {
        match Config::parse(text("config")) {
            Ok(config) => Some(Rules::compile(&config)),
            Err(_) => return Ok(json!({ "config_error": true })),
        }
    };
    let mut headers = HeaderMap::new();
    for pair in options["headers"].as_array().into_iter().flatten() {
        let name = pair[0].as_str().unwrap_or_default();
        let value = pair[1].as_str().unwrap_or_default();
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
        let value = HeaderValue::from_str(value).map_err(|error| error.to_string())?;
        headers.append(name, value);
    }
    let tracked: Vec<&str> = options["tracked"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let original = match options["original"].as_str() {
        None | Some("") => None,
        Some(original) => {
            Some(serde_json::from_str(original).map_err(|error| format!("original: {error}"))?)
        }
    };
    let mut body: Value =
        serde_json::from_str(&case.request).map_err(|error| format!("body: {error}"))?;
    let call = Call {
        executor: text("executor"),
        protocol: text("protocol"),
        from: text("from"),
        model: &case.model,
        requested_model: text("requested_model"),
        request_path: text("request_path"),
        root: text("root"),
        headers: &headers,
        tracked: &tracked,
    };
    let touched = apply_call(rules.as_ref(), &call, || original, &mut body);
    Ok(json!({ "body": body, "touched": touched.iter().collect::<Vec<_>>() }))
}
