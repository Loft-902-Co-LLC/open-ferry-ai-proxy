//! Not upstream's: upstream tests `HttpRequest` only through its handlers.
//! These send to a mock on 127.0.0.1, with a dummy key, and never to xAI.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::http::Uri;
use http::{HeaderMap, Method};

use super::*;
use crate::codex::request::CONTROL_CHARACTER;

const KEY: &str = "xai-http-key";

/// One request the mock received.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }
}

/// A mock that answers every request with `status` and `body`. Returns its
/// URL and what it received.
async fn serve(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Seen {
                    path: uri.path().to_owned(),
                    headers,
                });
            axum::response::Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, seen)
}

fn last(seen: &Mutex<Vec<Seen>>) -> Seen {
    seen.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .last()
        .cloned()
        .expect("no request reached the mock")
}

fn call(target: HttpTarget) -> HttpCall {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    HttpCall {
        method: Method::POST,
        target,
        headers,
        body: Bytes::from_static(b"{}"),
        client_headers: HeaderMap::new(),
        response_limit: 1 << 20,
        observation: None,
    }
}

fn api_key(base_url: &str) -> Auth {
    let mut auth = Auth {
        provider: "xai".into(),
        ..Auth::default()
    };
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("api_key".into(), KEY.into());
    auth
}

#[tokio::test]
async fn a_path_goes_under_the_base_url_with_the_key() {
    let (url, seen) = serve(200, r#"{"ok":true}"#).await;
    let reply = XaiExecutor::new("direct")
        .http_request_inner(
            &api_key(&format!("{url}/v1/")),
            call(HttpTarget::Path("/files".into())),
        )
        .await
        .unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, r#"{"ok":true}"#);
    let seen = last(&seen);
    assert_eq!(seen.path, "/v1/files");
    assert_eq!(seen.header("authorization"), Some("Bearer xai-http-key"));
    assert_eq!(seen.header("user-agent"), Some(USER_AGENT));
    assert_eq!(seen.header("content-type"), Some("application/json"));
}

#[tokio::test]
async fn sends_no_grok_cli_identity_and_keeps_the_calls_own_conversation() {
    let (url, seen) = serve(200, "{}").await;
    let mut auth = api_key(&url);
    for (name, value) in [
        ("header:X-XAI-Token-Auth", "xai-grok-cli"),
        ("header:x-grok-client-version", "1.0.44"),
        ("header:x-authenticateresponse", "authenticate-response"),
        ("header:User-Agent", "xai-grok-workspace/1.0"),
        ("header:x-grok-conv-id", "made-up-conversation"),
    ] {
        auth.attributes.insert(name.into(), value.into());
    }
    let mut with_conversation = call(HttpTarget::Url(format!("{url}/responses")));
    with_conversation.headers.insert(
        CONV_ID_HEADER,
        HeaderValue::from_static("client-conversation"),
    );
    with_conversation
        .headers
        .insert(header::USER_AGENT, HeaderValue::from_static("grok-cli/1.0"));
    let executor = XaiExecutor::new("direct");
    executor
        .http_request_inner(&auth, with_conversation)
        .await
        .unwrap();
    let first = last(&seen);
    assert_eq!(first.path, "/responses");
    assert_eq!(first.header(CONV_ID_HEADER), Some("client-conversation"));
    executor
        .http_request_inner(&auth, call(HttpTarget::Url(format!("{url}/responses"))))
        .await
        .unwrap();
    let second = last(&seen);
    assert!(second.header(CONV_ID_HEADER).is_none(), "{second:?}");
    for seen in [first, second] {
        assert_eq!(seen.header("user-agent"), Some(USER_AGENT));
        for name in [
            "x-xai-token-auth",
            "x-grok-client-version",
            "x-authenticateresponse",
        ] {
            assert!(seen.header(name).is_none(), "{name} was sent");
        }
    }
}

#[tokio::test]
async fn without_a_key_no_authorization_is_sent() {
    let (url, seen) = serve(200, "{}").await;
    let mut auth = api_key(&url);
    auth.attributes.remove("api_key");
    let mut call = call(HttpTarget::Path("/models".into()));
    call.headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer stale"),
    );
    XaiExecutor::new("direct")
        .http_request_inner(&auth, call)
        .await
        .unwrap();
    assert!(last(&seen).header("authorization").is_none());
}

#[tokio::test]
async fn a_url_with_a_control_character_is_refused_unsent() {
    let (url, seen) = serve(200, "{}").await;
    let error = XaiExecutor::new("direct")
        .http_request_inner(
            &api_key(&url),
            call(HttpTarget::Url(format!(
                "{url}/fi{}les",
                char::from(0x7f_u8)
            ))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, CONTROL_CHARACTER);
    assert!(
        seen.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    );
}

// Not upstream's: a failure's body, which the handler hands on, quotes none
// of the secrets the request sent, nor the password of a proxy that answers
// 407; and the error log, with `request-log` off, has the answer's status
// and body, scrubbed.
#[tokio::test]
async fn a_failure_body_hides_every_secret_sent() {
    for case in crate::secret_echo::cases(api_key).await {
        let base_url = case.auth.attribute("base_url").unwrap_or_default();
        let log = crate::secret_echo::ErrorLog::start();
        let mut call = call(HttpTarget::Url(format!("{base_url}/files")));
        call.client_headers = case.headers.clone();
        call.observation = Some(log.observation());
        let reply = XaiExecutor::new("direct")
            .http_request_inner(&case.auth, call)
            .await
            .unwrap();
        assert!(reply.status == 401 || reply.status == 407, "{reply:?}");
        let body = String::from_utf8_lossy(&reply.body);
        assert!(!body.contains(KEY), "{body}");
        case.check_text(&body);
        let log = log.answered(reply.status);
        assert!(!log.contains(KEY), "{log}");
        case.check_log(&log);
    }
}

// Not upstream's: an answer that succeeds hides the secrets the request sent
// as well: a body that quotes the key, and a 200 with an `error` object that
// does, reach the caller without it, while the call's taps read the body as
// it came.
#[tokio::test]
async fn a_successful_body_hides_every_secret_sent() {
    for (name, body) in [
        (
            "a success",
            r#"{"data":[{"note":"the key is xai-http-key"}]}"#,
        ),
        (
            "an error object",
            r#"{"error":{"message":"bad key xai-http-key","type":"auth"}}"#,
        ),
    ] {
        let (url, _) = serve(200, body).await;
        let (observation, raw) = crate::secret_echo::Raw::observe();
        let mut files = call(HttpTarget::Path("/files".into()));
        files.observation = Some(observation);
        let reply = XaiExecutor::new("direct")
            .http_request_inner(&api_key(&url), files)
            .await
            .unwrap();
        assert_eq!(reply.status, 200);
        let shown = String::from_utf8_lossy(&reply.body);
        assert!(!shown.contains(KEY), "{name}: {shown}");
        assert!(shown.contains("[redacted]"), "{name}: {shown}");
        assert_eq!(raw.seen(), body, "{name}: the taps read it as it came");
    }
}
