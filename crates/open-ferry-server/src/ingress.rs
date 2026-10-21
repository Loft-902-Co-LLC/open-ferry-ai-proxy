//! open-ferry additions: one body-storage budget for both listeners and a
//! reloadable source of ingress settings. Reservations follow the actual
//! `Bytes` owners, including clones retained by the request logger.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, header};
use tokio::sync::watch;
use tokio::time::{Instant, Sleep};

use crate::config::IngressConfig;
use crate::errors::{JSON_UTF8, error_response};

const MANAGEMENT_BODY_LIMIT: usize = 32 << 20;
const MANAGEMENT_RESERVE: usize = 3 * MANAGEMENT_BODY_LIMIT;
fn budget_floor(body_limit: usize) -> usize {
    body_limit.saturating_mul(3).saturating_add(MANAGEMENT_RESERVE)
}

tokio::task_local! {
    static REQUEST_INGRESS: RequestScope;
}

#[derive(Clone)]
struct RequestScope {
    ingress: Ingress,
    body_limit: usize,
    rejected: Arc<AtomicBool>,
    decoded: Arc<Mutex<Vec<Bytes>>>,
}

/// The limit captured when this upload started, including downstream decoding.
pub(crate) fn body_limit(current: usize) -> usize {
    REQUEST_INGRESS.try_with(|scope| scope.body_limit).unwrap_or(current)
}

/// The budget of the request currently being decoded. Standalone body-reader
/// unit tests have no router; production requests always run inside this scope.
pub(crate) fn request_budget() -> Option<BodyBudget> {
    REQUEST_INGRESS
        .try_with(|scope| scope.ingress.budget.clone())
        .ok()
}

pub(crate) fn reserve_decoder(bytes: usize) -> Result<Reservation, ()> {
    let mut reservation = Reservation { budget: request_budget(), bytes: 0 };
    reservation.grow_to(bytes)?;
    Ok(reservation)
}

pub(crate) fn warn_budget() {
    let _ = REQUEST_INGRESS.try_with(|scope| {
        scope.rejected.store(true, Ordering::Release);
        scope.ingress.warn("buffered body bytes");
    });
}

/// Keep decoded storage charged while a handler retains parsed/transformed
/// input for its response stream, even if it drops the original Bytes early.
pub(crate) fn retain_decoded(bytes: &Bytes) {
    let _ = REQUEST_INGRESS.try_with(|scope| {
        scope
            .decoded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(bytes.clone());
    });
}

/// Let the request logger see the same budget response the client receives,
/// even if a route normally rewrites body-read errors into another format.
pub(crate) fn budget_response(response: Response) -> Response {
    let rejected_budget = REQUEST_INGRESS
        .try_with(|scope| scope.rejected.load(Ordering::Acquire))
        .unwrap_or(false);
    if rejected_budget && response.extensions().get::<IngressRejection>().is_none() {
        rejected(503, "request body buffer budget exhausted")
    } else {
        response
    }
}

/// Reloadable ingress state shared by the proxy and management routers.
#[derive(Clone)]
pub struct Ingress {
    settings: watch::Sender<Snapshot>,
    pub(crate) budget: BodyBudget,
    warnings: Arc<Warnings>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Effective ingress settings.
    pub limits: IngressConfig,
    body_limit: usize,
}

impl Ingress {
    /// A budget and settings source for a service, not a process-global budget.
    pub fn new(limits: IngressConfig, body_limit: usize) -> Self {
        let limit = limits
            .buffered_body_bytes
            .map(|limit| limit.max(budget_floor(body_limit)));
        Self {
            settings: watch::channel(Snapshot { limits, body_limit }).0,
            budget: BodyBudget::new(limit),
            warnings: Arc::default(),
        }
    }

    /// Applies to new connections and uploads. Existing reservations stay charged
    /// to this budget, even across a reduction below the bytes already held.
    pub fn reconfigure(&self, limits: IngressConfig, body_limit: usize) {
        self.budget.set_limit(
            limits
                .buffered_body_bytes
                .map(|limit| limit.max(budget_floor(body_limit))),
        );
        self.settings.send_replace(Snapshot { limits, body_limit });
    }

    /// The settings a newly accepted connection should use.
    pub fn limits(&self) -> IngressConfig {
        self.settings.borrow().limits.clone()
    }

    /// Current charged request storage, without inspecting any request data.
    pub fn reserved_body_bytes(&self) -> usize { self.budget.used() }

    /// Waits for a reload, without polling or opening any network connection.
    pub async fn changed(&self) {
        let mut settings = self.settings.subscribe();
        let _ = settings.changed().await;
    }

    /// A reload subscription, established before checking admission capacity.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.settings.subscribe()
    }

    /// Rate-limited counters, containing no request data or peer identity.
    pub fn warn(&self, guard: &'static str) {
        self.warnings.warn(guard);
    }

    /// Installs the outermost middleware, covering every route and extractor,
    /// including management/dashboard routes supplied by other crates.
    pub fn wrap(&self, router: Router) -> Router {
        self.probe(self.protect(router.layer(middleware::from_fn_with_state(self.clone(), layer))))
    }

    pub(crate) fn protect(&self, router: Router) -> Router {
        self.install(router)
    }

    /// Supplies admission to route authentication without reading any body.
    /// The route's existing successful key check calls `after_auth`.
    pub fn install(&self, router: Router) -> Router {
        router.layer(middleware::from_fn_with_state(self.clone(), install))
    }

    pub(crate) fn probe(&self, router: Router) -> Router {
        router.layer(middleware::from_fn_with_state(self.clone(), probe))
    }
}

async fn install(State(ingress): State<Ingress>, mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(ingress);
    next.run(request).await
}

/// Called only after the route's own authentication has succeeded.
pub async fn after_auth(request: Request, next: Next) -> Response {
    match request.extensions().get::<Ingress>().cloned() {
        Some(mut ingress) => {
            ingress.budget.1 = Pool::Proxy;
            layer(State(ingress), request, next).await
        },
        None => next.run(request).await,
    }
}

/// Management uses its own handler bounds and reserved headroom in the same
/// aggregate accounting pool, on either listener.
pub async fn after_management_auth(request: Request, next: Next) -> Response {
    match request.extensions().get::<Ingress>().cloned() {
        Some(mut ingress) => {
            ingress.budget.1 = Pool::Management;
            layer(State(ingress), request, next).await
        },
        None => next.run(request).await,
    }
}

/// OAuth callbacks accept at most 64 KiB, without using the authenticated pool.
pub async fn public_request(request: Request, next: Next) -> Response {
    let mut limits = request.extensions().get::<Ingress>()
        .map(Ingress::limits).unwrap_or_default();
    limits.buffered_body_bytes = None;
    layer(State(Ingress::new(limits, 64 << 10)), request, next).await
}

#[derive(Default)]
struct Warnings(Mutex<HashMap<&'static str, Warning>>);

#[derive(Default)]
struct Warning {
    total: u64,
    since_log: u64,
    last: Option<Instant>,
}

impl Warnings {
    fn warn(&self, guard: &'static str) {
        let mut warnings = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let count = warnings.entry(guard).or_default();
        count.total = count.total.saturating_add(1);
        count.since_log = count.since_log.saturating_add(1);
        let now = Instant::now();
        if count
            .last
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(30))
        {
            tracing::warn!(
                guard,
                total = count.total,
                since_last_warning = count.since_log,
                "ingress limit reached"
            );
            count.last = Some(now);
            count.since_log = 0;
        }
    }
}

/// An in-process service query. The accept loop uses this before accepting to
/// obtain the router's state without a global singleton or a public endpoint.
/// Only this Rust extension can trigger it; HTTP clients cannot construct one.
#[derive(Clone)]
pub struct IngressProbe;

/// An ingress rejection that asks HTTP/2 to stop admitting further streams.
#[derive(Clone)]
pub struct IngressRejection;

async fn probe(State(ingress): State<Ingress>, request: Request, next: Next) -> Response {
    if request.extensions().get::<IngressProbe>().is_some() {
        let mut response = Response::new(Body::empty());
        response.extensions_mut().insert(ingress);
        return response;
    }
    next.run(request).await
}

async fn layer(State(ingress): State<Ingress>, request: Request, next: Next) -> Response {
    // CONNECT's DATA is the upgraded stream, not a request upload. HTTP/1
    // upgrades normally have no body, but must not consume upgraded bytes.
    if request.body().is_end_stream()
        && (request.method() == Method::CONNECT
            || (request.method() == Method::GET
                && request
                    .headers()
                    .get(header::UPGRADE)
                    .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))))
    {
        return next.run(request).await;
    }
    let mut snapshot = ingress.settings.borrow().clone();
    if ingress.budget.1 == Pool::Management { snapshot.body_limit = MANAGEMENT_BODY_LIMIT; }
    let (parts, body) = request.into_parts();
    let (raw, parser, oversized) = match buffer(&ingress, &snapshot, &parts.headers, body).await {
        Ok(buffered) => buffered,
        Err(response) => return response,
    };
    // Management writes already run in gate-owned tasks that finish after a
    // disconnect. Keep multipart copy accounting until that existing work
    // finishes, rather than dropping it with the caller's HTTP future.
    let owned_upload = parser.bytes > 0
        && [
            "/v0/management",
            "/v8/management",
            "/management",
            "/open-ferry/",
        ]
        .iter()
        .any(|prefix| parts.uri.path().starts_with(prefix));
    let response = admitted(ingress, snapshot.body_limit, parts, raw, parser, oversized, next);
    if owned_upload {
        return match tokio::spawn(response).await {
            Ok(response) => response,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(_) => rejected(500, "request handling stopped"),
        };
    }
    response.await
}

async fn admitted(
    ingress: Ingress,
    body_limit: usize,
    parts: http::request::Parts,
    raw: Bytes,
    parser: Reservation,
    oversized: bool,
    next: Next,
) -> Response {
    // Keep the reservation through downstream extractors, including ones
    // whose collected bytes no longer share the input Bytes owner.
    let scope = RequestScope {
        ingress,
        body_limit,
        rejected: Arc::new(AtomicBool::new(false)),
        decoded: Arc::default(),
    };
    let _hold = (raw.clone(), parser);
    let body = if oversized {
        Body::from_stream(futures_util::stream::iter([
            Ok(raw), Err(BodyTooLarge),
        ]))
    } else { Body::from(raw) };
    let response = REQUEST_INGRESS
        .scope(
            scope.clone(),
            next.run(Request::from_parts(parts, body)),
        )
        .await;
    // Handlers may translate body failures; preserve the budget's 503 and
    // close/GOAWAY marker even when a route gives other errors its own shape.
    if scope.rejected.load(Ordering::Acquire)
        && response.extensions().get::<IngressRejection>().is_none()
    {
        return rejected(503, "request body buffer budget exhausted");
    }
    if response.extensions().get::<IngressRejection>().is_some() {
        return response;
    }
    // Only owners actually retained by the response or logger stay charged.
    // A long SSE response does not extend the handler's temporary allowance.
    response
}

#[derive(Debug)]
pub(crate) struct BodyTooLarge;
impl std::fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("http: request body too large") }
}
impl std::error::Error for BodyTooLarge {}

/// The shape used by the server's pre-handler body-size rejection.
pub(crate) fn rejected(status: u16, message: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    if status != 413 {
        crate::app::set_cors_headers(&mut headers);
    }
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    if status == 503 {
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    let mut response = error_response(
        status,
        headers,
        Bytes::from(serde_json::json!({"error": message}).to_string()),
        JSON_UTF8,
    );
    response.extensions_mut().insert(IngressRejection);
    response
}

async fn optional_deadline(deadline: &mut Option<Pin<Box<Sleep>>>) {
    match deadline {
        Some(deadline) => deadline.as_mut().await,
        None => std::future::pending().await,
    }
}

async fn buffer(
    ingress: &Ingress,
    snapshot: &Snapshot,
    headers: &HeaderMap,
    body: Body,
) -> Result<(Bytes, Reservation, bool), Response> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok())
        .and_then(|length| length.parse::<u64>().ok());
    if declared.is_some_and(|length| length > snapshot.body_limit as u64) {
        return Err(rejected(413, "http: request body too large"));
    }
    let mut buffer = BudgetBuffer::new(Some(ingress.budget.clone()));
    // Multipart parsers may copy fields out of their raw input (multer on
    // management routes, quoted-printable parts in the proxy). Reserve an
    // two additional body lengths for parser input and collected field copies
    // before either allocation can be made (multer can hold both at once).
    let multipart = headers.get(header::CONTENT_TYPE).is_some_and(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .trim()
                .to_ascii_lowercase()
                .starts_with("multipart/form-data")
        })
    });
    let mut parser = Reservation {
        budget: Some(ingress.budget.clone()),
        bytes: 0,
    };
    // Reserve before even polling the body, including Expect: 100-continue.
    if let Some(length) = declared {
        buffer.reservation.grow_to(length as usize).map_err(|()| {
            ingress.warn("buffered body bytes");
            rejected(503, "request body buffer budget exhausted")
        })?;
        if multipart {
            parser.grow_copies(length as usize).map_err(|()| {
                ingress.warn("buffered body bytes");
                rejected(503, "request body buffer budget exhausted")
            })?;
        }
        // The entire allocation is already charged, including parser copies.
        buffer.bytes = Vec::with_capacity(length as usize);
    }
    if body.is_end_stream() {
        return Ok((buffer.finish(), parser, false));
    }
    let mut data = body.into_data_stream();
    let mut idle = snapshot
        .limits
        .body_idle_timeout
        .map(|duration| Box::pin(tokio::time::sleep(duration)));
    let mut total = snapshot
        .limits
        .body_total_timeout
        .map(|duration| Box::pin(tokio::time::sleep(duration)));
    loop {
        let frame = tokio::select! {
            biased;
            () = optional_deadline(&mut total) => {
                ingress.warn("total body time");
                return Err(rejected(408, "request body deadline exceeded"));
            }
            () = optional_deadline(&mut idle) => {
                ingress.warn("body progress");
                return Err(rejected(408, "request body read timed out"));
            }
            frame = data.next() => frame,
        };
        let Some(frame) = frame else {
            break;
        };
        let frame = frame.map_err(|_| rejected(400, "failed to read request body"))?;
        if frame.is_empty() {
            continue;
        }
        if frame.len() > snapshot.body_limit.saturating_sub(buffer.len()) {
            // Preserve the route's upstream error shape. The bounded prefix
            // is replayed once, then its reader receives the size error.
            return Ok((buffer.finish(), parser, true));
        }
        buffer.push(&frame).map_err(|()| {
            ingress.warn("buffered body bytes");
            rejected(503, "request body buffer budget exhausted")
        })?;
        if multipart {
            parser.grow_copies(buffer.len()).map_err(|()| {
                ingress.warn("buffered body bytes");
                rejected(503, "request body buffer budget exhausted")
            })?;
        }
        if let (Some(timer), Some(duration)) = (&mut idle, snapshot.limits.body_idle_timeout) {
            timer.as_mut().reset(Instant::now() + duration);
        }
    }
    Ok((buffer.finish(), parser, false))
}

#[derive(Clone, Debug)]
pub(crate) struct BodyBudget(Arc<Mutex<Accounting>>, Pool);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pool { Shared, Proxy, Management }

#[derive(Debug, Default)]
struct Accounting {
    limit: Option<usize>,
    used: usize,
    proxy_used: usize,
    management_reserve: usize,
    #[cfg(test)]
    peak: usize,
}

impl BodyBudget {
    fn new(limit: Option<usize>) -> Self {
        Self(Arc::new(Mutex::new(Accounting {
            limit,
            management_reserve: MANAGEMENT_RESERVE,
            ..Accounting::default()
        })), Pool::Shared)
    }

    fn set_limit(&self, limit: Option<usize>) {
        let mut accounting = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        accounting.limit = limit;
        accounting.management_reserve = MANAGEMENT_RESERVE;
    }

    fn used(&self) -> usize {
        self.0.lock().unwrap().used
    }
}

/// Charged bytes are returned even on cancellation, unwind, and disconnect.
#[derive(Debug)]
pub(crate) struct Reservation {
    budget: Option<BodyBudget>,
    bytes: usize,
}

impl Reservation {
    fn grow_copies(&mut self, bytes: usize) -> Result<(), ()> {
        self.grow_to(bytes.checked_mul(2).ok_or(())?)
    }

    fn grow_to(&mut self, bytes: usize) -> Result<(), ()> {
        let additional = bytes.saturating_sub(self.bytes);
        if additional == 0 {
            return Ok(());
        }
        if let Some(budget) = &self.budget {
            let mut accounting = budget.0.lock().unwrap_or_else(PoisonError::into_inner);
            let used = accounting.used.checked_add(additional).ok_or(())?;
            let proxy_used = accounting.proxy_used.checked_add(additional).ok_or(())?;
            if accounting.limit.is_some_and(|limit| used > limit ||
                (budget.1 == Pool::Proxy && proxy_used > limit.saturating_sub(accounting.management_reserve))) {
                return Err(());
            }
            accounting.used = used;
            if budget.1 == Pool::Proxy { accounting.proxy_used = proxy_used; }
            #[cfg(test)]
            {
                accounting.peak = accounting.peak.max(used);
            }
        }
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(budget) = &self.budget {
            let mut accounting = budget.0.lock().unwrap_or_else(PoisonError::into_inner);
            accounting.used -= self.bytes;
            if budget.1 == Pool::Proxy { accounting.proxy_used -= self.bytes; }
        }
    }
}

/// Owns storage and its reservation together. `Bytes::from_owner` keeps the
/// reservation until the last clone or slice disappears, including log queues.
pub(crate) struct BudgetBuffer {
    bytes: Vec<u8>,
    reservation: Reservation,
}

impl BudgetBuffer {
    pub(crate) fn new(budget: Option<BodyBudget>) -> Self {
        Self {
            bytes: Vec::new(),
            reservation: Reservation { budget, bytes: 0 },
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        self.reservation
            .grow_to(self.bytes.len().checked_add(bytes.len()).ok_or(())?)?;
        self.bytes.reserve(bytes.len());
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn finish(self) -> Bytes {
        Bytes::from_owner(self)
    }
}

impl AsRef<[u8]> for BudgetBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests;
