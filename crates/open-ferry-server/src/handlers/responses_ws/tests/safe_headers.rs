use super::*;

#[test]
fn p47_ws_safe_retry_headers_override_or_remove_raw_999() {
    use std::time::Instant;
    for trusted in [Some(true), Some(false), None] {
        let mut source = ExecError::upstream(429, "busy");
        source
            .headers
            .insert("retry-after", HeaderValue::from_static("999"));
        source.trusted_retry_deadline = trusted.map(|live| {
            if live {
                Instant::now() + Duration::from_secs(120)
            } else {
                Instant::now() - Duration::from_secs(1)
            }
        });
        let error = ErrorMessage::from_exec(source);
        let event = parse(&error_payload(&error));
        match trusted {
            Some(true) => {
                let wait: u64 = event["headers"]["Retry-After"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap();
                assert!((1..=120).contains(&wait), "{event}");
            }
            Some(false) => assert!(event["headers"].get("Retry-After").is_none(), "{event}"),
            None => {
                assert_eq!(event["headers"]["Retry-After"], "999");
                assert!(!error.response_headers(false).contains_key("retry-after"));
            }
        }
    }
}

#[test]
fn round2_safe_header_difference_reserved_headers_and_forced_passthrough() {
    let mut source = ExecError::upstream(429, "busy");
    for (name, value) in [
        ("access-control-allow-origin", "https://example.test"),
        ("x-cpa-trace-id", "fake-trace"),
        ("x-custom-upstream", "kept"),
        ("retry-after", "999"),
    ] {
        source.headers.insert(name, HeaderValue::from_static(value));
    }
    let error = ErrorMessage::from_exec(source);
    assert!(error.response_headers(false).is_empty());
    let event = parse(&error_payload(&error));
    assert_eq!(event["headers"]["X-Custom-Upstream"], "kept");
    assert_eq!(event["headers"]["Retry-After"], "999");
    assert!(
        event["headers"]
            .get("Access-Control-Allow-Origin")
            .is_none()
    );
    assert!(event["headers"].get("X-Cpa-Trace-Id").is_none());
}
