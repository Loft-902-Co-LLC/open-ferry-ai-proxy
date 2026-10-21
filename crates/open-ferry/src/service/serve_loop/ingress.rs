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
/// h2's graceful ping exchange. Accepted streams also have an absolute drain
/// bound of this grace plus the configured body deadline.
const GOAWAY_GRACE: Duration = Duration::from_secs(5);
// A signalled reset is an attempt, not proof that h2 released queued DATA.
const RESET_GRACE: Duration = Duration::from_secs(1);

/// Reserve half the process limit (at least 256 descriptors) for upstream
/// sockets, credentials and files. Split the remainder between two listeners,
/// then allow two descriptors for each accepted connection. A practical ceiling
/// also bounds parser/task overhead on hosts with a million-descriptor limit.
fn cap_from_limit(limit: u64) -> usize {
    (limit.saturating_sub((limit / 2).max(256)) / 4)
        .max((limit / 8).max(16))
        .min(8192) as usize
}

#[cfg(any(unix, test))]
fn raise_target(hard: u64, ceiling: u64) -> u64 {
    hard.min(ceiling)
}

#[cfg(target_os = "macos")]
fn descriptor_ceiling() -> u64 {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&value);
    // SAFETY: sysctl writes only to the sized local integer.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&mut value as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result == 0 && value > 0 {
        value as u64
    } else {
        10240
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn descriptor_ceiling() -> u64 {
    u64::MAX
}

// Portable seam shared by actual syscalls and injected raise/retry regressions.
#[cfg(any(unix,test))]
fn raise_nofile_with(
    get: impl FnOnce()->Result<(u64,u64),i32>,
    ceiling:u64,
    retry_einval:bool,
    mut set: impl FnMut((u64,u64))->Result<(),i32>,
    mut remember: impl FnMut((u64,u64)),
) -> Result<((u64,u64),bool),i32> {
    let original=get()?;
    if original.0>=original.1 { return Ok((original,false)); }
    let mut raised=(raise_target(original.1,ceiling),original.1);
    if raised.0<=original.0 {return Ok((original,false));}
    let mut result=set(raised);
    if result==Err(22) && retry_einval {
        raised.0=raised.0.min(10240);
        result=set(raised);
    }
    Ok(if result.is_ok() {remember(original); (raised,false)} else {(original,true)})
}

pub(super) fn default_connection_cap() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        #[cfg(unix)]
        {
            let outcome=raise_nofile_with(|| {
                let mut limit=libc::rlimit {rlim_cur:0,rlim_max:0};
                // SAFETY: getrlimit initializes the sized local structure.
                if unsafe {libc::getrlimit(libc::RLIMIT_NOFILE,&mut limit)}==0 {Ok((limit.rlim_cur,limit.rlim_max))}
                else {Err(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL))}
            },descriptor_ceiling(),cfg!(target_os="macos"),|next| {
                let limit=libc::rlimit {rlim_cur:next.0,rlim_max:next.1};
                // SAFETY: only the process soft limit changes, within hard.
                if unsafe {libc::setrlimit(libc::RLIMIT_NOFILE,&limit)}==0 {Ok(())}
                else {Err(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL))}
            },|original| open_ferry_core::process_limits::remember_nofile_limit(libc::rlimit {rlim_cur:original.0,rlim_max:original.1}));
            match outcome {
                Ok((limit,failed))=> {
                    if failed {tracing::warn!(cap=cap_from_limit(limit.0),"could not raise open-file limit; using the current limit for admission");}
                    return cap_from_limit(limit.0);
                }
                Err(_)=>tracing::warn!("could not read open-file limit; using the default admission cap"),
            }
        }
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
    read_frames: Frames,
    write_frames: Frames,
    write_pending: Option<Instant>,
    deliveries: HashMap<u64, StreamProgress>,
    protocol_tasks: usize,
    send_window: i64,
    connection_blocked_since: Option<Instant>,
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
                read_frames: Frames::default(),
                write_frames: Frames::default(),
                write_pending: None,
                deliveries: HashMap::new(),
                protocol_tasks: 0,
                send_window: 65535,
                connection_blocked_since: None,
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
        if state.active == 0 && state.protocol_tasks == 0 && state.idle_since.is_none() {
            state.idle_since = Some(Instant::now());
        }
    }

    fn received(&self, mut bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        let mut state = self.state();
        let before = (state.protocol, state.partial_head, state.connection_blocked_since);
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
        if before != (state.protocol, state.partial_head, state.connection_blocked_since) {
            self.changed.notify_one();
        }
    }

    fn written(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut state = self.state();
        let idle_was_none = state.idle_since.is_none();
        let connection_was_unblocked = state.connection_blocked_since.is_none();
        if state.protocol == Protocol::Http2 {
            let mut frames = std::mem::take(&mut state.write_frames);
            frames.feed(bytes, false, &mut state, self.config.header_timeout);
            state.write_frames = frames;
        } else if state.active == 0 {
            // Count from delivery of the last response bytes, not just from
            // the handler returning its response head/body.
            state.idle_since = Some(Instant::now());
        }
        if (idle_was_none && state.idle_since.is_some())
            || (connection_was_unblocked && state.connection_blocked_since.is_some()) {
            self.changed.notify_one();
        }
    }

    pub(super) fn established(&self, started: Instant) {
        let mut state = self.state();
        state.first_head = self
            .config
            .header_timeout
            .map(|duration| started + duration);
        state.idle_since = Some(started);
        state.write_pending = None;
    }

    fn socket_written(&self, count: usize) {
        if count == 0 {
            return;
        }
        let mut state = self.state();
        if state.write_pending.take().is_some() {
            self.changed.notify_one();
        }
    }

    fn pending_write(&self, pending: bool) {
        let mut state = self.state();
        let was_pending = state.write_pending.is_some();
        if pending {
            state.write_pending.get_or_insert_with(Instant::now);
        } else {
            state.write_pending = None;
        }
        if was_pending != pending {
            self.changed.notify_one();
        }
    }

    fn due(&self) -> Option<(Instant, &'static str, Option<u64>)> {
        let state = self.state();
        let mut due = None;
        let mut consider = |at, guard, stream| {
            if due.is_none_or(|(earlier, _, _)| at < earlier) {
                due = Some((at, guard, stream));
            }
        };
        if state.active == 0
            && state.protocol_tasks == 0
            && let Some(started) = state.draining_since
        {
            consider(
                state.idle_since.unwrap_or(started).max(started) + GOAWAY_GRACE,
                "GOAWAY completion",
                None,
            );
        }
        if (state.protocol != Protocol::Http1 || !state.upgraded)
            && let Some(at) = state.first_head
        {
            consider(at, "request headers", None);
        }
        if let Some(at) = state.partial_head {
            consider(at, "request headers", None);
        }
        if let Some(started) = state.draining_since {
            let body_grace = self
                .config
                .body_total_timeout
                .or(self.config.body_idle_timeout)
                .unwrap_or(Duration::from_secs(900));
            consider(started + GOAWAY_GRACE + body_grace, "drain deadline", None);
        }
        if !state.draining
            && let Some(idle) = state
                .idle_since
                .filter(|_| state.active == 0 && state.protocol_tasks == 0)
            && (state.protocol != Protocol::Http1 || !state.upgraded)
        {
            if let Some(duration) = self.config.idle_timeout {
                consider(idle + duration, "idle connection", None);
            }
            if state.protocol == Protocol::Http1
                && let Some(duration) = self.config.header_timeout
            {
                consider(idle + duration, "request headers", None);
            }
        }
        if let Some(duration) = self.config.write_stall_timeout {
            if let Some(at) = state.write_pending {
                consider(at + duration, "response write progress", None);
            }
            let connection_blocked = state.protocol == Protocol::Http2 && state.send_window <= 0;
            if connection_blocked && state.deliveries.values().any(|progress| progress.pending != 0)
                && let Some(at) = state.connection_blocked_since {
                consider(at + duration, "connection window progress", None);
            }
            for (&id, progress) in &state.deliveries {
                if progress.reset {
                    consider(progress.progressed + RESET_GRACE, "stream reset grace", None);
                } else if progress.pending != 0 && !connection_blocked {
                    let stream = (state.protocol == Protocol::Http2).then_some(id);
                    consider(
                        progress.progressed + duration,
                        "response write progress",
                        stream,
                    );
                }
            }
        }
        due
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
        self.state()
            .deliveries
            .values()
            .any(|progress| progress.pending != 0)
    }

    #[cfg(test)]
    pub(super) fn has_partial_headers(&self) -> bool {
        self.state().partial_head.is_some()
    }

    #[cfg(test)]
    pub(super) fn connection_send_window(&self) -> i64 {
        self.state().send_window
    }
}

/// Only frame headers (nine bytes), lengths and window updates are kept.
/// HPACK decoding/validation belongs to hyper. A HEADERS/CONTINUATION block gets
/// one deadline, never reset by trickled fragments or unrelated HTTP/2 frames.
#[derive(Default)]
struct Frames {
    head: [u8; 9],
    used: usize,
    remaining: usize,
    payload: [u8; 4],
    payload_used: usize,
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
            let keep = len.min(4 - self.payload_used);
            self.payload[self.payload_used..self.payload_used + keep]
                .copy_from_slice(&bytes[..keep]);
            self.payload_used += keep;
            bytes = &bytes[len..];
            self.remaining -= len;
            if self.remaining != 0 {
                return;
            }
            let kind = self.head[3];
            let flags = self.head[4];
            let stream = u32::from_be_bytes(self.head[5..9].try_into().unwrap()) & 0x7fff_ffff;
            if incoming && kind == 8 && stream == 0 && self.payload_used == 4 {
                state.send_window = state
                    .send_window
                    .saturating_add(i64::from(u32::from_be_bytes(self.payload) & 0x7fff_ffff));
                state.connection_blocked_since = (state.send_window <= 0).then(Instant::now);
            } else if !incoming && kind == 0 {
                let length = (usize::from(self.head[0]) << 16)
                    | (usize::from(self.head[1]) << 8)
                    | usize::from(self.head[2]);
                state.send_window -= length as i64;
                // DATA delivery is connection progress even when it used the
                // last credit. Never borrow a stream's earlier timestamp.
                state.connection_blocked_since = (state.send_window <= 0).then(Instant::now);
            }
            if incoming && matches!(kind, 1 | 9) && flags & 4 != 0 {
                state.partial_head = None;
            }
            self.used = 0;
            self.payload_used = 0;
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
                    Some((at, _, _)) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                // Recheck after the wake: progress at the same instant wins.
                if let Some((_, guard, stream)) = traffic.due().filter(|(at, _, _)| *at <= Instant::now()) {
                    traffic.ingress.warn(guard);
                    let Some(core) = core.upgrade() else { return; };
                    let mut state = traffic.state();
                    if let Some(id) = stream
                        && let Some(progress) = state.deliveries.get_mut(&id)
                        && let Some(cancel) = progress.cancel.clone()
                        && !cancel.done.load(Ordering::Acquire) {
                        progress.reset = true;
                        progress.progressed = Instant::now();
                        drop(state);
                        cancel.cancel();
                        continue;
                    }
                    if guard == "idle connection" && state.protocol == Protocol::Http2 {
                        state.draining = true;
                        state.draining_since.get_or_insert_with(Instant::now);
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
struct StreamProgress {
    pending: usize,
    progressed: Instant,
    cancel: Option<Arc<TaskCancel>>,
    reset: bool,
}

#[derive(Default)]
struct TaskCancel {
    cancelled: AtomicBool,
    done: AtomicBool,
    wake: Notify,
}
impl TaskCancel {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_one();
    }
    async fn cancelled(&self) {
        let notified = self.wake.notified();
        if !self.cancelled.load(Ordering::Acquire) {
            notified.await;
        }
    }
}

struct TaskCompletion(Arc<TaskCancel>);
impl Drop for TaskCompletion {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
    }
}

tokio::task_local! { static STREAM_TASK: Arc<TaskCancel>; }

/// Hyper's H2 request and upgrade tasks have independent cancellation scopes.
/// Cancelling a live producer can reset its send handle. A completed producer
/// may leave EOS DATA queued in h2, so task completion is tracked separately.
#[derive(Clone)]
pub(super) struct StreamExecutor(pub(super) Arc<Traffic>);
impl<F> hyper::rt::Executor<F> for StreamExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        let cancel = Arc::new(TaskCancel::default());
        let task = ProtocolTask::new(self.0.clone());
        tokio::spawn(STREAM_TASK.scope(cancel.clone(), async move {
            let _task = task;
            let _completed = TaskCompletion(cancel.clone());
            tokio::select! { biased; () = cancel.cancelled() => {}, _ = future => {} }
        }));
    }
}

struct ProtocolTask(Arc<Traffic>);
impl ProtocolTask {
    fn new(traffic: Arc<Traffic>) -> Self {
        {
            let mut state = traffic.state();
            state.protocol_tasks += 1;
            state.idle_since = None;
        }
        traffic.changed.notify_one();
        Self(traffic)
    }
}
impl Drop for ProtocolTask {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.protocol_tasks -= 1;
        Traffic::idle(&mut state);
        self.0.changed.notify_one();
    }
}

pub(super) struct Flight {
    traffic: Arc<Traffic>,
    id: u64,
}
impl Flight {
    pub(super) fn close_after_response(&self) {
        let mut state = self.traffic.state();
        state.draining = true;
        state.draining_since.get_or_insert_with(Instant::now);
        self.traffic.idle_expired.store(true, Ordering::Release);
        self.traffic.idle_wake.notify_one();
        self.traffic.changed.notify_one();
    }
    pub(super) fn bind_task(&self) {
        if let Ok(cancel) = STREAM_TASK.try_with(Arc::clone) {
            self.traffic
                .state()
                .deliveries
                .get_mut(&self.id)
                .unwrap()
                .cancel = Some(cancel);
        }
    }
    pub(super) fn new(traffic: Arc<Traffic>) -> Arc<Self> {
        let id = {
            let mut state = traffic.state();
            state.active += 1;
            state.idle_since = None;
            state.first_head = None;
            let id = state.next_delivery;
            state.next_delivery = state.next_delivery.wrapping_add(1);
            state.deliveries.insert(
                id,
                StreamProgress {
                    pending: 0,
                    progressed: Instant::now(),
                    cancel: None,
                    reset: false,
                },
            );
            id
        };
        traffic.changed.notify_one();
        Arc::new(Self { traffic, id })
    }
}
impl Drop for Flight {
    fn drop(&mut self) {
        let mut state = self.traffic.state();
        state.deliveries.remove(&self.id);
        state.active -= 1;
        Traffic::idle(&mut state);
        self.traffic.changed.notify_one();
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
}
impl Delivery {
    fn new(bytes: Bytes, flight: Arc<Flight>) -> Self {
        if !bytes.is_empty() {
            let mut state = flight.traffic.state();
            if state.send_window <= 0 && !state.deliveries.values().any(|progress| progress.pending != 0) {
                state.connection_blocked_since = Some(Instant::now());
            }
            let progress = state.deliveries.get_mut(&flight.id).unwrap();
            let starting = progress.pending == 0;
            if starting {
                progress.progressed = Instant::now();
            }
            progress.pending = progress.pending.saturating_add(bytes.len());
            if starting {
                flight.traffic.changed.notify_one();
            }
        }
        Self { bytes, flight }
    }
    fn consumed(&self, count: usize, made_progress: bool) {
        if count == 0 {
            return;
        }
        let mut state = self.flight.traffic.state();
        let progress = state.deliveries.get_mut(&self.flight.id).unwrap();
        progress.pending = progress.pending.saturating_sub(count);
        if made_progress {
            progress.progressed = Instant::now();
        }
        // Extending an armed deadline needs no wake: its old timer rechecks.
        if progress.pending == 0 {
            self.flight.traffic.changed.notify_one();
        }
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
        self.consumed(count, true);
    }
}
impl Drop for Delivery {
    fn drop(&mut self) {
        self.consumed(self.bytes.len(), false);
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
        }
        result
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        slices: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, slices);
        if let Poll::Ready(Ok(mut count)) = result {
            for slice in slices {
                let used = count.min(slice.len());
                this.traffic.written(&slice[..used]);
                count -= used;
                if count == 0 {
                    break;
                }
            }
        }
        result
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Observes only the TCP transport, below TLS encryption and buffering.
/// Plaintext frame observation stays in Guarded, above TLS.
pub(super) struct SocketIo<T> {
    pub(super) inner: T,
    pub(super) traffic: Arc<Traffic>,
}
impl<T: AsyncRead + Unpin> AsyncRead for SocketIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buffer)
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for SocketIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, bytes);
        match result {
            Poll::Ready(Ok(count)) => this.traffic.socket_written(count),
            Poll::Pending => this.traffic.pending_write(true),
            Poll::Ready(Err(_)) => this.traffic.pending_write(false),
        }
        result
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        slices: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, slices);
        match result {
            Poll::Ready(Ok(count)) => this.traffic.socket_written(count),
            Poll::Pending => this.traffic.pending_write(true),
            Poll::Ready(Err(_)) => this.traffic.pending_write(false),
        }
        result
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests;
