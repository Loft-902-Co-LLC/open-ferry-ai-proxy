//! The usage ledger: a SQLite file, [`LEDGER_FILE`] in the log directory,
//! with a row for each usage record the server makes, which the dashboard
//! API's usage routes read.
//!
//! **Recording.** The ledger observes the usage records
//! ([`Usage::observe`]): it gets each one as it is made, without taking it
//! from the usage queue, so the queue's own consumers get every record as
//! before. Records are made only while `usage-statistics-enabled` is on. A
//! thread of the ledger's own writes them, many to a transaction, so no
//! request waits on the disk. When it falls behind, records are lost and
//! counted, not queued without bound.
//!
//! **What a row holds.** A row is one upstream call (one attempt): when it
//! started, the client request's ID and route, the provider, the model and
//! the model asked for, the credential by its ID, index and label, the
//! client's key as a masked form and a keyed hash, the outcome, the
//! latency and time to first token, and the tokens in their buckets. No
//! prompt or answer text is kept, and no key in clear. The hash is an
//! HMAC-SHA256 under a random secret the ledger keeps, cut to 64 bits: the
//! same key always gets the same ID, and the ID can't be looked up in a
//! table made for other ledgers.
//!
//! **Keeping the file small.** Rows older than the retention (90 days by
//! default) are deleted, then the oldest rows beyond the row cap (a
//! million by default), at start, hourly, every thousand rows and soon
//! after either setting changes, and the freed pages are given back.
//!
//! **Settings and prices** live in the ledger, not in `config.yaml`, whose
//! sections are upstream's. Costs are worked out when asked, from the
//! prices the user set; none are shipped.
//!
//! The schema has a version table. A file of a newer schema than this
//! binary knows is left alone, and the ledger is unavailable.

mod query;
mod schema;
#[cfg(test)]
mod tests;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use open_ferry_core::observe::usage::{Observation, Usage, UsageEvent};
use rusqlite::Connection;

pub(crate) use query::{
    Bucketing, Filter, GroupBy, GroupInfo, Metrics, RequestCursor, aggregate, delete_all,
    delete_price, format_time, group_info, ledger_counts, list_prices, list_requests, top_groups,
    unpriced_models, upsert_price,
};
pub(crate) use schema::{Settings, read_settings, write_settings};

/// The ledger's file name, in the log directory.
pub const LEDGER_FILE: &str = "open-ferry-usage.sqlite3";

/// The usage ledger, shared by the server and its writer thread. Clones
/// are the same ledger.
#[derive(Clone)]
pub struct Ledger {
    inner: Arc<Inner>,
}

struct Inner {
    /// The file.
    path: PathBuf,
    /// The open ledger, or why it isn't.
    store: Result<Store, String>,
    /// The observation the writer reads, until shutdown.
    observation: Mutex<Option<Observation>>,
    /// The writer thread, until shutdown.
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// An open ledger.
struct Store {
    /// The statistics recorded, for whether they are on.
    usage: Usage,
    /// The connection the API's reads and writes use; the writer has its
    /// own.
    connection: Mutex<Connection>,
    /// What the API and the writer share.
    shared: Arc<Shared>,
}

/// What the API and the writer thread share.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    /// Prune soon: a setting changed.
    pub(crate) prune: AtomicBool,
    /// Records lost because they couldn't be written.
    pub(crate) write_failures: AtomicU64,
}

/// Why a ledger call failed.
#[derive(Debug)]
pub(crate) enum LedgerError {
    /// The ledger couldn't be opened, for this reason.
    Unavailable(String),
    /// A query failed.
    Failed(String),
}

impl Ledger {
    /// Opens or makes the ledger in `dir`, and records `usage`'s records
    /// into it until [`Ledger::shutdown`]. A ledger that can't be opened is
    /// unavailable, and says why; the server runs on without it.
    pub fn start(dir: &Path, usage: &Usage) -> Self {
        let path = dir.join(LEDGER_FILE);
        let opened = schema::open(&path).and_then(|writer| {
            let api = schema::connect(&path)?;
            Ok((writer, api))
        });
        let (writer, api) = match opened {
            Ok(connections) => connections,
            Err(reason) => {
                tracing::warn!("usage ledger unavailable: {reason}");
                return Self::failed(path, reason);
            }
        };
        let (receiver, observation) = usage.observe();
        Self::running(
            path,
            usage.clone(),
            writer,
            api,
            receiver,
            Some(observation),
        )
    }

    /// A ledger that isn't there, for `reason`, as before the server starts
    /// one.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::failed(PathBuf::new(), reason.into())
    }

    /// The ledger at `path`, unavailable for `reason`.
    fn failed(path: PathBuf, reason: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                path,
                store: Err(reason),
                observation: Mutex::new(None),
                thread: Mutex::new(None),
            }),
        }
    }

    /// The ledger at `path` over its open connections, its writer thread
    /// reading `receiver`.
    fn running(
        path: PathBuf,
        usage: Usage,
        writer: Connection,
        api: Connection,
        receiver: Receiver<UsageEvent>,
        observation: Option<Observation>,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let spawned = std::thread::Builder::new()
            .name("open-ferry-usage-ledger".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                move || writer::run(writer, &receiver, &shared)
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(error) => {
                let reason = format!("start the ledger's writer: {error}");
                tracing::warn!("usage ledger unavailable: {reason}");
                return Self::failed(path, reason);
            }
        };
        Self {
            inner: Arc::new(Inner {
                path,
                store: Ok(Store {
                    usage,
                    connection: Mutex::new(api),
                    shared,
                }),
                observation: Mutex::new(observation),
                thread: Mutex::new(Some(thread)),
            }),
        }
    }

    /// The ledger in `dir`, written from `receiver` rather than from the
    /// usage records, for tests to send it events of their own.
    #[cfg(test)]
    pub(crate) fn for_test(dir: &Path, usage: &Usage, receiver: Receiver<UsageEvent>) -> Self {
        let path = dir.join(LEDGER_FILE);
        let writer = schema::open(&path).unwrap();
        let api = schema::connect(&path).unwrap();
        Self::running(path, usage.clone(), writer, api, receiver, None)
    }

    /// The ledger in `dir`, its writer done, for tests to write rows into
    /// with [`Ledger::insert_for_test`].
    #[cfg(test)]
    pub(crate) fn idle_for_test(dir: &Path, usage: &Usage) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let ledger = Self::for_test(dir, usage, receiver);
        drop(sender);
        ledger.shutdown();
        ledger
    }

    /// Writes `events` as the writer would, now.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&self, events: &[UsageEvent]) {
        let Ok(store) = &self.inner.store else {
            panic!("the ledger isn't open");
        };
        let mut connection = store.connection.lock().unwrap();
        let secret = schema::client_key_secret(&connection).unwrap();
        writer::insert(&mut connection, events, &secret).unwrap();
    }

    /// Stops recording, and waits for the records already made to be
    /// written. Blocks; the ledger's reads still work after.
    pub fn shutdown(&self) {
        drop(
            self.inner
                .observation
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let thread = self
            .inner
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(thread) = thread
            && thread.join().is_err()
        {
            tracing::warn!("the usage ledger's writer panicked");
        }
    }

    /// The file, when the ledger is open.
    pub(crate) fn file(&self) -> Option<&Path> {
        self.inner
            .store
            .as_ref()
            .ok()
            .map(|_| self.inner.path.as_path())
    }

    /// Why the ledger isn't open, if it isn't.
    pub(crate) fn unavailable_reason(&self) -> Option<&str> {
        self.inner.store.as_ref().err().map(String::as_str)
    }

    /// Whether records are being written: the ledger is open and
    /// `usage-statistics-enabled` is on.
    pub(crate) fn recording(&self) -> bool {
        self.inner
            .store
            .as_ref()
            .is_ok_and(|store| store.usage.usage_statistics_enabled())
    }

    /// The records lost since start: dropped because the writer fell
    /// behind, or not written because the write failed.
    pub(crate) fn dropped_records(&self) -> u64 {
        let dropped = self
            .inner
            .observation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map_or(0, Observation::dropped);
        let failed = self.inner.store.as_ref().map_or(0, |store| {
            store.shared.write_failures.load(Ordering::Relaxed)
        });
        dropped.saturating_add(failed)
    }

    /// Asks the writer to prune soon, after a setting changed.
    pub(crate) fn prune_soon(&self) {
        if let Ok(store) = &self.inner.store {
            store.shared.prune.store(true, Ordering::Relaxed);
        }
    }

    /// The size of the file and its write-ahead log, in bytes.
    pub(crate) fn size_bytes(&self) -> Option<u64> {
        let path = self.file()?;
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        let size = |path: &Path| std::fs::metadata(path).map_or(0, |meta| meta.len());
        Some(size(path).saturating_add(size(Path::new(&wal))))
    }

    /// Runs `work` on the API's connection, off the async threads.
    pub(crate) async fn run<T, F>(&self, work: F) -> Result<T, LedgerError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        if let Err(reason) = &self.inner.store {
            return Err(LedgerError::Unavailable(reason.clone()));
        }
        let ledger = self.clone();
        let joined = tokio::task::spawn_blocking(move || match &ledger.inner.store {
            Ok(store) => {
                let mut connection = store
                    .connection
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                work(&mut connection).map_err(|error| LedgerError::Failed(error.to_string()))
            }
            Err(reason) => Err(LedgerError::Unavailable(reason.clone())),
        })
        .await;
        joined
            .unwrap_or_else(|error| Err(LedgerError::Failed(format!("the query stopped: {error}"))))
    }
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("path", &self.inner.path)
            .field("available", &self.inner.store.is_ok())
            .finish_non_exhaustive()
    }
}
