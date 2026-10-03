// Ported from CLIProxyAPI internal/auth/codex/pkce.go and oauth_server.go,
// which internal/auth/claude repeats (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What every OAuth login needs: PKCE codes, a `state` value, and a local
//! server for the provider's redirect.
//!
//! Deviations from upstream:
//! - The callback server listens on 127.0.0.1 only; upstream listens on
//!   every interface.
//! - The success page is this project's own and takes nothing from the
//!   query; upstream's copies a `platform_url` parameter into its HTML
//!   unescaped.
//! - A callback whose `state` isn't the login's is turned away with 400
//!   and doesn't end the wait; upstream hands it on and checks it later.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

/// A PKCE verifier and its S256 challenge (upstream's `PKCECodes`).
#[derive(Clone)]
pub struct Pkce {
    /// Sent with the code exchange.
    pub verifier: String,
    /// Sent with the authorization request.
    pub challenge: String,
}

impl Pkce {
    /// A fresh pair: 96 random bytes as unpadded URL-safe base64, and its
    /// SHA-256 the same way.
    pub fn generate() -> Self {
        let verifier = URL_SAFE_NO_PAD.encode(random_bytes::<96>());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

/// A fresh `state` value for an authorization request.
pub fn generate_state() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

fn random_bytes<const N: usize>() -> [u8; N] {
    rand::random()
}

/// What came back to the redirect URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallbackResult {
    /// The authorization code.
    Code(String),
    /// The provider's `error` parameter, or `no_code`.
    Error(String),
}

/// Why waiting for the callback failed.
#[derive(Debug)]
pub enum CallbackError {
    /// No callback came in time.
    Timeout,
    /// The server stopped before one came.
    Closed,
}

impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Timeout => "timeout waiting for OAuth callback",
            Self::Closed => "the OAuth callback server stopped",
        })
    }
}

impl std::error::Error for CallbackError {}

/// A local server for a provider's OAuth redirect (upstream's
/// `OAuthServer`). It stops when dropped.
pub struct CallbackServer {
    port: u16,
    result: oneshot::Receiver<CallbackResult>,
    shutdown: watch::Sender<bool>,
}

#[derive(Clone)]
struct CallbackState {
    state: Arc<str>,
    result: Arc<Mutex<Option<oneshot::Sender<CallbackResult>>>>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

impl CallbackServer {
    /// Listens on 127.0.0.1:`port` (0 for any free port) for a redirect to
    /// `path` that carries `state`.
    pub async fn start(port: u16, path: &str, state: &str) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
        let port = listener.local_addr()?.port();
        let (sender, result) = oneshot::channel();
        let app = Router::new()
            .route(path, get(callback))
            .route("/success", get(success))
            .with_state(CallbackState {
                state: state.into(),
                result: Arc::new(Mutex::new(Some(sender))),
            });
        let (shutdown, mut stopped) = watch::channel(false);
        tokio::spawn(async move {
            let stop = async move {
                let _ = stopped.wait_for(|stop| *stop).await;
            };
            if let Err(error) = axum::serve(listener, app)
                .with_graceful_shutdown(stop)
                .await
            {
                tracing::warn!("OAuth callback server failed: {error}");
            }
        });
        Ok(Self {
            port,
            result,
            shutdown,
        })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Waits up to `timeout` for the redirect.
    pub async fn wait(&mut self, timeout: Duration) -> Result<CallbackResult, CallbackError> {
        match tokio::time::timeout(timeout, &mut self.result).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(CallbackError::Closed),
            Err(_) => Err(CallbackError::Timeout),
        }
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

async fn callback(
    State(server): State<CallbackState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let nonempty = |value: Option<String>| value.filter(|value| !value.is_empty());
    let (result, response) = if let Some(error) = nonempty(query.error) {
        let response = (StatusCode::BAD_REQUEST, format!("OAuth error: {error}")).into_response();
        (CallbackResult::Error(error), response)
    } else if nonempty(query.state).as_deref() != Some(&*server.state) {
        return (StatusCode::BAD_REQUEST, "State parameter doesn't match").into_response();
    } else if let Some(code) = nonempty(query.code) {
        let response = (StatusCode::FOUND, [(http::header::LOCATION, "/success")]).into_response();
        (CallbackResult::Code(code), response)
    } else {
        let response = (StatusCode::BAD_REQUEST, "No authorization code received").into_response();
        (CallbackResult::Error("no_code".to_owned()), response)
    };
    let sender = server
        .result
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(sender) = sender {
        let _ = sender.send(result);
    }
    response
}

async fn success() -> Html<&'static str> {
    Html(SUCCESS_PAGE)
}

const SUCCESS_PAGE: &str = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\
<meta charset=\"utf-8\"><title>Signed in</title></head>\n<body style=\"font-family: \
system-ui, sans-serif; text-align: center; margin-top: 4em\">\n<h1>Signed in</h1>\n\
<p>open-ferry has your credentials. You can close this window.</p>\n</body>\n</html>\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_verifiers_hash() {
        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier.len(), 128);
        let hash = Sha256::digest(pkce.verifier.as_bytes());
        assert_eq!(pkce.challenge, URL_SAFE_NO_PAD.encode(hash));
        assert_ne!(Pkce::generate().verifier, pkce.verifier);
        assert_eq!(generate_state().len(), 43);
    }

    async fn get(port: u16, path_and_query: &str) -> (u16, String) {
        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}{path_and_query}"))
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    #[tokio::test]
    async fn callback_gives_the_code() {
        let mut server = CallbackServer::start(0, "/auth/callback", "s1")
            .await
            .unwrap();
        let port = server.port();
        assert_eq!(get(port, "/auth/callback?code=c1&state=wrong").await.0, 400);
        assert_eq!(get(port, "/auth/callback?code=c1").await.0, 400);
        assert_eq!(get(port, "/auth/callback?code=c1&state=s1").await.0, 302);
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Code("c1".into()));
        let (status, page) = get(port, "/success?platform_url=<script>").await;
        assert_eq!(status, 200);
        assert!(!page.contains("<script>"));
    }

    #[tokio::test]
    async fn callback_gives_the_providers_error() {
        let mut server = CallbackServer::start(0, "/callback", "s1").await.unwrap();
        assert_eq!(
            get(server.port(), "/callback?error=access_denied").await.0,
            400
        );
        let result = server.wait(Duration::from_secs(5)).await.unwrap();
        assert_eq!(result, CallbackResult::Error("access_denied".into()));
    }

    #[tokio::test]
    async fn waiting_times_out() {
        let mut server = CallbackServer::start(0, "/callback", "s1").await.unwrap();
        let error = server.wait(Duration::from_millis(10)).await.unwrap_err();
        assert!(matches!(error, CallbackError::Timeout));
    }
}
