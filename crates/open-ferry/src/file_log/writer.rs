// Ported from CLIProxyAPI internal/logging/global_logger.go
// (ConfigureLogOutput's lumberjack.Logger) and gopkg.in/natefinch/lumberjack.v2
// lumberjack.go (Write, openExistingOrNew, openNew, rotate, backupName)
// (v8.0.10 and lumberjack v2.2.1, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Where the lines go: standard output, or `main.log`, rotated once it
//! would pass 10 MB. Rotation renames `main.log` to
//! `main-2006-01-02T15-04-05.000.log` and starts a new one; every rotated
//! file is kept (no backups limit, no age limit, no compression, as
//! upstream configures lumberjack).
//!
//! The file's lines go through a bounded queue to a thread that owns the
//! file, so logging never waits on the disk; a line that finds the queue
//! full is dropped and counted, and the thread logs the count. The file is
//! opened with Rust's default sharing, so on Windows other processes can
//! read it, truncate it (the logs route's DELETE; writes go on at the new
//! end, as the file is opened to append) and delete it while it is open.
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
//! - A line is queued and may be dropped, as the module says.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;
use tracing::Level;

use super::format::{Entry, Fields, format};
use super::{file_name, retry_shared};

/// The size `main.log` is rotated at (lumberjack's `MaxSize` of 10
/// megabytes).
pub(super) const MAX_SIZE: u64 = 10 * 1024 * 1024;

/// The lines the queue to the file holds.
const QUEUE: usize = 4096;

/// How long the writer waits after a failed rotation before trying again.
const ROTATION_RETRY: Duration = Duration::from_secs(60);

/// How often, at most, the writer logs the lines it dropped.
const DROP_REPORT: Duration = Duration::from_secs(10);

/// What the writer thread is sent.
enum Job {
    /// A line to write.
    Line(Vec<u8>),
    /// A request to say once the lines before it are written.
    #[cfg_attr(not(test), allow(dead_code, reason = "tests wait for the writer"))]
    Sync(mpsc::Sender<()>),
}

/// The output the log lines go to.
#[derive(Debug, Default)]
pub(super) struct Output {
    file: RwLock<Option<FileOutput>>,
    dropped: Arc<AtomicU64>,
}

/// The queue to the thread that writes `main.log` at `path`.
#[derive(Debug)]
struct FileOutput {
    path: PathBuf,
    queue: SyncSender<Job>,
}

impl Output {
    /// Writes `line` where lines go now.
    pub(super) fn write(&self, line: Vec<u8>) {
        let file = self.file.read().unwrap_or_else(PoisonError::into_inner);
        let line = match file.as_ref() {
            Some(file) => match file.queue.try_send(Job::Line(line)) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                // The writer is gone: standard output it is.
                Err(TrySendError::Disconnected(Job::Line(line))) => line,
                Err(TrySendError::Disconnected(Job::Sync(_))) => return,
            },
            None => line,
        };
        drop(file);
        let _ = io::stdout().lock().write_all(&line);
    }

    /// Sends the lines to the file at `path`. A file already being written
    /// there stays open; another file is left once its queued lines are
    /// written.
    pub(super) fn to_file(&self, path: &Path) -> io::Result<()> {
        let mut file = self.file.write().unwrap_or_else(PoisonError::into_inner);
        if file.as_ref().is_some_and(|file| file.path == path) {
            return Ok(());
        }
        let (queue, jobs) = mpsc::sync_channel(QUEUE);
        let writer = Rotating::new(path.to_path_buf(), MAX_SIZE);
        let dropped = Arc::clone(&self.dropped);
        thread::Builder::new()
            .name("main-log".to_owned())
            .spawn(move || run(writer, &jobs, &dropped))?;
        *file = Some(FileOutput {
            path: path.to_path_buf(),
            queue,
        });
        Ok(())
    }

    /// Sends the lines to standard output. The file is left once its
    /// queued lines are written.
    pub(super) fn to_stdout(&self) {
        *self.file.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The file the lines go to, `None` for standard output.
    #[cfg(test)]
    pub(super) fn file(&self) -> Option<PathBuf> {
        let file = self.file.read().unwrap_or_else(PoisonError::into_inner);
        file.as_ref().map(|file| file.path.clone())
    }

    /// Waits until the lines queued so far are written.
    #[cfg(test)]
    pub(super) fn sync(&self) {
        let (done, wait) = mpsc::channel();
        let file = self.file.read().unwrap_or_else(PoisonError::into_inner);
        let sent = file
            .as_ref()
            .is_some_and(|file| file.queue.send(Job::Sync(done)).is_ok());
        drop(file);
        if sent {
            let _ = wait.recv();
        }
    }
}

/// The writer thread: writes the lines it is sent until the queue is
/// dropped and drained, flushing whenever it runs dry, and says now and
/// then how many lines were dropped.
fn run(mut writer: Rotating, jobs: &mpsc::Receiver<Job>, dropped: &AtomicU64) {
    let mut reported = None::<Instant>;
    loop {
        match jobs.recv_timeout(DROP_REPORT) {
            Ok(job) => {
                writer.handle(job);
                for job in jobs.try_iter().take(QUEUE) {
                    writer.handle(job);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if reported.is_none_or(|at| at.elapsed() >= DROP_REPORT) {
            let lost = dropped.swap(0, Ordering::Relaxed);
            if lost > 0 {
                writer.write_own(
                    Level::WARN,
                    &format!(
                        "logging: dropped {lost} log line(s): the main log writer fell behind"
                    ),
                );
                reported = Some(Instant::now());
            }
        }
        writer.flush_quietly();
    }
    writer.flush_quietly();
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

    /// Does what `job` asks.
    fn handle(&mut self, job: Job) {
        match job {
            Job::Line(line) => self.write_line(&line),
            Job::Sync(done) => {
                self.flush_quietly();
                let _ = done.send(());
            }
        }
    }

    /// Writes `line`, telling standard error when it can't, as logrus
    /// does when its output fails.
    fn write_line(&mut self, line: &[u8]) {
        if let Err(error) = self.write(line) {
            let _ = writeln!(io::stderr(), "Failed to write to log, {error}");
        }
    }

    /// Writes a line of the writer's own: what it dropped, or why it
    /// couldn't rotate.
    fn write_own(&mut self, level: Level, message: &str) {
        let line = format(&Entry {
            time: Local::now(),
            level,
            caller: None,
            message,
            fields: &Fields::default(),
        });
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

    fn flush_quietly(&mut self) {
        if let Err(error) = self.flush() {
            let _ = writeln!(io::stderr(), "Failed to write to log, {error}");
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
