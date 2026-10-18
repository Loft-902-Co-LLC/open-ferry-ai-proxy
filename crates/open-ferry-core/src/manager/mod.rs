// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor.go (the Manager type,
// NewManager, SetConfig and CloseExecutionSession) (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The credential manager: the [`Dispatcher`] that picks a credential for
//! each call, calls its provider's executor, and keeps every credential's
//! cooldowns and token refreshes.
//!
//! A call names the providers that serve its model. The manager picks a
//! ready credential among them (round-robin, fill-first, weighted, or by
//! the quota the providers report, within the highest priority), calls the
//! executor, and records the outcome: a failure cools the model or the
//! credential down for a while, a success clears it. When a credential
//! fails, the next one is tried; when every one has failed with an error a
//! later round could fix, the manager waits for the nearest cooldown (up to
//! `max_retry_interval`) and goes round again, up to `request_retry` times.
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
//! With `routing.quota.check-after` set, a quota rest longer than that is
//! cut to it, and then one call is let through to check (see
//! `quota_check`).
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
//! - open-ferry's own `quota` routing strategy (see `quota_rank`) and cap
//!   on long quota rests (see `quota_check`) aren't upstream's.
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
mod quota_check;
mod quota_rank;
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
pub use lifecycle::{AdoptionResult, CompletedTokenCommand, QuotaReset};
pub use quota_check::QuotaCheck;
pub use quota_rank::{QuotaPreference, QuotaPrefs};
pub use quota_signals::provider_supports_quota_observation;
pub use refresh::ForceRefreshResult;
pub use select::{ClientModels, ModelProjection};
pub use settings::{
    ApiKeyEntry, ModelAlias, OpenAiCompat, RequestScopedErrorRule, RoutingStrategy, Settings,
};

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, Weak};

use chrono::Utc;
use futures_core::future::BoxFuture;

use crate::auth::{Auth, AuthStore, Timestamp};
use crate::exec::{
    AlphaSearch, Dispatcher, Download, Downloaded, ExecError, HttpReply, Options, ProviderId,
    Request, Response, StreamResponse, WebsocketSupport,
};
use crate::executor::ProviderExecutor;
use crate::gate::{ActiveGate, NotActive};
use affinity::Affinity;
use models::OAuthAliasTable;
use refresh::{RefreshJob, RefreshLoopHandle};
use select::{CLOSE_ALL_EXECUTION_SESSIONS, SelectorState};
use settings::RoutingState;

/// The last (epoch, generation) saved for one credential.
type PersistLock = Arc<Mutex<(u64, u64)>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Persisted {
    Written {
        requested: UndurablePartitions,
    },
    Skipped,
    Ineligible,
    /// The store refused a Codex save without writing; the change is owed.
    Deferred(crate::auth::SaveDeferral),
}

/// A refused save whose change is still owed to the file. Memory only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwedSave {
    /// Why the latest refusal happened.
    pub(crate) reason: crate::auth::SaveDeferral,
    /// The snapshot generation of the refused save, raised to the largest seen
    /// while owed. Only a `Written` Configuration save of this generation or a
    /// later one covers it.
    pub(crate) version: u64,
}

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
    /// Revokes snapshots of refresh work discarded by ineligibility.
    pub(crate) discard_epoch: u64,
    /// Partitions whose rotated live credentials have lost persistence work.
    pub(crate) undurable: UndurablePartitions,
    /// Baseline sets without eviction, with one agreement entry per identity in
    /// each set. Clearing a marker empties its set. Patch snapshot copies can
    /// still use quadratic aggregate history storage across failed saves.
    pub(crate) undurable_baselines: UndurableBaselines,
    /// A post-send origin that could not be proved. Memory only, like
    /// `undurable`.
    pub(crate) unresolved_origin: Option<UnresolvedOrigin>,
    /// Why each marked partition is marked. Memory only; set exactly where
    /// `undurable` is.
    pub(crate) undurable_causes: MarkCauses,
    /// Partitions held because a read-back mark's later pass found a third
    /// token. Memory only; only a token command ends it.
    pub(crate) landing_unconfirmed: UndurablePartitions,
    /// A refused save whose change is still owed. Memory only.
    pub(crate) owed_save: Option<OwedSave>,
    /// The newest generation a Configuration save has written for this
    /// registration. A refusal at or below it owes nothing, as that save
    /// covered it. Memory only.
    pub(crate) written_configuration: u64,
    /// Partitions whose unsettled read-back mark has already been warned about.
    /// Memory only; cleared wherever the partition's `ReadBack` cause ends, so a
    /// later cause warns again.
    pub(crate) read_back_warned: UndurablePartitions,
}

/// A post-send origin that could not be proved. Memory only, like `undurable`.
#[derive(Clone)]
pub(crate) struct UnresolvedOrigin {
    /// The pinned origin of the pass that published.
    pub(crate) origin: crate::auth::CodexOrigin,
    /// The partitions whose live tokens unleased adoption must keep.
    pub(crate) partitions: UndurablePartitions,
    /// The rotation whose baselines a `Different` resolution marks.
    pub(crate) rotation: crate::auth::codex_refresh::CredentialRefreshPatch,
    /// Deferrals in a row with the state present. 0 when the state is
    /// recorded.
    pub(crate) unproven: u32,
}

/// Why a partition is marked. Memory only.
#[derive(Clone, Debug)]
pub(crate) enum MarkCause {
    /// The write of the rotation to `origin` may have landed: the commits were
    /// exhausted and the confirmation read failed or found no file, or the
    /// lease was lost. A later leased pass on that file settles it.
    ReadBack { origin: crate::auth::CodexOrigin },
    /// Every other mark.
    Different,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MarkCauses {
    pub(crate) outer: Option<MarkCause>,
    pub(crate) siwc: Option<MarkCause>,
}

impl Entry {
    /// The partitions unleased adoption must not replace: `undurable` plus
    /// the unresolved state's.
    pub(crate) fn protected(&self) -> UndurablePartitions {
        self.undurable.union(self.unresolved_partitions())
    }

    /// The unresolved state's partitions (none without a state).
    pub(crate) fn unresolved_partitions(&self) -> UndurablePartitions {
        self.unresolved_origin
            .as_ref()
            .map(|u| u.partitions)
            .unwrap_or_default()
    }

    /// The partitions whose mark has a `ReadBack` cause.
    pub(crate) fn read_back_partitions(&self) -> UndurablePartitions {
        UndurablePartitions {
            outer: matches!(
                self.undurable_causes.outer,
                Some(MarkCause::ReadBack { .. })
            ),
            siwc: matches!(self.undurable_causes.siwc, Some(MarkCause::ReadBack { .. })),
        }
    }

    /// What every unleased save withholds: the unresolved state's partitions
    /// and the partitions with a `ReadBack` cause.
    pub(crate) fn withheld(&self) -> UndurablePartitions {
        self.unresolved_partitions()
            .union(self.read_back_partitions())
    }

    /// Records the state for `rotation.requested()`. A present state with the
    /// same origin gains the partitions and takes the newer rotation; any
    /// other present state is replaced. At most one setter runs per pass, so
    /// neither happens in practice.
    pub(crate) fn record_unresolved(
        &mut self,
        origin: &crate::auth::CodexOrigin,
        rotation: &crate::auth::codex_refresh::CredentialRefreshPatch,
    ) {
        let requested = rotation.requested();
        let partitions = UndurablePartitions {
            outer: requested.outer,
            siwc: requested.siwc,
        };
        match self.unresolved_origin.as_mut() {
            Some(present) if present.origin == *origin => {
                present.partitions = present.partitions.union(partitions);
                present.rotation = rotation.clone();
            }
            _ => {
                self.unresolved_origin = Some(UnresolvedOrigin {
                    origin: origin.clone(),
                    partitions,
                    rotation: rotation.clone(),
                    unproven: 0,
                });
            }
        }
    }

    /// Sets the state to `None`. The record is not changed.
    pub(crate) fn clear_unresolved(&mut self) {
        self.unresolved_origin = None;
    }

    /// Why this process has paused refreshing the credential, and which
    /// partitions are held. Derived, so it clears with the state it reads.
    pub(crate) fn refresh_hold(&self) -> Option<(RefreshHold, HeldPartitions)> {
        let surfaced = self
            .unresolved_origin
            .as_ref()
            .filter(|u| u.unproven >= refresh::ORIGIN_UNRESOLVED_SURFACE_AFTER);
        let mut held = self.landing_unconfirmed;
        if surfaced.is_some() {
            let u = self.unresolved_partitions();
            held.outer |= u.outer;
            held.siwc |= u.siwc;
        }
        let parts = match (held.outer, held.siwc) {
            (true, true) => HeldPartitions::Both,
            (true, false) => HeldPartitions::Outer,
            (false, true) => HeldPartitions::Siwc,
            (false, false) => return None,
        };
        if self.landing_unconfirmed.outer || self.landing_unconfirmed.siwc {
            return Some((RefreshHold::LandingUnconfirmed, parts));
        }
        surfaced.map(|_| (RefreshHold::OriginUnresolved, parts))
    }
}

/// Why this process has paused refreshing a credential. Display only: memory
/// only, never written to the auth file, and it carries no token, path or
/// account data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshHold {
    /// The unresolved state has deferred `ORIGIN_UNRESOLVED_SURFACE_AFTER`
    /// times.
    OriginUnresolved,
    /// A read-back mark's later pass found a third token; only a token
    /// command or a restart ends it.
    LandingUnconfirmed,
}

impl RefreshHold {
    /// The JSON value of `refresh_hold`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OriginUnresolved => "origin_unresolved",
            Self::LandingUnconfirmed => "landing_unconfirmed",
        }
    }
}

/// Which partitions the standing holds cover, so a reader can say which
/// sign-in or bind ends them. Memory only; no token, path or account data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldPartitions {
    Outer,
    Siwc,
    Both,
}

impl HeldPartitions {
    /// The JSON value of `refresh_hold_partitions`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outer => "outer",
            Self::Siwc => "siwc",
            Self::Both => "both",
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct UndurableBaselines {
    pub(crate) outer: Vec<serde_json::Map<String, serde_json::Value>>,
    pub(crate) siwc: Vec<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UndurablePartitions {
    pub(crate) outer: bool,
    pub(crate) siwc: bool,
}

impl UndurablePartitions {
    pub(crate) fn union(self, other: Self) -> Self {
        Self {
            outer: self.outer || other.outer,
            siwc: self.siwc || other.siwc,
        }
    }

    pub(crate) fn contains(self, partition: crate::auth::CodexCredentialPartition) -> bool {
        match partition {
            crate::auth::CodexCredentialPartition::Outer => self.outer,
            crate::auth::CodexCredentialPartition::Siwc => self.siwc,
        }
    }
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
    /// For each credential, how many published snapshots are still to be
    /// saved: raised with the publication, lowered when its save returns,
    /// whatever the result (see `PendingSave`). Adopting a credential from
    /// the store waits for it to be zero, so an older save can't land on top
    /// of what was adopted.
    pub(crate) pending_saves: HashMap<String, u32>,
    /// Counts the publications of a snapshot to be saved, over all
    /// credentials, and never goes back, so a number it gave is never given
    /// again.
    pub(crate) save_publications: u64,
    /// For each credential, the number [`save_publications`](Self) gave its
    /// latest publication of a snapshot to be saved: the credential's
    /// save-publication sequence, which only rises. Unlike `pending_saves`
    /// it is not dropped when its saves return, for adopting from the store
    /// must see a save that began and finished while it read. Dropped with
    /// the credential.
    pub(crate) save_sequence: HashMap<String, u64>,
    /// The quota rests capped by `routing.quota.check-after`.
    pub(crate) quota_checks: quota_check::QuotaChecks,
    /// For each credential whose refresh was deferred for a missing file, the
    /// registration epoch of the episode that was logged at warn. The episode
    /// ends with a non-empty Codex save or the registration's removal.
    pub(crate) missing_file_deferrals: HashMap<String, u64>,
}

pub(crate) struct Shared {
    state: Mutex<State>,
    store: Option<Arc<dyn AuthStore>>,
    models: Arc<dyn ClientModels>,
    clock: Clock,
    /// The saved or adopted (epoch, generation) watermark per credential.
    /// Its locks are short-held and never held across I/O.
    persist_locks: Mutex<HashMap<String, PersistLock>>,
    /// Serializes saves and read-back through live reconciliation, including their I/O (upstream's `persistLocks`).
    persist_save_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Serializes refreshes per credential (upstream's `refreshLocks`).
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The reload barrier (upstream's `authLoadGate`): held shared by a
    /// change from before its publication until its save is done, and
    /// exclusively by [`Manager::load`] from listing the store until every
    /// credential is replaced. Taken before the state lock, never twice by
    /// one thread.
    load_gate: Arc<ReloadBarrier>,
    refresh_loop: Mutex<Option<RefreshLoopHandle>>,
    /// Stamps upstream dispatches with a monotonic sequence, so a success
    /// from a call dispatched before a cooldown was recorded can't clear
    /// that cooldown (upstream's `executionSeq`).
    execution_seq: AtomicU64,
    /// What failed calls are told to, once set.
    error_events: OnceLock<Arc<dyn ErrorEvents>>,
    /// The cooldown state store.
    cooldown_store: cooldown_store::CooldownStore,
    /// Not upstream's: whether this process may write; always open until a
    /// switch sets one.
    gate: RwLock<ActiveGate>,
    /// Test seam: called before eligibility checks and the save mutex,
    /// including for no-store and unsaved updates.
    #[cfg(test)]
    persist_hook: Mutex<Option<PersistHook>>,
    /// Test seam: after final write admission, with save mutex held and no state lock.
    #[cfg(test)]
    persist_admitted_hook: Mutex<Option<PersistHook>>,
    /// Test seam: called before Written reconciliation takes the save mutex.
    #[cfg(test)]
    reconcile_hook: Mutex<Option<PersistHook>>,
    /// Test seam: called after read-back while the save mutex is held.
    #[cfg(test)]
    read_back_hook: Mutex<Option<PersistHook>>,
    /// Test seam: called after adopting from the store has read the record
    /// and before it takes the state lock to apply it.
    #[cfg(test)]
    adopt_hook: Mutex<Option<AdoptHook>>,
    /// Test seam: called as adopting from the store goes to take the save
    /// mutex, and again after it has released it.
    #[cfg(test)]
    adopt_unheld_hook: Mutex<Option<AdoptHook>>,
    /// Test seam: called after each origin proof of a leased origin check,
    /// with the attempt number (1-based).
    #[cfg(test)]
    origin_check_hook: Mutex<Option<OriginCheckHook>>,
    /// Test seam: overrides the rotation commit's retry window.
    #[cfg(test)]
    lease_retry_window: Mutex<Option<std::time::Duration>>,
    /// Test seam: the send fence signals the first notify when a pass is
    /// about to take the fence, and waits for the second before it does.
    #[cfg(test)]
    send_fence_pause: Mutex<Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>>,
    /// Test seam: called with the published snapshot after the lease is
    /// released and before the ordinary save.
    #[cfg(test)]
    lease_tail_hook: Mutex<Option<PersistHook>>,
    /// Test seam: called with the saved snapshot after an unleased save has
    /// written and released check (vi)'s hold on the refresh mutex.
    #[cfg(test)]
    unleased_saved_hook: Mutex<Option<PersistHook>>,
}

/// What a test runs after each proof of a leased origin check.
#[cfg(test)]
pub(crate) type OriginCheckHook = Arc<dyn Fn(u32) + Send + Sync>;

/// What a test runs before a save takes the persist lock.
#[cfg(test)]
pub(crate) type PersistHook = Arc<dyn Fn(&Auth) + Send + Sync>;

/// What a test runs between adopting's read and its swap.
#[cfg(test)]
pub(crate) type AdoptHook = Arc<dyn Fn() + Send + Sync>;

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
    /// Not upstream's: this process isn't the active version, so it didn't
    /// refresh or write.
    NotActive(NotActive),
    /// Replace could not obtain a valid current record; nothing was published.
    /// The caller must retry the observation, including its settings.
    AdoptionRetry,
    /// Anything else, in upstream's words.
    Other(String),
}

impl fmt::Display for ManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWeight(message) | Self::Other(message) => f.write_str(message),
            Self::Store(err) => err.fmt(f),
            Self::Refresh(err) => err.fmt(f),
            Self::NotActive(err) => err.fmt(f),
            Self::AdoptionRetry => f.write_str("credential observation must be retried"),
        }
    }
}

impl std::error::Error for ManagerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Refresh(err) => Some(err),
            Self::NotActive(err) => Some(err),
            Self::InvalidWeight(_) | Self::Other(_) | Self::AdoptionRetry => None,
        }
    }
}

/// Why [`Manager::edit`] failed.
#[derive(Debug)]
pub enum EditError<E> {
    /// The closure refused the change; nothing was published.
    Rejected(E),
    /// The manager failed, as [`Manager::update`] can.
    Manager(ManagerError),
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
            pending_saves: HashMap::new(),
            save_publications: 0,
            save_sequence: HashMap::new(),
            quota_checks: quota_check::QuotaChecks::default(),
            missing_file_deferrals: HashMap::new(),
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            store,
            models,
            clock,
            persist_locks: Mutex::new(HashMap::new()),
            persist_save_locks: Mutex::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
            load_gate: Arc::new(ReloadBarrier::default()),
            refresh_loop: Mutex::new(None),
            error_events: OnceLock::new(),
            cooldown_store: cooldown_store::CooldownStore::default(),
            gate: RwLock::new(ActiveGate::open()),
            execution_seq: AtomicU64::new(0),
            #[cfg(test)]
            persist_hook: Mutex::new(None),
            #[cfg(test)]
            persist_admitted_hook: Mutex::new(None),
            #[cfg(test)]
            reconcile_hook: Mutex::new(None),
            #[cfg(test)]
            read_back_hook: Mutex::new(None),
            #[cfg(test)]
            adopt_hook: Mutex::new(None),
            #[cfg(test)]
            adopt_unheld_hook: Mutex::new(None),
            #[cfg(test)]
            origin_check_hook: Mutex::new(None),
            #[cfg(test)]
            lease_retry_window: Mutex::new(None),
            #[cfg(test)]
            send_fence_pause: Mutex::new(None),
            #[cfg(test)]
            lease_tail_hook: Mutex::new(None),
            #[cfg(test)]
            unleased_saved_hook: Mutex::new(None),
        });
        let owner = Some(Arc::new(Owner(Arc::downgrade(&shared))));
        Self {
            shared,
            _owner: owner,
        }
    }

    /// Not upstream's: puts the manager under `gate`. Refreshes are
    /// operations of it, and a worker whose gate isn't open never refreshes.
    pub fn set_gate(&self, gate: ActiveGate) {
        *self
            .shared
            .gate
            .write()
            .unwrap_or_else(PoisonError::into_inner) = gate;
    }

    /// The gate the manager runs under.
    pub fn gate(&self) -> ActiveGate {
        self.shared
            .gate
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stamps a new upstream dispatch with the next execution sequence
    /// (upstream's `nextExecutionSeq`). The stamp travels on the call's
    /// [`CallResult::execution_seq`].
    pub(crate) fn next_execution_seq(&self) -> u64 {
        self.shared.execution_seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The latest execution sequence issued (upstream's
    /// `currentExecutionSeq`): the watermark a cooldown records, which
    /// marks every dispatch up to now as older than it.
    pub(crate) fn current_execution_seq(&self) -> u64 {
        self.shared.execution_seq.load(Ordering::SeqCst)
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.shared.state)
    }

    /// The credential's refresh hold and the partitions it covers, if any,
    /// read in one short hold of the state lock. `None` for an unknown id.
    pub fn refresh_hold(&self, id: &str) -> Option<(RefreshHold, HeldPartitions)> {
        self.lock().auths.get(id)?.refresh_hold()
    }

    /// Takes the reload barrier for one change, to hold from before the
    /// state lock until the change is saved (upstream's `lockAuthMutation`,
    /// less its per-credential lock). Waits while a reload runs.
    pub(crate) fn mutation_gate(&self) -> ReloadGuard {
        self.shared.load_gate.acquire(false)
    }

    /// Takes the reload barrier for a reload: waits for every change being
    /// published or saved, and holds new ones off.
    pub(crate) fn reload_gate(&self) -> ReloadGuard {
        self.shared.load_gate.acquire(true)
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

// An owned reload permit can follow a detached save onto a blocking thread;
// std's borrowed RwLock guard cannot be transferred between threads.
#[derive(Default)]
struct ReloadBarrier {
    state: Mutex<(usize, bool, usize)>,
    changed: Condvar,
}

#[must_use]
pub(crate) struct ReloadGuard {
    barrier: Arc<ReloadBarrier>,
    reload: bool,
}

impl ReloadBarrier {
    fn acquire(self: &Arc<Self>, reload: bool) -> ReloadGuard {
        let mut state = lock(&self.state);
        if reload {
            state.2 += 1;
            while state.0 != 0 || state.1 {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            state.2 -= 1;
            state.1 = true;
        } else {
            while state.1 || state.2 != 0 {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            state.0 += 1;
        }
        ReloadGuard {
            barrier: self.clone(),
            reload,
        }
    }
}

impl Drop for ReloadGuard {
    fn drop(&mut self) {
        let mut state = lock(&self.barrier.state);
        if self.reload {
            state.1 = false;
        } else {
            state.0 -= 1;
        }
        self.barrier.changed.notify_all();
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

    fn interrupt_execution_session<'a>(
        &'a self,
        session_id: &'a str,
        payload: &'a [u8],
    ) -> BoxFuture<'a, Result<bool, ExecError>> {
        Box::pin(Manager::interrupt_execution_session(
            self, session_id, payload,
        ))
    }

    fn websocket_support(
        &self,
        providers: &[ProviderId],
        model: &str,
        auth_id: Option<&str>,
    ) -> WebsocketSupport {
        self.websocket_support_for(providers, model, auth_id)
    }

    fn websocket_auth(&self, auth_id: &str) -> Option<Arc<Auth>> {
        self.websocket_auth_for(auth_id)
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
