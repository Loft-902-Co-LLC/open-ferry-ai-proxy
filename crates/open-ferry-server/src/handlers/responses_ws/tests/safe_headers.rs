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
