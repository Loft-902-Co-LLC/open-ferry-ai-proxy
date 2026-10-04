//! The status errors, ported from upstream's `xai_status_err_test.go`.

use super::*;

// TestXAIStatusErr_FreeUsageExhaustedSets24hRetryAfter.
#[test]
fn free_usage_exhausted_sets_24h_retry_after() {
    let body = r#"{"code":"subscription:free-usage-exhausted","error":"You've used all the included free usage for model grok-4.5-build-free for now. Usage resets over a rolling 24-hour window — tokens (actual/limit): 1065387/1000000."}"#;
    let error = status_error(429, body.as_bytes());
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::from_secs(86_400)));
    assert_eq!(error.message, body);
}

// TestXAIStatusErr_Generic429HasNoRetryAfter.
#[test]
fn generic_429_has_no_retry_after() {
    let error = status_error(429, br#"{"code":"rate_limit","error":"too many requests"}"#);
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, None);
}

// TestXAIStatusErr_Non429Unchanged.
#[test]
fn non_429_unchanged() {
    let error = status_error(400, br#"{"error":"nope"}"#);
    assert_eq!(error.status, 400);
    assert_eq!(error.retry_after, None);
}

// TestXAIStatusErr_BadCredentials403RemapsToUnauthorized.
#[test]
fn bad_credentials_403_remaps_to_unauthorized() {
    let error = status_error(
        403,
        br#"{"code":"unauthenticated:bad-credentials","error":"The OAuth2 access token could not be validated."}"#,
    );
    assert_eq!(error.status, 401);
    assert!(error.text().contains("bad-credentials"), "{}", error.text());
    assert_eq!(error.retry_after, None);
}

// TestXAIStatusErr_BadCredentialsByMessageOnly.
#[test]
fn bad_credentials_by_message_only() {
    let error = status_error(
        403,
        br#"{"error":"The OAuth2 access token could not be validated."}"#,
    );
    assert_eq!(error.status, 401);
}

// TestXAIStatusErr_BadCredentialsNestedErrorCode.
#[test]
fn bad_credentials_nested_error_code() {
    let error = status_error(
        403,
        br#"{"type":"error","status":403,"error":{"code":"unauthenticated:bad-credentials","message":"The OAuth2 access token could not be validated."}}"#,
    );
    assert_eq!(error.status, 401);
}

// TestXAIStatusErr_Generic403Unchanged.
#[test]
fn generic_403_unchanged() {
    let error = status_error(
        403,
        br#"{"code":"permission_denied","error":"model access is not allowed for this account"}"#,
    );
    assert_eq!(error.status, 403);
    assert_eq!(error.retry_after, None);
}

// TestXAIStatusErr_EmptyBodyForbiddenUnchanged.
#[test]
fn empty_body_forbidden_unchanged() {
    let error = status_error(403, b"");
    assert_eq!(error.status, 403);
    assert_eq!(error.text(), "status 403");
}

// Not upstream's: a body that isn't JSON is matched as text, the free
// usage message read from the whole body, and the 401 is made only from a
// 403.
#[test]
fn plain_text_bodies_match_as_text() {
    assert_eq!(status_error(403, b"Bad-Credentials").status, 401);
    let error = status_error(429, b"You have used the INCLUDED FREE USAGE");
    assert_eq!(error.retry_after, Some(FREE_USAGE_EXHAUSTED_COOLDOWN));
    assert_eq!(status_error(500, b"bad-credentials").status, 500);
}
