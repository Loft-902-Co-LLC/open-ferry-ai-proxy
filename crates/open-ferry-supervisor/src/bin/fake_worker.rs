//! Feature-only process-test worker, not a real open-ferry worker.
//!
//! `fake_worker -worker -config <file> -worker-control <control>` takes its
//! inherited transport before creating a runtime. Config contents are ignored;
//! welcome supplies the data directory. `test-marker` beside current_exe,
//! trimmed, supplies the x-open-ferry-test-marker HTTP header (absent: version).
//!
//! Fault files under `<data_dir>/.blue-green-test/fault.<kind>` contain trimmed
//! `<value>[@<marker>]`; presence faults accept empty or `@<marker>`.
//! fail-load, fail-before-ready, ignore-handover and hold-refresh are presence
//! faults. exit-after = welcome|loaded|handed_over|ready|go exits 42 immediately;
//! slow-ready = decimal milliseconds; drop-channel = taking-over|watching|
//! handing-over leaves the lease/process alive until killed (600s safety net).
//! hold-refresh waits at refresh.before_save between gate_closing and gate_closed.
//! signins-pending = decimal count: while the file exists (and matches the marker)
//! every status/ping answer reports `signins_pending: N`; removing it makes the next
//! answer report 0. It is re-read on each answer, so no barrier is needed.
//! spawn-grandchild is a fake-only presence fault (like send-fail, outside the fixed
//! event set): when the worker becomes active (not on a resume) it starts one child
//! process of its own, this binary in a sleep-only mode (`-grandchild-sleep`, at most
//! 10 minutes, no breakaway from a Windows job), and logs the event `grandchild`
//! with a `child_pid` field.
//! send-fail is a fake-only presence fault, outside L7/L9; the real worker's
//! send-failure path is covered by its own machine tests.
//! Barriers use test_seams::hit in spawn_blocking. A failed step stops the
//! transition, not the process. Resume aborts and awaits the continuation,
//! discarding a held blocking barrier's eventual result.
//! Management's 503 ends before resumed is sent.
//!
//! events.log contains JSON lines {pid, marker, event, lease}, one append per
//! line. Fixed events: loaded, active, standby, handover.written,
//! handover_failed, handed_over, resumed, ready, go, draining, drained, peers_gone, and
//! barrier arrivals: handover.gate_closing, refresh.before_save,
//! handover.gate_closed, lease.release, take_over.before_reload.
//! Serving starts at active welcome and ready; go only starts background work.
//! An already-closed gate skips both gate barriers and their arrival events.
//! Resume to a worker that began a hand over in taking-over or ready (L9.16),
//! completed or failed, is invalid and ignored; no process test relies on resuming it.
//! peers_gone logs only in active/watching, once until a hand over resets cleaning.
//! Release drains for at most welcome.drain_ms; stop/EOF retains the lease until exit.
//! `live` counts accepted connections not yet closed, from the TCP accept.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use open_ferry_supervisor::protocol::{Message, Role, VERSION};
use open_ferry_supervisor::timings::STATUS_EVERY;
use open_ferry_supervisor::worker::{self, Hello};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

#[derive(Clone)]
struct Context {
    data_dir: PathBuf,
    marker: String,
}

impl Context {
    fn fault(&self, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(
            self.data_dir
                .join(".blue-green-test")
                .join(format!("fault.{name}")),
        )
        .ok()?;
        let text = text.trim();
        match text.rsplit_once('@') {
            Some((value, marker)) if marker == self.marker => Some(value.to_owned()),
            Some(_) => None,
            None => Some(text.to_owned()),
        }
    }

    fn event(&self, name: &str, lease: bool) -> io::Result<()> {
        self.event_with(name, lease, None)
    }

    /// `child_pid` is only set by the `grandchild` event.
    fn event_with(&self, name: &str, lease: bool, child_pid: Option<u32>) -> io::Result<()> {
        let mut value = serde_json::json!({
            "pid": std::process::id(), "marker": self.marker,
            "event": name, "lease": lease
        });
        if let (Some(pid), Some(object)) = (child_pid, value.as_object_mut()) {
            object.insert("child_pid".to_owned(), pid.into());
        }
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.data_dir.join(".blue-green-test/events.log"))?
            .write_all(&bytes)
    }

    /// The `spawn-grandchild` fault: one sleep-only child of this process.
    fn spawn_grandchild(&self) -> io::Result<()> {
        if self.fault("spawn-grandchild").is_none() {
            return Ok(());
        }
        let child = std::process::Command::new(std::env::current_exe()?)
            .arg("-grandchild-sleep")
            .arg(std::process::id().to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        self.event_with("grandchild", false, Some(child.id()))
    }

    fn exit_after(&self, name: &str) {
        if self.fault("exit-after").as_deref() == Some(name) {
            std::process::exit(42);
        }
    }

    async fn barrier(&self, name: &'static str) -> io::Result<()> {
        self.event(name, true)?;
        let root = self.data_dir.clone();
        tokio::task::spawn_blocking(move || open_ferry_update::test_seams::hit(&root, name))
            .await
            .map_err(io::Error::other)?
    }

    fn clear_arrivals(&self, names: &[&str]) -> io::Result<()> {
        for name in names {
            match std::fs::remove_file(
                self.data_dir
                    .join(".blue-green-test")
                    .join(format!("{name}.arrived")),
            ) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn acquire(&self) -> io::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.data_dir.join("active-worker.lock"))?;
        // Rust opens files with close-on-exec / non-inheritable handles.
        file.try_lock().map_err(io::Error::other)?;
        Ok(file)
    }

    async fn acquire_until(&self, budget: u64) -> io::Result<File> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(budget);
        loop {
            match self.acquire() {
                Ok(file) => return Ok(file),
                Err(error) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }

    fn write_handover(&self) -> io::Result<String> {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = self.data_dir.join(format!("handover.{id}.tmp"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| {
            let mut file = options.open(&temporary)?;
            file.write_all(&serde_json::to_vec(&serde_json::json!({
                "id": id, "at": chrono::Utc::now().to_rfc3339()
            }))?)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, self.data_dir.join("handover.json"))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
        self.event("handover.written", true)?;
        Ok(id)
    }
}

fn flag(args: &[OsString], name: &str) -> Option<OsString> {
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1).cloned()
}

fn exit_on_panic() {
    // Do not unwind the lease or wait for held spawn_blocking barriers.
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        std::process::exit(101);
    }));
}

/// The grandchild's whole life: up to 10 minutes. On Unix it also ends when its
/// parent does (a job object ends it on Windows), so a killed worker leaves
/// nothing behind. The parent's pid comes from the worker as an argument, so a
/// worker that died before this process looked at its own parent is noticed
/// too: the check is at once and then every second.
fn grandchild_sleep(parent: Option<u32>) {
    #[cfg(unix)]
    let gone = || {
        // SAFETY: `getppid` has no preconditions.
        let now = unsafe { libc::getppid() };
        parent.is_some_and(|parent| u32::try_from(now).ok() != Some(parent))
    };
    #[cfg(not(unix))]
    let gone = || parent.is_none();
    if gone() {
        return;
    }
    for _ in 0..600 {
        std::thread::sleep(Duration::from_secs(1));
        if gone() {
            return;
        }
    }
}

fn main() {
    exit_on_panic();
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "-grandchild-sleep")
    {
        let parent = std::env::args()
            .nth(2)
            .and_then(|pid| pid.parse::<u32>().ok());
        grandchild_sleep(parent);
        return;
    }
    if let Err(error) = start() {
        eprintln!("fake worker: {error}");
        std::process::exit(1);
    }
}

fn start() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let _config = flag(&args, "-config").ok_or("missing -config")?;
    let control = flag(&args, "-worker-control").ok_or("missing -worker-control")?;
    if !args.iter().any(|arg| arg == "-worker") {
        return Err("missing -worker".into());
    }
    let pending = worker::take(&control)?;
    let version = env!("CARGO_PKG_VERSION");
    let marker =
        match std::fs::read_to_string(std::env::current_exe()?.with_file_name("test-marker")) {
            Ok(marker) => marker.trim().to_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => version.to_owned(),
            Err(error) => return Err(error.into()),
        };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut lease = None;
    let result = runtime.block_on(run(pending, marker, version, &mut lease));
    // A stop must not await a held barrier's independent blocking thread.
    runtime.shutdown_timeout(Duration::ZERO);
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("fake worker: {error}");
            1
        }
    };
    // Retain the lease through runtime shutdown and let the OS release it at exit.
    std::process::exit(code);
}

async fn accept(listener: Option<&TcpListener>) -> io::Result<TcpStream> {
    match listener {
        Some(listener) => listener.accept().await.map(|(stream, _)| stream),
        None => std::future::pending().await,
    }
}

async fn serve(mut stream: TcpStream, marker: String, management_closed: bool) -> io::Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|part| part == b"\r\n\r\n") {
            break;
        }
        if request.len() > 64 * 1024 {
            return Err(io::Error::other("request headers too large"));
        }
    }
    let status = if management_closed {
        "503 Service Unavailable"
    } else {
        "200 OK"
    };
    stream.write_all(format!(
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nx-open-ferry-test-marker: {marker}\r\nConnection: close\r\n\r\n"
    ).as_bytes()).await?;
    stream.shutdown().await
}

async fn finish(connections: &mut JoinSet<io::Result<()>>, budget_ms: u64) {
    let _ = tokio::time::timeout(Duration::from_millis(budget_ms), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

enum Completed {
    HandedOver(String),
    Ready,
    Dropped,
    Active(File, bool),
}
#[derive(Clone, Copy)]
enum Kind {
    Handover,
    TakeOver,
    LeaseWait,
}
struct Transition {
    task: JoinHandle<io::Result<Completed>>,
    kind: Kind,
}

async fn cancel(transition: &mut Option<Transition>, lease: &mut Option<File>) {
    if let Some(transition) = transition.take() {
        transition.task.abort();
        if let Ok(Ok(Completed::Active(file, _))) = transition.task.await {
            *lease = Some(file);
        }
    }
}

/// `signins-pending` is re-read from its fault file on every answer, so a test
/// that deletes the file sees the next answer report 0.  Absent, empty or
/// unparseable counts as 0.
fn status(ctx: &Context, phase: &str, live: usize) -> Message {
    let signins_pending = ctx
        .fault("signins-pending")
        .and_then(|count| count.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Message::Status {
        mode: "fake".into(),
        live: live as u64,
        signins_pending,
        phase: phase.into(),
    }
}

async fn run(
    pending: worker::Pending,
    marker: String,
    version: &str,
    lease: &mut Option<File>,
) -> io::Result<()> {
    let mut attached = pending
        .attach(Hello {
            max_version: VERSION,
            min_supervisor: VERSION,
            version: version.into(),
            caps: open_ferry_supervisor::protocol::REQUIRED_CAPS
                .iter()
                .map(|cap| (*cap).to_owned())
                .collect(),
        })
        .await
        .map_err(io::Error::other)?;
    let ctx = Context {
        data_dir: attached.welcome.installation.data_dir.clone(),
        marker,
    };
    std::fs::create_dir_all(ctx.data_dir.join(".blue-green-test"))?;
    ctx.exit_after("welcome");
    let mut listeners = Vec::new();
    for (_, listener) in attached.listeners {
        listeners.push(TcpListener::from_std(listener)?);
    }
    let mut serving = false;
    let mut phase = "loading";
    let mut connections = JoinSet::new();
    let mut transition: Option<Transition> = None;
    macro_rules! send {
        ($message:expr) => {{
            let result = if ctx.fault("send-fail").is_some() {
                Err(io::Error::other("injected send failure"))
            } else {
                tokio::time::timeout(
                    Duration::from_millis(attached.welcome.budgets.stop_ms),
                    attached.channel.send($message),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "control send timeout",
                    ))
                })
            };
            if result.is_err() {
                cancel(&mut transition, lease).await;
                listeners.clear();
                finish(&mut connections, attached.welcome.budgets.stop_ms).await;
                return Ok(());
            }
        }};
    }
    send!(&status(&ctx, phase, 0));
    if ctx.fault("fail-load").is_some() {
        send!(&Message::LoadFailed {
            reason: "fail-load".into(),
        });
        return Err(io::Error::other("fail-load"));
    }
    if attached.welcome.role == Role::Standby {
        if ctx.fault("hold-load").is_some() {
            ctx.barrier("load.before_loaded").await?;
        }
        send!(&Message::Loaded { ms: 0 });
        ctx.event("loaded", false)?;
        ctx.exit_after("loaded");
        phase = "standby";
        ctx.event("standby", false)?;
    } else {
        let ctx = ctx.clone();
        let budget = attached.welcome.budgets.load_ms;
        transition = Some(Transition {
            kind: Kind::LeaseWait,
            task: tokio::spawn(async move {
                Ok(Completed::Active(ctx.acquire_until(budget).await?, false))
            }),
        });
    }
    if phase != "loading" {
        send!(&status(&ctx, phase, 0));
    }
    let mut peers_gone = false;
    let mut handover_gate_open = false;
    let mut handover_resumable = false;
    let mut drain_deadline = None;
    let (pause_tx, mut pause_rx) = mpsc::channel::<oneshot::Sender<()>>(1);
    let mut ticks =
        tokio::time::interval_at(tokio::time::Instant::now() + STATUS_EVERY, STATUS_EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let old_phase = phase;
        tokio::select! {
            biased;
            frame = attached.channel.receive::<Message>() => {
                let frame = match frame {
                    Ok(Some(frame)) => frame,
                    Ok(None) | Err(_) => {
                        cancel(&mut transition, lease).await;
                        listeners.clear();
                        finish(&mut connections, attached.welcome.budgets.stop_ms).await;
                        return Ok(());
                    }
                };
                match frame.message {
                    Message::HandOver { .. } if matches!(phase, "active" | "watching" | "taking-over" | "ready") => {
                        if ctx.fault("ignore-handover").is_some() { continue; }
                        cancel(&mut transition, lease).await;
                        while pause_rx.try_recv().is_ok() {}
                        ctx.clear_arrivals(&["handover.gate_closing", "refresh.before_save", "handover.gate_closed", "lease.release"])?;
                        handover_resumable = matches!(phase, "active" | "watching");
                        handover_gate_open = serving;
                        peers_gone = false;
                        phase = "handing-over";
                        if ctx.fault("drop-channel").as_deref() == Some(phase) {
                            drop(attached.channel);
                            tokio::time::sleep(Duration::from_secs(600)).await;
                            return Ok(());
                        }
                        let ctx = ctx.clone();
                        let pause = pause_tx.clone();
                        let gate_open = serving;
                        transition = Some(Transition { kind: Kind::Handover, task: tokio::spawn(async move {
                            if gate_open {
                                ctx.barrier("handover.gate_closing").await?;
                                if ctx.fault("hold-refresh").is_some() { ctx.barrier("refresh.before_save").await?; }
                                ctx.barrier("handover.gate_closed").await?;
                            }
                            let id = ctx.write_handover()?;
                            let (tx, rx) = oneshot::channel();
                            pause.send(tx).await.map_err(io::Error::other)?;
                            rx.await.map_err(io::Error::other)?;
                            ctx.barrier("lease.release").await?;
                            Ok(Completed::HandedOver(id))
                        }) });
                    }
                    Message::Resume if matches!(phase, "active" | "watching") && lease.is_some() && transition.is_none() => {
                        send!(&Message::Resumed);
                        ctx.event("resumed", true)?;
                    }
                    Message::Resume if matches!(phase, "handing-over" | "handed-over") && handover_resumable && !matches!(transition.as_ref().map(|t| t.kind), Some(Kind::LeaseWait)) => {
                        cancel(&mut transition, lease).await;
                        // Drain pause requests from the cancelled continuation before restarting.
                        while pause_rx.try_recv().is_ok() {}
                        if lease.is_none() {
                            let ctx = ctx.clone();
                            let budget = attached.welcome.budgets.resume_ms;
                            transition = Some(Transition { kind: Kind::LeaseWait, task: tokio::spawn(async move {
                                Ok(Completed::Active(ctx.acquire_until(budget).await?, true))
                            }) });
                        } else {
                            phase = "active";
                            serving = true;
                            send!(&Message::Resumed);
                            ctx.event("resumed", true)?;
                        }
                    }
                    Message::TakeOver if phase == "standby" => {
                        ctx.clear_arrivals(&["take_over.before_reload"])?;
                        phase = "taking-over";
                        match ctx.acquire() {
                            Err(error) => {
                                send!(&status(&ctx, phase, connections.len()));
                                send!(&Message::TakeFailed { reason: format!("lease: {error}") });
                                phase = "standby";
                                send!(&status(&ctx, phase, connections.len()));
                            }
                            Ok(file) => {
                                *lease = Some(file);
                                let ctx = ctx.clone();
                                transition = Some(Transition { kind: Kind::TakeOver, task: tokio::spawn(async move {
                                    ctx.barrier("take_over.before_reload").await?;
                                    let path = ctx.data_dir.join("handover.json");
                                    let read = (|| -> io::Result<()> {
                                        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                                        let id = value["id"].as_str().ok_or_else(|| io::Error::other("missing id"))?;
                                        if id.len() != 32 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
                                            return Err(io::Error::other("invalid id"));
                                        }
                                        std::fs::remove_file(path)
                                    })();
                                    read.map_err(|error| io::Error::other(format!("handover: {error}")))?;
                                    if ctx.fault("drop-channel").as_deref() == Some("taking-over") { return Ok(Completed::Dropped); }
                                    if let Some(delay) = ctx.fault("slow-ready") {
                                        let delay: u64 = delay.parse().map_err(io::Error::other)?;
                                        tokio::time::sleep(Duration::from_millis(delay)).await;
                                    }
                                    if ctx.fault("fail-before-ready").is_some() { return Err(io::Error::other("fail-before-ready")); }
                                    Ok(Completed::Ready)
                                }) });
                            }
                        }
                    }
                    Message::Go if phase == "ready" => {
                        phase = "watching";
                        ctx.event("go", true)?;
                        ctx.exit_after("go");
                        if ctx.fault("drop-channel").as_deref() == Some(phase) {
                            drop(attached.channel);
                            tokio::time::sleep(Duration::from_secs(600)).await;
                            return Ok(());
                        }
                    }
                    Message::PeersGone if matches!(phase, "active" | "watching") => {
                        if peers_gone {
                            tracing::debug!(phase, "repeated peers_gone ignored");
                        } else {
                            peers_gone = true;
                            ctx.event("peers_gone", lease.is_some())?;
                        }
                    }
                    Message::Release if transition.is_none() && matches!(phase, "handed-over" | "active" | "watching" | "ready") => {
                        serving = false;
                        listeners.clear();
                        phase = "draining";
                        drain_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(attached.welcome.budgets.drain_ms));
                        ctx.event("draining", lease.is_some())?;
                    }
                    Message::Stop { budget_ms } => {
                        cancel(&mut transition, lease).await;
                        listeners.clear();
                        finish(&mut connections, budget_ms).await;
                        return Ok(());
                    }
                    Message::Ping => send!(&status(&ctx, phase, connections.len())),
                    Message::Status { .. } => {}
                    Message::Unknown => send!(&Message::Unsupported { message_type: frame.message_type }),
                    message => tracing::debug!(?message, phase, "invalid worker message ignored"),
                }
            }
            result = async { (&mut transition.as_mut().unwrap().task).await }, if transition.is_some() => {
                let kind = transition.take().unwrap().kind;
                match result.map_err(io::Error::other)? {
                    Ok(Completed::Active(file, resumed)) => {
                        *lease = Some(file);
                        phase = "active";
                        serving = true;
                        if resumed {
                            send!(&Message::Resumed);
                            ctx.event("resumed", true)?;
                        } else {
                            // A worker reports `loaded` once it has loaded, in
                            // either role: a standby before its hold, an active
                            // start once it is serving, before its first
                            // status as active. A resume sends `resumed` only.
                            send!(&Message::Loaded { ms: 0 });
                            ctx.event("active", true)?;
                            ctx.spawn_grandchild()?;
                        }
                    }
                    Ok(Completed::HandedOver(id)) => {
                        drop(lease.take());
                        phase = "handed-over";
                        send!(&Message::HandedOver { state_id: id, live: connections.len() as u64 });
                        ctx.event("handed_over", false)?;
                        ctx.exit_after("handed_over");
                    }
                    Ok(Completed::Ready) => {
                        phase = "ready";
                        serving = true;
                        send!(&Message::Ready);
                        ctx.event("ready", true)?;
                        ctx.exit_after("ready");
                    }
                    Ok(Completed::Dropped) => {
                        drop(attached.channel);
                        tokio::time::sleep(Duration::from_secs(600)).await;
                        return Ok(());
                    }
                    Err(error) if matches!(kind, Kind::LeaseWait) => return Err(error),
                    Err(error) if matches!(kind, Kind::Handover) => {
                        // L9.4 restores accept for active/watching workers; L9.16
                        // leaves ready/taking-over workers' accept as the failed step found it.
                        if handover_resumable {
                            serving = handover_gate_open;
                        }
                        ctx.event("handover_failed", lease.is_some())?;
                        tracing::debug!(%error, "handover failed; awaiting resume");
                    }
                    Err(error) => {
                        send!(&Message::TakeFailed { reason: error.to_string() });
                        // Failed activation retains its lease until stop; not eligible for commands.
                    }
                }
            }
            Some(ack) = pause_rx.recv() => {
                serving = false;
                let _ = ack.send(());
            }
            _ = ticks.tick() => send!(&status(&ctx, phase, connections.len())),
            stream = accept(listeners.first()), if serving => match stream {
                Ok(stream) => { connections.spawn(serve(stream, ctx.marker.clone(), false)); }
                Err(error) => tracing::debug!(%error, "transient accept error"),
            },
            stream = accept(listeners.get(1)), if serving => match stream {
                Ok(stream) => { connections.spawn(serve(stream, ctx.marker.clone(), phase == "handing-over")); }
                Err(error) => tracing::debug!(%error, "transient accept error"),
            },
            _ = tokio::time::sleep_until(drain_deadline.unwrap_or_else(tokio::time::Instant::now)), if drain_deadline.is_some() => {
                connections.abort_all();
                while connections.join_next().await.is_some() {}
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
        if phase != old_phase {
            send!(&status(&ctx, phase, connections.len()));
        }
        if phase == "draining" && connections.is_empty() {
            send!(&Message::Drained);
            ctx.event("drained", lease.is_some())?;
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Keep a fork-to-exec child from briefly inheriting another test's lease.
    static LEASE_AND_SPAWN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn panic_exits_without_waiting_for_a_held_blocking_task() {
        let _guard = LEASE_AND_SPAWN.blocking_lock();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::panic_with_held_blocking_task",
                "--ignored",
                "--nocapture",
            ])
            .env("FAKE_WORKER_PANIC_FIXTURE", "1")
            .env("RUST_BACKTRACE", "0")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        fn read_pipe(
            mut pipe: impl std::io::Read + Send + 'static,
        ) -> std::thread::JoinHandle<Vec<u8>> {
            std::thread::spawn(move || {
                let mut output = Vec::new();
                let _ = pipe.read_to_end(&mut output);
                output
            })
        }
        let stdout = read_pipe(child.stdout.take().unwrap());
        let stderr = read_pipe(child.stderr.take().unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (exit, timed_out) = loop {
            if let Some(exit) = child.try_wait().unwrap() {
                break (exit, false);
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                break (child.wait().unwrap(), true);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = stdout.join().unwrap();
        let stderr = stderr.join().unwrap();
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);
        let output = format!("stdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(
            !timed_out,
            "panicking fake waited for a held blocking task; {output}"
        );
        assert_eq!(exit.code(), Some(101), "{output}");
        assert!(!stdout.contains("test result:"), "{output}");
        assert!(
            stderr.contains("panic with a held blocking task"),
            "{output}"
        );
    }

    #[test]
    #[ignore = "subprocess fixture for the panic-hook test"]
    fn panic_with_held_blocking_task() {
        if std::env::var_os("FAKE_WORKER_PANIC_FIXTURE").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return;
        }
        exit_on_panic();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn_blocking(move || {
            tx.send(()).unwrap();
            std::thread::park();
        });
        rx.recv().unwrap();
        panic!("panic with a held blocking task");
    }

    #[tokio::test]
    async fn cancel_retains_a_completed_lease_wait() {
        let _guard = LEASE_AND_SPAWN.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context {
            data_dir: dir.path().to_owned(),
            marker: "unit".into(),
        };
        let file = ctx.acquire().unwrap();
        let task = tokio::spawn(async move { Ok(Completed::Active(file, false)) });
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut transition = Some(Transition {
            task,
            kind: Kind::LeaseWait,
        });
        let mut lease = None;
        cancel(&mut transition, &mut lease).await;
        assert!(lease.is_some());
        assert!(ctx.acquire().is_err());
        drop(lease);
        assert!(ctx.acquire().is_ok());
    }
}
