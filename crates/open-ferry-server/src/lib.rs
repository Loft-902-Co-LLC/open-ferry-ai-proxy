//! HTTP and WebSocket handlers for `/v1/*` and the Gemini API's `/v1beta/*`.
//!
//! [`router`] serves the proxy's routes for an [`AppState`], which holds the
//! [`ServerConfig`], the dispatcher that makes provider calls and the catalog
//! that says which providers serve a model. Handlers read the client's body,
//! route its model, hand the call to the dispatcher, and write the result in
//! the client's format, as upstream's handlers in `sdk/api/handlers` do.
//! [`router_with`] serves other routes beside them, such as the management
//! API's, which this crate doesn't depend on.
//!
//! Each request gets a context as it arrives (see
//! [`open_ferry_core::observe`]), which the access log, the request log and
//! every call the request makes share. [`AppState::with_observability`]
//! gives the server the request logger and the usage statistics that tap
//! the calls.
//!
//! Deviations from upstream, besides those noted on each module:
//! - Routes match paths exactly. Gin redirects a path with a trailing slash,
//!   and matches percent-encoded paths decoded; both get 404 here.
//! - A request body over [`ServerConfig::body_limit`], before or after
//!   decoding, gets 413. Upstream reads any size.
//! - `Content-Encoding: gzip` takes concatenated members, and zstd errors
//!   read differently.
//! - JSON written here, except on the Gemini routes, doesn't escape `<`,
//!   `>` and `&` as Go's encoder does. Where a body has a key twice, the
//!   last one counts; gjson takes the first. A body that isn't JSON has no
//!   fields here, where gjson may read some out of it.
//! - Client keys are compared in constant time.
//! - The client's proxy credentials are taken out of the headers and query
//!   handed to executors.
//! - Safe mode has no warning page at `/` or `/management.html`; the
//!   binary sends `/management.html` to the dashboard.
//! - `/` names this port.
//! - Nothing disguises the client: no request cloaking, rewritten model IDs
//!   or made-up session and user IDs. Only official OAuth and documented
//!   headers are ported.

mod access_log;
mod app;
mod auth;
mod body;
pub mod config;
mod entry_protocol;
mod errors;
mod exec;
mod handlers;
mod headers;
mod json;
mod query;
mod request_context;
mod request_log;
mod routing;
mod sse_check;
mod state;
mod status;
mod stream;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use app::{router, router_with};
pub use config::{DEFAULT_BODY_LIMIT, ServerConfig, StreamingConfig};
pub use errors::ErrorMessage;
pub use state::AppState;

use open_ferry_core::exec::{Format, ProviderId};

/// Which of `providers`, the providers serving `model` in order of
/// preference, a call in `format` for `model` may go to, as the proxy
/// routes it: none for a model only the image endpoints serve, and the
/// Gemini Interactions provider only for the formats it takes. The
/// dashboard's client setup lists each route's models with it.
pub fn entry_providers(
    format: &Format,
    model: &str,
    providers: Vec<ProviderId>,
) -> Vec<ProviderId> {
    if routing::check_image_only(model).is_err() {
        return Vec::new();
    }
    entry_protocol::adjust_execution_providers(format, providers)
}
