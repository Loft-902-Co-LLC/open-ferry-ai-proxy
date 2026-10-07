//! Our side of the harness's `quota-signals/observe` entry: the quota
//! snapshot `QuotaState::observe_response_headers_for_provider` keeps of one
//! response's headers, written as the harness writes upstream's (see
//! `go/parity_quota_signals.go`).

use chrono::{TimeZone as _, Utc};
use http::{HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::auth::QuotaState;
use serde_json::{Value, json};

use crate::cases::Case;

/// `quota-signals/observe`: whether the case's response changed its prior
/// snapshot, and the snapshot after it.
pub fn observe(case: &Case) -> Result<Value, String> {
    let options = &case.options;
    let headers = headers(case)?;
    let prior = &options["prior"];
    let mut quota = QuotaState {
        signals: prior["signals"]
            .as_object()
            .map(|signals| {
                signals
                    .iter()
                    .map(|(name, value)| (name.clone(), value.as_str().unwrap_or("").to_owned()))
                    .collect()
            })
            .unwrap_or_default(),
        ..QuotaState::default()
    };
    let prior_at = prior["observed_at"].as_i64().unwrap_or(0);
    if prior_at != 0 {
        quota.observed_at = Some(at(prior_at)?);
    }
    let provider = options["provider"].as_str().unwrap_or_default();
    let changed = quota.observe_response_headers_for_provider(provider, &headers, at(1000)?);
    Ok(json!({
        "changed": changed,
        "observed_at": quota.observed_at.map_or(0, |at| at.timestamp()),
        "signals": quota.signals,
    }))
}

/// The case's response headers, added in order.
fn headers(case: &Case) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    let Some(list) = case.options["headers"].as_array() else {
        return Ok(headers);
    };
    for header in list {
        let name = header["name"].as_str().unwrap_or_default();
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|err| format!("case {}: header name {name:?}: {err}", case.name))?;
        let bytes = match (header["hex"].as_str(), header["value"].as_str()) {
            (Some(hex), _) => {
                unhex(hex).ok_or_else(|| format!("case {}: bad hex {hex:?}", case.name))?
            }
            (None, Some(value)) => value.as_bytes().to_vec(),
            (None, None) => Vec::new(),
        };
        let value = HeaderValue::from_bytes(&bytes)
            .map_err(|err| format!("case {}: header value {bytes:?}: {err}", case.name))?;
        headers.append(name, value);
    }
    Ok(headers)
}

fn at(secs: i64) -> Result<chrono::DateTime<Utc>, String> {
    Utc.timestamp_opt(secs, 0)
        .single()
        .ok_or_else(|| format!("no time at {secs}"))
}

/// The bytes `hex` spells, two digits each.
fn unhex(hex: &str) -> Option<Vec<u8>> {
    let digits = hex.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}
