//! Tests of the session module, one module per upstream test file, and
//! what they share.
//!
//! Upstream's sdk/cliproxy/session/lcp_test.go and tree_compat's tests
//! aren't here: the LCP fingerprints and the session tree store aren't
//! ported.

mod identity;
mod info;

use http::{HeaderMap, HeaderName, HeaderValue};

use super::{Payload, SessionInfo, extract_session_info};

/// Headers holding each of `pairs`, in order; a name given twice holds
/// both values.
fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

/// The session the request with `pairs` and `payload` names, on a
/// connection with the execution session `execution`.
fn extract(pairs: &[(&str, &str)], payload: &str, execution: &str) -> Option<SessionInfo> {
    extract_session_info(
        &headers(pairs),
        &Payload::parse(payload.as_bytes()),
        execution,
    )
}
