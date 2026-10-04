// Ported from CLIProxyAPI internal/runtime/executor/codex_executor_request.go
// (PrepareRequest, HttpRequest) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`CodexExecutor`]'s plain HTTP requests (upstream's `HttpRequest`), which
//! Codex Alpha Search sends.
//!
//! The request gets the credential's token as `Authorization: Bearer` (the
//! `api_key` attribute, else the OAuth access token), or loses any
//! `Authorization` when there is none, then the credential's custom
//! headers. A path goes under the executor's base URL: ChatGPT's Codex API,
//! unless [`CodexExecutor::with_base_url`] names another.
//!
//! Deviations from upstream:
//! - The request goes through the credential's shared proxy client with
//!   rustls, where upstream builds a uTLS client per request. Like every
//!   call here, it has a connect timeout; like upstream's, no overall one.
//! - A request without a `User-Agent` says `open-ferry/<version>`, where
//!   Go's says `Go-http-client`.
//! - The answer's body is read here, up to the call's limit, and the rest
//!   dropped; a connection error reads as no answer, without the URL.
//! - A custom header can't set a client identity header (see
//!   [`crate::custom_headers`]).
//! - The URL is read as a WHATWG URL, so its `.` and `..` segments are
//!   resolved, percent-encoded ones such as `%2e%2e` included, and a `\`
//!   reads as `/`. Go sends `/v1/%2e%2e/alpha/search` as written and a `\`
//!   as `%5C`. A URL with an ASCII control character before any `#`,
//!   which the WHATWG parser would drop or encode, fails before anything is
//!   sent, as Go's does, with Go's message (`net/url: invalid control
//!   character in URL`) but without the URL, which may hold a secret.
//! - An answer, of any status, has every secret the request sent redacted
//!   from its body, if it is of eight bytes or more, as every client error is
//!   (see `Policy::Client`, [`observe_send::secrets`] and
//!   [`crate::redact`]): the token, the credential headers after the custom
//!   ones, each cookie, the URL's credentials and the proxy's password. So a
//!   search that quotes one in its results, as well as a failure that does
//!   in its error, reaches the handler without it, where upstream hands the
//!   body on as it came. The call's taps read the body as it came.

use std::borrow::Cow;

use bytes::Bytes;
use http::header::{self, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ErrorKind, ExecError, Format, HttpCall, HttpReply, HttpTarget};
use open_ferry_core::observe::AttemptKind;

use super::CodexExecutor;
use crate::codex::client::{USER_AGENT, error_chain, read_body_prefix};
use crate::codex::request::{credentials, refuse_control_characters};
use crate::custom_headers;
use crate::observe_send::{self, Attempt, BodyTap};
use crate::redact::Policy;

impl CodexExecutor {
    /// Sends `call` with the credential's token and custom headers, and
    /// reads the answer, whatever its status (`HttpRequest`); the body
    /// without the secrets the request sent.
    pub(super) async fn http_request_inner(
        &self,
        auth: &Auth,
        call: HttpCall,
    ) -> Result<HttpReply, ExecError> {
        let HttpCall {
            method,
            target,
            mut headers,
            body,
            client_headers,
            response_limit,
            observation,
        } = call;
        let url = self.target_url(target);
        refuse_control_characters(&url)?;
        let (token, _) = credentials(auth);
        if token.trim().is_empty() {
            headers.remove(header::AUTHORIZATION);
        } else {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                ExecError::new(
                    ErrorKind::Upstream,
                    "codex executor: the credential's token isn't a valid header value",
                )
            })?;
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
        }
        custom_headers::apply(&mut headers, &auth.attributes, &client_headers, "codex");
        if !headers.contains_key(header::USER_AGENT) {
            headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
        }
        // Upstream's Alpha Search handler records the request as it is sent,
        // with no model.
        let attempt = Attempt {
            observation: observation
                .as_ref()
                .filter(|observation| observation.is_tapped()),
            kind: AttemptKind::Http,
            provider: "codex",
            model: "",
            format: &Format::CODEX,
            auth,
        };
        let secrets = observe_send::secrets(&url, &headers, &self.proxy_for(auth), auth);
        let tap = attempt.observation.map(|observation| {
            observe_send::announce(
                observation,
                &attempt.request(&method, &url, &headers, &body, &secrets),
            )
        });
        let mut response = self
            .clients
            .get(&auth.proxy_url)
            .request(method, url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                ExecError::new(
                    ErrorKind::Upstream,
                    secrets.text(error_chain(&error.without_url()), Policy::Client),
                )
            })?;
        observe_send::response(tap, &mut response);
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let tap = BodyTap::of(&response);
        let (mut body, read_error) = read_body_prefix(response, response_limit).await;
        let read_error = read_error.map(|error| error_chain(&error));
        if let Some(error) = &read_error {
            observe_send::attempt_error(tap.as_ref(), error);
        }
        if let Cow::Owned(scrubbed) = secrets.bytes(&body, Policy::Client) {
            body = scrubbed;
        }
        Ok(HttpReply {
            status,
            headers,
            body: Bytes::from(body),
            read_error,
        })
    }

    /// Where `target` is: a URL as it is, a path under the base URL.
    fn target_url(&self, target: HttpTarget) -> String {
        match target {
            HttpTarget::Url(url) => url,
            HttpTarget::Path(path) => format!("{}{path}", self.base_url.trim_end_matches('/')),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Mostly not upstream's: upstream tests `HttpRequest` only through the
    //! Alpha Search handler, with a fake executor. These send to a mock on
    //! 127.0.0.1, and never to ChatGPT.

    use std::sync::{Arc, Mutex, PoisonError};

    use axum::Router;
    use axum::http::Uri;
    use http::{HeaderMap, Method};

    use super::*;
    use crate::codex::request::CONTROL_CHARACTER;

    /// The API key of the credentials that quote it.
    const TOKEN: &str = "alpha-token-0123456789";
    /// A search's results that quote the token, as a page can.
    const RESULTS_QUOTING_TOKEN: &str =
        r#"{"results":[{"snippet":"the key is alpha-token-0123456789"}]}"#;
    /// A body that answers a success with an error object that quotes the
    /// token.
    const ERROR_QUOTING_TOKEN: &str =
        r#"{"error":{"message":"bad key alpha-token-0123456789","type":"auth"}}"#;

    /// One request the mock received.
    #[derive(Clone, Debug)]
    struct Seen {
        method: Method,
        path: String,
        headers: HeaderMap,
        body: Bytes,
    }

    /// A mock that answers every request with `status`, a JSON content type
    /// and `body`. Returns its URL and what it received.
    async fn serve(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, received: Bytes| {
                let recorder = Arc::clone(&recorder);
                async move {
                    recorder
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(Seen {
                            method,
                            path: uri.path().to_owned(),
                            headers,
                            body: received,
                        });
                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .header("x-upstream", "yes")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                }
            },
        );
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

    fn call(target: HttpTarget, body: &'static str) -> HttpCall {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        HttpCall {
            method: Method::POST,
            target,
            headers,
            body: Bytes::from_static(body.as_bytes()),
            client_headers: HeaderMap::new(),
            response_limit: 1 << 20,
            observation: None,
        }
    }

    // Ports the URL check of TestCodexAlphaSearchForwardsRequest
    // (internal/api/server_test.go), without calling ChatGPT.
    #[test]
    fn a_path_goes_under_chatgpts_codex_api_by_default() {
        let executor = CodexExecutor::new("direct");
        assert_eq!(
            executor.target_url(HttpTarget::Path("/alpha/search".into())),
            "https://chatgpt.com/backend-api/codex/alpha/search"
        );
        let url = "https://codex.example.com/v1/alpha/search";
        assert_eq!(executor.target_url(HttpTarget::Url(url.into())), url);
    }

    #[tokio::test]
    async fn sends_a_path_under_the_base_url_with_the_oauth_token() {
        let (url, seen) = serve(200, r#"{"results":[]}"#).await;
        let executor =
            CodexExecutor::new("direct").with_base_url(format!("{url}/backend-api/codex/"));
        let mut auth = Auth::default();
        auth.metadata
            .insert("access_token".into(), "oauth-token".into());
        let reply = executor
            .http_request_inner(
                &auth,
                call(HttpTarget::Path("/alpha/search".into()), r#"{"query":"x"}"#),
            )
            .await
            .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.headers["x-upstream"], "yes");
        assert_eq!(reply.body, r#"{"results":[]}"#);
        assert!(reply.read_error.is_none());
        let request = last(&seen);
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.path, "/backend-api/codex/alpha/search");
        assert_eq!(request.headers["authorization"], "Bearer oauth-token");
        assert_eq!(request.headers["content-type"], "application/json");
        assert_eq!(request.headers["user-agent"], USER_AGENT);
        assert_eq!(request.body, r#"{"query":"x"}"#);
    }

    #[tokio::test]
    async fn sends_a_url_with_the_api_key_and_custom_headers() {
        let (url, seen) = serve(429, r#"{"error":"slow down"}"#).await;
        let executor = CodexExecutor::new("direct");
        let mut auth = Auth::default();
        for (key, value) in [
            ("api_key", "sk-alpha"),
            ("header:X-Tenant", "$X-Tenant"),
            ("header:Originator", "codex_cli_rs"),
        ] {
            auth.attributes.insert(key.into(), value.into());
        }
        let mut search = call(HttpTarget::Url(format!("{url}/v1/alpha/search")), "{}");
        search
            .headers
            .insert("user-agent", HeaderValue::from_static("my-client/1"));
        search
            .client_headers
            .insert("x-tenant", HeaderValue::from_static("blue"));
        let reply = executor.http_request_inner(&auth, search).await.unwrap();
        assert_eq!(reply.status, 429);
        assert_eq!(reply.body, r#"{"error":"slow down"}"#);
        let request = last(&seen);
        assert_eq!(request.path, "/v1/alpha/search");
        assert_eq!(request.headers["authorization"], "Bearer sk-alpha");
        assert_eq!(request.headers["x-tenant"], "blue");
        assert_eq!(request.headers["user-agent"], "my-client/1");
        assert!(request.headers.get("originator").is_none());
    }

    #[tokio::test]
    async fn drops_authorization_without_a_token_and_truncates_the_body() {
        let (url, seen) = serve(200, "0123456789").await;
        let executor = CodexExecutor::new("direct");
        let mut search = call(HttpTarget::Url(format!("{url}/alpha/search")), "{}");
        search
            .headers
            .insert("authorization", HeaderValue::from_static("Bearer stale"));
        search.response_limit = 4;
        let reply = executor
            .http_request_inner(&Auth::default(), search)
            .await
            .unwrap();
        assert_eq!(reply.body, "0123");
        assert!(reply.read_error.is_none());
        assert!(last(&seen).headers.get("authorization").is_none());
    }

    // Not upstream's: Go's url.Parse refuses a control character before
    // anything is sent (Go 1.26.4 answers `parse "<url>": net/url: invalid
    // control character in URL`), where the WHATWG parser would drop a
    // newline or tab and encode the rest.
    #[tokio::test]
    async fn a_url_with_a_control_character_is_refused_unsent() {
        let (url, seen) = serve(200, "{}").await;
        let executor = CodexExecutor::new("direct").with_base_url(format!("{url}/v1\t"));
        for target in [
            HttpTarget::Url(format!("{url}/v1\n/x/alpha/search?key=secret")),
            HttpTarget::Url(format!("{url}/v1\u{7f}/alpha/search")),
            HttpTarget::Url(format!("{url}/v1\u{1}/alpha/search")),
            HttpTarget::Path("/alpha/search".into()),
        ] {
            let err = executor
                .http_request_inner(&Auth::default(), call(target, "{}"))
                .await
                .unwrap_err();
            assert_eq!(err.message, CONTROL_CHARACTER);
            assert_eq!(err.http_status(), 0);
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    // Not upstream's: pins a deviation. Go sends `/v1/%2e%2e/alpha/search`
    // and `/v1%5Cx/alpha/search` as written (Go 1.26.4); the WHATWG parser
    // resolves the encoded dot segment and reads `\` as `/`.
    #[tokio::test]
    async fn a_url_is_read_as_a_whatwg_url() {
        let (url, seen) = serve(200, "{}").await;
        let executor = CodexExecutor::new("direct");
        for (sent, path) in [
            (format!("{url}/v1/%2e%2e/alpha/search"), "/alpha/search"),
            (format!("{url}/v1/%2E%2e/alpha/search"), "/alpha/search"),
            (format!("{url}/v1\\x/alpha/search"), "/v1/x/alpha/search"),
        ] {
            executor
                .http_request_inner(&Auth::default(), call(HttpTarget::Url(sent), "{}"))
                .await
                .unwrap();
            assert_eq!(last(&seen).path, path);
        }
    }

    #[tokio::test]
    async fn a_connection_failure_is_an_error_without_the_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/alpha/search?key=secret",
            listener.local_addr().unwrap()
        );
        drop(listener);
        let err = CodexExecutor::new("direct")
            .http_request_inner(&Auth::default(), call(HttpTarget::Url(url), "{}"))
            .await
            .unwrap_err();
        assert_eq!(err.http_status(), 0);
        assert!(!err.message.contains("secret"), "{}", err.message);
    }

    // Not upstream's: a failure's body, which the handler hands on, quotes
    // none of the secrets the request sent (the token, the credential
    // headers after the custom ones, each cookie, the URL's credentials),
    // nor the password of a proxy that answers 407.
    #[tokio::test]
    async fn a_failures_body_hides_every_secret_sent() {
        let api_key = |base_url: &str| {
            let mut auth = Auth::default();
            auth.attributes.insert("base_url".into(), base_url.into());
            auth.attributes.insert("api_key".into(), TOKEN.into());
            auth
        };
        for case in crate::secret_echo::cases(api_key).await {
            let base_url = case.auth.attribute("base_url").unwrap_or_default();
            let mut call = call(HttpTarget::Url(format!("{base_url}/alpha/search")), "{}");
            call.client_headers = case.headers.clone();
            let reply = CodexExecutor::new("direct")
                .http_request_inner(&case.auth, call)
                .await
                .unwrap();
            assert!(reply.status == 401 || reply.status == 407, "{reply:?}");
            let body = String::from_utf8_lossy(&reply.body);
            assert!(!body.contains(TOKEN), "{body}");
            case.check_text(&body);
        }
    }

    // Not upstream's: an answer that succeeds hides the secrets the request
    // sent as well: a page that quotes the token, and a 200 with an `error`
    // object that does, reach the handler without it, while the call's taps
    // read the body as it came.
    #[tokio::test]
    async fn a_successful_body_hides_every_secret_sent() {
        for (name, body) in [
            ("results", RESULTS_QUOTING_TOKEN),
            ("an error object", ERROR_QUOTING_TOKEN),
        ] {
            let (url, _) = serve(200, body).await;
            let mut auth = Auth::default();
            auth.attributes.insert("api_key".into(), TOKEN.into());
            let (observation, raw) = crate::secret_echo::Raw::observe();
            let mut search = call(HttpTarget::Url(format!("{url}/alpha/search")), "{}");
            search.observation = Some(observation);
            let reply = CodexExecutor::new("direct")
                .http_request_inner(&auth, search)
                .await
                .unwrap();
            assert_eq!(reply.status, 200);
            let shown = String::from_utf8_lossy(&reply.body);
            assert!(!shown.contains(TOKEN), "{name}: {shown}");
            assert!(shown.contains("[redacted]"), "{name}: {shown}");
            assert_eq!(raw.seen(), body, "{name}: the taps read it as it came");
        }
    }
}
