//! Helpers for this module's tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::{env, fs, process};

use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, Subscriber, span};

/// A fresh directory under the system temp directory, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("open-ferry-config-{}-{n}", process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Writes `contents` to `name` inside the directory and returns its path.
    pub(crate) fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.join(name);
        fs::write(&path, contents).expect("write temp file");
        path
    }

    /// Creates the directory `name` inside this one and returns its path.
    pub(crate) fn mkdir(&self, name: &str) -> PathBuf {
        let path = self.join(name);
        fs::create_dir_all(&path).expect("create temp subdir");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs `f` and returns its result with the message of each warning and
/// error it logged on this thread.
pub(crate) fn logged<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    crate::test_tracing::keep_every_callsite_open();
    let messages = Arc::new(Mutex::new(Vec::new()));
    let value = tracing::subscriber::with_default(Warnings(Arc::clone(&messages)), f);
    let messages = messages
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    (value, messages)
}

/// A subscriber that keeps the message of each warning and error.
struct Warnings(Arc<Mutex<Vec<String>>>);

impl Subscriber for Warnings {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        matches!(*metadata.level(), Level::WARN | Level::ERROR)
    }

    fn new_span(&self, _attributes: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message.0);
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

/// An event's `message` field.
struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}
