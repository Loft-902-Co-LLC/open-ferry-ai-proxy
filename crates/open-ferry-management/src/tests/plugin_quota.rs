// Ported from CLIProxyAPI internal/api/handlers/management/plugin_quota_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `POST /v0/management/quota/fetch`, probing servers on 127.0.0.1.
//!
//! Deviations from upstream:
//! - `TestGetQuotaProviders_Endpoint`, `TestFetchCredentialQuota_Endpoint`,
//!   `TestResetCredentialQuota_Endpoint`, `TestPluginSpecificQuotaEndpoints`,
//!   `TestAuthFilesList_IncludesQuotaSupport` and
//!   `TestResetCredentialQuota_FailureDoesNotClearCooldown` are dropped: each
//!   needs a plugin that provides quotas, and this port has no plugin host.
//! - Upstream's tests give the handler an empty plugin host; this port has
//!   none, which answers the same.
//! - `TestFilterUsableQuotaSummary*`, `TestMapProbeResponseAcceptsSummaryOnly`
//!   and `TestExecuteQuotaProbeStripsSummaryKeyCaseInsensitively` call
//!   upstream's functions; these probe through the route, whose answer
//!   holds what the functions return.
//! - Where upstream checks only a 502, these check the whole answer.
//! - The tests from `the_probe_never_names_the_client` on are this port's.

use http::StatusCode;
use open_ferry_core::auth::Auth;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::{Answer, Api, Upstream, http_response};

const PATH: &str = "/v0/management/quota/fetch";

/// A credential holding `metadata`.
fn probe_auth(id: &str, metadata: Value) -> Auth {
    let Value::Object(metadata) = metadata else {
        panic!("metadata isn't an object");
    };
    Auth {
        id: id.into(),
        file_name: format!("{id}.json"),
        provider: "probe".into(),
        metadata,
        ..Auth::default()
    }
}

/// The answer for a credential holding `metadata`.
async fn fetch(metadata: Value) -> Answer {
    let api = Api::new();
    let index = api.register(probe_auth("probe-auth", metadata));
    api.post(PATH, &json!({ "auth_index": index }).to_string())
        .await
}

/// The answer for a credential probing `url`, without a token.
async fn fetch_url(url: &str) -> Answer {
    fetch(json!({ "quota_probe": { "url": url, "method": "GET" } })).await
}

/// An upstream answering every request with 200 and the JSON `body`.
async fn json_upstream(body: &str) -> Upstream {
    Upstream::answering(http_response(
        "200 OK",
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    ))
    .await
}

/// The values of the request's header `name`.
fn header_values<'a>(request: &'a str, name: &str) -> Vec<&'a str> {
    let head = &request[..request.find("\r\n\r\n").unwrap()];
    head.split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(": "))
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
        .collect()
}

/// The 502 a failed probe gives, for `reason`.
fn probe_failed(reason: &str) -> String {
    json!({ "error": format!("quota probe failed: {reason}") }).to_string()
}

/// Why a body in neither shape fails.
const NO_MATCH: &str =
    "upstream probe response does not match normalized quota shape or declared mapping";

// TestFetchCredentialQuota_DeclarativeProbe
#[tokio::test]
async fn declarative_probe() {
    let upstream = Upstream::start(|_, request| {
        if header_values(request, "Authorization") != ["Bearer secret-token"] {
            return http_response("401 Unauthorized", &[], b"unauthorized\n");
        }
        let date = chrono::Utc::now()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        http_response(
            "200 OK",
            &[("Content-Type", "application/json"), ("Date", &date)],
            br#"{
                "subscription": {"plan": "ProbePro"},
                "groups": [
                    {
                        "displayName": "API Limits",
                        "buckets": [
                            {"window": "monthly", "remainingFraction": 0.65, "resetTime": "2026-10-01T00:00:00Z"}
                        ]
                    }
                ]
            }"#,
        )
    })
    .await;
    let answer = fetch(json!({
        "token": "secret-token",
        "quota_probe": {
            "url": format!("{}/usage", upstream.url),
            "method": "GET",
            "header": { "Authorization": "Bearer $TOKEN$" },
        },
    }))
    .await;
    let body = answer.expect(StatusCode::OK);
    assert_eq!(body["subscription"], json!({ "plan": "ProbePro" }));
    assert_eq!(body["groups"][0]["buckets"][0]["remainingFraction"], 0.65);
    assert_eq!(body["groups"].as_array().unwrap().len(), 1);
}

// TestFetchCredentialQuota_DeclarativeProbeSummaryOnly
#[tokio::test]
async fn declarative_probe_summary_only() {
    let upstream = json_upstream(
        r#"{"summary":[{"key":"balance","label":"Balance","value":42,"unit":"credits"}]}"#,
    )
    .await;
    fetch_url(&upstream.url).await.assert(
        StatusCode::OK,
        r#"{"summary":[{"key":"balance","label":"Balance","value":42,"unit":"credits"}]}"#,
    );
}

// TestFilterUsableQuotaSummaryRequiresStringIdentifiers
#[tokio::test]
async fn summary_requires_string_identifiers() {
    let upstream = json_upstream(
        r#"{"summary":[{"key":123,"label":true,"value":1},{"key":"balance","label":"Balance","value":0}]}"#,
    )
    .await;
    fetch_url(&upstream.url).await.assert(
        StatusCode::OK,
        r#"{"summary":[{"key":"balance","label":"Balance","value":0}]}"#,
    );
}

// TestExecuteQuotaProbeStripsSummaryKeyCaseInsensitively
#[tokio::test]
async fn probe_strips_summary_key_case_insensitively() {
    let upstream =
        json_upstream(r#"{"subscription":{"plan":"ProbePro"},"Summary":"usage text"}"#).await;
    fetch(json!({ "quota_probe": { "url": upstream.url } }))
        .await
        .assert(StatusCode::OK, r#"{"subscription":{"plan":"ProbePro"}}"#);
}

// TestMapProbeResponseAcceptsSummaryOnly
#[tokio::test]
async fn mapping_accepts_summary_only() {
    let upstream =
        json_upstream(r#"{"summary":[{"key":"balance","label":"Balance","value":42}]}"#).await;
    fetch(json!({
        "quota_probe": { "url": upstream.url, "mapping": { "plan": "missing.plan" } },
    }))
    .await
    .assert(
        StatusCode::OK,
        r#"{"summary":[{"key":"balance","label":"Balance","value":42}]}"#,
    );
}

// TestFilterUsableQuotaSummaryRequiresValidCurrencyCode
#[tokio::test]
async fn summary_requires_valid_currency_code() {
    let upstream = json_upstream(
        r#"{"summary":[
            {"key":"invalid","label":"Invalid","value":1,"format":"currency","currency":"US"},
            {"key":"valid","label":"Valid","value":2,"format":"currency","currency":"USD"}
        ]}"#,
    )
    .await;
    fetch_url(&upstream.url).await.assert(
        StatusCode::OK,
        r#"{"summary":[{"key":"invalid","label":"Invalid","value":1},{"key":"valid","label":"Valid","value":2,"format":"currency","currency":"USD"}]}"#,
    );
}

// TestFilterUsableQuotaSummaryOmitsNonStringOptionalMetadata
#[tokio::test]
async fn summary_omits_non_string_optional_metadata() {
    let upstream = json_upstream(
        r#"{"summary":[{"key":"balance","label":"Balance","value":42,"unit":123,"format":true,"currency":["USD"]}]}"#,
    )
    .await;
    fetch_url(&upstream.url).await.assert(
        StatusCode::OK,
        r#"{"summary":[{"key":"balance","label":"Balance","value":42}]}"#,
    );
}

// TestFetchCredentialQuota_DeclarativeProbeSummaryWithoutValueReturnsError
#[tokio::test]
async fn summary_without_value_fails() {
    let upstream = json_upstream(r#"{"summary":[{"key":"balance","label":"Balance"}]}"#).await;
    fetch_url(&upstream.url)
        .await
        .assert(StatusCode::BAD_GATEWAY, &probe_failed(NO_MATCH));
}

// TestFetchCredentialQuota_DeclarativeProbeIgnoresMalformedOptionalSummary
#[tokio::test]
async fn malformed_optional_summary_is_ignored() {
    let upstream =
        json_upstream(r#"{"subscription":{"plan":"ProbePro"},"summary":"usage text"}"#).await;
    fetch_url(&upstream.url)
        .await
        .assert(StatusCode::OK, r#"{"subscription":{"plan":"ProbePro"}}"#);
}

// TestFetchCredentialQuota_DeclarativeProbeWithMapping
#[tokio::test]
async fn declarative_probe_with_mapping() {
    let date = (chrono::Utc::now() + chrono::Duration::minutes(5))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let upstream = Upstream::answering(http_response(
        "200 OK",
        &[("Content-Type", "application/json"), ("Date", &date)],
        br#"{
            "user": {"tier": "Enterprise"},
            "Summary": [{"key": "credits_used", "label": "Credits used", "value": 40}],
            "packages": [
                {
                    "period": "monthly",
                    "used": 40,
                    "total": 200,
                    "remain": 160,
                    "expires": "2026-10-15T00:00:00Z"
                }
            ]
        }"#,
    ))
    .await;
    let answer = fetch(json!({
        "quota_probe": {
            "url": format!("{}/billing", upstream.url),
            "method": "GET",
            "mapping": {
                "plan": "user.tier",
                "groups": [{
                    "display_name": "Resource Packages",
                    "buckets_path": "packages",
                    "window_key": "period",
                    "remaining_amount_key": "remain",
                    "total_amount_key": "total",
                    "reset_time_key": "expires",
                }],
            },
        },
    }))
    .await;
    let mut body = answer.expect(StatusCode::OK);
    let offset = body["serverTimeOffsetMs"].as_i64().unwrap();
    assert!(offset > 0, "{offset}");
    body.as_object_mut().unwrap().remove("serverTimeOffsetMs");
    assert_eq!(
        body,
        json!({
            "subscription": { "plan": "Enterprise" },
            "summary": [{ "key": "credits_used", "label": "Credits used", "value": 40 }],
            "groups": [{
                "displayName": "Resource Packages",
                "buckets": [{
                    "window": "monthly",
                    "remainingFraction": 0.8,
                    "resetTime": "2026-10-15T00:00:00Z",
                }],
            }],
        })
    );
}

// TestFetchCredentialQuota_DeclarativeProbeInvalidReturnsError
#[tokio::test]
async fn invalid_probe_response_fails() {
    let upstream = Upstream::answering(http_response(
        "200 OK",
        &[("Content-Type", "text/plain")],
        b"random unmapped text",
    ))
    .await;
    fetch_url(&format!("{}/invalid", upstream.url))
        .await
        .assert(
            StatusCode::BAD_GATEWAY,
            &probe_failed("upstream probe response is not valid JSON"),
        );
}

// TestFetchCredentialQuota_DeclarativeProbeMissingPathReturnsError
#[tokio::test]
async fn missing_mapping_paths_fail() {
    let upstream = json_upstream("{}").await;
    fetch(json!({
        "quota_probe": {
            "url": format!("{}/empty", upstream.url),
            "method": "GET",
            "mapping": {
                "plan": "missing.plan.path",
                "groups": [{
                    "display_name": "Monthly",
                    "buckets": [{ "remaining_fraction": "missing.fraction.path" }],
                }],
            },
        },
    }))
    .await
    .assert(
        StatusCode::BAD_GATEWAY,
        &probe_failed(
            "probe response mapping failed: response mapping did not match any valid quota \
             fields in upstream response",
        ),
    );
}

// TestFetchCredentialQuota_DeclarativeProbeNonJSONWithMappingReturnsError
#[tokio::test]
async fn non_json_with_mapping_fails() {
    let upstream = Upstream::answering(http_response(
        "200 OK",
        &[("Content-Type", "text/html")],
        b"<html>502 Gateway Timeout</html>",
    ))
    .await;
    fetch(json!({
        "quota_probe": {
            "url": format!("{}/html", upstream.url),
            "method": "GET",
            "mapping": { "plan": "user.plan" },
        },
    }))
    .await
    .assert(
        StatusCode::BAD_GATEWAY,
        &probe_failed("upstream probe response is not valid JSON"),
    );
}

// TestFetchCredentialQuota_MalformedNormalizedGroupsReturnsError
#[tokio::test]
async fn malformed_normalized_groups_fail() {
    let upstream = json_upstream(r#"{"groups": [{}]}"#).await;
    fetch_url(&upstream.url)
        .await
        .assert(StatusCode::BAD_GATEWAY, &probe_failed(NO_MATCH));
}

// TestFetchCredentialQuota_NonNumericFractionReturnsError
#[tokio::test]
async fn non_numeric_fraction_fails() {
    let upstream =
        json_upstream(r#"{"quota": {"remaining": "unknown", "total": "unlimited"}}"#).await;
    fetch(json!({
        "quota_probe": {
            "url": format!("{}/usage", upstream.url),
            "method": "GET",
            "mapping": {
                "groups": [{
                    "display_name": "API Limits",
                    "buckets": [{ "remaining_fraction": "quota.remaining" }],
                }],
            },
        },
    }))
    .await
    .assert(
        StatusCode::BAD_GATEWAY,
        &probe_failed(
            "probe response mapping failed: response mapping did not match any valid quota \
             fields in upstream response",
        ),
    );
}

// TestFetchCredentialQuota_EmptyBucketsNormalizedReturnsError
#[tokio::test]
async fn empty_normalized_buckets_fail() {
    let upstream = json_upstream(r#"{"groups":[{"buckets":[{}]}]}"#).await;
    fetch_url(&upstream.url)
        .await
        .assert(StatusCode::BAD_GATEWAY, &probe_failed(NO_MATCH));
}

// TestFetchCredentialQuota_LegitimateZeroQuotaAccepted
#[tokio::test]
async fn legitimate_zero_quota_is_accepted() {
    let upstream = json_upstream(
        r#"{"groups":[{"displayName":"Daily","buckets":[{"window":"daily","remainingFraction":0}]}]}"#,
    )
    .await;
    fetch_url(&upstream.url).await.assert(
        StatusCode::OK,
        r#"{"groups":[{"displayName":"Daily","buckets":[{"window":"daily","remainingFraction":0}]}]}"#,
    );
}

// TestFetchCredentialQuota_WindowOnlyBucketReturnsError
#[tokio::test]
async fn window_only_bucket_fails() {
    let upstream =
        json_upstream(r#"{"groups":[{"displayName":"Weekly","buckets":[{"window":"weekly"}]}]}"#)
            .await;
    fetch_url(&upstream.url)
        .await
        .assert(StatusCode::BAD_GATEWAY, &probe_failed(NO_MATCH));
}

// TestFetchCredentialQuota_MixedValidAndInvalidBuckets
#[tokio::test]
async fn mixed_valid_and_invalid_buckets() {
    let upstream = json_upstream(
        r#"{"groups":[{"displayName":"Limits","buckets":[
            {"window":"daily","remainingFraction":0.8,"description":"valid"},
            {"window":"weekly","description":"missing fraction"}
        ]}]}"#,
    )
    .await;
    fetch_url(&upstream.url).await.assert(
        StatusCode::OK,
        r#"{"groups":[{"displayName":"Limits","buckets":[{"window":"daily","remainingFraction":0.8,"description":"valid"}]}]}"#,
    );
}

// TestFetchCredentialQuota_MissingTokenDoesNotHitUpstream
#[tokio::test]
async fn missing_token_does_not_hit_upstream() {
    let upstream = json_upstream(r#"{"subscription":{"plan":"Fake"}}"#).await;
    fetch(json!({
        "quota_probe": {
            "url": format!("{}/usage", upstream.url),
            "method": "GET",
            "header": { "Authorization": "Bearer $TOKEN$" },
        },
    }))
    .await
    .assert(
        StatusCode::BAD_GATEWAY,
        &probe_failed("probe authentication token not found for credential"),
    );
    assert!(upstream.requests().is_empty());
}

// Not upstream's: the probe never sends a header that names the client, nor
// its own Host; its user agent is this port's. Its method, data and other
// headers are sent, with the token in place.
#[tokio::test]
async fn the_probe_never_names_the_client() {
    let upstream = json_upstream(r#"{"subscription":{"plan":"P"}}"#).await;
    let answer = fetch(json!({
        "api_key": "secret-key",
        "quota_probe": {
            "url": format!("{}/usage?key=$TOKEN$", upstream.url),
            "method": " post ",
            "data": r#"{"key":"$TOKEN$"}"#,
            "header": {
                "User-Agent": "claude-cli/1.0.0 (external, cli)",
                "X-App": "cli",
                "originator": "codex_cli_rs",
                "Session_id": "abc",
                "X-Stainless-Os": "Linux",
                "Host": "elsewhere.example",
                "Authorization": "Bearer $TOKEN$",
                "x-custom": "kept",
                "X-Number": 1,
            },
        },
    }))
    .await;
    answer.assert(StatusCode::OK, r#"{"subscription":{"plan":"P"}}"#);
    let requests = upstream.requests();
    let [request] = requests.as_slice() else {
        panic!("{requests:?}");
    };
    assert!(
        request.starts_with("POST /usage?key=secret-key HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.ends_with("\r\n\r\n{\"key\":\"secret-key\"}"),
        "{request}"
    );
    assert_eq!(
        header_values(request, "User-Agent"),
        [open_ferry_providers::codex::USER_AGENT]
    );
    let host = upstream.url.trim_start_matches("http://");
    assert_eq!(header_values(request, "Host"), [host]);
    assert_eq!(
        header_values(request, "Authorization"),
        ["Bearer secret-key"]
    );
    assert_eq!(header_values(request, "X-Custom"), ["kept"]);
    for name in [
        "X-App",
        "Originator",
        "Session_id",
        "X-Stainless-Os",
        "X-Number",
    ] {
        assert_eq!(header_values(request, name), Vec::<&str>::new(), "{name}");
    }
}

// Not upstream's: without a probe it can make, the credential has no quota
// provider.
#[tokio::test]
async fn no_probe_gives_501() {
    let unprobed = r#"{"error":"no quota provider available for credential"}"#;
    for metadata in [
        json!({}),
        json!({ "quota_probe": "https://example.invalid" }),
        json!({ "quota_probe": { "method": "GET" } }),
        json!({ "quota_probe": { "url": "  " } }),
        json!({ "quota_probe": { "url": "http://127.0.0.1:9/", "method": "BAD METHOD" } }),
        json!({ "quota_probe": { "url": "http://[::1" } }),
    ] {
        let answer = fetch(metadata.clone()).await;
        assert_eq!(
            (answer.status, answer.body.as_str()),
            (StatusCode::NOT_IMPLEMENTED, unprobed),
            "{metadata}"
        );
    }
}

// Not upstream's: the body and the index are checked as upstream checks
// them, the index trimmed and taken from the first name that gives one.
#[tokio::test]
async fn bad_requests_answer_as_upstream() {
    let api = Api::new();
    let index = api.register(probe_auth("probe-auth", json!({})));
    let invalid = r#"{"error":"invalid request body"}"#;
    let required = r#"{"error":"auth_index is required"}"#;
    let not_found = r#"{"error":"auth not found"}"#;
    let unprobed = r#"{"error":"no quota provider available for credential"}"#;
    let cases = [
        ("", StatusCode::BAD_REQUEST, invalid),
        ("[]", StatusCode::BAD_REQUEST, invalid),
        (r#"{"auth_index":1}"#, StatusCode::BAD_REQUEST, invalid),
        (r#"{"provider":1}"#, StatusCode::BAD_REQUEST, invalid),
        (r#"{"plugin_id":null}"#, StatusCode::BAD_REQUEST, required),
        ("{}", StatusCode::BAD_REQUEST, required),
        (
            r#"{"auth_index":" ","authIndex":null}"#,
            StatusCode::BAD_REQUEST,
            required,
        ),
        (
            r#"{"auth_index":"missing"}"#,
            StatusCode::NOT_FOUND,
            not_found,
        ),
    ];
    for (body, status, answer) in cases {
        let got = api.post(PATH, body).await;
        assert_eq!((got.status, got.body.as_str()), (status, answer), "{body}");
    }
    for body in [
        json!({ "auth_index": format!(" {index} ") }),
        json!({ "auth_index": "", "authIndex": index, "plugin_id": "p" }),
        json!({ "AuthIndex": index, "provider": "x" }),
    ] {
        api.post(PATH, &body.to_string())
            .await
            .assert(StatusCode::NOT_IMPLEMENTED, unprobed);
    }
}

// Not upstream's: a reason never shows the token, whether the upstream
// echoes it, the URL's scheme holds it, or the URL a failed request names
// does.
#[tokio::test]
async fn reasons_never_show_the_token() {
    let token = "Sec-Ret-Token";
    let echo = Upstream::start(|_, request| {
        let line = request.lines().next().unwrap_or_default().to_owned();
        http_response("401 Unauthorized", &[], format!("bad: {line}").as_bytes())
    })
    .await;
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let probes = [
        (
            format!("{}/q?key=$TOKEN$", echo.url),
            probe_failed("probe returned status 401: bad: GET /q?key=$TOKEN$ HTTP/1.1"),
        ),
        (
            "$TOKEN$://host/path".to_owned(),
            probe_failed("probe request failed: unsupported protocol scheme \"$TOKEN$\""),
        ),
        (format!("http://{closed}/q?key=$TOKEN$"), String::new()),
    ];
    for (url, reason) in probes {
        let answer = fetch(json!({
            "token": token,
            "quota_probe": { "url": url },
        }))
        .await;
        assert_eq!(answer.status, StatusCode::BAD_GATEWAY, "{url}");
        assert!(!answer.body.contains(token), "{}", answer.body);
        assert!(
            !answer.body.contains(&token.to_ascii_lowercase()),
            "{}",
            answer.body
        );
        if reason.is_empty() {
            assert!(
                answer
                    .body
                    .starts_with(r#"{"error":"quota probe failed: probe request failed: "#),
                "{}",
                answer.body
            );
        } else {
            assert_eq!(answer.body, reason);
        }
    }
    assert_eq!(echo.requests().len(), 1);
}

/// A token with a quote and a backslash, which JSON writes with escapes and
/// a URL with an escape for the quote.
const AWKWARD_TOKEN: &str = "synthetic-secret\"back\\slash";

/// Fails if any run of letters in `AWKWARD_TOKEN` is in `text`.
fn assert_no_token(text: &str) {
    for part in ["synthetic-secret", "back", "slash"] {
        assert!(!text.contains(part), "{part} in {text}");
    }
}

// Not upstream's: a reason doesn't show the token as the upstream writes it
// in JSON, with escapes, or the URL a request was made to writes it.
#[tokio::test]
async fn reasons_never_show_the_token_as_escaped() {
    let echo = Upstream::start(|_, request| {
        let line = request.lines().next().unwrap_or_default();
        let bearer = header_values(request, "Authorization").join(",");
        let body = json!({ "error": { "message": format!("bad {bearer} at {line}") } });
        http_response(
            "401 Unauthorized",
            &[("Content-Type", "application/json")],
            body.to_string().as_bytes(),
        )
    })
    .await;

    let answer = fetch(json!({
        "token": AWKWARD_TOKEN,
        "quota_probe": {
            "url": echo.url,
            "header": { "Authorization": "Bearer $TOKEN$" },
        },
    }))
    .await;
    answer.assert(
        StatusCode::BAD_GATEWAY,
        &probe_failed(
            r#"probe returned status 401: {"error":{"message":"bad Bearer $TOKEN$ at GET / HTTP/1.1"}}"#,
        ),
    );

    // In the URL: made as the client writes it, and shown as the upstream
    // does.
    let answer = fetch(json!({
        "token": AWKWARD_TOKEN,
        "quota_probe": { "url": format!("{}/q?key=$TOKEN$", echo.url) },
    }))
    .await;
    assert_eq!(answer.status, StatusCode::BAD_GATEWAY, "{}", answer.body);
    assert_no_token(&answer.body);
    assert!(answer.body.contains("$TOKEN$"), "{}", answer.body);
}

// Not upstream's: nor does an answer show the token, where the response
// holds it, whether the body is read as it is or by a mapping.
#[tokio::test]
async fn answers_never_show_the_token() {
    let upstream = json_upstream(
        &json!({
            "subscription": {
                "plan": AWKWARD_TOKEN,
                "tierName": format!("tier {AWKWARD_TOKEN}!"),
                "tierId": AWKWARD_TOKEN.to_ascii_uppercase(),
            },
            "summary": [{
                "key": AWKWARD_TOKEN,
                "label": format!("<{AWKWARD_TOKEN}>"),
                "value": 1,
                "unit": AWKWARD_TOKEN,
            }],
            "groups": [{
                "displayName": AWKWARD_TOKEN,
                "buckets": [{
                    "window": AWKWARD_TOKEN,
                    "remainingFraction": 0.5,
                    "resetTime": AWKWARD_TOKEN,
                    "description": format!("a {AWKWARD_TOKEN} b"),
                }],
            }],
        })
        .to_string(),
    )
    .await;
    let answer = fetch(json!({
        "token": AWKWARD_TOKEN,
        "quota_probe": {
            "url": upstream.url,
            "header": { "Authorization": "Bearer $TOKEN$" },
        },
    }))
    .await;
    let mut body = answer.expect(StatusCode::OK);
    body.as_object_mut().unwrap().remove("serverTimeOffsetMs");
    assert_eq!(
        body,
        json!({
            "subscription": {
                "plan": "$TOKEN$",
                "tierName": "tier $TOKEN$!",
                "tierId": "$TOKEN$",
            },
            "summary": [{
                "key": "$TOKEN$",
                "label": "<$TOKEN$>",
                "value": 1,
                "unit": "$TOKEN$",
            }],
            "groups": [{
                "displayName": "$TOKEN$",
                "buckets": [{
                    "window": "$TOKEN$",
                    "remainingFraction": 0.5,
                    "resetTime": "$TOKEN$",
                    "description": "a $TOKEN$ b",
                }],
            }],
        })
    );

    let upstream = json_upstream(
        &json!({
            "user": { "tier": AWKWARD_TOKEN, "id": "t-1" },
            "packages": [{ "period": AWKWARD_TOKEN, "left": 1, "all": 4 }],
        })
        .to_string(),
    )
    .await;
    let answer = fetch(json!({
        "token": AWKWARD_TOKEN,
        "quota_probe": {
            "url": upstream.url,
            "header": { "Authorization": "Bearer $TOKEN$" },
            "mapping": {
                "plan": "user.tier",
                "tier_id": "user.id",
                "groups": [{
                    "display_name": "Packages",
                    "buckets_path": "packages",
                    "window_key": "period",
                    "remaining_amount_key": "left",
                    "total_amount_key": "all",
                }],
            },
        },
    }))
    .await;
    let mut body = answer.expect(StatusCode::OK);
    body.as_object_mut().unwrap().remove("serverTimeOffsetMs");
    assert_eq!(
        body,
        json!({
            "subscription": { "plan": "$TOKEN$", "tierId": "t-1" },
            "groups": [{
                "displayName": "Packages",
                "buckets": [{ "window": "$TOKEN$", "remainingFraction": 0.25 }],
            }],
        })
    );
}

// Not upstream's: a mapping path as deep as the response is read, as Go
// reads it, where a recursive read would overflow the stack.
#[tokio::test]
async fn a_deep_mapping_path_is_read() {
    const DEPTH: usize = 3_000;
    let body = format!("{}\"Pro\"{}", "{\"a\":".repeat(DEPTH), "}".repeat(DEPTH));
    let upstream = json_upstream(&body).await;
    let mapping = json!({ "plan": vec!["a"; DEPTH].join(".") });
    fetch(json!({ "quota_probe": { "url": upstream.url, "mapping": mapping } }))
        .await
        .assert(StatusCode::OK, r#"{"subscription":{"plan":"Pro"}}"#);
}

// Not upstream's: the headers are checked as Go's transport checks them,
// before anything is sent.
#[tokio::test]
async fn invalid_probe_headers_fail_before_sending() {
    let upstream = json_upstream(r#"{"subscription":{"plan":"P"}}"#).await;
    let cases = [
        (
            json!({ "Bad Name": "x" }),
            "probe request failed: net/http: invalid header field name \"Bad Name\"",
        ),
        (
            json!({ "X-Ok": "a\u{1}b" }),
            "probe request failed: net/http: invalid header field value for \"X-Ok\"",
        ),
    ];
    for (headers, reason) in cases {
        fetch(json!({ "quota_probe": { "url": upstream.url, "headers": headers } }))
            .await
            .assert(StatusCode::BAD_GATEWAY, &probe_failed(reason));
    }
    assert!(upstream.requests().is_empty());
}

// Not upstream's: an answer Go's encoder can't write (an infinite fraction,
// from amounts) leaves gin's 200 without a body.
#[tokio::test]
async fn an_unwritable_answer_is_an_empty_200() {
    let upstream = json_upstream(r#"{"left":1e308,"total":1e-10}"#).await;
    let mapping = json!({
        "groups": [{
            "display_name": "G",
            "buckets": [{ "remaining_amount": "left", "total_amount": "total" }],
        }],
    });
    let answer = fetch(json!({ "quota_probe": { "url": upstream.url, "mapping": mapping } })).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, "");
    assert_eq!(
        answer.header("content-type"),
        Some("application/json; charset=utf-8")
    );
}
