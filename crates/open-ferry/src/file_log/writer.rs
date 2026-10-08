// Ported from CLIProxyAPI internal/logging/global_logger.go
// (ConfigureLogOutput's lumberjack.Logger) and gopkg.in/natefinch/lumberjack.v2
// lumberjack.go (Write, openExistingOrNew, openNew, rotate, backupName)
// (v8.0.20 and lumberjack v2.2.1, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Where the lines go: standard output, or `main.log`, rotated once it
//! would pass 10 MB. Rotation renames `main.log` to
//! `main-2006-01-02T15-04-05.000.log` and starts a new one; every rotated
//! file is kept (no backups limit, no age limit, no compression, as
//! upstream configures lumberjack).
//!
//! The lines go through a bounded queue to a thread that writes them, one
//! for `main.log` and one for standard output, so logging never waits on
//! the disk or on whatever reads standard output; a line that finds the
//! queue full is dropped and counted, and the thread logs the count. A
//! switch to another output waits until the file left has written the
//! lines queued for it and is closed, and a new file's thread opens it
//! only then. The file is opened with Rust's default sharing, so on
//! Windows other processes can read it, truncate it (the logs route's
//! DELETE; writes go on at the new end, as the file is opened to append)
//! and delete it while it is open.
//!
//! While the TUI runs in standalone mode, standard output is muted: lines
//! that would go there are dropped, and a tap sees every line, for the
//! TUI's logs tab. The lines for `main.log` are still written.
//!
//! Deviations from upstream:
//! - The rotated name's time is local, where lumberjack's is UTC: the logs
//!   routes order rotated files by that time read as local, as upstream's
//!   do.
//! - Before rotating, the size is read again from the file, so a file
//!   truncated by the logs route's DELETE isn't rotated early.
//! - On Windows, renaming is retried while another process has the file
//!   open. When rotation still fails, lines go on to the old file, and
//!   rotation is tried again a minute later; lumberjack fails the write.
//! - A line is queued and may be dropped, as the module says, for standard
//!   output as for `main.log`; upstream writes each line as it is logged,
//!   and waits while standard output isn't read.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::Local;
use tracing::Level;

use super::format::{Entry, Fields, format};
use super::{file_name, retry_shared};

/// The size `main.log` is rotated at (lumberjack's `MaxSize` of 10
/// megabytes).
pub(super) const MAX_SIZE: u64 = 10 * 1024 * 1024;

/// The lines a queue to a writer thread holds.
pub(super) const QUEUE: usize = 4096;

/// How long the writer waits after a failed rotation before trying again.
const ROTATION_RETRY: Duration = Duration::from_secs(60);

/// How often, at most, the writer logs the lines it dropped.
const DROP_REPORT: Duration = Duration::from_secs(10);

/// What a writer thread is sent.
enum Job {
    /// A line to write.
    Line(Vec<u8>),
    /// A request to say once the lines before it are written.
    Sync(mpsc::Sender<()>),
    /// Tests: wait until told to go on, as a slow disk would.
    #[cfg_attr(not(test), allow(dead_code, reason = "tests hold the writer"))]
    Hold(Receiver<()>),
}

/// What sees each line as it is logged, wherever it goes.
pub(crate) type Tap = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// The output the log lines go to.
#[derive(Debug)]
pub(super) struct Output {
    state: RwLock<State>,
    /// Held while the output switches, so one switch ends before the next.
    switching: Mutex<()>,
    /// The lines dropped and not yet reported, by either queue.
    dropped: Arc<AtomicU64>,
    /// Whether lines for standard output are dropped.
    muted: Arc<AtomicBool>,
    tap: TapSlot,
}

/// The tap, if any.
#[derive(Default)]
struct TapSlot(RwLock<Option<Tap>>);

impl std::fmt::Debug for TapSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TapSlot")
    }
}

/// The writer threads.
#[derive(Debug)]
struct State {
    /// `main.log`'s, while the lines go there.
    file: Option<FileOutput>,
    /// Standard output's; `None` when its thread couldn't start, and the
    /// lines are written as they are logged.
    console: Option<Worker>,
}

/// The thread that writes `main.log` at `path`.
#[derive(Debug)]
struct FileOutput {
    path: PathBuf,
    worker: Worker,
}

/// A writer thread and the queue to it.
#[derive(Debug)]
struct Worker {
    queue: SyncSender<Job>,
    thread: JoinHandle<()>,
}

impl Default for Output {
    fn default() -> Self {
        Self::with_console(Box::new(io::stdout()))
    }
}

impl Output {
    /// An output whose standard output is `console`.
    pub(super) fn with_console(console: Box<dyn Write + Send>) -> Self {
        let dropped = Arc::new(AtomicU64::new(0));
        let muted = Arc::new(AtomicBool::new(false));
        let console = Console {
            out: console,
            muted: Arc::clone(&muted),
        };
        let console = Worker::spawn("log-stdout", console, Arc::clone(&dropped), None)
            .inspect_err(|error| {
                let _ = writeln!(
                    io::stderr(),
                    "logging: failed to start the standard output writer: {error}"
                );
            })
            .ok();
        Self {
            state: RwLock::new(State {
                file: None,
                console,
            }),
            switching: Mutex::new(()),
            dropped,
            muted,
            tap: TapSlot::default(),
        }
    }

    /// Drops the lines for standard output while `muted`.
    pub(super) fn mute_console(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Shows every line to `tap` as it is logged; `None` stops.
    pub(super) fn set_tap(&self, tap: Option<Tap>) {
        *self.tap.0.write().unwrap_or_else(PoisonError::into_inner) = tap;
    }

    /// Writes `line` where lines go now, after showing it to the tap.
    pub(super) fn write(&self, line: Vec<u8>) {
        let tap = self
            .tap
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(tap) = tap {
            tap(&line);
        }
        let muted = self.muted.load(Ordering::Relaxed);
        let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
        let worker = match &state.file {
            Some(file) => Some(&file.worker),
            None if muted => return,
            None => state.console.as_ref(),
        };
        let line = match worker {
            Some(worker) => match worker.queue.try_send(Job::Line(line)) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                // The writer is gone: standard output it is.
                Err(TrySendError::Disconnected(Job::Line(line))) => line,
                Err(TrySendError::Disconnected(_)) => return,
            },
            // No thread for standard output could start.
            None => line,
        };
        drop(state);
        if !muted {
            let _ = io::stdout().lock().write_all(&line);
        }
    }

    /// Sends the lines to the file at `path`. A file already being written
    /// there stays open. Another file is closed once its queued lines are
    /// written, before this returns; lines logged meanwhile wait in the new
    /// file's queue, and its thread opens it only once the old file is
    /// closed.
    pub(super) fn to_file(&self, path: &Path) -> io::Result<()> {
        let _switching = self
            .switching
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.file().as_deref() == Some(path) {
            return Ok(());
        }
        let (start, gate) = mpsc::channel();
        let writer = Rotating::new(path.to_path_buf(), MAX_SIZE);
        let worker = Worker::spawn("main-log", writer, Arc::clone(&self.dropped), Some(gate))?;
        let old = self
            .state
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .file
            .replace(FileOutput {
                path: path.to_path_buf(),
                worker,
            });
        if let Some(old) = old {
            old.worker.retire();
        }
        let _ = start.send(());
        Ok(())
    }

    /// Sends the lines to standard output. The file is closed once its
    /// queued lines are written, before this returns.
    pub(super) fn to_stdout(&self) {
        let _switching = self
            .switching
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let old = self
            .state
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .file
            .take();
        if let Some(old) = old {
            old.worker.retire();
        }
    }

    /// The file the lines go to, `None` for standard output.
    pub(super) fn file(&self) -> Option<PathBuf> {
        let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
        state.file.as_ref().map(|file| file.path.clone())
    }

    /// Waits, until `timeout` has passed at most, for the lines queued so
    /// far to be written.
    pub(super) fn flush(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let queues: Vec<SyncSender<Job>> = {
            let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
            let file = state.file.as_ref().map(|file| &file.worker);
            file.into_iter()
                .chain(state.console.as_ref())
                .map(|worker| worker.queue.clone())
                .collect()
        };
        for queue in &queues {
            sync(queue, deadline);
        }
    }

    /// Tests: makes the thread the lines go to now wait until `go` says.
    #[cfg(test)]
    pub(super) fn hold(&self, go: Receiver<()>) {
        let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
        let worker = match &state.file {
            Some(file) => Some(&file.worker),
            None => state.console.as_ref(),
        };
        if let Some(worker) = worker {
            let _ = worker.queue.send(Job::Hold(go));
        }
    }
}

/// Asks the thread behind `queue` to say when the lines queued before are
/// written, and waits for it until `deadline` at most.
fn sync(queue: &SyncSender<Job>, deadline: Instant) {
    let (done, wait) = mpsc::channel();
    let mut job = Job::Sync(done);
    loop {
        match queue.try_send(job) {
            Ok(()) => break,
            Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                job = back;
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
    let _ = wait.recv_timeout(deadline.saturating_duration_since(Instant::now()));
}

impl Worker {
    /// Starts a thread named `name` writing to `sink` the lines queued to
    /// it, once `gate` says go or is dropped when there is a gate.
    fn spawn(
        name: &str,
        mut sink: impl Sink,
        dropped: Arc<AtomicU64>,
        gate: Option<Receiver<()>>,
    ) -> io::Result<Self> {
        let (queue, jobs) = mpsc::sync_channel(QUEUE);
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                if let Some(gate) = gate {
                    let _ = gate.recv();
                }
                run(&mut sink, &jobs, &dropped);
            })?;
        Ok(Self { queue, thread })
    }

    /// Closes the queue and waits until the thread has written the lines
    /// queued and closed its output.
    fn retire(self) {
        drop(self.queue);
        let _ = self.thread.join();
    }
}

/// Where a writer thread puts the lines.
trait Sink: Send + 'static {
    /// Writes `line`.
    fn write_line(&mut self, line: &[u8]);

    /// Writes out what is buffered.
    fn flush_quietly(&mut self);
}

/// Standard output, or what stands in for it in tests. A line that can't
/// be written is lost, as upstream ignores the error; so is one, such as
/// the count of dropped lines, written while the output is muted.
struct Console {
    out: Box<dyn Write + Send>,
    muted: Arc<AtomicBool>,
}

impl Sink for Console {
    fn write_line(&mut self, line: &[u8]) {
        if !self.muted.load(Ordering::Relaxed) {
            let _ = self.out.write_all(line);
        }
    }

    fn flush_quietly(&mut self) {
        let _ = self.out.flush();
    }
}

/// The writer thread: writes the lines it is sent until the queue is
/// dropped and drained, flushing whenever it runs dry, and says now and
/// then how many lines were dropped.
fn run(sink: &mut impl Sink, jobs: &Receiver<Job>, dropped: &AtomicU64) {
    let mut reported = None::<Instant>;
    loop {
        match jobs.recv_timeout(DROP_REPORT) {
            Ok(job) => {
                handle(sink, job, dropped, &mut reported);
                for job in jobs.try_iter().take(QUEUE) {
                    handle(sink, job, dropped, &mut reported);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        report_dropped(sink, dropped, &mut reported);
        sink.flush_quietly();
    }
    sink.flush_quietly();
}

/// Does what `job` asks.
fn handle(sink: &mut impl Sink, job: Job, dropped: &AtomicU64, reported: &mut Option<Instant>) {
    match job {
        Job::Line(line) => sink.write_line(&line),
        Job::Sync(done) => {
            report_dropped(sink, dropped, reported);
            sink.flush_quietly();
            let _ = done.send(());
        }
        Job::Hold(go) => {
            let _ = go.recv();
        }
    }
}

/// Writes how many lines were dropped since it last did, unless it did in
/// the last [`DROP_REPORT`].
fn report_dropped(sink: &mut impl Sink, dropped: &AtomicU64, reported: &mut Option<Instant>) {
    if reported.is_some_and(|at| at.elapsed() < DROP_REPORT) {
        return;
    }
    let lost = dropped.swap(0, Ordering::Relaxed);
    if lost > 0 {
        let note = own_line(
            Level::WARN,
            &format!("logging: dropped {lost} log line(s): the main log writer fell behind"),
        );
        sink.write_line(note.as_bytes());
        *reported = Some(Instant::now());
    }
}

/// A line of the writer's own: what it dropped, or why it couldn't rotate.
fn own_line(level: Level, message: &str) -> String {
    format(&Entry {
        time: Local::now(),
        level,
        caller: None,
        message,
        fields: &Fields::default(),
    })
}

/// `main.log` and its rotation (lumberjack's `Logger`).
pub(super) struct Rotating {
    path: PathBuf,
    max_size: u64,
    file: Option<BufWriter<File>>,
    size: u64,
    retry_rotation_at: Option<Instant>,
}

impl Rotating {
    /// The log file at `path`, rotated at `max_size` bytes. Nothing is
    /// opened until the first line.
    pub(super) fn new(path: PathBuf, max_size: u64) -> Self {
        Self {
            path,
            max_size,
            file: None,
            size: 0,
            retry_rotation_at: None,
        }
    }

    /// Writes a line of the writer's own: why it couldn't rotate.
    fn write_own(&mut self, level: Level, message: &str) {
        let line = own_line(level, message);
        self.write_line(line.as_bytes());
    }

    /// Writes `line`, rotating first when it would take the file past the
    /// limit (lumberjack's `Write`).
    pub(super) fn write(&mut self, line: &[u8]) -> io::Result<()> {
        let len = line.len() as u64;
        if len > self.max_size {
            return Err(io::Error::other(format!(
                "write length {len} exceeds maximum file size {}",
                self.max_size
            )));
        }
        if self.file.is_none() {
            self.open_existing_or_new(len)?;
        }
        if self.size + len > self.max_size {
            self.rotate(len)?;
        }
        let file = self.file.as_mut().ok_or_else(closed)?;
        file.write_all(line)?;
        self.size += len;
        Ok(())
    }

    /// Writes out what is buffered.
    pub(super) fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }

    /// Opens the log file to append to it, or rotates it first when the
    /// next `len` bytes would reach the limit (lumberjack's
    /// `openExistingOrNew`).
    fn open_existing_or_new(&mut self, len: u64) -> io::Result<()> {
        let size = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return self.open_new(),
            Err(error) => return Err(error),
        };
        if size + len >= self.max_size {
            self.size = size;
            return self.rotate(len);
        }
        match self.open_append() {
            Ok(()) => Ok(()),
            Err(_) => self.open_new(),
        }
    }

    /// Starts a new file when the file, as it is on disk now, has no room
    /// for `len` more bytes (lumberjack's `rotate`). When renaming fails,
    /// lines go on to the old file until [`ROTATION_RETRY`] has passed.
    fn rotate(&mut self, len: u64) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
            if let Ok(metadata) = file.get_ref().metadata() {
                self.size = metadata.len();
            }
            if self.size + len <= self.max_size {
                return Ok(());
            }
        }
        if self.retry_rotation_at.is_some_and(|at| Instant::now() < at) && self.file.is_some() {
            return Ok(());
        }
        self.file = None;
        match self.open_new() {
            Ok(()) => {
                self.retry_rotation_at = None;
                Ok(())
            }
            Err(error) => {
                self.retry_rotation_at = Some(Instant::now() + ROTATION_RETRY);
                self.open_append()?;
                self.write_own(
                    Level::WARN,
                    &format!(
                        "logging: failed to rotate {}: {error}",
                        file_name(&self.path)
                    ),
                );
                Ok(())
            }
        }
    }

    /// Moves the current file aside, if there is one, and opens a new one
    /// (lumberjack's `openNew`).
    fn open_new(&mut self) -> io::Result<()> {
        if let Some(dir) = self.path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir)?;
        }
        match fs::metadata(&self.path) {
            Ok(_) => {
                let backup = backup_path(&self.path);
                retry_shared(|| fs::rename(&self.path, &backup))?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let file = retry_shared(|| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .truncate(false)
                .open(&self.path)
        })?;
        self.size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        self.file = Some(BufWriter::new(file));
        Ok(())
    }

    /// Opens the current file to append to it.
    fn open_append(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = file.metadata()?.len();
        self.file = Some(BufWriter::new(file));
        Ok(())
    }
}

impl Sink for Rotating {
    /// Writes `line`, telling standard error when it can't, as logrus
    /// does when its output fails.
    fn write_line(&mut self, line: &[u8]) {
        if let Err(error) = self.write(line) {
            let _ = writeln!(io::stderr(), "Failed to write to log, {error}");
        }
    }

    fn flush_quietly(&mut self) {
        if let Err(error) = self.flush() {
            let _ = writeln!(io::stderr(), "Failed to write to log, {error}");
        }
    }
}

/// The error for a write with no file open, which `write` never meets.
fn closed() -> io::Error {
    io::Error::other("log file is not open")
}

/// Where `path` goes when it is rotated now: `main.log` becomes
/// `main-2006-01-02T15-04-05.000.log`, in local time (lumberjack's
/// `backupName`).
pub(super) fn backup_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let time = Local::now().format("%Y-%m-%dT%H-%M-%S%.3f");
    path.with_file_name(format!("{stem}-{time}{extension}"))
}
