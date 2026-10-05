// Ported from CLIProxyAPI internal/api/handlers/management/oauth_callback.go
// (oauthCallbackRequest, PostOAuthCallback, GetOAuthCallback,
// handleOAuthCallback, firstNonEmpty) and internal/api/server_routes.go
// (oauthCallbackSuccessHTML, the /anthropic/callback and /codex/callback
// handlers) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The routes that take a login's callback, which need no key: the
//! management API's `oauth-callback`, which a client calls with what the
//! provider's redirect carried, and the main server's callback pages, which
//! the browser reaches through a callback forwarder.
//!
//! Each hands the code, or the error the provider reported, to the login
//! waiting on the pending session for the callback's `state`. A state that
//! isn't 1 to 128 letters, digits, `-`, `_` and `.` is never looked up.
//!
//! Deviations from upstream:
//! - A callback is never written to a file, so `oauth-callback` never
//!   answers 500 `failed to persist oauth callback`.
//! - Only Claude and Codex logins are served, so a callback naming another
//!   provider, a plugin's included, is answered `provider does not match
//!   state` or `unsupported provider`; there are no plugin sessions.
//! - A code that isn't UTF-8 once decoded from the query is read with each
//!   bad byte as U+FFFD; Go keeps its bytes.
//! - A callback for a failed login is answered 409 `{"error":"oauth flow
//!   failed","status":"error"}`. Upstream answers with the session's status,
//!   which may quote the token endpoint's answer, to anyone, as this route
//!   needs no key; the key-protected `get-auth-status` still answers it.

use std::fmt;

use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use open_ferry_translate::go::trim_space;
use serde::de::MapAccess;

use super::sessions::{Callback, is_valid_state, normalize_callback_provider};
use super::{Provider, Redacted};
use crate::bind::{self, GoStruct, set_string};
use crate::go::{equal_fold, lossy};
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;

/// The page the main server's callback routes answer with
/// (`oauthCallbackSuccessHTML`).
const SUCCESS_PAGE: &str = "<html><head><meta charset=\"utf-8\"><title>Authentication \
    successful</title><script>setTimeout(function(){window.close();},5000);</script></head>\
    <body><h1>Authentication successful!</h1><p>You can close this window.</p><p>This window \
    will close automatically in 5 seconds.</p></body></html>";

/// What `oauth-callback` answers for a login that failed.
const FLOW_FAILED: &str = "oauth flow failed";

/// What a callback carries (`oauthCallbackRequest`). Its `Debug` hides all
/// but the provider: the redirect's URL holds the code, and the error may
/// quote it.
#[derive(Default)]
struct CallbackRequest {
    provider: String,
    redirect_url: String,
    code: String,
    state: String,
    error: String,
}

impl fmt::Debug for CallbackRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallbackRequest")
            .field("provider", &self.provider)
            .field("redirect_url", &Redacted(&self.redirect_url))
            .field("code", &Redacted(&self.code))
            .field("state", &Redacted(&self.state))
            .field("error", &Redacted(&self.error))
            .finish()
    }
}

impl GoStruct for CallbackRequest {
    const FIELDS: &'static [&'static str] = &["provider", "redirect_url", "code", "state", "error"];

    fn set<'de, A: MapAccess<'de>>(&mut self, index: usize, map: &mut A) -> Result<(), A::Error> {
        let field = match index {
            0 => &mut self.provider,
            1 => &mut self.redirect_url,
            2 => &mut self.code,
            3 => &mut self.state,
            _ => &mut self.error,
        };
        set_string(field, map)
    }
}

/// `POST /v0/management/oauth-callback` (`PostOAuthCallback`): the
/// callback as a JSON body, which may give the redirect's URL instead.
pub(super) async fn post(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match bind::read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(request) = bind::decode::<CallbackRequest>(&body) else {
        return failure(StatusCode::BAD_REQUEST, "invalid body");
    };
    handle(&state, request)
}

/// `GET /v0/management/oauth-callback` (`GetOAuthCallback`): the callback
/// in the query.
pub(super) async fn get(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
) -> Response {
    let query = Query::parse(query.as_deref());
    let request = CallbackRequest {
        provider: text(query.value("provider")),
        code: text(query.value("code")),
        state: text(query.value("state")),
        error: first_non_empty(&[query.value("error"), query.value("error_description")]),
        redirect_url: String::new(),
    };
    handle(&state, request)
}

/// Hands a callback to its login (`handleOAuthCallback`).
fn handle(state: &ManagementState, request: CallbackRequest) -> Response {
    let mut oauth_state = request.state.trim().to_owned();
    let mut code = request.code.trim().to_owned();
    let mut error = request.error.trim().to_owned();

    let redirect = request.redirect_url.trim();
    if !redirect.is_empty() {
        let Some(query) = redirect_query(redirect) else {
            return failure(StatusCode::BAD_REQUEST, "invalid redirect_url");
        };
        if oauth_state.is_empty() {
            oauth_state = text(query.value("state"));
        }
        if code.is_empty() {
            code = text(query.value("code"));
        }
        if error.is_empty() {
            error = first_non_empty(&[query.value("error"), query.value("error_description")]);
        }
    }

    if oauth_state.is_empty() {
        return failure(StatusCode::BAD_REQUEST, "state is required");
    }
    if !is_valid_state(&oauth_state) {
        return failure(StatusCode::BAD_REQUEST, "invalid state");
    }
    if code.is_empty() && error.is_empty() {
        return failure(StatusCode::BAD_REQUEST, "code or error is required");
    }

    let store = state.oauth_sessions().store();
    let Some(session) = store.get(&oauth_state) else {
        return failure(StatusCode::NOT_FOUND, "unknown or expired state");
    };
    if session.completed {
        return failure(StatusCode::CONFLICT, "oauth flow is already completed");
    }
    let provider = match request.provider.trim() {
        "" => session.provider.as_str(),
        provider => provider,
    };
    let Some(provider) = normalize_callback_provider(provider) else {
        return failure(StatusCode::BAD_REQUEST, "unsupported provider");
    };
    // The status may quote the token endpoint's answer: only a key reads
    // it.
    if !session.status.is_empty() {
        return failure(StatusCode::CONFLICT, FLOW_FAILED);
    }
    if !equal_fold(&session.provider, &provider) {
        return failure(StatusCode::BAD_REQUEST, "provider does not match state");
    }

    if store
        .deliver(&oauth_state, &provider, Callback { code, error })
        .is_err()
    {
        return match store.active(&oauth_state) {
            Some(session) if !session.status.is_empty() => {
                failure(StatusCode::CONFLICT, FLOW_FAILED)
            }
            _ => failure(StatusCode::CONFLICT, "oauth flow is not pending"),
        };
    }
    json::response(
        StatusCode::OK,
        &Json::map([("status", Json::Str("ok".into()))]),
    )
}

/// The query of `url`, or `None` where Go's `url.Parse` fails.
fn redirect_query(url: &str) -> Option<Query> {
    crate::go_url::parse(url.as_bytes())?;
    let url = url.split_once('#').map_or(url, |(url, _)| url);
    Some(Query::parse(url.split_once('?').map(|(_, query)| query)))
}

/// `GET /anthropic/callback` on the main server.
pub(super) async fn anthropic_page(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
) -> Response {
    page(&state, Provider::Claude, query.as_deref())
}

/// `GET /codex/callback` on the main server.
pub(super) async fn codex_page(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
) -> Response {
    page(&state, Provider::Codex, query.as_deref())
}

/// Hands the callback in `query` to the pending `provider` login it names,
/// if there is one, and answers with the success page whatever happened.
fn page(state: &ManagementState, provider: Provider, query: Option<&str>) -> Response {
    let query = Query::parse(query);
    let oauth_state = query.value("state");
    if !oauth_state.is_empty() {
        let oauth_state = lossy(oauth_state);
        let error = match query.value("error") {
            b"" => query.value("error_description"),
            error => error,
        };
        // Upstream writes the callback under the state as given, where the
        // login looks for it under the trimmed state: a state with spaces
        // around it never reaches the login.
        if is_valid_state(&oauth_state) && oauth_state.trim() == oauth_state {
            let callback = Callback {
                code: text(query.value("code")),
                error: text(error),
            };
            let _ = state
                .oauth_sessions()
                .store()
                .deliver(&oauth_state, provider.name(), callback);
        }
    }
    let mut response = (StatusCode::OK, SUCCESS_PAGE).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

/// `{"error":message,"status":"error"}` with `status`.
fn failure(status: StatusCode, message: &str) -> Response {
    json::response(
        status,
        &Json::map([
            ("status", Json::Str("error".into())),
            ("error", Json::Str(message.to_owned())),
        ]),
    )
}

/// A query value, trimmed, as text.
fn text(value: &[u8]) -> String {
    lossy(trim_space(value))
}

/// The first of `values` that isn't empty once trimmed, trimmed
/// (`firstNonEmpty`).
fn first_non_empty(values: &[&[u8]]) -> String {
    values
        .iter()
        .map(|value| text(value))
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not upstream's: the query a redirect URL gives, as Go's `url.Parse`
    /// and `URL.Query` read it.
    #[test]
    fn redirect_query_reads_as_go() {
        let state = |url: &str| redirect_query(url).map(|q| text(q.value("state")));
        assert_eq!(state("http://h/cb?state=a&code=b").as_deref(), Some("a"));
        assert_eq!(state("http://h/cb?code=b#state=a").as_deref(), Some(""));
        assert_eq!(state("http://h/cb#x?state=a").as_deref(), Some(""));
        assert_eq!(state("/cb?state=a%2Eb").as_deref(), Some("a.b"));
        assert_eq!(state("cb?state=1&state=2").as_deref(), Some("1"));
        assert_eq!(state("http://h/cb?").as_deref(), Some(""));
        assert_eq!(state("http://[::1/cb?state=a"), None);
        assert_eq!(state("http://h/cb?state=a\x7f"), None);
        assert_eq!(state("http://h/cb?state=a#%zz"), None);
    }

    /// Not upstream's: Go's decode of these bodies into the callback request.
    #[test]
    fn bodies_decode_as_go_decodes_them() {
        assert!(bind::decode::<CallbackRequest>(b"").is_none());
        assert!(bind::decode::<CallbackRequest>(b"[]").is_none());
        assert!(bind::decode::<CallbackRequest>(br#"{"code":1}"#).is_none());
        let request = bind::decode::<CallbackRequest>(
            br#"{"Provider":"codex","REDIRECT_URL":"u","code":"c","state":"s","error":null}"#,
        )
        .unwrap();
        assert_eq!(
            (
                request.provider.as_str(),
                request.redirect_url.as_str(),
                request.code.as_str(),
                request.state.as_str(),
                request.error.as_str(),
            ),
            ("codex", "u", "c", "s", "")
        );
    }

    /// Not upstream's: a callback's `Debug` shows its provider and nothing
    /// that may hold the code.
    #[test]
    fn debug_hides_the_callback() {
        let request = CallbackRequest {
            provider: "codex".into(),
            redirect_url: "http://h/cb?code=MARKER-code".into(),
            code: "MARKER-code".into(),
            state: "MARKER-state".into(),
            error: "MARKER-error".into(),
        };
        let shown = format!("{request:?}");
        assert!(!shown.contains("MARKER"), "{shown}");
        assert_eq!(
            shown,
            "CallbackRequest { provider: \"codex\", redirect_url: \"[redacted]\", \
             code: \"[redacted]\", state: \"[redacted]\", error: \"[redacted]\" }"
        );
        assert!(format!("{:?}", CallbackRequest::default()).contains("code: \"\""));
    }
}
