//! The routes of `crate::config_lists`: the client API keys and the OAuth
//! channels' lists.
//!
//! Upstream has no tests of these routes but those of the file they save
//! (in config_v8_test.go), which are ported in `config_v8_write`; these
//! are open-ferry's, and check the config the
//! [`FakeWriter`](super::FakeWriter) is asked to save.

use std::collections::BTreeMap;

use http::{Method, StatusCode};
use open_ferry_core::config::{Config, OAuthModelAlias, RequestScopedErrorRule};

use super::{Api, keyed_config};

const OK: &str = r#"{"status":"ok"}"#;
const API_KEYS: &str = "/v0/management/api-keys";
const EXCLUDED: &str = "/v0/management/oauth-excluded-models";
const ALIAS: &str = "/v0/management/oauth-model-alias";
const SCOPED: &str = "/v0/management/oauth-request-scoped-errors";

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|&item| item.to_owned()).collect()
}

/// The API, its config holding `keys` as the client API keys.
fn with_keys(keys: &[&str]) -> Api {
    let mut config = keyed_config();
    config.api_keys = strings(keys);
    Api::writing(config)
}

/// Checks that each of `cases`, a method, path, body and the answer's
/// status and body, answers so.
async fn assert_answers(api: &Api, cases: &[(Method, &str, &str, StatusCode, &str)]) {
    for (method, path, body, status, answer) in cases {
        let got = api.call(method.clone(), path, body).await;
        assert_eq!(
            (got.status, got.body.as_str()),
            (*status, *answer),
            "{method} {path} {body}"
        );
    }
}

/// `method path` with `body`, which must answer `{"status":"ok"}`; then the
/// config saved.
async fn change(api: &Api, method: Method, path: &str, body: &str) -> Config {
    let answer = api.call(method.clone(), path, body).await;
    assert_eq!(
        (answer.status, answer.body.as_str()),
        (StatusCode::OK, OK),
        "{method} {path} {body}"
    );
    api.saved()
}

// Not upstream's: PUT replaces the keys, given as a list or as items.
#[tokio::test]
async fn api_keys_put() {
    let api = with_keys(&["old"]);
    let config = change(&api, Method::PUT, API_KEYS, r#"["a"," b "]"#).await;
    assert_eq!(config.api_keys, strings(&["a", " b "]));
    let config = change(&api, Method::PUT, API_KEYS, r#"{"items":["c"]}"#).await;
    assert_eq!(config.api_keys, strings(&["c"]));
    let config = change(&api, Method::PUT, API_KEYS, "[]").await;
    assert!(config.api_keys.is_empty());

    let saves = api.writer.saved().len();
    for body in [
        "",
        "{",
        r#"{"items":[]}"#,
        r#""a""#,
        r#"[1]"#,
        r#"{"other":["a"]}"#,
    ] {
        api.call(Method::PUT, API_KEYS, body)
            .await
            .assert(StatusCode::BAD_REQUEST, r#"{"error":"invalid body"}"#);
    }
    assert_eq!(api.writer.saved().len(), saves);
    assert_eq!(api.reload.count(), saves);
}

// Not upstream's: PATCH replaces a key by index, else by its old value, or
// adds the new one.
#[tokio::test]
async fn api_keys_patch() {
    let api = with_keys(&["a", "b"]);
    let config = change(&api, Method::PATCH, API_KEYS, r#"{"index":1,"value":"c"}"#).await;
    assert_eq!(config.api_keys, strings(&["a", "c"]));
    let config = change(&api, Method::PATCH, API_KEYS, r#"{"old":"a","new":"d"}"#).await;
    assert_eq!(config.api_keys, strings(&["d", "c"]));
    let config = change(&api, Method::PATCH, API_KEYS, r#"{"old":"zz","new":"e"}"#).await;
    assert_eq!(config.api_keys, strings(&["d", "c", "e"]));
    // An index out of range falls back to old and new.
    let body = r#"{"index":9,"value":"x","old":"c","new":"f"}"#;
    let config = change(&api, Method::PATCH, API_KEYS, body).await;
    assert_eq!(config.api_keys, strings(&["d", "f", "e"]));

    let missing = r#"{"error":"missing fields"}"#;
    let invalid = r#"{"error":"invalid body"}"#;
    assert_answers(
        &api,
        &[
            (
                Method::PATCH,
                API_KEYS,
                "{}",
                StatusCode::BAD_REQUEST,
                missing,
            ),
            (
                Method::PATCH,
                API_KEYS,
                r#"{"index":9,"value":"x"}"#,
                StatusCode::BAD_REQUEST,
                missing,
            ),
            (
                Method::PATCH,
                API_KEYS,
                r#"{"old":"d"}"#,
                StatusCode::BAD_REQUEST,
                missing,
            ),
            (
                Method::PATCH,
                API_KEYS,
                "[",
                StatusCode::BAD_REQUEST,
                invalid,
            ),
            (
                Method::PATCH,
                API_KEYS,
                r#"{"index":"0","value":"x"}"#,
                StatusCode::BAD_REQUEST,
                invalid,
            ),
        ],
    )
    .await;
    assert_eq!(api.state.config().api_keys, strings(&["d", "f", "e"]));
    assert_eq!(api.writer.saved().len(), 4);
}

// Not upstream's: DELETE removes a key by index, else every key that is the
// value once trimmed.
#[tokio::test]
async fn api_keys_delete() {
    let api = with_keys(&["a", " b ", "b", "c", "d"]);
    let config = change(&api, Method::DELETE, &format!("{API_KEYS}?index=0"), "").await;
    assert_eq!(config.api_keys, strings(&[" b ", "b", "c", "d"]));
    let config = change(&api, Method::DELETE, &format!("{API_KEYS}?value=%20b"), "").await;
    assert_eq!(config.api_keys, strings(&["c", "d"]));
    let path = format!("{API_KEYS}?index=9&value=c");
    let config = change(&api, Method::DELETE, &path, "").await;
    assert_eq!(config.api_keys, strings(&["d"]));
    let config = change(&api, Method::DELETE, &format!("{API_KEYS}?value=none"), "").await;
    assert_eq!(config.api_keys, strings(&["d"]));

    let missing = r#"{"error":"missing index or value"}"#;
    assert_answers(
        &api,
        &[
            (
                Method::DELETE,
                API_KEYS,
                "",
                StatusCode::BAD_REQUEST,
                missing,
            ),
            (
                Method::DELETE,
                &format!("{API_KEYS}?index=9"),
                "",
                StatusCode::BAD_REQUEST,
                missing,
            ),
            (
                Method::DELETE,
                &format!("{API_KEYS}?index=x&value=%20"),
                "",
                StatusCode::BAD_REQUEST,
                missing,
            ),
        ],
    )
    .await;
    assert_eq!(api.state.config().api_keys, strings(&["d"]));
}

// Not upstream's: the excluded models of the OAuth channels.
#[tokio::test]
async fn excluded_models() {
    let api = Api::writing(keyed_config());
    let body = r#"{" Codex ":[" A ","a"],"empty":[],"Claude":["B"]}"#;
    let config = change(&api, Method::PUT, EXCLUDED, body).await;
    assert_eq!(
        config.oauth_excluded_models,
        BTreeMap::from([
            ("claude".into(), strings(&["b"])),
            ("codex".into(), strings(&["a"])),
        ])
    );
    let body = r#"{"items":{"codex":["x"]}}"#;
    let config = change(&api, Method::PUT, EXCLUDED, body).await;
    assert_eq!(
        config.oauth_excluded_models,
        BTreeMap::from([("codex".into(), strings(&["x"]))])
    );

    let body = r#"{"provider":" Claude ","models":["X"," y "]}"#;
    let config = change(&api, Method::PATCH, EXCLUDED, body).await;
    assert_eq!(config.oauth_excluded_models["claude"], strings(&["x", "y"]));
    let body = r#"{"provider":"claude","models":[" "]}"#;
    let config = change(&api, Method::PATCH, EXCLUDED, body).await;
    assert!(!config.oauth_excluded_models.contains_key("claude"));
    let path = format!("{EXCLUDED}?provider=%20CODEX");
    let config = change(&api, Method::DELETE, &path, "").await;
    assert!(config.oauth_excluded_models.is_empty());

    let not_found = r#"{"error":"provider not found"}"#;
    let invalid = r#"{"error":"invalid body"}"#;
    assert_answers(
        &api,
        &[
            (
                Method::PATCH,
                EXCLUDED,
                r#"{"provider":"claude","models":[]}"#,
                StatusCode::NOT_FOUND,
                not_found,
            ),
            (
                Method::PATCH,
                EXCLUDED,
                r#"{"provider":" ","models":["x"]}"#,
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid provider"}"#,
            ),
            (
                Method::PATCH,
                EXCLUDED,
                r#"{"models":["x"]}"#,
                StatusCode::BAD_REQUEST,
                invalid,
            ),
            (
                Method::PATCH,
                EXCLUDED,
                r#"{"provider":"codex","models":"x"}"#,
                StatusCode::BAD_REQUEST,
                invalid,
            ),
            (
                Method::PUT,
                EXCLUDED,
                "[]",
                StatusCode::BAD_REQUEST,
                invalid,
            ),
            (
                Method::DELETE,
                &format!("{EXCLUDED}?provider=codex"),
                "",
                StatusCode::NOT_FOUND,
                not_found,
            ),
            (
                Method::DELETE,
                EXCLUDED,
                "",
                StatusCode::BAD_REQUEST,
                r#"{"error":"missing provider"}"#,
            ),
        ],
    )
    .await;
    assert_eq!(api.writer.saved().len(), 5);
}

/// An alias of `name` as `alias`.
fn alias(name: &str, alias: &str) -> OAuthModelAlias {
    OAuthModelAlias {
        name: name.into(),
        alias: alias.into(),
        ..OAuthModelAlias::default()
    }
}

// Not upstream's: the model aliases of the OAuth channels.
#[tokio::test]
async fn model_alias() {
    let api = Api::writing(keyed_config());
    let body = r#"{"items":{" Claude ":[{"name":" m ","alias":" n "},{"name":"x"}]}}"#;
    let config = change(&api, Method::PUT, ALIAS, body).await;
    assert_eq!(
        config.oauth_model_alias,
        BTreeMap::from([("claude".into(), vec![alias("m", "n")])])
    );

    let body = r#"{"channel":"Codex","aliases":[{"name":"a","alias":"b","fork":true}]}"#;
    let config = change(&api, Method::PATCH, ALIAS, body).await;
    let mut forked = alias("a", "b");
    forked.fork = true;
    assert_eq!(config.oauth_model_alias["codex"], vec![forked]);
    // `provider` names the channel when `channel` is missing.
    let body = r#"{"provider":"gemini","aliases":[{"name":"c","alias":"d"}]}"#;
    let config = change(&api, Method::PATCH, ALIAS, body).await;
    assert_eq!(config.oauth_model_alias["gemini"], vec![alias("c", "d")]);
    let body = r#"{"channel":"codex","aliases":[]}"#;
    let config = change(&api, Method::PATCH, ALIAS, body).await;
    assert!(!config.oauth_model_alias.contains_key("codex"));
    let config = change(&api, Method::DELETE, &format!("{ALIAS}?channel=CLAUDE"), "").await;
    assert!(!config.oauth_model_alias.contains_key("claude"));
    let config = change(
        &api,
        Method::DELETE,
        &format!("{ALIAS}?provider=gemini"),
        "",
    )
    .await;
    assert!(config.oauth_model_alias.is_empty());

    let not_found = r#"{"error":"channel not found"}"#;
    assert_answers(
        &api,
        &[
            (
                Method::PATCH,
                ALIAS,
                r#"{"channel":"codex","aliases":[{"name":"","alias":"x"}]}"#,
                StatusCode::NOT_FOUND,
                not_found,
            ),
            (
                Method::PATCH,
                ALIAS,
                r#"{"aliases":[{"name":"a","alias":"b"}]}"#,
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid channel"}"#,
            ),
            (
                Method::PATCH,
                ALIAS,
                r#"{"channel":"codex","aliases":{}}"#,
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid body"}"#,
            ),
            (
                Method::DELETE,
                &format!("{ALIAS}?channel=codex"),
                "",
                StatusCode::NOT_FOUND,
                not_found,
            ),
            (
                Method::DELETE,
                &format!("{ALIAS}?channel=%20&provider="),
                "",
                StatusCode::BAD_REQUEST,
                r#"{"error":"missing channel"}"#,
            ),
        ],
    )
    .await;
    assert_eq!(api.writer.saved().len(), 6);
}

/// A rule stopping on `status` when the body holds `matched`.
fn rule(status: i64, matched: &str) -> RequestScopedErrorRule {
    RequestScopedErrorRule {
        status,
        matches: strings(&[matched]),
        action: "stop".into(),
        ..RequestScopedErrorRule::default()
    }
}

// Not upstream's: the request-scoped error rules of the OAuth channels.
#[tokio::test]
async fn request_scoped_errors() {
    let api = Api::writing(keyed_config());
    let body = r#"{"Codex":[{"status":429,"match":[" quota "],"action":" Stop "},{"status":0,"match":["x"],"action":"stop"}]}"#;
    let config = change(&api, Method::PUT, SCOPED, body).await;
    assert_eq!(
        config.oauth_request_scoped_errors,
        BTreeMap::from([("codex".into(), vec![rule(429, "quota")])])
    );

    let body = r#"{"channel":"claude","rules":[{"status":500,"match":["boom"],"action":"stop"}]}"#;
    let config = change(&api, Method::PATCH, SCOPED, body).await;
    assert_eq!(
        config.oauth_request_scoped_errors["claude"],
        vec![rule(500, "boom")]
    );
    let body = r#"{"provider":"codex","rules":[{"status":429,"match":[],"action":"stop"}]}"#;
    let config = change(&api, Method::PATCH, SCOPED, body).await;
    assert!(!config.oauth_request_scoped_errors.contains_key("codex"));
    let config = change(
        &api,
        Method::DELETE,
        &format!("{SCOPED}?channel=claude"),
        "",
    )
    .await;
    assert!(config.oauth_request_scoped_errors.is_empty());
    // An object that isn't the map reads as one without items, as upstream
    // reads it, and clears the rules.
    change(&api, Method::PUT, SCOPED, r#"{"items":{"codex":[]}}"#).await;
    let body = r#"{"codex":[{"status":429,"match":["q"],"action":"stop"}]}"#;
    change(&api, Method::PUT, SCOPED, body).await;
    let config = change(&api, Method::PUT, SCOPED, r#"{"codex":{}}"#).await;
    assert!(config.oauth_request_scoped_errors.is_empty());

    assert_answers(
        &api,
        &[
            (
                Method::PATCH,
                SCOPED,
                r#"{"channel":"codex","rules":[]}"#,
                StatusCode::NOT_FOUND,
                r#"{"error":"channel not found"}"#,
            ),
            (
                Method::PATCH,
                SCOPED,
                r#"{"channel":" ","rules":[]}"#,
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid channel"}"#,
            ),
            (
                Method::PUT,
                SCOPED,
                r#""rules""#,
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid body"}"#,
            ),
            (
                Method::DELETE,
                SCOPED,
                "",
                StatusCode::BAD_REQUEST,
                r#"{"error":"missing channel"}"#,
            ),
        ],
    )
    .await;
    assert_eq!(api.writer.saved().len(), 7);
}
