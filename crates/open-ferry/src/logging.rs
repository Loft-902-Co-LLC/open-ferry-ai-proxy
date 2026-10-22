// Ported from the log setup in CLIProxyAPI internal/logging and
// internal/util's SetLogLevel (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Logging, at debug level when the config's `debug` is on and at info
//! level otherwise. A reload can change the level. Where the lines go, and
//! how they are written, is the main log's output (see
//! [`crate::file_log`]).
//!
//! Deviations from upstream:
//! - Debug level applies to this project's crates only; libraries such as
//!   the HTTP client stay at info, as their debug output would bury ours.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Registry, reload};

use crate::file_log::{self, FileLog, LogControls};

/// Changes the installed logger: its level, and through
/// [`LogLevel::file_log`] where its lines go.
#[derive(Clone)]
pub struct LogLevel {
    level: reload::Handle<Targets, Registry>,
    file_log: FileLog,
}

impl LogLevel {
    /// Logs at debug level when `debug`, and at info level otherwise.
    pub fn set_debug(&self, debug: bool) {
        if let Err(error) = self.level.reload(targets(debug)) {
            tracing::warn!("failed to change the log level: {error}");
        }
    }

    /// Where the lines go.
    pub fn file_log(&self) -> &FileLog {
        &self.file_log
    }

    /// A level for a logger that was never installed, for tests.
    #[cfg(test)]
    pub fn detached() -> Self {
        Self {
            level: reload::Layer::new(targets(false)).1,
            file_log: FileLog::default(),
        }
    }

    /// Not upstream's: like [`detached`](Self::detached), with controls of its
    /// own that start as a supervised worker's do (rotation and cleaning
    /// off), so a test's state is its own, not the process's.
    #[cfg(test)]
    pub fn detached_supervised() -> Self {
        Self {
            level: reload::Layer::new(targets(false)).1,
            file_log: FileLog::with_controls(LogControls::supervised()),
        }
    }
}

/// Installs the logger at info level, writing to the main log's output.
pub fn init() -> LogLevel {
    let (filter, level) = reload::Layer::new(targets(false));
    let (output, file_log) = file_log::init();
    tracing_subscriber::registry()
        .with(filter)
        .with(output)
        .init();
    LogLevel { level, file_log }
}

/// Installs the logger as `init` does, for a supervised worker: the main
/// log's rotation and cleaning start off (`LogControls::supervised`), and
/// the worker turns them on through `LogLevel::file_log`'s controls.
pub fn init_supervised() -> LogLevel {
    let (filter, level) = reload::Layer::new(targets(false));
    let (output, file_log) = file_log::init_with_controls(LogControls::supervised());
    tracing_subscriber::registry()
        .with(filter)
        .with(output)
        .init();
    LogLevel { level, file_log }
}

/// The level of the supervisor's logger, which writes to standard error
/// only. The supervisor never writes `main.log`: that is the workers'.
#[derive(Clone)]
pub struct StderrLevel {
    level: reload::Handle<Targets, Registry>,
    writer: BoundedWriter,
}

/// How long the supervisor's exit waits for its log lines to be written.
const FLUSH_BOUND: Duration = Duration::from_secs(1);

/// Lines the supervisor's log holds while standard error is slow.
const QUEUE_LINES: usize = 1024;

impl StderrLevel {
    /// Logs at debug level when `debug`, and at info level otherwise.
    pub fn set_debug(&self, debug: bool) {
        if let Err(error) = self.level.reload(targets(debug)) {
            tracing::warn!("failed to change the log level: {error}");
        }
    }

    /// Runs `body`, then waits up to a second for the lines queued so far to
    /// be written, on every way out; a stalled standard error does not hold
    /// the exit longer than that. A panic that escapes `body` has already
    /// queued its report through the panic hook, and is flushed before the
    /// unwind resumes. A report from an abort (`panic=abort`, or a panic
    /// while panicking) never reaches here and is best effort.
    pub fn flush_after<R>(&self, body: impl FnOnce() -> R) -> R {
        flush_after(&self.writer, FLUSH_BOUND, body)
    }
}

fn flush_after<R>(writer: &BoundedWriter, bound: Duration, body: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => {
            writer.flush(bound);
            result
        }
        Err(payload) => {
            writer.flush(bound);
            std::panic::resume_unwind(payload)
        }
    }
}

/// Installs the supervisor's logger at info level: lines go to standard
/// error, with the same level targets as `init`, through a bounded queue and
/// a writer thread, so a stalled standard error never blocks the code that
/// logs. Nothing is written to a file.
pub fn init_stderr() -> StderrLevel {
    let (filter, level) = reload::Layer::new(targets(false));
    let writer = BoundedWriter::new(std::io::stderr(), QUEUE_LINES);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer.clone())
                .with_ansi(false),
        )
        .init();
    // The default panic hook writes standard error directly, which blocks
    // when it is stalled, before the supervisor can catch the panic.
    std::panic::set_hook(panic_hook(writer.clone()));
    StderrLevel { level, writer }
}

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// A panic hook that queues the panic's message and location as an error
/// line, with a backtrace when `RUST_BACKTRACE` asks for one, and writes
/// nothing to standard error itself.
fn panic_hook(writer: BoundedWriter) -> PanicHook {
    Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic with no message".to_owned());
        let place = info
            .location()
            .map(|at| format!("{}:{}:{}", at.file(), at.line(), at.column()))
            .unwrap_or_else(|| "an unknown place".to_owned());
        let thread = std::thread::current();
        let mut text = format!(
            "ERROR panic in thread '{}' at {place}: {message}\n",
            thread.name().unwrap_or("<unnamed>")
        );
        let backtrace = std::backtrace::Backtrace::capture();
        if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
            text.push_str(&format!("{backtrace}\n"));
        }
        writer.queue_report(text);
    })
}

/// A log writer that never blocks its caller. Each event's line goes into a
/// queue of at most `capacity` lines that a dedicated thread writes to the
/// sink. When the queue is full the line is dropped and counted, and the
/// writer thread reports the count in a line of its own before the next one.
#[derive(Clone)]
struct BoundedWriter {
    shared: Arc<WriterShared>,
}

struct WriterShared {
    state: Mutex<WriterState>,
    changed: Condvar,
    capacity: usize,
    /// Panic reports given up because the queue's lock was held (see
    /// `push_report`), folded into the next drop notice.
    abandoned: AtomicU64,
    /// Runs once on the writer thread, with the lock held, between its check
    /// for work and its wait.
    #[cfg(test)]
    before_wait: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// A flush has gone into its wait; read without the lock.
    #[cfg(test)]
    flushing: std::sync::atomic::AtomicBool,
}

/// How often an idle writer rechecks for an abandoned report. Short enough
/// that the notice follows the panic closely, long enough to cost nothing.
const RECHECK: Duration = Duration::from_millis(200);

#[derive(Default)]
struct WriterState {
    lines: VecDeque<Vec<u8>>,
    dropped: u64,
    /// The writer thread is inside a write to the sink.
    busy: bool,
    #[cfg(test)]
    closed: bool,
    /// The writer thread is inside its wait for work.
    #[cfg(test)]
    waiting: bool,
}

impl WriterShared {
    /// The state is plain data a panic cannot leave half changed.
    fn lock(&self) -> MutexGuard<'_, WriterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, line: Vec<u8>) {
        let mut state = self.lock();
        Self::admit(&mut state, self.capacity, line);
        drop(state);
        self.changed.notify_all();
    }

    /// A drop count never panics under the lock, even in a debug build.
    fn admit(state: &mut WriterState, capacity: usize, line: Vec<u8>) {
        if state.lines.len() >= capacity {
            state.dropped = state.dropped.saturating_add(1);
        } else {
            state.lines.push_back(line);
        }
    }

    /// [`push`](Self::push) for a panic report, which runs on the panicking
    /// thread: that thread may already hold the lock (a panic inside `push`
    /// or the writer's own region), and a second lock on it would never
    /// return. The lock is only tried; when it is held the report is given up
    /// and counted outside it, for the next drop notice. A poisoned lock is
    /// taken: its data is plain.
    fn push_report(&self, line: Vec<u8>) {
        let state = match self.state.try_lock() {
            Ok(state) => Some(state),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        match state {
            Some(mut state) => Self::admit(&mut state, self.capacity, line),
            None => {
                let mut seen = self.abandoned.load(Ordering::SeqCst);
                while let Err(now) = self.abandoned.compare_exchange_weak(
                    seen,
                    seen.saturating_add(1),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    seen = now;
                }
            }
        }
        self.changed.notify_all();
    }
}

impl BoundedWriter {
    fn new<W: Write + Send + 'static>(sink: W, capacity: usize) -> Self {
        Self::with_recheck(sink, capacity, RECHECK)
    }

    fn with_recheck<W: Write + Send + 'static>(
        mut sink: W,
        capacity: usize,
        recheck: Duration,
    ) -> Self {
        let shared = Arc::new(WriterShared {
            state: Mutex::new(WriterState::default()),
            changed: Condvar::new(),
            capacity,
            abandoned: AtomicU64::new(0),
            #[cfg(test)]
            before_wait: Mutex::new(None),
            #[cfg(test)]
            flushing: std::sync::atomic::AtomicBool::new(false),
        });
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("stderr-log".to_owned())
            .spawn(move || {
                let shared = thread_shared;
                loop {
                    let mut state = shared.lock();
                    let job = loop {
                        // The drop count and one event are taken together
                        // and written together, notice first, so a flood of
                        // drops never keeps the admitted lines waiting.
                        let abandoned = shared.abandoned.load(Ordering::SeqCst);
                        if state.dropped > 0 || abandoned > 0 || !state.lines.is_empty() {
                            let count = std::mem::take(&mut state.dropped)
                                .saturating_add(shared.abandoned.swap(0, Ordering::SeqCst));
                            let mut job = if count > 0 {
                                format!(
                                    "{count} log lines were dropped: standard error is not keeping up\n"
                                )
                                .into_bytes()
                            } else {
                                Vec::new()
                            };
                            if let Some(line) = state.lines.pop_front() {
                                job.extend_from_slice(&line);
                            }
                            break job;
                        }
                        #[cfg(test)]
                        if state.closed {
                            return;
                        }
                        #[cfg(test)]
                        {
                            let hook = shared
                                .before_wait
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .take();
                            if let Some(hook) = hook {
                                hook();
                            }
                            state.waiting = true;
                        }
                        // A bounded wait: an abandoned report is counted
                        // without the lock, so its notify can land between
                        // the check above and this wait and be lost. The
                        // recheck picks the count up regardless.
                        state = shared
                            .changed
                            .wait_timeout(state, recheck)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0;
                        #[cfg(test)]
                        {
                            state.waiting = false;
                        }
                    };
                    state.busy = true;
                    drop(state);
                    let _ = sink.write_all(&job);
                    let _ = sink.flush();
                    shared.lock().busy = false;
                    shared.changed.notify_all();
                }
            });
        // Without the thread every line is dropped and counted, which is the
        // same as a standard error that never moves.
        let _ = spawned;
        Self { shared }
    }

    /// Waits up to `bound` for the queue to be empty and written. `false`
    /// when it was not by then.
    fn flush(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        let mut state = self.shared.lock();
        while !(state.lines.is_empty()
            && state.dropped == 0
            && self.shared.abandoned.load(Ordering::SeqCst) == 0
            && !state.busy)
        {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            #[cfg(test)]
            {
                self.shared.flushing.store(true, Ordering::SeqCst);
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Queues a panic report as a line of its own. Safe to call from a panic
    /// hook on any thread, whatever that thread holds.
    fn queue_report(&self, text: String) {
        self.shared.push_report(text.into_bytes());
    }

    #[cfg(test)]
    fn close(&self) {
        self.shared.lock().closed = true;
        self.shared.changed.notify_all();
    }
}

/// One event's line, queued when the writer is dropped.
struct EventLine {
    shared: Arc<WriterShared>,
    bytes: Vec<u8>,
}

impl Write for EventLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for EventLine {
    fn drop(&mut self) {
        if !self.bytes.is_empty() {
            self.shared.push(std::mem::take(&mut self.bytes));
        }
    }
}

impl<'a> MakeWriter<'a> for BoundedWriter {
    type Writer = EventLine;

    fn make_writer(&'a self) -> EventLine {
        EventLine {
            shared: self.shared.clone(),
            bytes: Vec::new(),
        }
    }
}

fn targets(debug: bool) -> Targets {
    let level = if debug {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("open_ferry", level)
}

#[cfg(test)]
mod tests {
    use tracing::Level;

    use super::*;

    /// A sink that records what it is given, and holds every write until the
    /// gate opens.
    #[derive(Clone, Default)]
    struct Sink {
        written: Arc<Mutex<Vec<u8>>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
        /// Each write on its own.
        writes: Arc<Mutex<Vec<String>>>,
        /// When set, every write (up to twenty) logs five more lines to this
        /// writer, which keeps its queue full and so keeps dropping.
        flood: Arc<Mutex<Option<BoundedWriter>>>,
    }

    impl Sink {
        fn open(&self) {
            *self.gate.0.lock().unwrap() = true;
            self.gate.1.notify_all();
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.written.lock().unwrap()).into_owned()
        }
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut open = self.gate.0.lock().unwrap();
            while !*open {
                open = self.gate.1.wait(open).unwrap();
            }
            drop(open);
            self.written.lock().unwrap().extend_from_slice(buf);
            let count = {
                let mut writes = self.writes.lock().unwrap();
                writes.push(String::from_utf8_lossy(buf).into_owned());
                writes.len()
            };
            if count <= 20 {
                let flood = self.flood.lock().unwrap().clone();
                if let Some(flood) = flood {
                    for _ in 0..5 {
                        log(&flood, "flood\n");
                    }
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn log(writer: &BoundedWriter, line: &str) {
        let mut event = writer.make_writer();
        event.write_all(line.as_bytes()).unwrap();
    }

    fn wait_until_busy(writer: &BoundedWriter) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !writer.shared.lock().busy {
            assert!(Instant::now() < deadline, "the writer never took a line");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_stalled_sink_never_blocks_the_logging_call_and_overflow_is_dropped_and_counted() {
        let sink = Sink::default();
        let writer = BoundedWriter::new(sink.clone(), 2);
        log(&writer, "one\n");
        // The writer thread holds "one" inside the stalled sink.
        wait_until_busy(&writer);
        let began = Instant::now();
        for line in ["two\n", "three\n", "four\n", "five\n", "six\n"] {
            log(&writer, line);
        }
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "logging waited for the sink"
        );
        assert_eq!(writer.shared.lock().dropped, 3);
        // The flush gives up at its bound instead of waiting for the sink.
        let began = Instant::now();
        assert!(!writer.flush(Duration::from_millis(100)));
        assert!(began.elapsed() < Duration::from_secs(5));
        sink.open();
        assert!(writer.flush(Duration::from_secs(10)));
        let text = sink.text();
        assert!(text.starts_with("one\n"), "{text}");
        assert!(text.contains("two\n") && text.contains("three\n"), "{text}");
        assert!(text.contains("3 log lines were dropped"), "{text}");
        assert!(!text.contains("four"), "{text}");
        writer.close();
    }

    #[test]
    fn admitted_lines_are_written_while_the_overflow_goes_on() {
        let sink = Sink::default();
        let writer = BoundedWriter::new(sink.clone(), 2);
        *sink.flood.lock().unwrap() = Some(writer.clone());
        log(&writer, "one\n");
        wait_until_busy(&writer);
        log(&writer, "a\n");
        log(&writer, "b\n");
        // The queue is full: this one is dropped and counted.
        log(&writer, "c\n");
        sink.open();
        let deadline = Instant::now() + Duration::from_secs(10);
        while sink.writes.lock().unwrap().len() < 3 {
            assert!(Instant::now() < deadline, "the writer stopped writing");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Every write drops five more lines, yet both admitted lines come
        // out within the first three writes.
        let writes = sink.writes.lock().unwrap().clone();
        let early = writes[..3].concat();
        assert!(
            early.contains("a\n") && early.contains("b\n"),
            "admitted lines starved: {writes:?}"
        );
        *sink.flood.lock().unwrap() = None;
        assert!(writer.flush(Duration::from_secs(10)));
        writer.close();
    }

    /// The panic hook is process-wide: the tests that install it take turns.
    static HOOK_TESTS: Mutex<()> = Mutex::new(());

    /// Installs [`panic_hook`] for panics on the thread named `thread` only;
    /// every other panic (other tests panic on purpose) goes to the hook that
    /// was there before. Dropping it puts that hook back, which Rust only
    /// allows from a thread that is not panicking: see [`with_hook`].
    struct HookGuard {
        previous: Arc<PanicHook>,
    }

    impl HookGuard {
        fn install(writer: &BoundedWriter, thread: &str) -> Self {
            let previous: Arc<PanicHook> = Arc::new(std::panic::take_hook());
            let ours = panic_hook(writer.clone());
            let name = thread.to_owned();
            let fallback = previous.clone();
            std::panic::set_hook(Box::new(move |info| {
                if std::thread::current().name() == Some(name.as_str()) {
                    ours(info);
                } else {
                    fallback(info);
                }
            }));
            Self { previous }
        }
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            let previous = self.previous.clone();
            std::panic::set_hook(Box::new(move |info| previous(info)));
        }
    }

    /// Runs `body` with the hook installed for the thread named `thread`.
    /// Whatever the body does, the previous hook is restored and the writer
    /// closed on this thread, which is not panicking by then, before a
    /// panic that escaped the body goes on. The body only does what needs
    /// the hook: a failed assertion inside it would be queued, not printed.
    fn with_hook<R>(writer: &BoundedWriter, thread: &str, body: impl FnOnce() -> R) -> R {
        let hook = HookGuard::install(writer, thread);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        drop(hook);
        match result {
            Ok(value) => value,
            Err(payload) => {
                writer.close();
                std::panic::resume_unwind(payload)
            }
        }
    }

    fn current_thread_name() -> String {
        std::thread::current().name().unwrap().to_owned()
    }

    fn wait_until_waiting(writer: &BoundedWriter) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !writer.shared.lock().waiting {
            assert!(Instant::now() < deadline, "the writer never went idle");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A panic that escapes the body is flushed before the unwind resumes.
    /// The sink is stalled until the flush is waiting, so a flush that never
    /// happens leaves the sink empty. The writer may wake on its own just as
    /// the hook runs, and then the hook gives the report up: the writer
    /// reports that in a drop notice within its recheck. Either shows the
    /// flush ran before the unwind resumed, so the test accepts either.
    #[test]
    fn a_panic_escaping_the_body_is_flushed_before_the_unwind_resumes() {
        let _turn = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let sink = Sink::default();
        let writer = BoundedWriter::new(sink.clone(), 8);
        wait_until_waiting(&writer);
        let shared = writer.shared.clone();
        let released = sink.clone();
        let (caught, opener) = with_hook(&writer, &current_thread_name(), || {
            // Opens the sink once the flush is in its wait; reads no lock.
            let opener = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !shared.flushing.load(Ordering::SeqCst) {
                    if Instant::now() > deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                released.open();
            });
            let caught = std::panic::catch_unwind(|| {
                flush_after(&writer, Duration::from_secs(30), || {
                    panic!("fatal in the body");
                })
            });
            (caught, opener)
        });
        assert!(caught.is_err());
        let text = sink.text();
        assert!(
            text.contains("fatal in the body") || text.contains("1 log lines were dropped"),
            "{text}"
        );
        opener.join().unwrap();
        writer.close();
    }

    #[test]
    fn a_panic_is_queued_by_the_hook_and_never_waits_for_a_stalled_sink() {
        let _turn = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let sink = Sink::default();
        let writer = BoundedWriter::new(sink.clone(), 8);
        log(&writer, "one\n");
        wait_until_busy(&writer);
        let (caught, took) = with_hook(&writer, &current_thread_name(), || {
            let began = Instant::now();
            let caught = std::panic::catch_unwind(|| panic!("boom for the hook"));
            (caught, began.elapsed())
        });
        assert!(caught.is_err());
        assert!(
            took < Duration::from_secs(5),
            "the hook waited for the sink"
        );
        let queued: Vec<u8> = writer
            .shared
            .lock()
            .lines
            .iter()
            .flatten()
            .copied()
            .collect();
        let queued = String::from_utf8_lossy(&queued).into_owned();
        assert!(queued.contains("boom for the hook"), "{queued}");
        assert!(queued.contains("ERROR panic"), "{queued}");
        sink.open();
        assert!(writer.flush(Duration::from_secs(10)));
        writer.close();
    }

    /// A panic on a thread that holds the queue's lock must unwind, not wait
    /// for itself: the report is given up and counted in the next notice,
    /// which needs no later event to be written.
    #[test]
    fn a_panic_on_the_thread_that_holds_the_queue_lock_unwinds_and_is_counted() {
        let _turn = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let sink = Sink::default();
        sink.open();
        let writer = BoundedWriter::new(sink.clone(), 8);
        let shared = writer.shared.clone();
        let unwound = with_hook(&writer, "queue-lock-holder", || {
            let (done, finished) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("queue-lock-holder".to_owned())
                .spawn(move || {
                    let guard = shared.lock();
                    let caught = std::panic::catch_unwind(|| panic!("boom under the lock"));
                    drop(guard);
                    let _ = done.send(caught.is_err());
                })
                .unwrap();
            let unwound = finished.recv_timeout(Duration::from_secs(10));
            if unwound != Ok(true) {
                // The thread is stuck inside the hook, which holds the panic
                // hook's lock: restoring the hook, or panicking here, would
                // wait for it too. Fail the whole test run instead of
                // hanging it.
                eprintln!("the panic did not unwind: {unwound:?}");
                std::process::exit(101);
            }
            unwound
        });
        assert_eq!(unwound, Ok(true));
        assert!(writer.flush(Duration::from_secs(10)));
        let text = sink.text();
        assert!(text.contains("1 log lines were dropped"), "{text}");
        assert!(!text.contains("boom under the lock"), "{text}");
        writer.close();
    }

    /// The abandon path notifies without the lock, so the notify can land
    /// between the writer's check and its wait. The writer must still pick
    /// the count up with no later event.
    #[test]
    fn an_abandoned_report_is_written_though_its_wakeup_is_lost() {
        let sink = Sink::default();
        sink.open();
        let writer = BoundedWriter::new(sink.clone(), 8);
        wait_until_waiting(&writer);
        let (done, finished) = std::sync::mpsc::channel();
        let shared = writer.shared.clone();
        *writer.shared.before_wait.lock().unwrap() = Some(Box::new(move || {
            // The writer holds the lock here, so the report is abandoned,
            // and its notify finds nobody waiting.
            std::thread::spawn(move || shared.push_report(b"lost\n".to_vec()))
                .join()
                .unwrap();
            let _ = done.send(());
        }));
        writer.shared.changed.notify_all();
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the writer never reached the hook");
        assert!(
            writer.flush(Duration::from_secs(5)),
            "the abandoned report was never written"
        );
        let text = sink.text();
        assert!(text.contains("1 log lines were dropped"), "{text}");
        assert!(!text.contains("lost"), "{text}");
        writer.close();
    }

    #[test]
    fn lines_logged_through_tracing_reach_the_sink_and_flush_waits_for_them() {
        let sink = Sink::default();
        sink.open();
        let writer = BoundedWriter::new(sink.clone(), 8);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("a line for the sink");
        });
        assert!(writer.flush(Duration::from_secs(10)));
        assert!(sink.text().contains("a line for the sink"));
        writer.close();
    }

    #[test]
    fn debug_is_for_this_projects_crates() {
        let debug = targets(true);
        assert!(debug.would_enable("open_ferry_core::auth", &Level::DEBUG));
        assert!(debug.would_enable("open_ferry", &Level::DEBUG));
        assert!(!debug.would_enable("hyper::proto", &Level::DEBUG));
        assert!(debug.would_enable("hyper::proto", &Level::INFO));
        assert!(!targets(false).would_enable("open_ferry_server", &Level::DEBUG));
    }
}
