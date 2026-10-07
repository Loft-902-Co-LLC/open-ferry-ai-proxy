// Ported from CLIProxyAPI internal/registry/catalog_sources.go
// (catalogUpdater, configure, refresh, readCatalogSource, catalogFetcher,
// StartModelCatalogUpdaters, UpdateModelCatalogSources, configureCatalogs)
// and internal/registry/model_updater.go (SetModelRefreshCallback,
// notifyModelRefresh, mergeProviderNames) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Reading the model catalogs from the files the `models` section names.
//!
//! `models.catalog` names the general catalog, in the format of upstream's
//! `models.json`, and `models.codex-catalog` the Codex client catalog, in
//! the format of its `codex_client_models.json`. An empty source is the
//! built-in catalog; an absolute path is a file, of at most 8 MiB.
//!
//! [`CatalogRuntime::start`] reads each catalog when the service starts,
//! [`CatalogRuntime::update`] when a reload changes its source, and a timer
//! reads a file again when it changes: every two seconds a thread for each
//! file source looks at the file's size and modification time, and reads
//! it when either has changed, and also every three hours, as upstream's
//! ticker does. It is a timer rather than a file watcher, so a file
//! replaced by renaming, or one that is missing at first, is followed too.
//!
//! A file is checked as upstream checks it (see [`StaticCatalog::from_json`]
//! and [`validate_codex_client_models_json`]) and published to the
//! [`CatalogStore`]. A file that can't be read or doesn't pass is logged,
//! and the last valid catalog stays. A general catalog that changes a
//! provider's models tells the listener ([`CatalogRuntime::set_listener`])
//! which providers, so their credentials' models can be registered again;
//! changes made with no listener set are kept, merged, until one is.
//! Readers of the catalogs take the one in use from the store, and never
//! wait for a file to be read.
//!
//! Deviations from upstream:
//! - No catalog is downloaded. An empty source is the built-in catalog,
//!   where upstream downloads one from its own URLs every three hours unless
//!   `-local-model` is given, so here `-local-model` changes nothing. A URL
//!   source isn't fetched: the load or reload that sets it logs a warning
//!   naming the setting, and the catalog in use stays.
//! - The first read of a source is done before [`CatalogRuntime::start`]
//!   and [`CatalogRuntime::update`] return, so the credentials registered
//!   next get its models. Upstream reads in the background, and registers
//!   the credentials' models again once it has read.
//! - A file is also read again when its size or modification time changes,
//!   checked every two seconds; upstream reads it every three hours only.
//! - The warnings name the setting and say why the catalog was refused;
//!   upstream's say neither.
//! - `models.devin-catalog` is checked with the others but not read, as
//!   Devin isn't ported. Home mode isn't ported, so the general catalog is
//!   never turned off.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use open_ferry_translate::go;
use open_ferry_translate::models::embedded_catalog_json;

use super::codex_client::{self, validate_codex_client_models_json};
use super::{CatalogStore, StaticCatalog};
use crate::config::{CatalogSources, is_url_source};
use crate::multipart::lossy;

/// The most a catalog file may hold (upstream's `maxCodexClientModelsSize`).
const MAX_CATALOG_SIZE: usize = 8 << 20;

/// How often a file is read again even if it looks the same (upstream's
/// `ModelsRefreshInterval`).
const REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 60 * 60);

/// How often a file source is checked for changes.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What is told which providers' models changed, each lowercase.
pub type Listener = Arc<dyn Fn(Vec<String>) + Send + Sync>;

/// Reads a source and checks what it read.
type Fetch = Box<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;

/// Publishes a catalog, returning the providers whose models changed.
type Publish = Box<dyn Fn(&[u8]) -> Result<Vec<String>, String> + Send + Sync>;

/// The process's runtime, over [`CatalogStore::global`].
static GLOBAL: LazyLock<CatalogRuntime> =
    LazyLock::new(|| CatalogRuntime::new(CatalogStore::global_shared(), POLL_INTERVAL));

/// Reads the catalogs from their sources while the service runs (upstream's
/// catalog updaters and `catalogRuntime`). Clones share it.
#[derive(Clone)]
pub struct CatalogRuntime {
    inner: Arc<Runtime>,
}

struct Runtime {
    store: Arc<CatalogStore>,
    notifier: Arc<Notifier>,
    general: Arc<Updater>,
    codex: Arc<Updater>,
    /// Whether the service is running, between start and stop.
    running: Mutex<bool>,
}

impl CatalogRuntime {
    /// The process's runtime, which publishes to [`CatalogStore::global`].
    pub fn global() -> &'static Self {
        &GLOBAL
    }

    /// A runtime publishing to `store`, checking file sources every
    /// `poll_interval`. Tests use one of their own.
    pub fn new(store: Arc<CatalogStore>, poll_interval: Duration) -> Self {
        let notifier = Arc::new(Notifier::default());
        let general = {
            let store = Arc::clone(&store);
            Updater::new(
                "catalog",
                Box::new(fetch_general),
                Box::new(move |data| {
                    store
                        .publish_general(data, "models.catalog")
                        .map_err(|error| error.to_string())
                }),
                Arc::clone(&notifier),
                poll_interval,
            )
        };
        let codex = {
            let store = Arc::clone(&store);
            Updater::new(
                "codex-catalog",
                Box::new(fetch_codex),
                Box::new(move |data| {
                    // Codex client catalog changes name no providers, as
                    // upstream's don't: its model list is made per request.
                    store
                        .publish_codex(data)
                        .map(|_| Vec::new())
                        .map_err(|error| error.to_string())
                }),
                Arc::clone(&notifier),
                poll_interval,
            )
        };
        Self::with_updaters(store, notifier, general, codex)
    }

    fn with_updaters(
        store: Arc<CatalogStore>,
        notifier: Arc<Notifier>,
        general: Arc<Updater>,
        codex: Arc<Updater>,
    ) -> Self {
        Self {
            inner: Arc::new(Runtime {
                store,
                notifier,
                general,
                codex,
                running: Mutex::new(false),
            }),
        }
    }

    /// The store the catalogs are published to.
    pub fn store(&self) -> &Arc<CatalogStore> {
        &self.inner.store
    }

    /// The general catalog in use.
    pub fn general(&self) -> Arc<StaticCatalog> {
        self.inner.store.general()
    }

    /// Sets what is told which providers' models changed, or clears it
    /// (upstream's `SetModelRefreshCallback`). Changes made while none was
    /// set are told the new one at once.
    pub fn set_listener(&self, listener: Option<Listener>) {
        self.inner.notifier.set_listener(listener);
    }

    /// Reads each catalog from its source in `sources`, and follows the
    /// files until [`CatalogRuntime::stop`] (upstream's
    /// `StartModelCatalogUpdaters`). Invalid sources are logged and change
    /// nothing.
    pub fn start(&self, sources: &CatalogSources) {
        let mut running = lock(&self.inner.running);
        *running = true;
        self.configure(sources);
    }

    /// Moves to `sources` after a reload, reading each source that changed
    /// (upstream's `UpdateModelCatalogSources`). Does nothing unless
    /// started.
    pub fn update(&self, sources: &CatalogSources) {
        let running = lock(&self.inner.running);
        if *running {
            self.configure(sources);
        }
    }

    /// Stops following the files, and has reads under way publish nothing.
    /// The catalogs in use stay; a later start reads every source again.
    pub fn stop(&self) {
        let mut running = lock(&self.inner.running);
        *running = false;
        self.inner.general.stop();
        self.inner.codex.stop();
    }

    /// Upstream's `configureCatalogs`, without Home and `-local-model`.
    fn configure(&self, sources: &CatalogSources) {
        if let Err(error) = sources.validate() {
            tracing::warn!(error = %error, "invalid catalog sources");
            return;
        }
        self.inner.general.configure(&sources.catalog);
        self.inner.codex.configure(&sources.codex_catalog);
    }
}

/// One catalog's source, and the reads of it (upstream's `catalogUpdater`).
/// A read publishes only if its source is still the current one, so a slow
/// read of an old source can't replace a newer source's catalog.
struct Updater {
    /// The setting under `models`, such as `codex-catalog`.
    setting: &'static str,
    fetch: Fetch,
    publish: Publish,
    notifier: Arc<Notifier>,
    poll_interval: Duration,
    state: Mutex<UpdaterState>,
}

#[derive(Default)]
struct UpdaterState {
    source: String,
    /// Whether the source is being followed: configured, and not stopped
    /// since.
    active: bool,
    /// Counts source changes and stops; a read publishes only for the
    /// current one.
    generation: u64,
    /// Stops the thread following the file when dropped.
    poller: Option<Sender<()>>,
}

impl Updater {
    fn new(
        setting: &'static str,
        fetch: Fetch,
        publish: Publish,
        notifier: Arc<Notifier>,
        poll_interval: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            setting,
            fetch,
            publish,
            notifier,
            poll_interval,
            state: Mutex::new(UpdaterState::default()),
        })
    }

    /// Moves to `source`, unless it is the source already followed
    /// (upstream's `configure`): reads it, and follows it if it is a file.
    fn configure(self: &Arc<Self>, source: &str) {
        let generation = {
            let mut state = lock(&self.state);
            if state.active && state.source == source {
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            state.source = source.to_owned();
            state.active = true;
            state.poller = None;
            state.generation
        };
        if is_url_source(source) {
            tracing::warn!(
                "models.{} names a URL; model catalogs are read only from files, so the catalog in use stays",
                self.setting
            );
            return;
        }
        if source.is_empty() {
            self.refresh(source, generation);
            return;
        }
        let stamp = stamp(source);
        self.refresh(source, generation);
        let (stop, stopped) = mpsc::channel();
        let poller = Poller {
            updater: Arc::downgrade(self),
            stopped,
            source: source.to_owned(),
            generation,
            stamp,
            interval: self.poll_interval,
        };
        match thread::Builder::new()
            .name("catalog-file".to_owned())
            .spawn(move || poller.run())
        {
            Ok(_) => {
                let mut state = lock(&self.state);
                if state.generation == generation {
                    state.poller = Some(stop);
                }
            }
            Err(error) => tracing::warn!(
                setting = %format_args!("models.{}", self.setting),
                error = %error,
                "can't follow the model catalog file; it is read again only when a reload changes it"
            ),
        }
    }

    /// Reads `source` and publishes it if `generation` is still current
    /// (upstream's `refresh`).
    fn refresh(&self, source: &str, generation: u64) {
        let data = match (self.fetch)(source) {
            Ok(data) => data,
            Err(error) => {
                if lock(&self.state).generation == generation {
                    tracing::warn!(
                        setting = %format_args!("models.{}", self.setting),
                        error = %error,
                        "model catalog refresh failed; keeping last valid catalog"
                    );
                }
                return;
            }
        };
        let published = {
            let state = lock(&self.state);
            if state.generation != generation {
                return;
            }
            (self.publish)(&data)
        };
        match published {
            Ok(changed) => self.notifier.notify(changed),
            Err(error) => tracing::warn!(
                setting = %format_args!("models.{}", self.setting),
                error = %error,
                "model catalog rejected; keeping last valid catalog"
            ),
        }
    }

    /// Stops following the source; reads under way publish nothing.
    fn stop(&self) {
        let mut state = lock(&self.state);
        state.generation = state.generation.wrapping_add(1);
        state.active = false;
        state.poller = None;
    }
}

/// A file's size and modification time, or `None` if it can't be read.
type Stamp = Option<(u64, Option<SystemTime>)>;

fn stamp(path: &str) -> Stamp {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()))
}

/// The thread that reads a file source again when it changes.
struct Poller {
    updater: Weak<Updater>,
    stopped: Receiver<()>,
    source: String,
    generation: u64,
    stamp: Stamp,
    interval: Duration,
}

impl Poller {
    fn run(mut self) {
        let mut last_read = Instant::now();
        loop {
            match self.stopped.recv_timeout(self.interval) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            }
            let Some(updater) = self.updater.upgrade() else {
                return;
            };
            let stamp = stamp(&self.source);
            if stamp != self.stamp || last_read.elapsed() >= REFRESH_INTERVAL {
                self.stamp = stamp;
                last_read = Instant::now();
                updater.refresh(&self.source, self.generation);
            }
        }
    }
}

/// The general catalog's source, checked (upstream's `catalogFetcher` over
/// `validateCatalogBytes`).
fn fetch_general(source: &str) -> Result<Vec<u8>, String> {
    if source.is_empty() {
        return Ok(embedded_catalog_json().as_bytes().to_vec());
    }
    let data = read_catalog_file(source)?;
    StaticCatalog::from_json(&lossy(&data), source).map_err(|error| error.to_string())?;
    Ok(data)
}

/// The Codex client catalog's source, checked (upstream's `catalogFetcher`
/// over `ValidateCodexClientModelsJSON`).
fn fetch_codex(source: &str) -> Result<Vec<u8>, String> {
    if source.is_empty() {
        return Ok(codex_client::EMBEDDED_CATALOG.to_vec());
    }
    let data = read_catalog_file(source)?;
    validate_codex_client_models_json(&data).map_err(|error| format!("{source}: {error}"))?;
    Ok(data)
}

/// The file at the absolute `path`, of at most [`MAX_CATALOG_SIZE`] bytes
/// (upstream's `readCatalogSource`, for a file).
fn read_catalog_file(path: &str) -> Result<Vec<u8>, String> {
    if !Path::new(path).is_absolute() {
        return Err(format!("{path} isn't an absolute path"));
    }
    let file = File::open(path).map_err(|error| format!("open {path}: {error}"))?;
    let mut data = Vec::new();
    file.take(MAX_CATALOG_SIZE as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|error| format!("read {path}: {error}"))?;
    if data.len() > MAX_CATALOG_SIZE {
        return Err("catalog exceeds size limit".to_owned());
    }
    Ok(data)
}

/// The listener, and the changes waiting for one.
#[derive(Default)]
struct Notifier {
    state: Mutex<NotifierState>,
}

#[derive(Default)]
struct NotifierState {
    listener: Option<Listener>,
    pending: Vec<String>,
}

impl Notifier {
    /// Upstream's `SetModelRefreshCallback`.
    fn set_listener(&self, listener: Option<Listener>) {
        let pending = {
            let mut state = lock(&self.state);
            state.listener.clone_from(&listener);
            if listener.is_some() {
                std::mem::take(&mut state.pending)
            } else {
                Vec::new()
            }
        };
        if let Some(listener) = listener
            && !pending.is_empty()
        {
            listener(pending);
        }
    }

    /// Upstream's `notifyModelRefresh`.
    fn notify(&self, changed: Vec<String>) {
        if changed.is_empty() {
            return;
        }
        let listener = {
            let mut state = lock(&self.state);
            match &state.listener {
                Some(listener) => Arc::clone(listener),
                None => {
                    let pending = std::mem::take(&mut state.pending);
                    state.pending = merge_provider_names(pending, changed);
                    return;
                }
            }
        };
        listener(changed);
    }
}

/// `existing` then `incoming`, each name trimmed and lowercased, without
/// empty names or repeats (upstream's `mergeProviderNames`).
fn merge_provider_names(existing: Vec<String>, incoming: Vec<String>) -> Vec<String> {
    if incoming.is_empty() {
        return existing;
    }
    let mut merged: Vec<String> = Vec::with_capacity(existing.len() + incoming.len());
    for provider in existing.into_iter().chain(incoming) {
        let name = go::to_lower(provider.trim());
        if !name.is_empty() && !merged.contains(&name) {
            merged.push(name);
        }
    }
    merged
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
