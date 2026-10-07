// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_cooldown.go
// (RestoreCooldownStates, restoreCooldownRecordLocked,
// persistCooldownStates, cooldownStateRecordsSnapshot,
// cooldownStateRecordsForAuthLocked, authCooldownStateRecord,
// modelCooldownStateRecord and ApplyConfigWithCooldownStateStore),
// sdk/cliproxy/auth/cooldown_state.go (cooldownAuthFile) and
// sdk/cliproxy/service_auth.go (resolveCooldownStateStore) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The cooldown state store: while `save-cooldown-status` is on, the
//! credentials' cooldowns are saved to `.cds` files beside their auth files
//! (see [`file`]), and those that haven't run out are put back when the
//! store is turned on, at start or by a reload.
//!
//! The binary calls [`reconfigure`] and then [`restore`] once the
//! credentials are loaded, at start and after each reload, and [`flush`]
//! as it shuts down. The manager calls `changed` after every change that
//! may move a cooldown, once its state lock is released; that only marks
//! the store dirty and wakes its worker, a thread that waits
//! [`DEBOUNCE`] for more changes and then saves them all at once.
//!
//! A save takes a snapshot of the cooldowns under the manager's lock,
//! cloning the credentials' handles and nothing more, and writes the files
//! with the lock released, so picking a credential never waits for the
//! disk. The store's own lock serializes saves, restores and the store's
//! moves, and a snapshot equal to the last one written isn't written
//! again.
//!
//! A saved cooldown is the credential's own while it is unavailable until
//! a time to come, and each model's likewise, unless the credential is
//! disabled or doesn't cool down. Restoring puts each such record that
//! hasn't run out back on its credential, models first, and then saves
//! what is left.
//!
//! Deviations from upstream:
//! - Only the file store is ported: no store a token store backend
//!   provides, no Postgres, no Home.
//! - Saves are debounced on a background thread; upstream saves on the
//!   caller's goroutine after each change, and only when that credential's
//!   records changed. Here any change marks the store dirty, and a save
//!   that would write what was last written is skipped.
//! - Cooldowns are restored when the store is turned on or moves to
//!   another auth directory, not on every reload: a reload here keeps the
//!   credentials' state (see the binary's service), so the files hold
//!   nothing the credentials don't. A credential-wide cooldown other than a
//!   quota, which a reload's update drops as upstream's does, stays
//!   dropped where upstream's next restore puts it back.
//! - Turning `save-cooldown-status` off leaves the files as they are;
//!   upstream saves to them once more. Moving the auth directory saves to
//!   the old one first, as upstream does.
//! - Moving the auth directory loses the saved cooldowns of the files in
//!   both directories: the binary drops the old directory's credentials
//!   before the store moves, and the watcher brings the new one's after
//!   the store has restored from it, so the last save to the old directory
//!   and the first to the new leave them out, and remove their files.
//! - A restore doesn't put back what a credential holds newer. A
//!   credential-wide record is applied only if its `updated_at` is later than
//!   that of the cooldown the credential already held, so a fresh cooldown
//!   keeps its deadline and its time; upstream applies it whatever it holds.
//!   A model's record isn't applied over a state a later result cleared;
//!   against a cooldown it is merged as upstream merges it. A credential
//!   whose own cooldown was cleared by a success looks like one that never
//!   had one, as the manager keeps no time for the clear, so a record for it
//!   is applied.
//! - A load reads at most 4 MiB of a file and restores at most 10,000
//!   records from it, skipping a file over either with a warning; upstream
//!   reads a file whole and restores every record.
//! - Load and save failures are logged; there is no context to cancel them.
//! - What a file keeps of a credential's free text is scrubbed of that
//!   credential's secrets: its own keys and tokens, its credential headers
//!   and each cookie value among them, and its proxy's password, every one
//!   however short, as `[redacted]`. Upstream writes the `reason`, the
//!   quota's `reason` and the error's `code` and `message` as they came, and
//!   an upstream that quotes a key or cookie it was sent ("Bad cookie:
//!   session=...") would leave it in the file.
//! - A save that fails is not forgotten: the store stays dirty, the worker
//!   tries again after the debounce doubled for each failure in a row (up to
//!   a minute), and [`flush`] at shutdown tries once more. Upstream saves
//!   only on a change, so a failed save is made again only when something
//!   else changes.

mod file;
mod record;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use super::classify::has_unauthorized_auth_failure;
use super::cooldown::{
    cooldown_disabled_for_auth, cooldown_reason, ensure_model_state, merge_model_state,
    update_aggregated_availability,
};
use super::credential::is_zero;
use super::quota_signals::{apply_cooldown_fields, cooldown_fields_of, merge_quota_observation};
use super::text::canonical_model_key;
use super::{Entry, Manager, Settings, Shared, lock};
use crate::auth::{Auth, ModelState, QuotaState, Status, Timestamp};
use crate::config::Config;
use crate::observe::redact::{Policy, Secrets};

pub(crate) use file::FileStore;
#[cfg(test)]
pub(crate) use file::{Limits, MAX_FILE_BYTES, sanitize};
pub(crate) use record::Record;

/// How long the worker waits after a change for more before saving.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Why a load or save failed.
#[derive(Debug)]
pub(crate) struct StoreError(pub(crate) String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where saved cooldowns live (upstream's `CooldownStateStore`).
pub(crate) trait StateStore: Send + Sync {
    /// Every saved record.
    fn load(&self) -> Result<Vec<Record>, StoreError>;
    /// Replaces the saved records with `records`, at `now`.
    fn save(&self, records: &[Record], now: Timestamp) -> Result<(), StoreError>;
}

/// The store's state, in the manager.
pub(crate) struct CooldownStore {
    signal: Arc<Signal>,
}

impl Default for CooldownStore {
    fn default() -> Self {
        Self {
            signal: Arc::new(Signal {
                control: Mutex::new(Control {
                    enabled: false,
                    dirty: false,
                    shutdown: false,
                    worker: false,
                    debounce: DEBOUNCE,
                }),
                wake: Condvar::new(),
                io: Mutex::new(Io::default()),
            }),
        }
    }
}

impl fmt::Debug for CooldownStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CooldownStore").finish_non_exhaustive()
    }
}

impl Drop for CooldownStore {
    /// Stops the worker. It isn't joined: it may be the one dropping the
    /// manager.
    fn drop(&mut self) {
        lock(&self.signal.control).shutdown = true;
        self.signal.wake.notify_all();
    }
}

/// What the manager and the worker share. Locks are taken in the order
/// `io`, then the manager's state, then `control`; `control` is never held
/// across anything slow.
struct Signal {
    control: Mutex<Control>,
    wake: Condvar,
    /// Held across every load and save.
    io: Mutex<Io>,
}

struct Control {
    /// Whether changes are saved: a store is installed and restored.
    enabled: bool,
    /// A change, or a save that failed, hasn't been saved yet.
    dirty: bool,
    /// The manager is gone.
    shutdown: bool,
    /// The worker thread is running.
    worker: bool,
    debounce: Duration,
}

#[derive(Default)]
struct Io {
    target: Option<Target>,
    /// The target was installed and hasn't been restored from yet; nothing
    /// is saved to it until it is.
    pending_restore: bool,
    /// What was last written to the target, when known.
    last_written: Option<Vec<Record>>,
}

/// An installed store.
struct Target {
    /// The auth directory a file store is in; `None` for a store made by
    /// hand, which is never the same as another.
    dir: Option<PathBuf>,
    store: Arc<dyn StateStore>,
}

/// Points `manager`'s store at `config`'s auth directory, or turns it off,
/// as `save-cooldown-status` says (upstream's `resolveCooldownStateStore`
/// and `ApplyConfigWithCooldownStateStore`). The store remembers where it
/// was, so `previous`, the config before (`None` at start), isn't needed.
///
/// Moving from one directory to another saves to the old one first; the
/// new one is restored from by [`restore`] before anything is saved to it.
pub fn reconfigure(manager: &Manager, previous: Option<&Config>, config: &Config) {
    let _ = previous;
    let dir = if config.save_cooldown_status {
        match config.resolve_auth_dir() {
            Ok(dir) if dir.as_os_str().is_empty() => None,
            Ok(dir) => Some(crate::auth::path::absolute(&dir)),
            Err(err) => {
                tracing::warn!("failed to resolve cooldown state directory: {err}");
                None
            }
        }
    } else {
        None
    };
    let target = dir.map(|dir| Target {
        store: Arc::new(FileStore::new(dir.clone())),
        dir: Some(dir),
    });
    install(manager, target);
}

/// Puts the saved cooldowns that haven't run out back on `manager`'s
/// credentials, when the store was just turned on or moved, and then saves
/// what is left (upstream's `RestoreCooldownStates`).
pub fn restore(manager: &Manager, config: &Config) {
    if config.save_cooldown_status {
        restore_pending(manager);
    }
}

/// Saves the cooldowns now if a change hasn't been saved yet, waiting for
/// a save under way. For shutting down. A save that failed is tried again
/// here, though nothing has changed since.
pub fn flush(manager: &Manager) {
    flush_once(manager);
}

/// [`flush`], saying whether nothing is left unsaved.
fn flush_once(manager: &Manager) -> bool {
    let signal = &manager.shared.cooldown_store.signal;
    let mut io = lock(&signal.io);
    if io.pending_restore || io.target.is_none() {
        return true;
    }
    {
        let mut control = lock(&signal.control);
        if !control.dirty {
            return true;
        }
        // Cleared before the snapshot, so a change made while it is written
        // marks the store again; a failed save marks it too.
        control.dirty = false;
    }
    save_locked(manager, &mut io)
}

/// Notes that the cooldowns of `manager`'s credentials may have changed,
/// for the store to save (upstream's `persistCooldownStates` calls). It
/// runs on the caller's task, outside the state lock, and returns at once.
pub(crate) fn changed(manager: &Manager) {
    let signal = &manager.shared.cooldown_store.signal;
    let mut control = lock(&signal.control);
    if !control.enabled || control.dirty {
        return;
    }
    control.dirty = true;
    drop(control);
    signal.wake.notify_all();
}

/// Installs `target`, or turns the store off (upstream's
/// `ApplyConfigWithCooldownStateStore`). The same directory again changes
/// nothing.
fn install(manager: &Manager, target: Option<Target>) {
    let signal = &manager.shared.cooldown_store.signal;
    let mut io = lock(&signal.io);
    let same = match (&io.target, &target) {
        (None, None) => true,
        (Some(old), Some(new)) => old.dir.is_some() && old.dir == new.dir,
        _ => false,
    };
    if same {
        return;
    }
    if io.target.is_some() && target.is_some() && !io.pending_restore {
        save_locked(manager, &mut io);
    }
    {
        let mut control = lock(&signal.control);
        control.enabled = false;
        control.dirty = false;
    }
    io.pending_restore = target.is_some();
    io.target = target;
    io.last_written = None;
}

/// Restores from the installed store if it hasn't been yet, then turns
/// saving on.
fn restore_pending(manager: &Manager) {
    let signal = &manager.shared.cooldown_store.signal;
    let mut io = lock(&signal.io);
    if !io.pending_restore {
        return;
    }
    io.pending_restore = false;
    let Some(store) = io.target.as_ref().map(|target| target.store.clone()) else {
        return;
    };
    let records = match store.load() {
        Ok(records) => records,
        Err(err) => {
            tracing::warn!("failed to restore cooldown state: {err}");
            enable(manager);
            return;
        }
    };
    if records.is_empty() {
        io.last_written = Some(Vec::new());
        enable(manager);
        return;
    }
    apply(manager, &records);
    enable(manager);
    save_locked(manager, &mut io);
}

/// Turns saving on, starting the worker if it isn't running.
fn enable(manager: &Manager) {
    let signal = &manager.shared.cooldown_store.signal;
    let start = {
        let mut control = lock(&signal.control);
        control.enabled = true;
        !std::mem::replace(&mut control.worker, true)
    };
    if !start {
        return;
    }
    let worker_signal = Arc::clone(signal);
    let shared = Arc::downgrade(&manager.shared);
    let spawned = std::thread::Builder::new()
        .name("cooldown-store".to_owned())
        .spawn(move || run_worker(&worker_signal, &shared));
    if let Err(err) = spawned {
        tracing::warn!("failed to start the cooldown state store: {err}");
        lock(&signal.control).worker = false;
    }
}

fn wait<'a>(signal: &Signal, guard: MutexGuard<'a, Control>) -> MutexGuard<'a, Control> {
    signal
        .wake
        .wait(guard)
        .unwrap_or_else(PoisonError::into_inner)
}

/// The longest the worker waits to save again after a save failed.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// How long the worker waits before a save, after `failures` saves in a row
/// have failed: the debounce, doubled for each, up to [`MAX_RETRY_DELAY`]
/// (or the debounce itself, if that is longer).
fn save_delay(debounce: Duration, failures: u32) -> Duration {
    debounce
        .saturating_mul(1u32 << failures.min(16))
        .min(MAX_RETRY_DELAY.max(debounce))
}

/// The worker: waits for a change, then [`Control::debounce`] for more,
/// then saves. A save that fails is tried again after a longer wait each
/// time, so a store that stays out of reach isn't written to in a loop. It
/// stops when the manager is gone.
fn run_worker(signal: &Signal, shared: &Weak<Shared>) {
    let mut failures = 0u32;
    loop {
        let mut control = lock(&signal.control);
        while !control.dirty && !control.shutdown {
            control = wait(signal, control);
        }
        let deadline = Instant::now() + save_delay(control.debounce, failures);
        while !control.shutdown {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            control = signal
                .wake
                .wait_timeout(control, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if control.shutdown {
            return;
        }
        drop(control);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let manager = Manager {
            shared,
            _owner: None,
        };
        failures = if flush_once(&manager) {
            0
        } else {
            failures.saturating_add(1)
        };
    }
}

/// Saves a snapshot to the target unless it is what was last written.
/// Returns whether the target is up to date; if it isn't, the store is
/// marked dirty again, so the worker and the next [`flush`] try once more.
fn save_locked(manager: &Manager, io: &mut Io) -> bool {
    let Some(target) = &io.target else {
        return true;
    };
    let now = manager.now();
    let records = snapshot(manager, now);
    if io.last_written.as_ref() == Some(&records) {
        return true;
    }
    match target.store.save(&records, now) {
        Ok(()) => {
            io.last_written = Some(records);
            true
        }
        Err(err) => {
            tracing::warn!("failed to persist cooldown state: {err}");
            // What the target holds is unknown now, a save may have got part
            // of the way.
            io.last_written = None;
            let signal = &manager.shared.cooldown_store.signal;
            let mut control = lock(&signal.control);
            if control.enabled {
                control.dirty = true;
                drop(control);
                signal.wake.notify_all();
            }
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// Every credential's records, by provider, credential and model (upstream's
/// `cooldownStateRecordsSnapshot`). Only the handles are taken under the
/// lock.
pub(crate) fn snapshot(manager: &Manager, now: Timestamp) -> Vec<Record> {
    let (settings, auths) = {
        let state = manager.lock();
        let auths: Vec<Arc<Auth>> = state
            .auths
            .values()
            .map(|entry| entry.auth.clone())
            .collect();
        (state.settings.clone(), auths)
    };
    let mut records = Vec::new();
    for auth in &auths {
        records_for_auth(&settings, auth, now, &mut records);
    }
    records.sort_by(|a, b| {
        a.provider
            .as_bytes()
            .cmp(b.provider.as_bytes())
            .then_with(|| a.auth_id.as_bytes().cmp(b.auth_id.as_bytes()))
            .then_with(|| a.model.as_bytes().cmp(b.model.as_bytes()))
    });
    records
}

/// Whether `time` is set and after `now`.
fn after(time: Option<Timestamp>, now: Timestamp) -> bool {
    !is_zero(time) && time.is_some_and(|time| time > now)
}

/// `auth`'s records (upstream's `cooldownStateRecordsForAuthLocked`): its
/// own cooldown and each model's, while they last. Their free text is
/// scrubbed of the credential's secrets (see [`scrub_record`]).
fn records_for_auth(settings: &Settings, auth: &Auth, now: Timestamp, out: &mut Vec<Record>) {
    let first = out.len();
    collect_records(settings, auth, now, out);
    if out.len() > first {
        let mut secrets = Secrets::new();
        secrets.add_auth(auth);
        for record in out.iter_mut().skip(first) {
            scrub_record(&secrets, record);
        }
    }
}

/// Hides `secrets`, every one however short, in the free text of `record`:
/// its `reason`, its quota's `reason`, and its error's `code` and
/// `message`. An upstream's error often quotes what it was sent, and the
/// file is kept on disk.
fn scrub_record(secrets: &Secrets, record: &mut Record) {
    let hide = |text: &mut String| {
        *text = secrets.text(std::mem::take(text), Policy::Disk);
    };
    hide(&mut record.reason);
    hide(&mut record.quota.reason);
    if let Some(error) = &mut record.last_error {
        hide(&mut error.code);
        hide(&mut error.message);
    }
}

/// `auth`'s records as the manager holds them, before they are scrubbed.
fn collect_records(settings: &Settings, auth: &Auth, now: Timestamp, out: &mut Vec<Record>) {
    if auth.id.is_empty()
        || auth.disabled
        || auth.status == Status::Disabled
        || cooldown_disabled_for_auth(settings, auth)
    {
        return;
    }
    let provider = auth.provider.trim();
    let auth_file = cooldown_auth_file(auth);
    if auth.unavailable && after(auth.next_retry_after, now) {
        out.push(Record {
            provider: provider.to_owned(),
            auth_id: auth.id.clone(),
            auth_file: auth_file.clone(),
            model: String::new(),
            status: "cooling".to_owned(),
            next_retry_after: auth.next_retry_after,
            reason: cooldown_reason(&auth.status_message, &auth.quota, auth.last_error.as_ref()),
            quota: cooldown_fields(&auth.quota),
            last_error: auth.last_error.clone(),
            updated_at: record::nonzero(auth.updated_at),
        });
    }
    for (model, state) in &auth.model_states {
        let model = model.trim();
        if model.is_empty() || !state.unavailable || !after(state.next_retry_after, now) {
            continue;
        }
        out.push(Record {
            provider: provider.to_owned(),
            auth_id: auth.id.clone(),
            auth_file: auth_file.clone(),
            model: model.to_owned(),
            status: "cooling".to_owned(),
            next_retry_after: state.next_retry_after,
            reason: cooldown_reason(
                &state.status_message,
                &state.quota,
                state.last_error.as_ref(),
            ),
            quota: cooldown_fields(&state.quota),
            last_error: state.last_error.clone(),
            updated_at: record::nonzero(state.updated_at),
        });
    }
}

/// The cooldown fields of `quota`, without its quota snapshot, which isn't
/// saved (upstream's `cooldownFieldsOf`).
fn cooldown_fields(quota: &QuotaState) -> QuotaState {
    let mut fields = cooldown_fields_of(quota);
    fields.next_recover_at = record::nonzero(fields.next_recover_at);
    fields
}

/// The auth file that names `auth`'s `.cds` file: its `path` attribute, or
/// else its file name (upstream's `cooldownAuthFile`).
fn cooldown_auth_file(auth: &Auth) -> String {
    if let Some(path) = auth.attributes.get("path").map(|path| path.trim())
        && !path.is_empty()
    {
        return path.to_owned();
    }
    auth.file_name.trim().to_owned()
}

// ---------------------------------------------------------------------------
// Restoring
// ---------------------------------------------------------------------------

/// Puts `records` back on the credentials, models first, and resyncs those
/// that changed (upstream's `RestoreCooldownStates` under the lock).
/// Returns how many credentials changed.
fn apply(manager: &Manager, records: &[Record]) -> usize {
    let now = manager.now();
    let mut guard = manager.lock();
    let state = &mut *guard;
    let settings = state.settings.clone();
    let (model_records, auth_records): (Vec<&Record>, Vec<&Record>) = records
        .iter()
        .partition(|record| !record.model.trim().is_empty());
    // Taken before the models are restored, which move a credential's own
    // cooldown too.
    let held: BTreeMap<String, Option<Timestamp>> = auth_records
        .iter()
        .filter_map(|record| {
            let id = record.auth_id.trim();
            let auth = &state.auths.get(id)?.auth;
            holds_cooldown(auth).then(|| (id.to_owned(), record::nonzero(auth.updated_at)))
        })
        .collect();
    let mut changed = BTreeSet::new();
    for record in model_records.into_iter().chain(auth_records) {
        if let Some(id) = restore_record(&settings, &mut state.auths, &held, record, now) {
            changed.insert(id);
        }
    }
    for id in &changed {
        state.sync_scheduler(manager.models(), id, now);
    }
    changed.len()
}

/// Whether `auth` has a cooldown of its own, running or not (as upstream's
/// `clearCooldownStateForAuth` counts one).
fn holds_cooldown(auth: &Auth) -> bool {
    auth.unavailable
        || !is_zero(auth.next_retry_after)
        || auth.quota.exceeded
        || !is_zero(auth.quota.next_recover_at)
}

/// Puts one record back on its credential, if it is still to run out, the
/// credential cools down, isn't out of rotation for a rejected token, and
/// holds nothing newer; returns
/// the credential's ID if so (upstream's `restoreCooldownRecordLocked`).
///
/// A credential-wide record is dropped unless it is newer than what the
/// credential holds: `held` has the credentials that held a cooldown of
/// their own before the restore, with when that was set. A model's record is
/// dropped when the model's state is newer and has no cooldown, as one that
/// a later result cleared; against one with a cooldown it is merged, as
/// upstream merges it, so the longer deadline and the newer text stay.
fn restore_record(
    settings: &Settings,
    auths: &mut BTreeMap<String, Entry>,
    held: &BTreeMap<String, Option<Timestamp>>,
    record: &Record,
    now: Timestamp,
) -> Option<String> {
    let auth_id = record.auth_id.trim();
    let next_retry_after = record::nonzero(record.next_retry_after)?;
    if auth_id.is_empty() || next_retry_after <= now {
        return None;
    }
    let entry = auths.get_mut(auth_id)?;
    if entry.auth.disabled
        || entry.auth.status == Status::Disabled
        || cooldown_disabled_for_auth(settings, &entry.auth)
        || has_unauthorized_auth_failure(&entry.auth)
    {
        return None;
    }
    let updated_at = record::nonzero(record.updated_at).unwrap_or(now);
    let reason = record.reason.trim();
    let model = record.model.trim();
    if model.is_empty() {
        if held
            .get(auth_id)
            .is_some_and(|held_at| Some(updated_at) <= *held_at)
        {
            return None;
        }
    } else if entry
        .auth
        .model_states
        .get(&canonical_model_key(model))
        .is_some_and(|state| {
            !state.unavailable && state.updated_at.is_some_and(|at| at > updated_at)
        })
    {
        return None;
    }
    let mut quota = record.quota.clone();
    if quota.exceeded && is_zero(quota.next_recover_at) {
        quota.next_recover_at = Some(next_retry_after);
    }
    let auth = Arc::make_mut(&mut entry.auth);
    if model.is_empty() {
        auth.unavailable = true;
        auth.status = Status::Error;
        auth.next_retry_after = Some(next_retry_after);
        apply_cooldown_fields(&mut auth.quota, quota.clone());
        // A snapshot in the file replaces only an older one.
        auth.quota = merge_quota_observation(std::mem::take(&mut auth.quota), &quota);
        auth.generation = auth.generation.saturating_add(1);
        auth.updated_at = Some(updated_at);
        if !reason.is_empty() {
            reason.clone_into(&mut auth.status_message);
        }
        auth.last_error = record.last_error.clone();
        return Some(auth.id.clone());
    }
    if let Some(key) = ensure_model_state(auth, model)
        && let Some(state) = auth.model_states.get_mut(&key)
    {
        merge_model_state(
            state,
            &ModelState {
                status: Status::Error,
                status_message: reason.to_owned(),
                unavailable: true,
                next_retry_after: Some(next_retry_after),
                last_error: record.last_error.clone(),
                quota,
                updated_at: Some(updated_at),
            },
        );
    }
    auth.generation = auth.generation.saturating_add(1);
    auth.updated_at = Some(updated_at);
    update_aggregated_availability(auth, now);
    Some(auth.id.clone())
}

// ---------------------------------------------------------------------------
// For tests
// ---------------------------------------------------------------------------

/// Installs `store` as if the config had turned it on; [`restore_now`]
/// then restores from it.
#[cfg(test)]
pub(crate) fn install_store(manager: &Manager, store: Arc<dyn StateStore>) {
    install(manager, Some(Target { dir: None, store }));
}

/// Restores from the store just installed.
#[cfg(test)]
pub(crate) fn restore_now(manager: &Manager) {
    restore_pending(manager);
}

/// Sets how long the worker waits after a change.
#[cfg(test)]
pub(crate) fn set_debounce(manager: &Manager, debounce: Duration) {
    let signal = &manager.shared.cooldown_store.signal;
    lock(&signal.control).debounce = debounce;
    signal.wake.notify_all();
}

/// The auth directory the store is in, if it is a file store.
#[cfg(test)]
pub(crate) fn installed_dir(manager: &Manager) -> Option<PathBuf> {
    lock(&manager.shared.cooldown_store.signal.io)
        .target
        .as_ref()
        .and_then(|target| target.dir.clone())
}

/// Whether the store is installed but not yet restored from.
#[cfg(test)]
pub(crate) fn is_pending(manager: &Manager) -> bool {
    lock(&manager.shared.cooldown_store.signal.io).pending_restore
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_after_failed_saves_doubles_up_to_a_limit() {
        let debounce = Duration::from_millis(500);
        let waits: Vec<Duration> = (0..4)
            .map(|failures| save_delay(debounce, failures))
            .collect();
        assert_eq!(
            waits,
            [
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ]
        );
        for failures in [8, 9, 64, u32::MAX] {
            assert_eq!(
                save_delay(debounce, failures),
                MAX_RETRY_DELAY,
                "{failures}"
            );
        }
        // A debounce longer than the limit isn't cut short by it.
        let long = Duration::from_secs(3600);
        for failures in [0, 1, u32::MAX] {
            assert_eq!(save_delay(long, failures), long, "{failures}");
        }
    }
}
