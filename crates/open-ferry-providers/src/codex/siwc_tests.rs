// Ported from CLIProxyAPI internal/auth/codex/siwc_auth_test.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use std::collections::VecDeque;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use http::{HeaderMap, StatusCode};
use serde_json::json;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use super::*;

const DEVICE: &str = "E61BBE28-07EF-466D-8E5D-A344F94AB305";
const ACCESS: &str = "fake-siwc-access";
const REFRESH: &str = "fake-siwc-refresh";
const ID: &str = "fake-siwc-id";

fn pkce() -> Pkce {
    Pkce {
        verifier: "test-verifier".into(),
        challenge: "test-challenge".into(),
    }
}

fn previous() -> SiwcTokenData {
    SiwcTokenData {
        access_token: ACCESS.into(),
        refresh_token: REFRESH.into(),
        id_token: ID.into(),
        client_id: "oaiapp_issued".into(),
        device_id: DEVICE.to_ascii_lowercase(),
        ..SiwcTokenData::default()
    }
}

fn grant() -> Value {
    json!({"access_token":ACCESS,"refresh_token":REFRESH,"id_token":ID,"scope":SCOPE,"expires_in":3600})
}

struct Captured {
    headers: HeaderMap,
    body: String,
}

type Replies = Arc<Mutex<VecDeque<(StatusCode, String)>>>;

#[derive(Clone)]
struct FakeState {
    replies: Replies,
    captured: mpsc::UnboundedSender<Captured>,
    release: Option<Arc<Semaphore>>,
}

async fn token_handler(
    State(state): State<FakeState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    state.captured.send(Captured { headers, body }).unwrap();
    if let Some(release) = &state.release {
        release.acquire().await.unwrap().forget();
    }
    let (status, body) = state.replies.lock().unwrap().pop_front().unwrap_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "unexpected request".into(),
    ));
    (
        status,
        [(http::header::CONTENT_TYPE, "application/json")],
        body,
    )
}

struct Fake {
    auth: SiwcAuth,
    captured: mpsc::UnboundedReceiver<Captured>,
    task: JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fake {
    async fn start(replies: Vec<(StatusCode, String)>, release: Option<Arc<Semaphore>>) -> Self {
        let (captured, receiver) = mpsc::unbounded_channel();
        let router = Router::new()
            .route("/api/accounts/oauth/token", post(token_handler))
            .with_state(FakeState {
                replies: Arc::new(Mutex::new(replies.into())),
                captured,
                release,
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        // An injected caller UA must still be replaced by the fixed SIWC UA.
        let client = reqwest::Client::builder()
            .no_proxy()
            .user_agent("caller-override")
            .redirect(crate::redirect::policy())
            .build()
            .unwrap();
        Self {
            auth: SiwcAuth::new(client).with_endpoints(Endpoints::with_base(&base)),
            captured: receiver,
            task,
        }
    }

    async fn request(&mut self) -> Captured {
        tokio::time::timeout(Duration::from_secs(5), self.captured.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

#[test]
fn authorization_url_has_upstreams_fields_and_our_fixed_identity() {
    let auth = SiwcAuth::with_proxy_url("direct");
    assert_eq!(auth.endpoints(), &Endpoints::default());
    let raw = auth
        .generate_auth_url(" test-state ", " test-nonce ", DEVICE, "", &pkce())
        .unwrap();
    let url = url::Url::parse(&raw).unwrap();
    assert_eq!(
        format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap(),
            url.path()
        ),
        AUTHORIZE_URL
    );
    let expected = encode_form(&[
        ("client_id", DYNAMIC_CLIENT_ID),
        ("agent_name_hint", "open-ferry"),
        (
            "ext_agent_host_id",
            &format!("urn:uuid:{}", DEVICE.to_ascii_lowercase()),
        ),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("resource", RESOURCE),
        ("scope", SCOPE),
        ("state", "test-state"),
        ("nonce", "test-nonce"),
        ("code_challenge", "test-challenge"),
        ("code_challenge_method", "S256"),
    ]);
    assert!(url.query() == Some(expected.as_str()));
    let redirect = redirect_uri(33445);
    let raw = auth
        .generate_auth_url("s", "n", DEVICE, &redirect, &pkce())
        .unwrap();
    assert!(
        url::Url::parse(&raw)
            .unwrap()
            .query_pairs()
            .any(|(key, value)| key == "redirect_uri" && value == redirect)
    );
    assert_eq!(redirect_uri(DEFAULT_CALLBACK_PORT), REDIRECT_URI);
    assert_eq!(
        user_agent(),
        format!(
            "open-ferry/{} ({}; {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    );
    let local = SiwcAuth::with_proxy_url("direct")
        .with_endpoints(Endpoints::with_base("http://127.0.0.1:33445/"));
    assert!(
        local
            .generate_auth_url("s", "n", DEVICE, "", &pkce())
            .unwrap()
            .starts_with("http://127.0.0.1:33445/api/accounts/authorize?")
    );
}

#[test]
fn authorization_rejects_invalid_devices_and_empty_state_or_nonce() {
    let auth = SiwcAuth::with_proxy_url("direct");
    for device in [
        "",
        "not-a-uuid",
        "e61bbe2807ef466d8e5da344f94ab305",
        "e61bbe28-07ef-466d-8e5d-a344f94ab30g",
        "é61bbe28-07ef-466d-8e5d-a344f94ab305",
    ] {
        let error = auth
            .generate_auth_url("s", "n", device, "", &pkce())
            .unwrap_err();
        assert_eq!(
            error.message(),
            "Sign in with ChatGPT requires a valid device UUID"
        );
    }
    assert!(is_valid_uuid(&format!(" {DEVICE} ")));
    assert!(
        auth.generate_auth_url(" ", "n", DEVICE, "", &pkce())
            .is_err()
    );
    assert!(
        auth.generate_auth_url("s", " ", DEVICE, "", &pkce())
            .is_err()
    );
}

fn assert_request(request: &Captured, expected: &[(&str, &str)]) {
    let mut actual: Vec<_> = url::form_urlencoded::parse(request.body.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let mut expected: Vec<_> = expected
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    actual.sort();
    expected.sort();
    // Compare the full list, including its length, so extra or duplicate
    // fields fail without printing token values.
    assert!(actual == expected);
    assert_eq!(
        request.headers[http::header::CONTENT_TYPE],
        "application/x-www-form-urlencoded"
    );
    assert_eq!(request.headers[http::header::ACCEPT], "application/json");
    assert_eq!(request.headers[http::header::USER_AGENT], user_agent());
    assert!(
        !request
            .body
            .to_ascii_lowercase()
            .contains(&DEVICE.to_ascii_lowercase())
    );
    for (name, value) in &request.headers {
        assert!(
            !value
                .as_bytes()
                .windows(DEVICE.len())
                .any(|part| part.eq_ignore_ascii_case(DEVICE.as_bytes()))
        );
        assert!(
            ![
                "originator",
                "x-client-id",
                "session_id",
                "x-client-request-id",
                "chatgpt-account-id"
            ]
            .contains(&name.as_str())
        );
    }
}

#[tokio::test]
async fn exchange_and_refresh_use_exact_forms_and_preserve_the_old_id_token() {
    let mut refreshed = grant();
    refreshed.as_object_mut().unwrap().remove("id_token");
    refreshed["access_token"] = json!("fake-rotated-access");
    refreshed["refresh_token"] = json!("fake-rotated-refresh");
    let mut fake = Fake::start(
        vec![
            (StatusCode::OK, grant().to_string()),
            (StatusCode::OK, refreshed.to_string()),
        ],
        None,
    )
    .await;
    let data = fake
        .auth
        .exchange_code_for_tokens(" code-xyz ", " oaiapp_issued ", "", DEVICE, &pkce())
        .await
        .unwrap();
    assert!(data.access_token == ACCESS && data.refresh_token == REFRESH && data.id_token == ID);
    assert!(data.client_id == "oaiapp_issued" && data.device_id == DEVICE);
    assert_request(
        &fake.request().await,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", "oaiapp_issued"),
            ("code", "code-xyz"),
            ("code_verifier", "test-verifier"),
            ("redirect_uri", REDIRECT_URI),
            ("resource", RESOURCE),
        ],
    );
    let tokens = fake.auth.refresh_tokens(&data).await.unwrap();
    assert!(
        tokens.access_token == "fake-rotated-access"
            && tokens.refresh_token == "fake-rotated-refresh"
    );
    assert!(
        tokens.id_token == ID && tokens.device_id == DEVICE && tokens.client_id == data.client_id
    );
    assert_request(
        &fake.request().await,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", "oaiapp_issued"),
            ("refresh_token", REFRESH),
            ("resource", RESOURCE),
        ],
    );
}

fn now() -> DateTime<Local> {
    DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")
        .unwrap()
        .with_timezone(&Local)
}

#[test]
fn token_validation_rejects_each_missing_or_invalid_field() {
    for (field, invalid, expected) in [
        ("access_token", json!(" "), "invalid access_token"),
        ("refresh_token", json!(" "), "invalid refresh_token"),
        ("id_token", json!(" "), "did not contain an ID token"),
        ("scope", json!(" "), "invalid scope"),
        (
            "scope",
            json!("openid chatgpt.tokens.use.direct.extra"),
            "did not include chatgpt.tokens.use.direct",
        ),
        ("expires_in", json!(0), "invalid expires_in"),
        ("expires_in", json!(-1), "invalid expires_in"),
    ] {
        let mut response = grant();
        response[field] = invalid;
        assert!(
            decode_token_response(response.to_string().as_bytes(), true, now())
                .unwrap_err()
                .message()
                .contains(expected)
        );
    }
    for field in [
        "access_token",
        "refresh_token",
        "id_token",
        "scope",
        "expires_in",
    ] {
        let mut response = grant();
        response.as_object_mut().unwrap().remove(field);
        assert!(decode_token_response(response.to_string().as_bytes(), true, now()).is_err());
        response[field] = Value::Null;
        assert!(decode_token_response(response.to_string().as_bytes(), true, now()).is_err());
    }
    for body in [b"[]".as_slice(), b"null", b"{", br#"{"access_token":123}"#] {
        assert!(decode_token_response(body, true, now()).is_err());
    }
    let mut huge = grant();
    huge["expires_in"] = json!(i64::MAX);
    assert!(decode_token_response(huge.to_string().as_bytes(), true, now()).is_err());
    // Refresh must validate every grant field too, with ID token optional.
    let mut response = grant();
    response.as_object_mut().unwrap().remove("id_token");
    assert!(decode_token_response(response.to_string().as_bytes(), false, now()).is_ok());
    response["scope"] = json!("openid");
    assert!(decode_token_response(response.to_string().as_bytes(), false, now()).is_err());
}

#[test]
fn expiry_uses_three_minute_margin_and_short_ttl_fallback() {
    for (issued, expected) in [(3600, 3420), (181, 1), (180, 180), (30, 30)] {
        let mut response = grant();
        response["expires_in"] = json!(issued);
        let tokens = decode_token_response(response.to_string().as_bytes(), true, now()).unwrap();
        let expire = DateTime::parse_from_rfc3339(&tokens.expire).unwrap();
        assert_eq!(expire.signed_duration_since(now()).num_seconds(), expected);
        assert_eq!(
            DateTime::parse_from_rfc3339(&tokens.last_refresh).unwrap(),
            now()
        );
    }
}

#[tokio::test]
async fn missing_issued_client_or_code_is_rejected_before_a_request() {
    let fake = Fake::start(Vec::new(), None).await;
    for client_id in ["", " "] {
        assert!(
            fake.auth
                .exchange_code_for_tokens("code", client_id, "", DEVICE, &pkce())
                .await
                .unwrap_err()
                .message()
                .contains("issued client ID")
        );
        let mut tokens = previous();
        tokens.client_id = client_id.into();
        assert!(
            fake.auth
                .refresh_tokens(&tokens)
                .await
                .unwrap_err()
                .message()
                .contains("issued client ID")
        );
    }
    assert!(
        fake.auth
            .exchange_code_for_tokens(" ", "issued", "", DEVICE, &pkce())
            .await
            .is_err()
    );
    let mut tokens = previous();
    tokens.refresh_token.clear();
    assert!(fake.auth.refresh_tokens(&tokens).await.is_err());
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn refresh_retries_are_bounded_and_stop_on_reused_tokens() {
    let mut fake = Fake::start(
        vec![(StatusCode::SERVICE_UNAVAILABLE, "temporary".into()); 3],
        None,
    )
    .await;
    let before = tokio::time::Instant::now();
    let error = fake
        .auth
        .refresh_tokens_with_retry(&previous(), 3)
        .await
        .unwrap_err();
    assert!(error.message().contains("after 3 attempts"));
    assert_eq!(error.status(), 503);
    assert!(before.elapsed() >= Duration::from_secs(3));
    for _ in 0..3 {
        fake.request().await;
    }
    assert!(fake.captured.is_empty());
    let mut fake = Fake::start(
        vec![(
            StatusCode::BAD_REQUEST,
            format!("REFRESH_TOKEN_REUSED {ACCESS} {REFRESH} {ID} {DEVICE}"),
        )],
        None,
    )
    .await;
    let error = fake
        .auth
        .refresh_tokens_with_retry(&previous(), 4)
        .await
        .unwrap_err();
    assert!(error.message().contains("refresh_token_reused"));
    for secret in [ACCESS, REFRESH, ID, DEVICE] {
        assert!(!format!("{error} {error:?}").contains(secret));
    }
    fake.request().await;
    assert!(fake.captured.is_empty());
    let mut fake = Fake::start(vec![(StatusCode::OK, grant().to_string())], None).await;
    assert!(
        fake.auth
            .refresh_tokens_with_retry(&previous(), 0)
            .await
            .is_ok()
    );
    fake.request().await;
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn refresh_can_recover_after_a_retry_and_replace_an_issued_id_token() {
    let mut response = grant();
    response["id_token"] = json!("fake-new-id");
    let mut fake = Fake::start(
        vec![
            (StatusCode::SERVICE_UNAVAILABLE, "temporary".into()),
            (StatusCode::OK, response.to_string()),
        ],
        None,
    )
    .await;
    let tokens = fake
        .auth
        .refresh_tokens_with_retry(&previous(), 2)
        .await
        .unwrap();
    assert!(tokens.id_token == "fake-new-id");
    fake.request().await;
    fake.request().await;
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn invalid_token_responses_and_error_bodies_never_echo_credentials() {
    let mut response = grant();
    response["id_token"] = json!({"secret":ID});
    let mut fake = Fake::start(
        vec![
            (StatusCode::OK, response.to_string()),
            (
                StatusCode::BAD_REQUEST,
                format!("{ACCESS} {REFRESH} {ID} {DEVICE}"),
            ),
            (StatusCode::OK, "x".repeat(MAX_RESPONSE_BYTES + 1)),
        ],
        None,
    )
    .await;
    for _ in 0..3 {
        let error = fake
            .auth
            .exchange_code_for_tokens("code", "issued", "", DEVICE, &pkce())
            .await
            .unwrap_err();
        for secret in [ACCESS, REFRESH, ID, DEVICE] {
            assert!(!format!("{error} {error:?}").contains(secret));
        }
        fake.request().await;
    }
}

#[tokio::test]
async fn concurrent_refreshes_share_one_request_and_keep_each_waiters_local_fields() {
    let release = Arc::new(Semaphore::new(0));
    let mut response = grant();
    response.as_object_mut().unwrap().remove("id_token");
    let mut fake = Fake::start(
        vec![(StatusCode::OK, response.to_string())],
        Some(Arc::clone(&release)),
    )
    .await;
    let auth = fake.auth.clone();
    let first = tokio::spawn(async move { auth.refresh_tokens(&previous()).await });
    fake.request().await;
    let mut other = previous();
    other.id_token = "fake-other-id".into();
    other.device_id = "other-local-device".into();
    let second = fake.auth.refresh_tokens(&other);
    tokio::pin!(second);
    assert!(futures_util::poll!(&mut second).is_pending());
    release.add_permits(1);
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap();
    assert!(first.id_token == ID && first.device_id == DEVICE.to_ascii_lowercase());
    assert!(second.id_token == "fake-other-id" && second.device_id == "other-local-device");
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn cancelling_the_first_waiter_does_not_cancel_a_shared_refresh() {
    let release = Arc::new(Semaphore::new(0));
    let mut fake = Fake::start(
        vec![(StatusCode::OK, grant().to_string())],
        Some(Arc::clone(&release)),
    )
    .await;
    let auth = fake.auth.clone();
    let first = tokio::spawn(async move { auth.refresh_tokens(&previous()).await });
    fake.request().await;
    let tokens = previous();
    let second = fake.auth.refresh_tokens(&tokens);
    tokio::pin!(second);
    assert!(futures_util::poll!(&mut second).is_pending());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    release.add_permits(1);
    assert!(second.await.unwrap().access_token == ACCESS);
    assert!(fake.captured.is_empty());
}

#[test]
fn nested_credentials_are_sparse_and_debug_is_redacted() {
    let mut tokens = previous();
    tokens.scope = SCOPE.into();
    tokens.scopes = SCOPE.split_whitespace().map(str::to_owned).collect();
    tokens.expire = "2026-10-13T12:57:00Z".into();
    tokens.last_refresh = "2026-10-13T12:00:00Z".into();
    let nested = tokens.to_json();
    assert!(SiwcTokenData::from_json(&nested).unwrap() == tokens);
    assert!(nested.contains_key("expired") && !nested.contains_key("expire"));
    assert_eq!(nested.len(), 9);
    let debug = format!("{tokens:?}");
    for secret in [ACCESS, REFRESH, ID, DEVICE, &DEVICE.to_ascii_lowercase()] {
        assert!(!debug.contains(secret));
    }
    let sparse = SiwcTokenData::default().to_json();
    assert_eq!(sparse.len(), 5);
    for field in ["id_token", "scope", "scopes", "device_id"] {
        assert!(!sparse.contains_key(field));
    }
    let invalid = json!({"access_token":{"secret":ACCESS}});
    let error = SiwcTokenData::from_json(invalid.as_object().unwrap()).unwrap_err();
    assert!(!error.contains(ACCESS));
}
