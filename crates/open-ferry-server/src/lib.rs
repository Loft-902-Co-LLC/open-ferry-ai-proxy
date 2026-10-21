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
//! - Unset or non-positive `max-request-body-bytes` keeps the 64 MiB
//!   [`ServerConfig::body_limit`]; upstream disables its cap.
//! - open-ferry's own [`ingress`] budgets buffered bodies across both listeners
//!   and bounds uploads. `server.ingress-limits` also configures the accept
//!   loop's admission and transport deadlines.
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
pub mod ingress;
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
mod test_tracing;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod allocation_probe {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    thread_local! { static LARGEST: Cell<Option<usize>> = const { Cell::new(None) }; }
    struct Probe;
    #[global_allocator]
    static ALLOCATOR: Probe = Probe;
    fn record(size: usize) {
        let _ = LARGEST.try_with(|largest| {
            if let Some(previous) = largest.get() {
                largest.set(Some(previous.max(size)));
            }
        });
    }
    // SAFETY: every allocation and deallocation is forwarded unchanged to System.
    unsafe impl GlobalAlloc for Probe {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record(layout.size());
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            record(size);
            unsafe { System.realloc(ptr, layout, size) }
        }
    }
    pub(crate) fn start() {
        LARGEST.with(|largest| largest.set(Some(0)));
    }
    pub(crate) fn finish() -> usize {
        LARGEST.with(|largest| largest.replace(None).unwrap())
    }
}

pub use app::{router, router_with, router_without_proxy};
pub use config::{DEFAULT_BODY_LIMIT, ServerConfig, StreamingConfig};
pub use errors::ErrorMessage;
pub use state::AppState;

use open_ferry_core::exec::{Format, ProviderId};

/// Which of `providers`, the providers serving `model` in order of
/// preference, a call in `format` for `model` may go to, as the proxy
/// routes it: none for a model only the image endpoints serve, nor, outside
/// the speech endpoints' format, for one only they serve, and the Gemini
/// Interactions provider only for the formats it takes. The dashboard's
/// client setup lists each route's models with it.
pub fn entry_providers(
    format: &Format,
    model: &str,
    providers: Vec<ProviderId>,
) -> Vec<ProviderId> {
    if routing::check_image_only(model).is_err()
        || (*format != Format::OPENAI_SPEECH && routing::check_speech_only(model).is_err())
    {
        return Vec::new();
    }
    entry_protocol::adjust_execution_providers(format, providers)
}
