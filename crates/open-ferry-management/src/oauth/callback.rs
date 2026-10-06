// Ported from CLIProxyAPI internal/api/handlers/management/oauth_callback.go
// (oauthCallbackRequest, PostOAuthCallback, GetOAuthCallback,
// handleOAuthCallback, firstNonEmpty) and internal/api/server_routes.go
// (the /anthropic/callback and /codex/callback handlers) (v8.0.15, MIT).
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
//! A callback page answers with the sign-in page
//! ([`open_ferry_providers::oauth::page`]), which says how the login ended
//! and sends the user back to the dashboard:
//! - Without a state, or with one no pending login of its provider has, it
//!   says so. If that login has already ended, it says how instead, as for
//!   a page loaded again, but not why a failed login failed.
//! - With the provider's error, it says the provider didn't sign the user
//!   in, naming the error only when RFC 6749 defines it.
//! - With a code, it waits up to 30 seconds for the login to exchange it
//!   and save the credential. Then it says the user is signed in, or that
//!   the login failed and why (the session's status, without the code or
//!   the PKCE verifier), or that it stopped, or that it is still finishing
//!   and the dashboard shows the result. Only the callback whose code the
//!   login took is told why the login failed.
//!
//! A page reporting a failure answers 400 Bad Request; one reporting a
//! sign-in, or one still finishing, answers 200 OK.
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
//! - The callback pages answer with open-ferry's sign-in page, which says
//!   how the login ended, once it has. Upstream's answer at once with
//!   `oauthCallbackSuccessHTML`, whatever the callback held: a page saying
//!   the login succeeded and that the window closes in 5 seconds. See
//!   [`open_ferry_providers::oauth::page`] for how the pages differ.
//! - A callback page reporting a failure answers 400; upstream's answer 200
//!   whatever happened.

use std::fmt;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::response::Response;
use http::StatusCode;
use open_ferry_core::observe::redact::{Policy, Secrets};
use open_ferry_providers::oauth::page::{self as sign_in, Failure, Origin, Outcome};
use open_ferry_translate::go::trim_space;
use serde::de::MapAccess;

use super::sessions::{Callback, Ended, NotPending, is_valid_state, normalize_callback_provider};
use super::{Provider, Redacted};
use crate::bind::{self, GoStruct, set_string};
use crate::go::{equal_fold, lossy};
use crate::json::{self, Json};
use crate::query::Query;
use crate::state::ManagementState;

/// How long a callback page waits for its login to exchange the code and
/// save the credential before saying the login is still finishing. An
/// exchange takes a second or two as a rule, but may take up to a minute;
/// the dashboard shows the result either way. This is half the minute a
/// reverse proxy in front of the server commonly waits for an answer.
pub(super) const PAGE_WAIT: Duration = Duration::from_secs(30);

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
    page(&state, Provider::Claude, query.as_deref()).await
}

/// `GET /codex/callback` on the main server.
pub(super) async fn codex_page(
    State(state): State<ManagementState>,
    RawQuery(query): RawQuery,
) -> Response {
    page(&state, Provider::Codex, query.as_deref()).await
}

/// Hands the callback in `query` to the pending `provider` login it names,
/// if there is one, and answers with the sign-in page saying how the login
/// ended.
async fn page(state: &ManagementState, provider: Provider, query: Option<&str>) -> Response {
    let outcome = page_outcome(state, provider, query).await;
    sign_in::response(&provider.to_string(), Origin::Dashboard, &outcome)
}

/// What the callback page for `query` says: how the `provider` login it
/// names ended, once it has, waiting up to [`PAGE_WAIT`] after handing it
/// a code.
async fn page_outcome(state: &ManagementState, provider: Provider, query: Option<&str>) -> Outcome {
    let query = Query::parse(query);
    let oauth_state = query.value("state");
    if oauth_state.is_empty() {
        return Outcome::Failed(Failure::new(
            "This page isn't from a sign-in",
            "Its address doesn't say which sign-in it belongs to, so open-ferry ignored it.",
        ));
    }
    let oauth_state = lossy(oauth_state);
    // Upstream writes the callback under the state as given, where the
    // login looks for it under the trimmed state: a state with spaces
    // around it never reaches the login.
    if !is_valid_state(&oauth_state) || oauth_state.trim() != oauth_state {
        return Outcome::Failed(nothing_waiting());
    }
    let error = match query.value("error") {
        b"" => query.value("error_description"),
        error => error,
    };
    let callback = Callback {
        code: text(query.value("code")),
        error: text(error),
    };
    let (code, error) = (callback.code.clone(), callback.error.clone());
    let sessions = state.oauth_sessions();
    let store = sessions.store();
    let delivered = match store.deliver(&oauth_state, provider.name(), callback) {
        Ok(delivered) => delivered,
        // The login may have ended: the page may have been loaded again.
        Err(NotPending) => {
            return match store.get(&oauth_state) {
                Some(session) if equal_fold(&session.provider, provider.name()) => {
                    if session.completed {
                        Outcome::SignedIn
                    } else if session.status.is_empty() {
                        Outcome::Failed(nothing_waiting())
                    } else {
                        Outcome::Failed(already_failed())
                    }
                }
                _ => Outcome::Failed(nothing_waiting()),
            };
        }
    };
    let name = provider.to_string();
    let taken = delivered.taken;
    if taken && !error.is_empty() {
        return Outcome::Failed(Failure::provider_error(&name, &error));
    }
    match store
        .ended(&oauth_state, delivered, sessions.page_wait())
        .await
    {
        Some(Ended::Completed) => Outcome::SignedIn,
        // The status is already without the code and the PKCE verifier as
        // they were sent; this hides the code as JSON escapes it too.
        Some(Ended::Failed(status)) if taken => {
            let secrets: Secrets = [code.as_str()].into_iter().collect();
            let reason = secrets.text(status, Policy::Client);
            Outcome::Failed(Failure::unfinished(&name, &reason))
        }
        Some(Ended::Failed(_)) => Outcome::Failed(already_failed()),
        Some(Ended::Dropped) => Outcome::Failed(Failure::stopped()),
        None => Outcome::Finishing,
    }
}

/// No pending login has the page's state.
fn nothing_waiting() -> Failure {
    Failure::new(
        "No sign-in is waiting for this page",
        "The sign-in may have expired or been cancelled, or a newer one may have replaced it, \
         so open-ferry ignored this page.",
    )
}

/// The page's login failed, and the page didn't bring the code it failed
/// with.
fn already_failed() -> Failure {
    Failure::new(
        "This sign-in has already failed",
        "open-ferry couldn't finish it, so nothing was saved. The dashboard shows why.",
    )
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
