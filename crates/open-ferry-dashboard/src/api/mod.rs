//! The dashboard API, under `/open-ferry/api/v1/`, as
//! `docs/dashboard-api.md` describes it: usage from the ledger, the request
//! logs, client setup, and the config's `claude-cli` entries, with their
//! state and whether each is signed in.
//!
//! Every route checks access as the management API does, with its code
//! ([`check_key`]): a key must be set, the request must carry it, from an
//! address allowed to, and failed attempts count toward the same ban. The
//! refusals are the contract's errors rather than the management API's.
//! A path under `/open-ferry/` that isn't a route answers `not_found`, and
//! a method a route doesn't serve `method_not_allowed`, before any check.
//! On the proxy's listener while `management.separate-address` is set
//! ([`Listener::Closed`](crate::Listener::Closed)), the API has no routes:
//! every path under `/open-ferry/` answers `not_found`, whatever the
//! method, without a check.
//!
//! Every answer is JSON with `Cache-Control: no-store`, but a log's
//! download, which is the log's bytes, also not to be stored.

mod claude_cli;
mod client_setup;
mod request_logs;
mod usage;

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any, get};
use chrono::{DateTime, Duration, Utc};
use http::{HeaderValue, StatusCode, Uri, header};
use open_ferry_management::{Refusal, check_key};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::ledger::LedgerError;
use crate::{DashboardState, Listener};

/// The API's prefix.
pub(crate) const PREFIX: &str = "/open-ferry/api/v1";

/// The largest body a route takes.
const MAX_BODY: usize = 64 * 1024;

/// The longest query a route takes, in bytes.
const MAX_QUERY: usize = 8 * 1024;

/// The API's routes, and its answer to the other paths under
/// `/open-ferry/`.
pub(crate) fn routes(state: &DashboardState) -> Router<DashboardState> {
    let other_paths = Router::new()
        .route("/open-ferry", any(not_found))
        .route("/open-ferry/", any(not_found))
        .route("/open-ferry/{*rest}", any(not_found));
    if state.listener == Listener::Closed {
        // The API isn't served here: its paths are like any other.
        return other_paths.layer(middleware::map_response(no_store));
    }
    // The key is checked on the methods a route serves; the others are
    // answered by the fallback, without a check.
    let route = |handler: MethodRouter<DashboardState>| {
        handler
            .route_layer(middleware::from_fn_with_state(state.clone(), guard))
            .fallback(method_not_allowed)
    };
    let api = Router::new()
        .route(
            &format!("{PREFIX}/usage/summary"),
            route(get(usage::summary)),
        )
        .route(&format!("{PREFIX}/usage/series"), route(get(usage::series)))
        .route(
            &format!("{PREFIX}/usage/requests"),
            route(get(usage::requests)),
        )
        .route(
            &format!("{PREFIX}/usage/ledger"),
            route(get(usage::ledger).patch(usage::update_ledger)),
        )
        .route(
            &format!("{PREFIX}/usage/records"),
            route(axum::routing::delete(usage::delete_records)),
        )
        .route(
            &format!("{PREFIX}/usage/prices"),
            route(
                get(usage::prices)
                    .put(usage::set_price)
                    .delete(usage::delete_price),
            ),
        )
        .route(
            &format!("{PREFIX}/request-logs"),
            route(get(request_logs::search)),
        )
        .route(
            &format!("{PREFIX}/request-logs/{{name}}"),
            route(get(request_logs::read)),
        )
        .route(
            &format!("{PREFIX}/request-logs/{{name}}/download"),
            route(get(request_logs::download)),
        )
        .route(
            &format!("{PREFIX}/client-setup"),
            route(get(client_setup::client_setup)),
        )
        .route(
            &format!("{PREFIX}/claude-cli/entries"),
            route(get(claude_cli::entries_route)),
        )
        .route(
            &format!("{PREFIX}/claude-cli/auth-status"),
            route(get(claude_cli::auth_status_route)),
        );
    api.merge(other_paths)
        .layer(middleware::map_response(no_store))
}

/// Checks the management key as the management API does, and answers a
/// refusal with the contract's error.
async fn guard(State(state): State<DashboardState>, request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    match check_key(&state.management, peer, request.headers()).await {
        Ok(()) => next.run(request).await,
        Err(refusal) => refused(&refusal).into_response(),
    }
}

/// The contract's error for `refusal`.
fn refused(refusal: &Refusal) -> ApiError {
    let message = refusal.message();
    match refusal {
        Refusal::Unavailable | Refusal::KeyNotSet => ApiError::new(
            StatusCode::NOT_FOUND,
            "management_disabled",
            "no management key is set; set management.secret-key or MANAGEMENT_PASSWORD",
        ),
        Refusal::Banned(_) => ApiError::new(StatusCode::FORBIDDEN, "ip_banned", message),
        Refusal::RemoteDisabled => {
            ApiError::new(StatusCode::FORBIDDEN, "remote_management_disabled", message)
        }
        Refusal::MissingKey => {
            ApiError::new(StatusCode::UNAUTHORIZED, "missing_management_key", message)
        }
        Refusal::InvalidKey => {
            ApiError::new(StatusCode::UNAUTHORIZED, "invalid_management_key", message)
        }
    }
}

/// Marks an answer as not to be stored.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// A path under `/open-ferry/` that isn't a route.
async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
}

/// A method a route doesn't serve.
async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "the route doesn't take this method",
    )
}

/// An error answer: a status, a code from the contract and a message.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// A `400 invalid_request`.
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    /// A `500 internal_error`.
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }
}

impl From<LedgerError> for ApiError {
    fn from(error: LedgerError) -> Self {
        match error {
            LedgerError::Unavailable(reason) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "ledger_unavailable",
                format!("the usage ledger is unavailable: {reason}"),
            ),
            LedgerError::Failed(reason) => Self::internal(format!("the usage ledger: {reason}")),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body<'a> {
            error: &'a str,
            message: &'a str,
        }
        json(
            self.status,
            &Body {
                error: self.code,
                message: &self.message,
            },
        )
    }
}

/// `value` as a JSON answer with `status`.
pub(crate) fn json<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (
            status,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            )],
            body,
        )
            .into_response(),
        Err(error) => {
            tracing::warn!("dashboard API: write the answer: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json; charset=utf-8"),
                )],
                r#"{"error":"internal_error","message":"the answer couldn't be written"}"#,
            )
                .into_response()
        }
    }
}

/// `value` as a `200` JSON answer.
pub(crate) fn ok<T: Serialize>(value: &T) -> Response {
    json(StatusCode::OK, value)
}

/// The body of `request` as a `T`: JSON, an object, of at most
/// [`MAX_BODY`] bytes.
pub(crate) async fn read_body<T: DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    let bytes = axum::body::to_bytes(body, MAX_BODY).await.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "the body is over 64 KiB",
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            format!("the body isn't JSON: {error}"),
        )
    })?;
    if !value.is_object() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "the body must be a JSON object",
        ));
    }
    serde_json::from_value(value).map_err(|error| ApiError::invalid(error.to_string()))
}

/// A request's query parameters.
#[derive(Debug, Default)]
pub(crate) struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    /// The query of `uri`.
    pub(crate) fn of(uri: &Uri) -> Result<Self, ApiError> {
        let query = uri.query().unwrap_or_default();
        if query.len() > MAX_QUERY {
            return Err(ApiError::invalid("the query is too long"));
        }
        Ok(Self {
            pairs: url::form_urlencoded::parse(query.as_bytes())
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect(),
        })
    }

    /// The parameter `name`, if given; given twice, it is refused.
    pub(crate) fn get(&self, name: &str) -> Result<Option<&str>, ApiError> {
        let mut values = self
            .pairs
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str());
        let first = values.next();
        if values.next().is_some() {
            return Err(ApiError::invalid(format!("{name} is given more than once")));
        }
        Ok(first)
    }

    /// The parameter `name`, if given and not empty.
    pub(crate) fn non_empty(&self, name: &str) -> Result<Option<&str>, ApiError> {
        Ok(self.get(name)?.filter(|value| !value.is_empty()))
    }

    /// The whole number `name`, from `min` to `max`, else `default`.
    pub(crate) fn int(
        &self,
        name: &str,
        min: i64,
        max: i64,
        default: i64,
    ) -> Result<i64, ApiError> {
        let Some(text) = self.get(name)? else {
            return Ok(default);
        };
        match text.parse::<i64>() {
            Ok(value) if (min..=max).contains(&value) => Ok(value),
            _ => Err(ApiError::invalid(format!(
                "{name} must be a whole number from {min} to {max}"
            ))),
        }
    }

    /// The count `name`, from `min` to `max`, else `default`.
    pub(crate) fn count(
        &self,
        name: &str,
        min: usize,
        max: usize,
        default: usize,
    ) -> Result<usize, ApiError> {
        let to_i64 = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
        let value = self.int(name, to_i64(min), to_i64(max), to_i64(default))?;
        Ok(usize::try_from(value).unwrap_or(default))
    }

    /// The time `name`, if given, as milliseconds since the epoch.
    pub(crate) fn time(&self, name: &str) -> Result<Option<i64>, ApiError> {
        let Some(text) = self.non_empty(name)? else {
            return Ok(None);
        };
        DateTime::parse_from_rfc3339(text)
            .map(|time| Some(time.timestamp_millis()))
            .map_err(|_| {
                ApiError::invalid(format!(
                    "{name} must be an RFC 3339 time, such as 2026-10-05T12:00:00Z"
                ))
            })
    }

    /// The range of `from` and `to`: to now and from a day before when not
    /// given.
    pub(crate) fn range(&self) -> Result<(i64, i64), ApiError> {
        let to = match self.time("to")? {
            Some(to) => to,
            None => Utc::now().timestamp_millis(),
        };
        let from = match self.time("from")? {
            Some(from) => from,
            None => to.saturating_sub(Duration::hours(24).num_milliseconds()),
        };
        if from >= to {
            return Err(ApiError::invalid("from must be before to"));
        }
        Ok((from, to))
    }
}
