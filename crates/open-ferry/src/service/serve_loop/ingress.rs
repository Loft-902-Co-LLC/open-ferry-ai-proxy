//! Transport admission and deadlines, owned by the same connection core as
//! blue-green handoff. No request/response content is retained here.

use super::*;
use axum::body::HttpBody;
use hyper::body::{Buf, Bytes, Frame, SizeHint};
use open_ferry_server::config::IngressConfig;
use open_ferry_server::ingress::{Ingress, IngressProbe};
use std::sync::atomic::AtomicUsize;
use tokio::time::Instant;

/// Let GOAWAY reach the peer, then reclaim an idle peer that never completes
/// h2's graceful ping exchange. Never cuts a stream that is still open.
const GOAWAY_GRACE: Duration = Duration::from_secs(5);

/// Reserve half the process limit (at least 256 descriptors) for upstream
/// sockets, credentials and files. Split the remainder between two listeners,
/// then allow two descriptors for each accepted connection. A practical ceiling
/// also bounds parser/task overhead on hosts with a million-descriptor limit.
fn cap_from_limit(limit: u64) -> usize {
    (limit.saturating_sub((limit / 2).max(256)) / 4).clamp(1, 8192) as usize
}

pub(super) fn default_connection_cap() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        #[cfg(unix)]
        {
            // SAFETY: these calls initialize/read a local rlimit and alter only
            // this process's descriptor soft limit, never its hard limit.
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
                if limit.rlim_cur < limit.rlim_max {
                    let raised = libc::rlimit {
                        rlim_cur: limit.rlim_max,
                        rlim_max: limit.rlim_max,
                    };
                    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
                        limit = raised;
                    }
                }
                return cap_from_limit(limit.rlim_cur);
            }
        }
        // Winsock sockets are not CRT file descriptors and Windows exposes no
        // RLIMIT_NOFILE for them. Use a conservative finite handle envelope.
        cap_from_limit(8192)
    })
}

#[derive(Default)]
pub(super) struct Admission {
    live: AtomicUsize,
    pub(super) changed: Notify,
}

impl Admission {
    pub(super) fn full(&self, config: &IngressConfig) -> bool {
        config.max_connections.is_some_and(|limit| {
            let limit = if limit == 0 {
                default_connection_cap()
            } else {
                limit
            };
            self.live.load(Ordering::Acquire) >= limit
        })
    }

    pub(super) fn admit(self: &Arc<Self>) -> AdmissionGuard {
        self.live.fetch_add(1, Ordering::AcqRel);
        AdmissionGuard(self.clone())
    }
}

pub(super) struct AdmissionGuard(Arc<Admission>);

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_one();
    }
}

/// Query a Rust service extension before accepting; no wire endpoint, global
/// registry, listener or credentials are involved. Raw test routers use defaults.
pub(super) async fn router_settings(app: &mut Router) -> Ingress {
    let request = hyper::Request::builder()
        .method(hyper::Method::OPTIONS)
        .uri("/__open_ferry_ingress_probe")
        .extension(IngressProbe)
        .body(Body::empty())
        .unwrap();
    let response = app
        .call(request)
        .await
        .unwrap_or_else(|error| match error {});
    response
        .extensions()
        .get::<Ingress>()
        .cloned()
        .unwrap_or_else(|| {
            Ingress::new(
                IngressConfig::default(),
                open_ferry_server::DEFAULT_BODY_LIMIT,
            )
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Unknown,
    Http1,
    Http2,
}

struct State {
    protocol: Protocol,
    preface: usize,
    active: usize,
    upgraded: bool,
    idle_since: Option<Instant>,
    first_head: Option<Instant>,
    partial_head: Option<Instant>,
    // HTTP/2 streams stay open until BOTH halves end, including CONNECT DATA.
    streams: HashMap<u32, (bool, bool)>,
    read_frames: Frames,
    write_frames: Frames,
    write_pending: Option<Instant>,
    deliveries: HashMap<u64, Instant>,
    next_delivery: u64,
    draining: bool,
    draining_since: Option<Instant>,
}

pub(super) struct Traffic {
    config: IngressConfig,
    ingress: Ingress,
    state: Mutex<State>,
    changed: Notify,
    pub(super) idle_wake: Notify,
    pub(super) idle_expired: AtomicBool,
}

impl Traffic {
    pub(super) fn new(config: IngressConfig, ingress: Ingress, started_at: Instant) -> Arc<Self> {
        let now = started_at;
        Arc::new(Self {
            state: Mutex::new(State {
                protocol: Protocol::Unknown,
                preface: 0,
                active: 0,
                upgraded: false,
                idle_since: Some(now),
                first_head: config.header_timeout.map(|duration| now + duration),
                partial_head: None,
                streams: HashMap::new(),
                read_frames: Frames::default(),
                write_frames: Frames::default(),
                write_pending: None,
                deliveries: HashMap::new(),
                next_delivery: 0,
                draining: false,
                draining_since: None,
            }),
            config,
            ingress,
            changed: Notify::new(),
            idle_wake: Notify::new(),
            idle_expired: AtomicBool::new(false),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn upgrade(&self) {
        let mut state = self.state();
        state.upgraded = true;
        state.first_head = None;
        self.changed.notify_one();
    }

    fn idle(state: &mut State) {
        if state.active == 0 && state.streams.is_empty() && state.idle_since.is_none() {
            state.idle_since = Some(Instant::now());
        }
    }

    fn received(&self, mut bytes: &[u8]) {
        const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        let mut state = self.state();
        while state.protocol == Protocol::Unknown && !bytes.is_empty() {
            if bytes[0] != PREFACE[state.preface] {
                state.protocol = Protocol::Http1;
                break;
            }
            state.preface += 1;
            bytes = &bytes[1..];
            if state.preface == PREFACE.len() {
                state.protocol = Protocol::Http2;
            }
        }
        if state.protocol == Protocol::Http2 {
            let mut frames = std::mem::take(&mut state.read_frames);
            frames.feed(bytes, true, &mut state, self.config.header_timeout);
            state.read_frames = frames;
        }
        self.changed.notify_one();
    }

    fn written(&self, bytes: &[u8]) {
        let mut state = self.state();
        state.write_pending = None;
        if state.protocol == Protocol::Http2 {
            let mut frames = std::mem::take(&mut state.write_frames);
            frames.feed(bytes, false, &mut state, self.config.header_timeout);
            state.write_frames = frames;
        } else if state.active == 0 {
            // Count from delivery of the last response bytes, not just from
            // the handler returning its response head/body.
            state.idle_since = Some(Instant::now());
        }
        self.changed.notify_one();
    }

    fn pending_write(&self, pending: bool) {
        let mut state = self.state();
        if pending {
            state.write_pending.get_or_insert_with(Instant::now);
        } else {
            state.write_pending = None;
        }
        self.changed.notify_one();
    }

    fn due(&self) -> Option<(Instant, &'static str)> {
        let state = self.state();
        let mut due = Vec::with_capacity(5);
        if state.active == 0
            && state.streams.is_empty()
            && let Some(started) = state.draining_since
        {
            due.push((
                state.idle_since.unwrap_or(started).max(started) + GOAWAY_GRACE,
                "GOAWAY completion",
            ));
        }
        if !state.draining {
            if state.protocol != Protocol::Http1 || !state.upgraded {
                due.extend(state.first_head.map(|at| (at, "request headers")));
            }
            due.extend(state.partial_head.map(|at| (at, "request headers")));
            if let Some(idle) = state
                .idle_since
                .filter(|_| state.active == 0 && state.streams.is_empty())
                && (state.protocol != Protocol::Http1 || !state.upgraded)
            {
                if let Some(duration) = self.config.idle_timeout {
                    due.push((idle + duration, "idle connection"));
                }
                if state.protocol == Protocol::Http1
                    && let Some(duration) = self.config.header_timeout
                {
                    due.push((idle + duration, "request headers"));
                }
            }
        }
        if let Some(duration) = self.config.write_stall_timeout {
            if let Some(at) = state.write_pending {
                due.push((at + duration, "response write progress"));
            }
            due.extend(
                state
                    .deliveries
                    .values()
                    .map(|at| (*at + duration, "response write progress")),
            );
        }
        due.into_iter().min_by_key(|(at, _)| *at)
    }

    pub(super) fn wake(&self) {
        self.changed.notify_one();
    }

    #[cfg(test)]
    pub(super) fn has_pending_write(&self) -> bool {
        self.state().write_pending.is_some()
    }

    #[cfg(test)]
    pub(super) fn has_pending_delivery(&self) -> bool {
        !self.state().deliveries.is_empty()
    }
}

/// Only frame headers (nine bytes), lengths and stream lifecycle bits are kept.
/// HPACK decoding/validation belongs to hyper. A HEADERS/CONTINUATION block gets
/// one deadline, never reset by trickled fragments or unrelated HTTP/2 frames.
#[derive(Default)]
struct Frames {
    head: [u8; 9],
    used: usize,
    remaining: usize,
}

impl Frames {
    fn feed(
        &mut self,
        mut bytes: &[u8],
        incoming: bool,
        state: &mut State,
        timeout: Option<Duration>,
    ) {
        while !bytes.is_empty() {
            if self.used < 9 {
                let len = (9 - self.used).min(bytes.len());
                self.head[self.used..self.used + len].copy_from_slice(&bytes[..len]);
                self.used += len;
                bytes = &bytes[len..];
                if incoming && self.used >= 4 && self.head[3] == 1 && state.partial_head.is_none() {
                    state.partial_head = timeout.map(|duration| Instant::now() + duration);
                }
                if self.used < 9 {
                    return;
                }
                self.remaining = (usize::from(self.head[0]) << 16)
                    | (usize::from(self.head[1]) << 8)
                    | usize::from(self.head[2]);
            }
            let len = self.remaining.min(bytes.len());
            bytes = &bytes[len..];
            self.remaining -= len;
            if self.remaining != 0 {
                return;
            }
            let kind = self.head[3];
            let flags = self.head[4];
            let stream = u32::from_be_bytes(self.head[5..9].try_into().unwrap()) & 0x7fff_ffff;
            if incoming && matches!(kind, 1 | 9) && flags & 4 != 0 {
                state.partial_head = None;
            }
            if stream != 0 {
                if kind == 3 {
                    state.streams.remove(&stream);
                } else if matches!(kind, 0 | 1) {
                    let ends = state.streams.entry(stream).or_default();
                    if flags & 1 != 0 {
                        if incoming {
                            ends.0 = true;
                        } else {
                            ends.1 = true;
                        }
                    }
                    if ends.0 && ends.1 {
                        state.streams.remove(&stream);
                    } else {
                        state.idle_since = None;
                    }
                }
                Traffic::idle(state);
            }
            self.used = 0;
        }
    }
}

pub(super) async fn watch_traffic(core: Weak<ConnCore>, traffic: Arc<Traffic>) {
    loop {
        let changed = traffic.changed.notified();
        if core.upgrade().is_none_or(|core| core.is_cut()) {
            return;
        }
        let deadline = traffic.due();
        tokio::select! {
            () = changed => {}
            () = async {
                match deadline {
                    Some((at, _)) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                // Recheck after the wake: progress at the same instant wins.
                if let Some((_, guard)) = traffic.due().filter(|(at, _)| *at <= Instant::now()) {
                    traffic.ingress.warn(guard);
                    let Some(core) = core.upgrade() else { return; };
                    let mut state = traffic.state();
                    if guard == "idle connection" && state.protocol == Protocol::Http2 {
                        state.draining = true;
                        state.draining_since = Some(Instant::now());
                        traffic.idle_expired.store(true, Ordering::Release);
                        traffic.idle_wake.notify_one();
                    } else { drop(state); core.cut(); return; }
                }
            }
        }
    }
}

/// Active through the last response data, rather than through just the service
/// future. Waiting for an upstream/SSE producer therefore never looks idle.
pub(super) struct Flight(Arc<Traffic>);

impl Flight {
    pub(super) fn close_after_response(&self) {
        let mut state = self.0.state();
        state.draining = true;
        state.draining_since = Some(Instant::now());
        self.0.idle_expired.store(true, Ordering::Release);
        self.0.idle_wake.notify_one();
    }

    pub(super) fn new(traffic: Arc<Traffic>) -> Arc<Self> {
        {
            let mut state = traffic.state();
            state.active += 1;
            state.idle_since = None;
            state.first_head = None;
        }
        traffic.changed.notify_one();
        Arc::new(Self(traffic))
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.active -= 1;
        Traffic::idle(&mut state);
        self.0.changed.notify_one();
    }
}

pub(super) struct ResponseBody {
    inner: Body,
    flight: Arc<Flight>,
}

impl ResponseBody {
    pub(super) fn new(inner: Body, flight: Arc<Flight>) -> Self {
        Self { inner, flight }
    }
}

impl HttpBody for ResponseBody {
    type Data = Delivery;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Delivery>, axum::Error>>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_frame(cx).map(|frame| {
            frame.map(|frame| {
                frame.map(|frame| frame.map_data(|bytes| Delivery::new(bytes, this.flight.clone())))
            })
        })
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Hyper/h2 advance this Buf when queued DATA is delivered to their transport.
/// Watching its bytes covers HTTP/2 flow-control stalls as well as TCP stalls.
/// No timer is armed while a response producer itself returns Pending.
pub(super) struct Delivery {
    bytes: Bytes,
    flight: Arc<Flight>,
    id: u64,
}

impl Delivery {
    fn new(bytes: Bytes, flight: Arc<Flight>) -> Self {
        let mut state = flight.0.state();
        let id = state.next_delivery;
        state.next_delivery = state.next_delivery.wrapping_add(1);
        if !bytes.is_empty() {
            state.deliveries.insert(id, Instant::now());
        }
        drop(state);
        flight.0.changed.notify_one();
        Self { bytes, flight, id }
    }
}

impl Buf for Delivery {
    fn remaining(&self) -> usize {
        self.bytes.remaining()
    }
    fn chunk(&self) -> &[u8] {
        self.bytes.chunk()
    }
    fn advance(&mut self, count: usize) {
        self.bytes.advance(count);
        if count > 0 {
            let mut state = self.flight.0.state();
            if self.bytes.is_empty() {
                state.deliveries.remove(&self.id);
            } else {
                state.deliveries.insert(self.id, Instant::now());
            }
            self.flight.0.changed.notify_one();
        }
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        self.flight.0.state().deliveries.remove(&self.id);
        self.flight.0.changed.notify_one();
    }
}

pub(super) struct Guarded<T> {
    pub(super) inner: T,
    pub(super) traffic: Arc<Traffic>,
}

impl<T: AsyncRead + Unpin> AsyncRead for Guarded<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) {
            this.traffic.received(&buf.filled()[before..]);
        }
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Guarded<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, bytes);
        if let Poll::Ready(Ok(count)) = result {
            this.traffic.written(&bytes[..count]);
        } else {
            this.traffic.pending_write(result.is_pending());
        }
        result
    }
    // The default vectored write delegates here, keeping frame observation exact.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        this.traffic.pending_write(result.is_pending());
        result
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests;
