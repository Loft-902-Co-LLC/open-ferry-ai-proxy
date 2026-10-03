//! HTTP and WebSocket handlers for `/v1/*`.
//!
//! [`router`] serves the proxy's routes for an [`AppState`], which holds the
//! [`ServerConfig`], the dispatcher that makes provider calls and the catalog
//! that says which providers serve a model. Handlers read the client's body,
//! route its model, hand the call to the dispatcher, and write the result in
//! the client's format, as upstream's handlers in `sdk/api/handlers` do.
//!
//! Deviations from upstream, besides those noted on each module:
//! - Routes match paths exactly. Gin redirects a path with a trailing slash,
//!   and matches percent-encoded paths decoded; both get 404 here.
//! - A request body over [`ServerConfig::body_limit`], before or after
//!   decoding, gets 413. Upstream reads any size.
//! - `Content-Encoding: gzip` takes concatenated members, and zstd errors
//!   read differently.
//! - JSON written here doesn't escape `<`, `>` and `&` as Go's encoder does.
//!   Where a body has a key twice, the last one counts; gjson takes the
//!   first. A body that isn't JSON has no fields here, where gjson may read
//!   some out of it.
//! - Client keys are compared in constant time.
//! - The client's proxy credentials are taken out of the headers and query
//!   handed to executors.
//! - Safe mode answers with its own message, and there is no warning page.
//! - `/` names this port.
//! - Nothing disguises the client: no request cloaking, rewritten model IDs
//!   or made-up session and user IDs. Only official OAuth and documented
//!   headers are ported.

mod app;
mod auth;
mod body;
pub mod config;
mod errors;
mod exec;
mod handlers;
mod headers;
mod json;
mod query;
mod routing;
mod sse_check;
mod state;
mod status;
mod stream;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use app::router;
pub use config::{DEFAULT_BODY_LIMIT, ServerConfig, StreamingConfig};
pub use errors::ErrorMessage;
pub use state::AppState;
