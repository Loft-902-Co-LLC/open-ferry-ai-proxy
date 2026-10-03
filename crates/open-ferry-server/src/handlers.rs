//! The endpoints.
//!
//! Handlers read the fields they route by from a body parsed as JSON. Where
//! upstream reads fields from a body that isn't JSON with gjson, which finds
//! what it can, these read nothing.

pub(crate) mod claude;
pub(crate) mod codex_client;
pub(crate) mod gemini;
pub(crate) mod health;
pub(crate) mod models;
pub(crate) mod openai;
pub(crate) mod responses;
pub(crate) mod responses_ws;

use axum::response::Response;
use bytes::Bytes;
use http::HeaderMap;
use serde_json::Value;

use crate::errors::{JSON_UTF8, error_response};

/// A body parsed as JSON, or `null` when it isn't JSON.
pub(crate) fn parse_body(raw: &[u8]) -> Value {
    serde_json::from_slice(raw).unwrap_or(Value::Null)
}

/// A field as gjson's `String()` gives it: a string as it is, `true` or
/// `false`, a number as written, other JSON as compact text, and nothing as
/// empty.
pub(crate) fn gjson_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// A 200 JSON response the server builds itself (gin's `c.JSON`).
pub(crate) fn json_utf8(body: String) -> Response {
    error_response(200, HeaderMap::new(), Bytes::from(body), JSON_UTF8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_fields_as_gjson_strings() {
        let body = parse_body(br#"{"a":"x","b":1.50,"c":true,"d":null,"e":{"f": [1]}}"#);
        assert_eq!(gjson_string(body.get("a")), "x");
        assert_eq!(gjson_string(body.get("b")), "1.50");
        assert_eq!(gjson_string(body.get("c")), "true");
        assert_eq!(gjson_string(body.get("d")), "");
        assert_eq!(gjson_string(body.get("e")), r#"{"f":[1]}"#);
        assert_eq!(gjson_string(body.get("missing")), "");
        assert_eq!(parse_body(b"{nope"), Value::Null);
        assert_eq!(parse_body(b"[1]"), json!([1]));
    }
}
