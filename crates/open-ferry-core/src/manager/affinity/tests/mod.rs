//! Tests of the session cache, the session keys and the results that
//! refresh or drop bindings, one module each, and what they share. The
//! tests of picking with affinity are in the manager's tests
//! (`manager/tests/affinity.rs`).
//!
//! Upstream runs these on its selector and cache objects with the wall
//! clock; here the time is passed in, from a fixed start.

mod cache;
mod keys;
mod results;

use chrono::{TimeDelta, TimeZone, Utc};
use http::{HeaderMap, HeaderName, HeaderValue};

use crate::auth::Timestamp;

/// The time the tests start at.
fn base() -> Timestamp {
    Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
        .single()
        .expect("valid time")
}

/// `ms` milliseconds after [`base`].
fn at(ms: i64) -> Timestamp {
    base() + TimeDelta::milliseconds(ms)
}

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
