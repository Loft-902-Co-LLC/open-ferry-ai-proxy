// Ported from CLIProxyAPI
// internal/api/handlers/management/api_key_usage_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Counts of the API key credentials' calls.
//!
//! Deviations from upstream: the tests drive the router with the key.

use http::StatusCode;
use open_ferry_core::manager::CallResult;
use serde_json::Value;

use super::{Api, auth, object};

/// The successes and failures in `buckets`, which must be 20.
fn sum_recent_request_buckets(buckets: &Value) -> (i64, i64) {
    let buckets = buckets.as_array().expect("recent_requests");
    assert_eq!(buckets.len(), 20);
    buckets.iter().fold((0, 0), |(success, failed), bucket| {
        (
            success + bucket["success"].as_i64().unwrap(),
            failed + bucket["failed"].as_i64().unwrap(),
        )
    })
}

/// A credential with `id` of `provider`, with `attributes`.
fn keyed_auth(
    id: &str,
    provider: &str,
    attributes: &[(&str, &str)],
) -> open_ferry_core::auth::Auth {
    let mut auth = auth(id, attributes);
    auth.provider = provider.into();
    auth
}

/// Records a call of `auth_id`.
fn mark(api: &Api, auth_id: &str, provider: &str, success: bool) {
    api.manager.mark_result(&CallResult {
        auth_id: auth_id.into(),
        provider: provider.into(),
        model: "gpt-5".into(),
        success,
        ..CallResult::default()
    });
}

/// The successes and failures of `entry`, and of its buckets.
fn totals(entry: &Value) -> ((i64, i64), (i64, i64)) {
    (
        (
            entry["success"].as_i64().unwrap(),
            entry["failed"].as_i64().unwrap(),
        ),
        sum_recent_request_buckets(&entry["recent_requests"]),
    )
}

/// Ports TestGetAPIKeyUsage_GroupsByProviderAndAPIKey.
#[tokio::test]
async fn get_api_key_usage_groups_by_provider_and_api_key() {
    let api = Api::new();
    api.register(keyed_auth(
        "codex-auth",
        "codex",
        &[
            ("api_key", "codex-key"),
            ("base_url", "https://codex.example.com"),
        ],
    ));
    api.register(keyed_auth(
        "claude-auth",
        "claude",
        &[
            ("api_key", "claude-key"),
            ("base_url", "https://claude.example.com"),
        ],
    ));
    mark(&api, "codex-auth", "codex", true);
    mark(&api, "codex-auth", "codex", false);
    mark(&api, "claude-auth", "claude", true);

    let payload = api
        .get("/v0/management/api-key-usage")
        .await
        .expect(StatusCode::OK);

    let codex = &payload["codex"]["https://codex.example.com|codex-key"];
    assert_eq!(totals(codex), ((1, 1), (1, 1)), "{payload}");
    let claude = &payload["claude"]["https://claude.example.com|claude-key"];
    assert_eq!(totals(claude), ((1, 0), (1, 0)), "{payload}");
}

/// Ports TestGetAPIKeyUsage_GroupsOpenAICompatibleByCompatName.
#[tokio::test]
async fn get_api_key_usage_groups_openai_compatible_by_compat_name() {
    let api = Api::new();
    api.register(keyed_auth(
        "vast-auth",
        "openai-compatible-vast",
        &[
            ("api_key", "vast-key"),
            ("base_url", "https://www.vastnum.com/v1"),
            ("compat_name", "VAST"),
        ],
    ));
    mark(&api, "vast-auth", "openai-compatible-vast", true);

    let payload = api
        .get("/v0/management/api-key-usage")
        .await
        .expect(StatusCode::OK);

    assert!(object(&payload).get("openai-compatible-vast").is_none());
    let vast = &payload["vast"]["https://www.vastnum.com/v1|vast-key"];
    assert_eq!(totals(vast).0, (1, 0), "{payload}");
}

/// Not upstream's: credentials sharing a provider, base URL and key are
/// summed, `base-url` stands in for `base_url`, a credential without a
/// key isn't listed, the entry's fields come in upstream's order, and the
/// v8 path answers the same.
#[tokio::test]
async fn api_key_usage_merges_shared_keys_and_skips_oauth() {
    let api = Api::new();
    api.register(keyed_auth(
        "first",
        "Claude",
        &[("api_key", " shared "), ("base_url", "https://x.example")],
    ));
    api.register(keyed_auth(
        "second",
        "claude",
        &[("api_key", "shared"), ("base-url", " https://x.example ")],
    ));
    api.register(keyed_auth("oauth", "claude", &[("runtime_only", "true")]));
    mark(&api, "first", "claude", true);
    mark(&api, "second", "claude", false);
    mark(&api, "oauth", "claude", true);

    let answer = api.get("/v0/management/api-key-usage").await;
    let payload = answer.expect(StatusCode::OK);
    assert_eq!(object(&payload).len(), 1, "{payload}");
    let claude = object(&payload["claude"]);
    assert_eq!(claude.len(), 1, "{payload}");
    let entry = &claude["https://x.example|shared"];
    assert_eq!(totals(entry), ((1, 1), (1, 1)), "{payload}");
    let keys: Vec<_> = object(entry).keys().map(String::as_str).collect();
    assert_eq!(keys, ["success", "failed", "recent_requests"]);

    let v8 = api
        .get("/v8/management/observability/usage/api-keys")
        .await
        .expect(StatusCode::OK);
    assert_eq!(v8, payload);
}
