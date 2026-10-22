// Ported from CLIProxyAPI internal/auth/codex/siwc_auth_test.go (v8.0.38, MIT).
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

// Paused-clock tests must use private maps: REFRESHES mixes instants from different runtimes.

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
        let prefix = unique_prefix();
        let router = Router::new()
            .route("/api/accounts/oauth/token", post(token_handler))
            .with_state(FakeState {
                replies: Arc::new(Mutex::new(replies.into())),
                captured,
                release,
            });
        let router = Router::new().nest(&prefix, router);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}{prefix}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            auth: SiwcAuth::with_proxy_url("direct").with_endpoints(Endpoints::with_base(&base)),
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

fn unique_prefix() -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    format!("/fake-{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

fn cache_entry(completed: Option<tokio::time::Instant>) -> RefreshEntry {
    RefreshEntry {
        task: None,
        started: Arc::new(AtomicBool::new(false)),
        shared: futures_util::future::ready(Ok(previous())).boxed().shared(),
        id: Arc::new(()),
        superseded: Arc::new(AtomicBool::new(false)),
        completed,
        successor: None,
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
                "session-id",
                "conversation_id",
                "conversation-id",
                "device_id",
                "device-id",
                "x-client-request-id",
                "chatgpt-account-id"
            ]
            .contains(&name.as_str())
        );
        assert!(!name.as_str().starts_with("x-stainless-"));
        assert!(!name.as_str().starts_with("x-codex-"));
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
        let real = DateTime::parse_from_rfc3339(&tokens.expires_at).unwrap();
        assert_eq!(real.signed_duration_since(now()).num_seconds(), issued);
        assert_eq!(tokens.to_json()["expires_at"], tokens.expires_at);
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
    let first = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap();
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
    assert!(
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .unwrap()
            .access_token
            == ACCESS
    );
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
    assert!(
        SiwcTokenData::from_json(invalid.as_object().unwrap())
            .unwrap()
            .access_token
            .is_empty()
    );
}

#[tokio::test]
async fn production_transport_refuses_cross_origin_307_and_308_token_redirects() {
    for status in [
        StatusCode::TEMPORARY_REDIRECT,
        StatusCode::PERMANENT_REDIRECT,
    ] {
        for refresh in [false, true] {
            let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let location = format!("http://{}/leak", target.local_addr().unwrap());
            let router = Router::new().route(
                "/api/accounts/oauth/token",
                post(move || async move {
                    (
                        status,
                        [(http::header::LOCATION, location)],
                        "remote-secret-body",
                    )
                }),
            );
            let prefix = unique_prefix();
            let router = Router::new().nest(&prefix, router);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}{prefix}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let auth =
                SiwcAuth::with_proxy_url("direct").with_endpoints(Endpoints::with_base(&base));
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                if refresh {
                    auth.refresh_tokens(&previous()).await
                } else {
                    auth.exchange_code_for_tokens("secret-code", "issued", "", DEVICE, &pkce())
                        .await
                }
            })
            .await
            .unwrap();
            let error = result.unwrap_err();
            for secret in [
                "secret-code",
                "test-verifier",
                REFRESH,
                "remote-secret-body",
                &base,
            ] {
                assert!(!format!("{error} {error:?}").contains(secret));
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(20), target.accept())
                    .await
                    .is_err()
            );
            server.abort();
        }
    }
}

#[tokio::test]
async fn test_client_user_agent_is_replaced_by_the_fixed_identity() {
    let mut fake = Fake::start(vec![(StatusCode::OK, grant().to_string())], None).await;
    fake.auth = SiwcAuth::new(
        reqwest::Client::builder()
            .no_proxy()
            .user_agent("caller-override")
            .build()
            .unwrap(),
    )
    .with_endpoints(fake.auth.endpoints.clone());
    fake.auth
        .exchange_code_for_tokens("code", "issued", "", DEVICE, &pkce())
        .await
        .unwrap();
    assert_eq!(
        fake.request().await.headers[http::header::USER_AGENT],
        user_agent()
    );
}

#[tokio::test]
async fn sole_cancelled_waiters_rotation_is_cached_until_the_grace_window_expires() {
    let release = Arc::new(Semaphore::new(0));
    let mut rotated = grant();
    rotated["refresh_token"] = json!("fake-rotated-refresh");
    rotated.as_object_mut().unwrap().remove("id_token");
    let mut fake = Fake::start(
        vec![(StatusCode::OK, rotated.to_string()); 2],
        Some(release.clone()),
    )
    .await;
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", REFRESH);
    let auth = fake.auth.clone();
    let waiter = tokio::spawn(async move { auth.refresh_tokens(&previous()).await });
    fake.request().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    release.add_permits(1);
    // Observe completion without a live refresh waiter: the task must still run.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if REFRESHES
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|entry| entry.completed.is_some())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut other = previous();
    other.id_token = "late-waiter-id".into();
    other.device_id = "late-waiter-device".into();
    let tokens = tokio::time::timeout(Duration::from_secs(5), fake.auth.refresh_tokens(&other))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tokens.refresh_token, "fake-rotated-refresh");
    assert_eq!(tokens.id_token, other.id_token);
    assert_eq!(tokens.device_id, other.device_id);
    assert!(fake.captured.is_empty());
    // Advance just this entry's clock; no real ten-minute sleep or global time change.
    if let Some(expired) = tokio::time::Instant::now().checked_sub(REFRESH_GRACE) {
        REFRESHES.lock().unwrap().get_mut(&key).unwrap().completed = Some(expired);
    } else {
        // Early after boot, expiration is pinned separately using paused time.
        REFRESHES.lock().unwrap().remove(&key);
    }
    release.add_permits(1);
    tokio::time::timeout(
        Duration::from_secs(5),
        fake.auth.refresh_tokens(&previous()),
    )
    .await
    .unwrap()
    .unwrap();
    fake.request().await;
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn completed_failed_refreshes_are_never_cached() {
    let mut fake = Fake::start(
        vec![
            (StatusCode::BAD_REQUEST, "refresh_token_reused".into()),
            (StatusCode::OK, grant().to_string()),
        ],
        None,
    )
    .await;
    let error = fake.auth.refresh_tokens(&previous()).await.unwrap_err();
    assert!(error.is_non_retryable());
    assert_eq!(error.status(), 400);
    fake.request().await;
    assert!(fake.auth.refresh_tokens(&previous()).await.is_ok());
    fake.request().await;
    assert!(fake.captured.is_empty());
}

#[test]
fn refresh_cache_never_exceeds_its_bound_and_evicts_the_oldest() {
    let mut map = HashMap::new();
    let now = tokio::time::Instant::now();
    for index in 0..(MAX_REFRESHES + 5) {
        let key = refresh_key("test-bound", "client", &index.to_string());
        insert_refresh(
            &mut map,
            key,
            RefreshEntry {
                task: None,
                started: Arc::new(AtomicBool::new(false)),
                shared: futures_util::future::ready(Ok(previous())).boxed().shared(),
                id: Arc::new(()),
                superseded: Arc::new(AtomicBool::new(false)),
                completed: Some(now + Duration::from_nanos(index as u64)),
                successor: None,
            },
        );
        assert!(map.len() <= MAX_REFRESHES);
    }
    assert_eq!(map.len(), MAX_REFRESHES);
    for index in 0..5 {
        assert!(!map.contains_key(&refresh_key("test-bound", "client", &index.to_string())));
    }
    assert!(map.contains_key(&refresh_key("test-bound", "client", "5")));
}

#[tokio::test]
async fn cached_rotations_are_reused_until_the_successor_is_rotated() {
    let mut first = grant();
    first["refresh_token"] = json!("fake-successor");
    let mut second = grant();
    second["refresh_token"] = json!("fake-successor-after-rotation");
    let mut fake = Fake::start(
        vec![
            (StatusCode::OK, first.to_string()),
            (StatusCode::OK, second.to_string()),
            (StatusCode::BAD_REQUEST, "refresh_token_reused".into()),
        ],
        None,
    )
    .await;
    let original = previous();
    let rotated = fake.auth.refresh_tokens(&original).await.unwrap();
    fake.request().await;
    assert_eq!(fake.auth.refresh_tokens(&original).await.unwrap(), rotated);
    assert!(fake.captured.is_empty());
    let latest = fake.auth.refresh_tokens(&rotated).await.unwrap();
    assert_eq!(latest.refresh_token, "fake-successor-after-rotation");
    assert!(
        fake.request()
            .await
            .body
            .contains("refresh_token=fake-successor&")
    );
    let error = fake.auth.refresh_tokens(&original).await.unwrap_err();
    assert!(error.is_non_retryable());
    assert_eq!(error, superseded_refresh());
    assert!(fake.captured.is_empty());
    assert_eq!(fake.auth.refresh_tokens(&rotated).await.unwrap(), latest);
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn failed_successor_refreshes_keep_the_predecessors_cached_rotation() {
    let mut first = grant();
    first["refresh_token"] = json!("fake-successor");
    let mut fake = Fake::start(
        vec![
            (StatusCode::OK, first.to_string()),
            (StatusCode::SERVICE_UNAVAILABLE, "temporary".into()),
        ],
        None,
    )
    .await;
    let original = previous();
    let rotated = fake.auth.refresh_tokens(&original).await.unwrap();
    fake.request().await;
    assert!(fake.auth.refresh_tokens(&rotated).await.is_err());
    fake.request().await;
    assert_eq!(fake.auth.refresh_tokens(&original).await.unwrap(), rotated);
    assert!(fake.captured.is_empty());
}

#[tokio::test(start_paused = true)]
async fn refresh_cache_prunes_expired_then_evicts_by_completion_time_and_refuses_in_flight() {
    let mut map = HashMap::new();
    let now = tokio::time::Instant::now();
    insert_refresh_with_limit(&mut map, [0; 32], cache_entry(None), 3).unwrap();
    // Insertion order differs from completion order.
    insert_refresh_with_limit(
        &mut map,
        [1; 32],
        cache_entry(Some(now + Duration::from_secs(2))),
        3,
    )
    .unwrap();
    insert_refresh_with_limit(
        &mut map,
        [2; 32],
        cache_entry(Some(now + Duration::from_secs(1))),
        3,
    )
    .unwrap();
    insert_refresh_with_limit(&mut map, [3; 32], cache_entry(None), 3).unwrap();
    assert!(map.contains_key(&[0; 32]));
    assert!(map.contains_key(&[1; 32]));
    assert!(!map.contains_key(&[2; 32]));
    tokio::time::advance(REFRESH_GRACE + Duration::from_secs(2)).await;
    insert_refresh_with_limit(&mut map, [4; 32], cache_entry(None), 3).unwrap();
    assert!(!map.contains_key(&[1; 32]));
    let error = insert_refresh_with_limit(&mut map, [5; 32], cache_entry(None), 3).unwrap_err();
    assert!(!error.is_non_retryable());
    assert_eq!(map.len(), 3);
    assert!(map.contains_key(&[0; 32]));
    assert!(!map.contains_key(&[5; 32]));
}

#[tokio::test]
async fn panicked_refresh_tasks_return_an_error_without_retrying() {
    let mut fake = Fake::start(vec![(StatusCode::OK, grant().to_string())], None).await;
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", REFRESH);
    let task = tokio::spawn(async { panic!("fake refresh task panic") });
    let id = Arc::new(());
    insert_refresh(
        &mut REFRESHES.lock().unwrap(),
        key,
        RefreshEntry {
            task: None,
            started: Arc::new(AtomicBool::new(false)),
            shared: shared_refresh(task, key, id.clone(), Arc::new(AtomicBool::new(false))),
            id,
            superseded: Arc::new(AtomicBool::new(false)),
            completed: None,
            successor: None,
        },
    );
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        fake.auth.refresh_tokens(&previous()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(
        error.message(),
        "SIWC token refresh failed: invalid single-flight result"
    );
    assert!(fake.captured.is_empty());
    assert!(!REFRESHES.lock().unwrap().contains_key(&key));
    assert!(fake.auth.refresh_tokens(&previous()).await.is_ok());
    fake.request().await;
}

#[tokio::test]
async fn an_unpolled_refresh_task_does_not_poison_the_key_or_remove_a_replacement() {
    let mut fake = Fake::start(vec![(StatusCode::OK, grant().to_string())], None).await;
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", REFRESH);
    let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = polled.clone();
    let task = tokio::spawn(async move {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(previous())
    });
    // This current-thread runtime cannot poll the spawned task before abort.
    task.abort();
    let id = Arc::new(());
    let shared = shared_refresh(task, key, id.clone(), Arc::new(AtomicBool::new(false)));
    insert_refresh(
        &mut REFRESHES.lock().unwrap(),
        key,
        RefreshEntry {
            task: None,
            started: Arc::new(AtomicBool::new(false)),
            shared,
            id: id.clone(),
            superseded: Arc::new(AtomicBool::new(false)),
            completed: None,
            successor: None,
        },
    );
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            fake.auth.refresh_tokens(&previous())
        )
        .await
        .unwrap()
        .is_ok()
    );
    assert!(!polled.load(std::sync::atomic::Ordering::SeqCst));
    fake.request().await;
    finish_refresh(key, &id, None, false);
    assert!(REFRESHES.lock().unwrap().contains_key(&key));
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn oversized_error_responses_keep_the_status_and_non_retryable_marker() {
    let mut fake = Fake::start(
        vec![(
            StatusCode::BAD_REQUEST,
            format!(
                "refresh_token_reused {REFRESH}{}",
                "x".repeat(MAX_RESPONSE_BYTES)
            ),
        )],
        None,
    )
    .await;
    let error = fake.auth.refresh_tokens(&previous()).await.unwrap_err();
    assert_eq!(error.status(), 400);
    assert!(error.is_non_retryable());
    assert!(!format!("{error} {error:?}").contains(REFRESH));
    fake.request().await;
}

#[tokio::test]
async fn send_errors_keep_the_connect_kind_without_echoing_the_url() {
    // Bound but non-listening, so nobody can race to take a "closed" port.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let base = format!("http://{}{}", socket.local_addr().unwrap(), unique_prefix());
    let auth = SiwcAuth::with_proxy_url("direct").with_endpoints(Endpoints::with_base(&base));
    let error = tokio::time::timeout(Duration::from_secs(5), auth.refresh_tokens(&previous()))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.message(), "SIWC token request failed (connect)");
    assert_eq!(error.status(), 0);
    assert!(!format!("{error} {error:?}").contains(&base));
}

#[test]
fn nested_credentials_treat_non_strings_as_absent_and_accept_the_access_alias() {
    for invalid in [
        Value::Null,
        json!(123),
        json!(false),
        json!({"secret":ACCESS}),
        json!([ACCESS]),
    ] {
        let mut object = previous().to_json();
        for field in [
            "access_token",
            "refresh_token",
            "id_token",
            "client_id",
            "scope",
            "expired",
            "last_refresh",
            "device_id",
        ] {
            object.insert(field.into(), invalid.clone());
        }
        object.insert("scopes".into(), Value::Null);
        let tokens = SiwcTokenData::from_json(&object).unwrap();
        assert_eq!(tokens, SiwcTokenData::default());
        object.insert("access".into(), json!(" alias-token "));
        assert_eq!(
            SiwcTokenData::from_json(&object).unwrap().access_token,
            "alias-token"
        );
    }
    for access in [Value::Null, json!(17), json!(" ")] {
        let object = json!({"access_token":access,"access":"alias"});
        assert_eq!(
            SiwcTokenData::from_json(object.as_object().unwrap())
                .unwrap()
                .access_token,
            "alias"
        );
    }
    let object = json!({"access_token":"preferred","access":"alias"});
    assert_eq!(
        SiwcTokenData::from_json(object.as_object().unwrap())
            .unwrap()
            .access_token,
        "preferred"
    );
    let object = json!({"scopes":["openid",null,17]});
    assert_eq!(
        SiwcTokenData::from_json(object.as_object().unwrap())
            .unwrap()
            .scopes,
        ["openid"]
    );
}

#[tokio::test]
async fn send_errors_distinguish_timeout_tls_and_other_without_remote_text() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/private-credential",
        listener.local_addr().unwrap()
    );
    let error = client
        .get(&url)
        .timeout(Duration::from_millis(20))
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        send_error(error).message(),
        "SIWC token request failed (timeout)"
    );
    drop(listener);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "https://{}/private-credential",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut hello = [0; 1024];
        assert!(stream.read(&mut hello).await.unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 400 private-remote-text\r\n\r\n")
            .await
            .unwrap();
    });
    let error = tokio::time::timeout(Duration::from_secs(5), client.get(&url).send())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        send_error(error).message(),
        "SIWC token request failed (TLS)"
    );
    server.await.unwrap();
    let error = client
        .get("invalid-url-with-private-credential")
        .send()
        .await
        .unwrap_err();
    assert_eq!(
        send_error(error).message(),
        "SIWC token request failed (other)"
    );
}

#[tokio::test]
async fn a_paused_waiter_rejects_a_rotation_spent_by_its_successor() {
    let release = Arc::new(Semaphore::new(0));
    let mut first = grant();
    first["refresh_token"] = json!("fake-successor");
    let mut second = grant();
    second["refresh_token"] = json!("fake-latest");
    let mut fake = Fake::start(
        vec![
            (StatusCode::OK, first.to_string()),
            (StatusCode::OK, second.to_string()),
        ],
        Some(release.clone()),
    )
    .await;
    let original = previous();
    let auth = fake.auth.clone();
    let mut held = Box::pin(auth.refresh_tokens(&original));
    let waker = futures_util::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    assert!(std::future::Future::poll(held.as_mut(), &mut context).is_pending());
    fake.request().await;
    release.add_permits(1);
    let rotated = fake.auth.refresh_tokens(&original).await.unwrap();
    release.add_permits(1);
    let latest = fake.auth.refresh_tokens(&rotated).await.unwrap();
    fake.request().await;
    assert_eq!(latest.refresh_token, "fake-latest");
    assert_eq!(held.await.unwrap_err(), superseded_refresh());
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn a_reused_successor_tombstones_its_predecessor_without_a_request() {
    let mut first = grant();
    first["refresh_token"] = json!("fake-successor");
    let mut fake = Fake::start(
        vec![
            (StatusCode::OK, first.to_string()),
            (StatusCode::BAD_REQUEST, "refresh_token_reused".into()),
        ],
        None,
    )
    .await;
    let original = previous();
    let rotated = fake.auth.refresh_tokens(&original).await.unwrap();
    fake.request().await;
    let error = fake.auth.refresh_tokens(&rotated).await.unwrap_err();
    assert!(error.is_non_retryable());
    assert_eq!(error.status(), 400);
    fake.request().await;
    assert_eq!(
        fake.auth.refresh_tokens(&original).await.unwrap_err(),
        superseded_refresh()
    );
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn a_successor_with_evicted_tracking_still_tombstones_its_predecessor() {
    let release = Arc::new(Semaphore::new(0));
    let mut first = grant();
    first["refresh_token"] = json!("fake-successor");
    let mut second = grant();
    second["refresh_token"] = json!("fake-latest");
    let mut fake = Fake::start(
        vec![
            (StatusCode::OK, first.to_string()),
            (StatusCode::OK, second.to_string()),
        ],
        Some(release.clone()),
    )
    .await;
    release.add_permits(1);
    let original = previous();
    let rotated = fake.auth.refresh_tokens(&original).await.unwrap();
    fake.request().await;
    let auth = fake.auth.clone();
    let successor = tokio::spawn(async move { auth.refresh_tokens(&rotated).await });
    fake.request().await;
    let key = refresh_key(
        &fake.auth.endpoints.token_url,
        "oaiapp_issued",
        "fake-successor",
    );
    // Production no longer evicts in-flight entries. Force tracking loss to pin
    // invalidation independently of the task's own bookkeeping identity.
    REFRESHES.lock().unwrap().remove(&key).unwrap();
    release.add_permits(1);
    assert_eq!(
        successor.await.unwrap().unwrap().refresh_token,
        "fake-latest"
    );
    assert_eq!(
        fake.auth.refresh_tokens(&original).await.unwrap_err(),
        superseded_refresh()
    );
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn a_second_pre_send_cancellation_is_not_retried() {
    let fake = Fake::start(vec![], None).await;
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", REFRESH);
    let first = tokio::spawn(async { Ok(previous()) });
    first.abort();
    let second = tokio::spawn(async { Ok(previous()) });
    second.abort();
    let first_id = Arc::new(());
    let first_shared = shared_refresh(
        first,
        key,
        first_id.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    let second_id = Arc::new(());
    let second_shared = shared_refresh(
        second,
        key,
        second_id.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    let shared = async move {
        let result = first_shared.await;
        let mut entry = cache_entry(None);
        entry.id = second_id;
        entry.shared = second_shared;
        insert_refresh(&mut REFRESHES.lock().unwrap(), key, entry);
        result
    }
    .boxed()
    .shared();
    let mut entry = cache_entry(None);
    entry.id = first_id;
    entry.shared = shared;
    insert_refresh(&mut REFRESHES.lock().unwrap(), key, entry);
    let error = fake.auth.refresh_tokens(&previous()).await.unwrap_err();
    assert_eq!(error, invalid_refresh_result());
    assert!(fake.captured.is_empty());
    assert!(!REFRESHES.lock().unwrap().contains_key(&key));
}

#[tokio::test]
async fn runtime_shutdown_after_sending_never_replays_the_refresh() {
    let release = Arc::new(Semaphore::new(0));
    let mut fake = Fake::start(vec![(StatusCode::OK, grant().to_string())], Some(release)).await;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let auth = fake.auth.clone();
    let waiter = runtime.spawn(async move { auth.refresh_tokens(&previous()).await });
    fake.request().await;
    runtime.shutdown_background();
    // The task reached send, proven by the handler signal, but never got a reply.
    assert!(waiter.await.unwrap_err().is_cancelled());
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        fake.auth.refresh_tokens_with_retry(&previous(), 3),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error, interrupted_refresh());
    assert!(error.is_non_retryable());
    assert_eq!(
        fake.auth.refresh_tokens(&previous()).await.unwrap_err(),
        error
    );
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn a_full_in_flight_cache_refuses_a_new_refresh_without_sending() {
    let cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));
    let release = Arc::new(Semaphore::new(0));
    let mut fake = Fake::start(
        vec![(StatusCode::OK, grant().to_string())],
        Some(release.clone()),
    )
    .await;
    let auth = fake.auth.clone();
    let first =
        tokio::spawn(async move { auth.refresh_tokens_in_cache(&previous(), 1, cache).await });
    fake.request().await;
    let mut other = previous();
    other.refresh_token = "fake-other-token".into();
    let error = fake
        .auth
        .refresh_tokens_in_cache(&other, 1, cache)
        .await
        .unwrap_err();
    assert_eq!(
        error.message(),
        "SIWC token refresh cache is full; retry later"
    );
    assert!(!error.is_non_retryable());
    assert_eq!(cache.lock().unwrap().len(), 1);
    assert!(fake.captured.is_empty());
    release.add_permits(1);
    assert!(first.await.unwrap().is_ok());
    cache.lock().unwrap().clear();
}

#[tokio::test]
async fn an_evicted_success_retires_its_held_waiter_before_successor_rotation() {
    let cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));
    let release = Arc::new(Semaphore::new(0));
    let mut first = grant();
    first["refresh_token"] = json!("fake-T1");
    let mut second = grant();
    second["refresh_token"] = json!("fake-T2");
    let mut fake = Fake::start(
        vec![
            (StatusCode::OK, first.to_string()),
            (StatusCode::OK, second.to_string()),
        ],
        Some(release.clone()),
    )
    .await;
    let original = previous();
    let auth = fake.auth.clone();
    let mut held = Box::pin(auth.refresh_tokens_in_cache(&original, 1, cache));
    let waker = futures_util::task::noop_waker();
    assert!(
        held.as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
    fake.request().await;
    release.add_permits(1);
    let rotated = fake
        .auth
        .refresh_tokens_in_cache(&original, 1, cache)
        .await
        .unwrap();
    // Reserving T1 evicts the completed T0 entry before T1 can finish.
    release.add_permits(1);
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&rotated, 1, cache)
            .await
            .unwrap()
            .refresh_token,
        "fake-T2"
    );
    fake.request().await;
    assert_eq!(held.await.unwrap_err(), superseded_refresh());
    assert!(fake.captured.is_empty());
    cache.lock().unwrap().clear();
}

#[tokio::test]
async fn an_expired_success_sets_its_invalidation_flag() {
    let mut map = HashMap::new();
    let mut entry = cache_entry(Some(tokio::time::Instant::now()));
    entry.successor = Some([1; 32]);
    let flag = entry.superseded.clone();
    map.insert([0; 32], entry);
    tokio::time::pause();
    tokio::time::advance(REFRESH_GRACE).await;
    prune_refreshes(&mut map);
    assert!(map.is_empty());
    assert!(flag.load(Ordering::SeqCst));
}

#[tokio::test]
async fn lost_post_send_tasks_admit_unrelated_tokens_without_replaying_the_old_token() {
    let cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));
    let release = Arc::new(Semaphore::new(0));
    let mut fake = Fake::start(
        vec![(StatusCode::OK, grant().to_string()); 2],
        Some(release.clone()),
    )
    .await;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let auth = fake.auth.clone();
    let waiter =
        runtime.spawn(async move { auth.refresh_tokens_in_cache(&previous(), 1, cache).await });
    fake.request().await;
    // Keep a caller parked without polling the Shared wrapper after shutdown.
    let original = previous();
    let auth = fake.auth.clone();
    let mut held = Box::pin(auth.refresh_tokens_in_cache(&original, 1, cache));
    let waker = futures_util::task::noop_waker();
    assert!(
        held.as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    runtime.shutdown_background();
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", REFRESH);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !cache.lock().unwrap()[&key]
            .task
            .as_ref()
            .unwrap()
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut other = previous();
    other.refresh_token = "fake-unrelated".into();
    release.add_permits(2);
    assert!(
        fake.auth
            .refresh_tokens_in_cache(&other, 1, cache)
            .await
            .is_ok()
    );
    assert!(
        fake.request()
            .await
            .body
            .contains("refresh_token=fake-unrelated&")
    );
    assert_eq!(held.await.unwrap_err(), interrupted_refresh());
    assert!(fake.captured.is_empty());
    cache.lock().unwrap().clear();
}

#[tokio::test]
async fn a_real_unpolled_task_is_removed_after_runtime_shutdown() {
    let cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));
    let mut fake = Fake::start(vec![(StatusCode::OK, grant().to_string())], None).await;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let original = previous();
    let auth = fake.auth.clone();
    let mut held = Box::pin(auth.refresh_tokens_in_cache(&original, 1, cache));
    {
        let _entered = runtime.enter();
        let waker = futures_util::task::noop_waker();
        assert!(
            held.as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_pending()
        );
    }
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", REFRESH);
    assert!(!cache.lock().unwrap()[&key].started.load(Ordering::SeqCst));
    drop(held);
    runtime.shutdown_background();
    prune_refreshes(&mut cache.lock().unwrap());
    assert!(!cache.lock().unwrap().contains_key(&key));
    let mut other = previous();
    other.refresh_token = "fake-unrelated".into();
    assert!(
        fake.auth
            .refresh_tokens_in_cache(&other, 1, cache)
            .await
            .is_ok()
    );
    fake.request().await;
    assert!(!cache.lock().unwrap().contains_key(&key));
    assert!(fake.captured.is_empty());
    cache.lock().unwrap().clear();
}

#[tokio::test]
async fn a_post_send_panic_is_kept_and_never_retried() {
    let mut fake = Fake::start(vec![(StatusCode::OK, grant().to_string())], None).await;
    fake.auth.panic_after_send = Some(true);
    let error = fake
        .auth
        .refresh_tokens_with_retry(&previous(), 3)
        .await
        .unwrap_err();
    assert_eq!(error, interrupted_refresh());
    assert!(error.is_non_retryable());
    fake.request().await;
    assert_eq!(
        fake.auth.refresh_tokens(&previous()).await.unwrap_err(),
        error
    );
    assert!(fake.captured.is_empty());
}

#[tokio::test]
async fn a_real_pre_send_panic_stays_retryable() {
    let mut fake = Fake::start(vec![(StatusCode::OK, grant().to_string())], None).await;
    fake.auth.panic_after_send = Some(false);
    let error = fake.auth.refresh_tokens(&previous()).await.unwrap_err();
    assert_eq!(error, invalid_refresh_result());
    assert!(!error.is_non_retryable());
    assert!(fake.captured.is_empty());
    fake.auth.panic_after_send = None;
    assert!(fake.auth.refresh_tokens(&previous()).await.is_ok());
    fake.request().await;
}

#[tokio::test]
async fn rotated_sets_tombstone_every_predecessor_then_expiry_sends_again() {
    let cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));
    let mut replies = Vec::new();
    for token in ["fake-T1", "fake-T2", "fake-T3", "fake-T4", "fake-T5"] {
        let mut response = grant();
        response["refresh_token"] = json!(token);
        replies.push((StatusCode::OK, response.to_string()));
    }
    let mut fake = Fake::start(replies, None).await;
    let mut tokens = vec![previous()];
    // T2 may still return unspent T3 until T3 itself is rotated (L15/L20).
    // Rotate T3 too before asserting that T0, T1 and T2 all fail closed.
    for _ in 0..4 {
        tokens.push(
            fake.auth
                .refresh_tokens_in_cache(tokens.last().unwrap(), MAX_REFRESHES, cache)
                .await
                .unwrap(),
        );
        fake.request().await;
    }
    for old in &tokens[..3] {
        assert_eq!(
            fake.auth
                .refresh_tokens_in_cache(old, MAX_REFRESHES, cache)
                .await
                .unwrap_err(),
            superseded_refresh()
        );
    }
    assert!(fake.captured.is_empty());
    tokio::time::pause();
    tokio::time::advance(REFRESH_GRACE).await;
    tokio::time::resume();
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&tokens[0], MAX_REFRESHES, cache)
            .await
            .unwrap()
            .refresh_token,
        "fake-T5"
    );
    fake.request().await;
    assert!(fake.captured.is_empty());
    cache.lock().unwrap().clear();
}

#[tokio::test]
async fn an_unobserved_post_send_panic_is_kept_from_detection_until_expiry() {
    let cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));
    let mut fake = Fake::start(vec![(StatusCode::OK, grant().to_string()); 3], None).await;
    let mut panicking = fake.auth.clone();
    panicking.panic_after_send = Some(true);
    let original = previous();
    let mut held = Box::pin(panicking.refresh_tokens_in_cache(&original, 2, cache));
    let waker = futures_util::task::noop_waker();
    assert!(
        held.as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
    fake.request().await;
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", REFRESH);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !cache.lock().unwrap()[&key]
            .task
            .as_ref()
            .unwrap()
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut other = previous();
    other.refresh_token = "fake-unrelated".into();
    assert!(
        fake.auth
            .refresh_tokens_in_cache(&other, 2, cache)
            .await
            .is_ok()
    );
    fake.request().await;
    let detected = cache.lock().unwrap()[&key].completed.unwrap();
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&original, 2, cache)
            .await
            .unwrap_err(),
        interrupted_refresh()
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(held.await.unwrap_err(), interrupted_refresh());
    assert_eq!(cache.lock().unwrap()[&key].completed, Some(detected));
    tokio::time::advance(REFRESH_GRACE - Duration::from_secs(60)).await;
    tokio::time::resume();
    assert!(
        fake.auth
            .refresh_tokens_in_cache(&original, 2, cache)
            .await
            .is_ok()
    );
    assert!(
        fake.request()
            .await
            .body
            .contains("refresh_token=fake-siwc-refresh&")
    );
    assert!(fake.captured.is_empty());
    cache.lock().unwrap().clear();
}

#[tokio::test]
async fn a_wrapper_observed_post_send_loss_retires_predecessors() {
    predecessor_task_loss(PredecessorLoss::WrapperPostSend).await;
}

#[tokio::test]
async fn a_reap_observed_post_send_loss_retires_predecessors() {
    predecessor_task_loss(PredecessorLoss::ReapPostSend).await;
}

#[tokio::test]
async fn a_pre_send_loss_preserves_predecessors() {
    predecessor_task_loss(PredecessorLoss::BeforeFirstPoll).await;
}

#[tokio::test]
async fn a_wrapper_observed_pre_send_loss_preserves_predecessors() {
    predecessor_task_loss(PredecessorLoss::WrapperPreSend).await;
}

#[tokio::test]
async fn a_wrapper_observed_pre_send_cancellation_preserves_predecessors() {
    predecessor_task_loss(PredecessorLoss::WrapperCancelled).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PredecessorLoss {
    ReapPostSend,
    WrapperPostSend,
    BeforeFirstPoll,
    WrapperPreSend,
    WrapperCancelled,
}

async fn predecessor_task_loss(mode: PredecessorLoss) {
    let cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));
    let release = Arc::new(Semaphore::new(0));
    let replies = ["fake-T1", "fake-T2"]
        .map(|token| {
            let mut response = grant();
            response["refresh_token"] = json!(token);
            (StatusCode::OK, response.to_string())
        })
        .to_vec();
    let mut fake = Fake::start(replies, Some(release.clone())).await;
    let original = previous();
    let auth = fake.auth.clone();
    let mut held = Box::pin(auth.refresh_tokens_in_cache(&original, 3, cache));
    let waker = futures_util::task::noop_waker();
    assert!(
        held.as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
    assert!(
        fake.request()
            .await
            .body
            .contains("refresh_token=fake-siwc-refresh&")
    );
    release.add_permits(1);
    let rotated = fake
        .auth
        .refresh_tokens_in_cache(&original, 3, cache)
        .await
        .unwrap();
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", "fake-T1");
    let mut successor_auth = fake.auth.clone();
    successor_auth.panic_after_send = match mode {
        PredecessorLoss::WrapperPostSend => Some(true),
        PredecessorLoss::WrapperPreSend => Some(false),
        _ => None,
    };
    if matches!(
        mode,
        PredecessorLoss::WrapperPostSend | PredecessorLoss::WrapperPreSend
    ) {
        // Join before the response, so this caller observes the wrapper, not the reap.
        let mut successor = Box::pin(successor_auth.refresh_tokens_in_cache(&rotated, 3, cache));
        assert!(
            successor
                .as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_pending()
        );
        if mode == PredecessorLoss::WrapperPreSend {
            assert_eq!(successor.await.unwrap_err(), invalid_refresh_result());
        } else {
            assert!(fake.request().await.body.contains("refresh_token=fake-T1&"));
            release.add_permits(1);
            assert_eq!(successor.await.unwrap_err(), interrupted_refresh());
        }
    } else {
        let runtime = if matches!(
            mode,
            PredecessorLoss::BeforeFirstPoll | PredecessorLoss::WrapperCancelled
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        } else {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap()
        };
        let mut successor = Box::pin(successor_auth.refresh_tokens_in_cache(&rotated, 3, cache));
        {
            let _entered = runtime.enter();
            assert!(
                successor
                    .as_mut()
                    .poll(&mut std::task::Context::from_waker(&waker))
                    .is_pending()
            );
        }
        if mode == PredecessorLoss::ReapPostSend {
            assert!(fake.request().await.body.contains("refresh_token=fake-T1&"));
        } else {
            assert!(!cache.lock().unwrap()[&key].started.load(Ordering::SeqCst));
        }
        // Keep the wrapper unpolled until shutdown so the reap cannot observe
        // this cancellation first, and don't let a caller retry T1.
        let shared = cache.lock().unwrap()[&key].shared.clone();
        drop(successor);
        runtime.shutdown_background();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !cache.lock().unwrap()[&key]
                .task
                .as_ref()
                .unwrap()
                .is_finished()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if matches!(
            mode,
            PredecessorLoss::BeforeFirstPoll | PredecessorLoss::WrapperCancelled
        ) {
            if mode == PredecessorLoss::WrapperCancelled {
                assert!(matches!(shared.await, Err(RefreshError::Cancelled)));
            } else {
                prune_refreshes(&mut cache.lock().unwrap());
            }
            assert!(!cache.lock().unwrap().contains_key(&key));
        } else {
            // This lookup's prune is the first observer of the loss.
            assert_eq!(
                fake.auth
                    .refresh_tokens_in_cache(&rotated, 3, cache)
                    .await
                    .unwrap_err(),
                interrupted_refresh()
            );
        }
    }
    if matches!(
        mode,
        PredecessorLoss::BeforeFirstPoll
            | PredecessorLoss::WrapperPreSend
            | PredecessorLoss::WrapperCancelled
    ) {
        assert_eq!(held.await.unwrap().refresh_token, "fake-T1");
        assert_eq!(
            fake.auth
                .refresh_tokens_in_cache(&original, 3, cache)
                .await
                .unwrap()
                .refresh_token,
            "fake-T1"
        );
        assert!(fake.captured.is_empty());
        cache.lock().unwrap().clear();
        return;
    }
    let detected = cache.lock().unwrap()[&key].completed.unwrap();
    assert_eq!(held.await.unwrap_err(), superseded_refresh());
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&original, 3, cache)
            .await
            .unwrap_err(),
        superseded_refresh()
    );
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&rotated, 3, cache)
            .await
            .unwrap_err(),
        interrupted_refresh()
    );
    assert_eq!(cache.lock().unwrap()[&key].completed, Some(detected));
    assert!(fake.captured.is_empty());
    cache.lock().unwrap().clear();
}

#[tokio::test]
async fn an_evicted_tombstone_allows_a_fresh_request() {
    let cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));
    let replies = ["fake-T1", "fake-T2", "fake-U1", "fake-T3"]
        .map(|token| {
            let mut response = grant();
            response["refresh_token"] = json!(token);
            (StatusCode::OK, response.to_string())
        })
        .to_vec();
    let mut fake = Fake::start(replies, None).await;
    let original = previous();
    let rotated = fake
        .auth
        .refresh_tokens_in_cache(&original, 2, cache)
        .await
        .unwrap();
    fake.request().await;
    fake.auth
        .refresh_tokens_in_cache(&rotated, 2, cache)
        .await
        .unwrap();
    fake.request().await;
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&original, 2, cache)
            .await
            .unwrap_err(),
        superseded_refresh()
    );
    let mut other = previous();
    other.refresh_token = "fake-unrelated".into();
    fake.auth
        .refresh_tokens_in_cache(&other, 2, cache)
        .await
        .unwrap();
    fake.request().await;
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", REFRESH);
    assert!(!cache.lock().unwrap().contains_key(&key));
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&original, 2, cache)
            .await
            .unwrap()
            .refresh_token,
        "fake-T3"
    );
    assert!(
        fake.request()
            .await
            .body
            .contains("refresh_token=fake-siwc-refresh&")
    );
    assert!(fake.captured.is_empty());
    cache.lock().unwrap().clear();
}

#[tokio::test]
async fn an_evicted_interruption_allows_a_fresh_request() {
    let cache: &'static Mutex<HashMap<[u8; 32], RefreshEntry>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));
    let replies = ["fake-T1", "fake-U1", "fake-T2"]
        .map(|token| {
            let mut response = grant();
            response["refresh_token"] = json!(token);
            (StatusCode::OK, response.to_string())
        })
        .to_vec();
    let mut fake = Fake::start(replies, None).await;
    let mut panicking = fake.auth.clone();
    panicking.panic_after_send = Some(true);
    let original = previous();
    assert_eq!(
        panicking
            .refresh_tokens_in_cache(&original, 1, cache)
            .await
            .unwrap_err(),
        interrupted_refresh()
    );
    fake.request().await;
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&original, 1, cache)
            .await
            .unwrap_err(),
        interrupted_refresh()
    );
    assert!(fake.captured.is_empty());
    let mut other = previous();
    other.refresh_token = "fake-unrelated".into();
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&other, 1, cache)
            .await
            .unwrap()
            .refresh_token,
        "fake-U1"
    );
    fake.request().await;
    let key = refresh_key(&fake.auth.endpoints.token_url, "oaiapp_issued", REFRESH);
    assert!(!cache.lock().unwrap().contains_key(&key));
    assert_eq!(
        fake.auth
            .refresh_tokens_in_cache(&original, 1, cache)
            .await
            .unwrap()
            .refresh_token,
        "fake-T2"
    );
    assert!(
        fake.request()
            .await
            .body
            .contains("refresh_token=fake-siwc-refresh&")
    );
    assert!(fake.captured.is_empty());
    cache.lock().unwrap().clear();
}
