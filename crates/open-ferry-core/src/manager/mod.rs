// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor.go (the Manager type,
// NewManager, SetConfig and CloseExecutionSession) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The credential manager: the [`Dispatcher`] that picks a credential for
//! each call, calls its provider's executor, and keeps every credential's
//! cooldowns and token refreshes.
//!
//! A call names the providers that serve its model. The manager picks a
//! ready credential among them (round-robin, fill-first or weighted, within
//! the highest priority), calls the executor, and records the outcome: a
//! failure cools the model or the credential down for a while, a success
//! clears it. When a credential fails, the next one is tried; when every
//! one has failed with an error a later round could fix, the manager waits
//! for the nearest cooldown (up to `max_retry_interval`) and goes round
//! again, up to `request_retry` times.
//!
//! Credentials live in memory as [`Arc<Auth>`] snapshots: a change makes a
//! new snapshot, and saves it through the [`AuthStore`] when there is one.
//! [`Manager::start_auto_refresh`] refreshes OAuth tokens in the background
//! before they expire, and a call that fails with 401 refreshes once and
//! tries again.
//!
//! With `routing.session-affinity` on, a conversation stays on the
//! credential that served it while that one is ready, whatever its priority
//! (see `affinity`).
//!
//! Deviations from upstream:
//! - The plugin scheduler, the Home dispatcher and fingerprints aren't
//!   ported, by policy or scope. A derived session ID is only a local
//!   routing key for session affinity, never sent or written anywhere.
//! - Of upstream's eligibility filters, required auth kinds aren't ported.
//!   A credential policy narrows its own pick (only Codex Alpha Search's),
//!   and the free-plan rule every pick and retry decision; see `policy`.
//! - The scheduler isn't a separate index kept in step with every change;
//!   picks read the credentials as they are, and keep only the rotation
//!   cursors between calls.
//! - Cancellation is dropping the call's future or stream, where upstream
//!   passes a context.
//! - Every executor error counts as an upstream attempt; upstream asks the
//!   executor whether it reached the provider.
//! - Empty metadata, attribute and state maps count as nil.
//! - Maps are walked in key order, where Go's order is random.
//! - The refresh failure count lives in the manager, not on [`Auth`].
//! - Saves go through the synchronous [`AuthStore`] after the state lock is
//!   released, still ordered per credential by epoch and generation. A
//!   change is published before it is saved, where upstream saves first;
//!   the store gets a copy, so nothing it does is merged back (upstream's
//!   `mergeAuthSaveDelta`). There are no per-credential mutation locks: a
//!   change is made whole under the state lock. The reload barrier is a
//!   lock that can't be given up while waiting (see `lifecycle`).
//! - The executor's [`ProviderExecutor::refresh_lead`] replaces upstream's
//!   registry of refresh leads.
//! - The selected credential goes to the `selected_auth` callback and the
//!   request's observation context; there is no metadata map to publish it
//!   in.
//! - Failed calls go to one [`ErrorEvents`] hook, which the usage
//!   statistics set, where upstream's manager queues the event itself.
//! - The cooldown state store saves on a background thread, debounced
//!   (see [`cooldown_store`]).
//! - Not ported: hooks, result policies, request preparation and
//!   interceptors, the round tripper, the Antigravity credits fallback and
//!   API-key capability metadata.

mod affinity;
mod alpha_search;
mod classify;
pub mod clienterror;
mod cooldown;
pub mod cooldown_store;
mod cooldown_view;
mod credential;
mod download;
mod error_events;
mod execute;
mod lifecycle;
mod merge;
mod models;
mod policy;
mod quota_signals;
mod refresh;
mod retry;
mod rewrite;
mod scoped;
mod select;
mod settings;
mod summary;
mod text;
mod websocket;

#[cfg(test)]
mod tests;

pub use classify::has_unauthorized_auth_failure;
pub use cooldown::CallResult;
pub use cooldown_view::{CooldownView, cooldown_snapshot_for_auth};
pub use credential::last_refresh_timestamp;
pub use error_events::ErrorEvents;
pub use lifecycle::QuotaReset;
pub use quota_signals::provider_supports_quota_observation;
pub use refresh::ForceRefreshResult;
pub use select::{ClientModels, ModelProjection};
pub use settings::{
    ApiKeyEntry, ModelAlias, OpenAiCompat, RequestScopedErrorRule, RoutingStrategy, Settings,
};

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
};

use chrono::Utc;
use futures_core::future::BoxFuture;

use crate::auth::{Auth, AuthStore, Timestamp};
use crate::exec::{
    AlphaSearch, Dispatcher, Download, Downloaded, ExecError, HttpReply, Options, ProviderId,
    Request, Response, StreamResponse, WebsocketSupport,
};
use crate::executor::ProviderExecutor;
use affinity::Affinity;
use models::OAuthAliasTable;
use refresh::{RefreshJob, RefreshLoopHandle};
use select::{CLOSE_ALL_EXECUTION_SESSIONS, SelectorState};
use settings::RoutingState;

/// The last (epoch, generation) saved for one credential.
type PersistLock = Arc<Mutex<(u64, u64)>>;

/// What the manager reads the time from.
pub(crate) type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// A registered credential and its registration bookkeeping.
pub(crate) struct Entry {
    /// The current snapshot, carrying its registration epoch and
    /// generation.
    pub(crate) auth: Arc<Auth>,
    /// Refreshes failed in a row with `invalid_grant` (upstream's
    /// `RefreshFailures`).
    pub(crate) refresh_failures: u32,
}

/// The manager's state, behind one lock (upstream's `Manager` fields under
/// `mu`).
pub(crate) struct State {
    pub(crate) settings: Arc<Settings>,
    pub(crate) oauth: Arc<OAuthAliasTable>,
    pub(crate) auths: BTreeMap<String, Entry>,
    pub(crate) epochs: HashMap<String, u64>,
    pub(crate) executors: HashMap<String, Arc<dyn ProviderExecutor>>,
    pub(crate) selector: SelectorState,
    /// The session bindings, while session affinity is on.
    pub(crate) affinity: Option<Affinity>,
    pub(crate) pool_offsets: HashMap<String, usize>,
    pub(crate) refresh_jobs: HashMap<String, RefreshJob>,
}

pub(crate) struct Shared {
    state: Mutex<State>,
    store: Option<Arc<dyn AuthStore>>,
    models: Arc<dyn ClientModels>,
    clock: Clock,
    /// The last (epoch, generation) saved per credential; the save happens
    /// under the credential's lock (upstream's `persistLocks`).
    persist_locks: Mutex<HashMap<String, PersistLock>>,
    /// Serializes refreshes per credential (upstream's `refreshLocks`).
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The reload barrier (upstream's `authLoadGate`): held shared by a
    /// change from before its publication until its save is done, and
    /// exclusively by [`Manager::load`] from listing the store until every
    /// credential is replaced. Taken before the state lock, never twice by
    /// one thread.
    load_gate: RwLock<()>,
    refresh_loop: Mutex<Option<RefreshLoopHandle>>,
    /// What failed calls are told to, once set.
    error_events: OnceLock<Arc<dyn ErrorEvents>>,
    /// The cooldown state store.
    cooldown_store: cooldown_store::CooldownStore,
}

/// The credential manager. Cloning it gives another handle to the same
/// manager.
#[derive(Clone)]
pub struct Manager {
    shared: Arc<Shared>,
    /// Held by the handles given out, not by the refresh loop's own: when
    /// the last one goes, the loop stops.
    _owner: Option<Arc<Owner>>,
}

/// Stops the refresh loop when the last handle outside it is dropped, as
/// [`Manager::stop_auto_refresh`] does. Refreshes already running finish.
struct Owner(Weak<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        let Some(shared) = self.0.upgrade() else {
            return;
        };
        let handle = lock(&shared.refresh_loop).take();
        if let Some(handle) = handle {
            handle.stop();
        }
    }
}

impl fmt::Debug for Manager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Manager").finish_non_exhaustive()
    }
}

/// Why a manager operation failed.
#[derive(Debug)]
pub enum ManagerError {
    /// A credential's `weight` is invalid; the text says which operation
    /// found it, as in `register auth: ...`.
    InvalidWeight(String),
    /// The store failed.
    Store(io::Error),
    /// The executor's refresh failed.
    Refresh(ExecError),
    /// Anything else, in upstream's words.
    Other(String),
}

impl fmt::Display for ManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWeight(message) | Self::Other(message) => f.write_str(message),
            Self::Store(err) => err.fmt(f),
            Self::Refresh(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for ManagerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Refresh(err) => Some(err),
            Self::InvalidWeight(_) | Self::Other(_) => None,
        }
    }
}

impl Manager {
    /// A manager with `settings`, reading credentials' models from `models`
    /// and saving credentials to `store` when there is one (upstream's
    /// `NewManager`). It starts empty: register executors and credentials,
    /// or [`load`](Self::load) them from the store.
    pub fn new(
        settings: Settings,
        models: Arc<dyn ClientModels>,
        store: Option<Arc<dyn AuthStore>>,
    ) -> Self {
        Self::with_clock(settings, models, store, Arc::new(Utc::now))
    }

    /// A manager reading the time from `clock`.
    pub(crate) fn with_clock(
        settings: Settings,
        models: Arc<dyn ClientModels>,
        store: Option<Arc<dyn AuthStore>>,
        clock: Clock,
    ) -> Self {
        let oauth = OAuthAliasTable::compile(&settings.oauth_model_alias);
        let affinity = Affinity::for_settings(&settings);
        let state = State {
            settings: Arc::new(settings),
            oauth: Arc::new(oauth),
            auths: BTreeMap::new(),
            epochs: HashMap::new(),
            executors: HashMap::new(),
            selector: SelectorState::default(),
            affinity,
            pool_offsets: HashMap::new(),
            refresh_jobs: HashMap::new(),
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            store,
            models,
            clock,
            persist_locks: Mutex::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
            load_gate: RwLock::new(()),
            refresh_loop: Mutex::new(None),
            error_events: OnceLock::new(),
            cooldown_store: cooldown_store::CooldownStore::default(),
        });
        let owner = Some(Arc::new(Owner(Arc::downgrade(&shared))));
        Self {
            shared,
            _owner: owner,
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.shared.state)
    }

    /// Takes the reload barrier for one change, to hold from before the
    /// state lock until the change is saved (upstream's `lockAuthMutation`,
    /// less its per-credential lock). Waits while a reload runs.
    pub(crate) fn mutation_gate(&self) -> RwLockReadGuard<'_, ()> {
        self.shared
            .load_gate
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the reload barrier for a reload: waits for every change being
    /// published or saved, and holds new ones off.
    pub(crate) fn reload_gate(&self) -> RwLockWriteGuard<'_, ()> {
        self.shared
            .load_gate
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn now(&self) -> Timestamp {
        (self.shared.clock)()
    }

    pub(crate) fn models(&self) -> &dyn ClientModels {
        &*self.shared.models
    }

    /// The current settings.
    pub fn settings(&self) -> Arc<Settings> {
        self.lock().settings.clone()
    }

    /// Replaces the settings (upstream's `SetConfig`): clears the cooldowns
    /// of credentials that no longer cool down, recompiles the OAuth model
    /// aliases, and resets the rotation and the session bindings when the
    /// routing strategy or the session affinity settings changed.
    pub fn set_settings(&self, settings: Settings) {
        let now = self.now();
        {
            let mut state = self.lock();
            // Upstream builds a new selector when the routing state
            // changes, which forgets its cursors and its bindings.
            let routing_changed = RoutingState::of(&state.settings) != RoutingState::of(&settings);
            if routing_changed {
                state.affinity = Affinity::for_settings(&settings);
            }
            state.oauth = Arc::new(OAuthAliasTable::compile(&settings.oauth_model_alias));
            state.settings = Arc::new(settings);
            let models = self.models();
            for id in lifecycle::clear_disabled_cooldown_states(&mut state, now) {
                state.sync_scheduler(models, &id, now);
            }
            if routing_changed {
                state.selector.reset_strategy();
            }
        }
        cooldown_store::changed(self);
    }

    /// The executor registered for `provider`.
    pub fn executor(&self, provider: &str) -> Option<Arc<dyn ProviderExecutor>> {
        select::lookup_executor(&self.lock().executors, provider)
    }

    /// Every credential, by ID.
    pub fn list(&self) -> Vec<Arc<Auth>> {
        self.lock()
            .auths
            .values()
            .map(|entry| entry.auth.clone())
            .collect()
    }

    /// The credential with `id`.
    pub fn get(&self, id: &str) -> Option<Arc<Auth>> {
        self.lock().auths.get(id).map(|entry| entry.auth.clone())
    }

    /// Asks every executor to close the WebSocket session `session_id`
    /// (upstream's `CloseExecutionSession`).
    pub fn close_execution_session(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        let executors: Vec<Arc<dyn ProviderExecutor>> =
            self.lock().executors.values().cloned().collect();
        for executor in executors {
            executor.close_execution_session(session_id);
        }
    }

    fn close_all_sessions(executor: &dyn ProviderExecutor) {
        executor.close_execution_session(CLOSE_ALL_EXECUTION_SESSIONS);
    }
}

/// Locks a mutex, taking the data back from a poisoned one: every update
/// leaves the state whole, so a panic elsewhere doesn't corrupt it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Dispatcher for Manager {
    fn execute<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>> {
        Box::pin(self.execute_unary(execute::CallKind::Execute, providers, request, options))
    }

    fn count_tokens<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<Response, ExecError>> {
        Box::pin(self.execute_unary(execute::CallKind::CountTokens, providers, request, options))
    }

    fn execute_stream<'a>(
        &'a self,
        providers: &'a [ProviderId],
        request: Request,
        options: Options,
    ) -> BoxFuture<'a, Result<StreamResponse, ExecError>> {
        Box::pin(self.execute_streaming(providers, request, options))
    }

    fn close_execution_session(&self, session_id: &str) {
        Manager::close_execution_session(self, session_id);
    }

    fn websocket_support(
        &self,
        providers: &[ProviderId],
        model: &str,
        auth_id: Option<&str>,
    ) -> WebsocketSupport {
        self.websocket_support_for(providers, model, auth_id)
    }

    fn codex_alpha_search(
        &self,
        request: AlphaSearch,
    ) -> BoxFuture<'_, Result<HttpReply, ExecError>> {
        Box::pin(self.alpha_search(request))
    }

    fn download(&self, download: Download) -> BoxFuture<'_, Result<Downloaded, ExecError>> {
        Box::pin(Manager::download(self, download))
    }
}
