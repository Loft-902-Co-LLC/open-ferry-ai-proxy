//! open-ferry additions: one body-storage budget for both listeners and a
//! reloadable source of ingress settings. Reservations follow the actual
//! `Bytes` owners, including clones retained by the request logger.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, header};
use http_body::{Frame, SizeHint};
use std::task::{Context, Poll};
use tokio::sync::watch;
use tokio::time::{Instant, Sleep};

use crate::config::IngressConfig;
use crate::errors::{JSON_UTF8, error_response};

tokio::task_local! {
    static REQUEST_INGRESS: Ingress;
}

/// The budget of the request currently being decoded. Standalone body-reader
/// unit tests have no router; production requests always run inside this scope.
pub(crate) fn request_budget() -> Option<BodyBudget> {
    REQUEST_INGRESS
        .try_with(|ingress| ingress.budget.clone())
        .ok()
}

pub(crate) fn warn_budget() {
    let _ = REQUEST_INGRESS.try_with(|ingress| ingress.warn("buffered body bytes"));
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
            .map(|limit| limit.max(body_limit));
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
                .map(|limit| limit.max(body_limit)),
        );
        self.settings.send_replace(Snapshot { limits, body_limit });
    }

    /// The settings a newly accepted connection should use.
    pub fn limits(&self) -> IngressConfig {
        self.settings.borrow().limits.clone()
    }

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
        self.probe(self.protect(router))
    }

    pub(crate) fn protect(&self, router: Router) -> Router {
        router.layer(middleware::from_fn_with_state(self.clone(), layer))
    }

    pub(crate) fn probe(&self, router: Router) -> Router {
        router.layer(middleware::from_fn_with_state(self.clone(), probe))
    }
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

/// The admitted raw bytes. Logging can share them instead of allocating a
/// second copy, and the lifetime of those clones keeps the reservation alive.
#[derive(Clone)]
pub(crate) struct BufferedRequest(pub(crate) Bytes);

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
    let snapshot = ingress.settings.borrow().clone();
    let (mut parts, body) = request.into_parts();
    let (raw, parser) = match buffer(&ingress, &snapshot, &parts.headers, body).await {
        Ok(buffered) => buffered,
        Err(response) => return response,
    };
    parts.extensions.insert(BufferedRequest(raw.clone()));
    // Keep the reservation through downstream extractors, including ones
    // whose collected bytes no longer share the input Bytes owner.
    let hold = (raw.clone(), parser);
    let response = REQUEST_INGRESS
        .scope(
            ingress.clone(),
            next.run(Request::from_parts(parts, Body::from(raw))),
        )
        .await;
    if response.extensions().get::<IngressRejection>().is_some() {
        return response;
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(HeldBody {
            inner: body,
            hold: Some(hold),
        }),
    )
}

struct HeldBody {
    inner: Body,
    hold: Option<(Bytes, Reservation)>,
}

impl HttpBody for HeldBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        let frame = Pin::new(&mut this.inner).poll_frame(cx);
        if matches!(frame, Poll::Ready(None | Some(Err(_)))) {
            this.hold = None;
        }
        frame
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

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
) -> Result<(Bytes, Reservation), Response> {
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
    // additional body's worth for those copies before they can be made.
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
            parser.grow_to(length as usize).map_err(|()| {
                ingress.warn("buffered body bytes");
                rejected(503, "request body buffer budget exhausted")
            })?;
        }
    }
    if body.is_end_stream() {
        return Ok((buffer.finish(), parser));
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
            return Err(rejected(413, "http: request body too large"));
        }
        buffer.push(&frame).map_err(|()| {
            ingress.warn("buffered body bytes");
            rejected(503, "request body buffer budget exhausted")
        })?;
        if multipart {
            parser.grow_to(buffer.len()).map_err(|()| {
                ingress.warn("buffered body bytes");
                rejected(503, "request body buffer budget exhausted")
            })?;
        }
        if let (Some(timer), Some(duration)) = (&mut idle, snapshot.limits.body_idle_timeout) {
            timer.as_mut().reset(Instant::now() + duration);
        }
    }
    Ok((buffer.finish(), parser))
}

#[derive(Clone, Debug)]
pub(crate) struct BodyBudget(Arc<Mutex<Accounting>>);

#[derive(Debug, Default)]
struct Accounting {
    limit: Option<usize>,
    used: usize,
    #[cfg(test)]
    peak: usize,
}

impl BodyBudget {
    fn new(limit: Option<usize>) -> Self {
        Self(Arc::new(Mutex::new(Accounting {
            limit,
            ..Accounting::default()
        })))
    }

    fn set_limit(&self, limit: Option<usize>) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).limit = limit;
    }

    #[cfg(test)]
    fn used(&self) -> usize {
        self.0.lock().unwrap().used
    }
}

/// Charged bytes are returned even on cancellation, unwind, and disconnect.
#[derive(Debug)]
struct Reservation {
    budget: Option<BodyBudget>,
    bytes: usize,
}

impl Reservation {
    fn grow_to(&mut self, bytes: usize) -> Result<(), ()> {
        let additional = bytes.saturating_sub(self.bytes);
        if additional == 0 {
            return Ok(());
        }
        if let Some(budget) = &self.budget {
            let mut accounting = budget.0.lock().unwrap_or_else(PoisonError::into_inner);
            let used = accounting.used.checked_add(additional).ok_or(())?;
            if accounting.limit.is_some_and(|limit| used > limit) {
                return Err(());
            }
            accounting.used = used;
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
        self.bytes.reserve_exact(bytes.len());
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
