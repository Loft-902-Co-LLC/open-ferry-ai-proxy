// Ported from CLIProxyAPI sdk/cliproxy/auth/types.go, status.go and the Error
// type in errors.go, and the Store interface in sdk/cliproxy/auth/store.go
// (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Credentials: an [`Auth`] record for each account or API key, and the
//! [`AuthStore`] that keeps them.
//!
//! A record's [`Auth::metadata`] is the credential's file as JSON, tokens
//! included, and is what gets saved; [`Auth::attributes`] are fixed settings
//! from the config, such as an API key and its base URL. The rest is runtime
//! state the credential manager keeps.
//!
//! [`FileStore`] keeps credentials as files in the auth directory, and the
//! [`synthesizer`] module builds records from those files and from the API
//! keys in the config. The other modules hold the settings records carry:
//! [`classification`] (kinds, sources and attribute names), [`compat`]
//! (OpenAI-compatible provider keys), [`metadata`]
//! (settings in a credential's file) and [`weight`] (routing weights).
//!
//! Deviations from upstream:
//! - Times are `Option`s, where upstream uses Go's zero time for "never".
//! - `Debug` leaves out metadata and attribute values, which hold secrets.
//! - Upstream's plugin fields aren't ported; nor is `Runtime`, nor
//!   `Storage`: a record's tokens live in its metadata.
//! - Upstream's store takes a context and tells a login apart from a
//!   runtime save by a flag on it; here a login calls
//!   [`AuthStore::save_new`].

pub mod classification;
pub mod codex_refresh;
pub mod compat;
pub mod credential_lock;
mod expiry;
pub mod file_guard;
pub mod file_store;
mod go;
mod index;
pub(crate) mod json;
pub mod metadata;
pub mod path;
mod recent;
pub mod synthesizer;
pub mod weight;

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

pub use classification::{AuthKind, AuthSource};
pub use expiry::{is_codex_client_user_agent, parse_go_rfc3339};
pub(crate) use expiry::{parse_time_value, zero_time};
pub use file_store::{CodexPersisted, FileStore};
pub(crate) use go::{atoi, equal_fold, parse_bool};
pub(crate) use metadata::{parse_bool_any, parse_int_any};
pub use recent::{RecentRequestBucket, RecentRequests};

/// A point in time, in UTC.
pub type Timestamp = DateTime<Utc>;

/// Where a credential is in its life (upstream's `Status`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Status {
    /// Not known yet.
    #[default]
    Unknown,
    /// Ready for calls.
    Active,
    /// Waiting on something outside the proxy, such as a sign-in.
    Pending,
    /// Being refreshed.
    Refreshing,
    /// Unavailable for now, after errors.
    Error,
    /// Turned off by the operator.
    Disabled,
}

impl Status {
    /// Upstream's name for the status.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Refreshing => "refreshing",
            Self::Error => "error",
            Self::Disabled => "disabled",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failure recorded on a credential (upstream's `auth.Error`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthError {
    /// A short code for machines, or empty.
    pub code: String,
    /// What went wrong.
    pub message: String,
    /// Whether trying again may work.
    pub retryable: bool,
    /// The HTTP status behind it, or 0.
    pub http_status: u16,
}

/// What the credential manager knows of a credential's quota (upstream's
/// `QuotaState`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotaState {
    /// Whether the credential hit its quota recently.
    pub exceeded: bool,
    /// Why, in the provider's words, or empty.
    pub reason: String,
    /// When the credential may be used again.
    pub next_recover_at: Option<Timestamp>,
    /// How many times in a row the cooldown has grown.
    pub backoff_level: u32,
    /// When the provider's response behind [`signals`](Self::signals) came,
    /// or `None` without a snapshot.
    pub observed_at: Option<Timestamp>,
    /// The quota headers of one provider response, by canonical name: a
    /// snapshot of that response, not an accumulation across responses
    /// (see the manager's `quota_signals`). Cooldown changes leave it
    /// alone.
    pub signals: BTreeMap<String, String>,
}

/// A credential's state for one model (upstream's `ModelState`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelState {
    /// Where the credential is for this model.
    pub status: Status,
    /// Why, or empty.
    pub status_message: String,
    /// Whether the model is blocked for this credential for now.
    pub unavailable: bool,
    /// When to try this model again.
    pub next_retry_after: Option<Timestamp>,
    /// The last failure for this model.
    pub last_error: Option<AuthError>,
    /// Quota for this model.
    pub quota: QuotaState,
    /// When this state last changed.
    pub updated_at: Option<Timestamp>,
    /// The execution sequence watermark taken when the current model
    /// cooldown was recorded: a success from an execution stamped at or
    /// below it started before the cooldown and must not clear it
    /// (upstream's `cooldownRecordedSeq`). Runtime only, never saved: zero
    /// after a restart, so a restored cooldown is clearable by any success.
    pub cooldown_recorded_seq: u64,
    /// This model's own cooldown while the outer fields present the
    /// credential-wide quota cooldown over it, so what the model's own
    /// failures justify isn't lost under the credential's (upstream's
    /// `credentialQuotaShadow`). `None` when no live block of its own lies
    /// beneath an outer `credential_quota`. A shadow never holds a shadow.
    /// Saved in the cooldown record's `independent_state`.
    pub credential_quota_shadow: Option<Box<ModelState>>,
    /// Whether the credential-wide quota cooldown has wrapped this state,
    /// which tells "no block of its own" from "not wrapped yet" (upstream's
    /// `credentialQuotaPropagated`). Saved in the cooldown record's
    /// `credential_quota_propagated`.
    pub credential_quota_propagated: bool,
}

impl ModelState {
    /// A copy without the shadow (upstream's `cloneShallow`): a shadow never
    /// nests.
    #[must_use]
    pub fn clone_shallow(&self) -> ModelState {
        ModelState {
            credential_quota_shadow: None,
            ..self.clone()
        }
    }
}

/// One credential and its runtime state (upstream's `Auth`).
#[derive(Clone, Default)]
pub struct Auth {
    /// Unique across restarts. For a credential file, its path relative to
    /// the auth directory.
    pub id: String,
    /// Which registration of the ID this is: the credential manager bumps
    /// it each time the ID is registered again (upstream's
    /// `RegistrationEpoch`). Zero on a record the manager didn't store,
    /// such as one just read from a file.
    pub registration_epoch: u64,
    /// How many times this registration has changed: the credential manager
    /// bumps it with each change it makes, token refreshes included
    /// (upstream's `Generation`). Zero on a record the manager didn't store.
    pub generation: u64,
    /// Which version of the credential's tokens or API key this is: the
    /// credential manager starts it at 1 and bumps it each time they are
    /// replaced, by a refresh too, so the outcome of a call made with
    /// replaced ones can be told apart (upstream's `CredentialVersion`).
    /// Zero on a record the manager didn't store.
    pub credential_version: u64,
    /// The provider, such as `codex` or `claude`.
    pub provider: String,
    /// Namespaces the credential's models, as in `team-a/gpt-5`, or empty.
    pub prefix: String,
    /// The credential's file, or empty when it comes from the config.
    pub file_name: String,
    /// A short stable ID for the management API and usage records, or empty
    /// until [`Auth::ensure_index`] derives it (upstream's `Index`).
    pub index: String,
    /// A name for logs, or empty.
    pub label: String,
    /// Where it is in its life.
    pub status: Status,
    /// Why, or empty.
    pub status_message: String,
    /// Turned off by the operator.
    pub disabled: bool,
    /// Unavailable for now, as after a quota error.
    pub unavailable: bool,
    /// A proxy for this credential's calls, or empty for the global one.
    pub proxy_url: String,
    /// Fixed settings for executors, such as `api_key` and `base_url`.
    /// `runtime_only=true` permanently discards existing refresh save work.
    /// Discard keeps rotated live tokens: supported observations cannot restore
    /// consumed disk tokens, even after the flag is removed. An explicit token
    /// command or a verified later rotation write repairs durability using the
    /// discarded-chain baseline sets without eviction while marked. Each set
    /// holds one representative per identity, keeping only its variants' agreement.
    /// Marker clearing empties the sets. Patch snapshots can still take quadratic
    /// aggregate history storage across failed saves. A different disk identity
    /// is preserved and leaves the partition marked rather than overwriting another writer.
    /// Opaque stores without read-back and no-store whole-record Replace can
    /// still restore stale tokens after cleanup; these are compatibility
    /// exceptions, not safety guarantees (see [`AuthStore::read_for_adoption`]).
    pub attributes: BTreeMap<String, String>,
    /// The credential's saved state, tokens included.
    pub metadata: Map<String, Value>,
    /// Quota across models.
    pub quota: QuotaState,
    /// The last failure, from a call or a refresh.
    pub last_error: Option<AuthError>,
    /// When the record was made.
    pub created_at: Option<Timestamp>,
    /// When it last changed.
    pub updated_at: Option<Timestamp>,
    /// When its tokens were last refreshed.
    pub last_refreshed_at: Option<Timestamp>,
    /// The earliest time to refresh again.
    pub next_refresh_after: Option<Timestamp>,
    /// The earliest time to call with it again.
    pub next_retry_after: Option<Timestamp>,
    /// State per model.
    pub model_states: BTreeMap<String, ModelState>,
    /// How many calls with the credential succeeded (upstream's `Success`).
    pub success: i64,
    /// How many calls with the credential failed (upstream's `Failed`).
    pub failed: i64,
    /// Calls per 10-minute window, for the last twenty windows.
    pub recent_requests: RecentRequests,
    /// The outer bearer a provider refused with a 401. For Codex it stays
    /// rejected until that bearer changes, independently of SIWC and refreshes.
    /// Other providers retain their successful-refresh clearing policy.
    /// Counts as expired regardless of nominal expiry. Never saved.
    pub rejected_access_token: String,
    /// Nested SIWC bearer refused with a 401, until that bearer changes.
    /// Independent of outer refreshes and the active switch. Never saved or debugged.
    pub rejected_siwc_access_token: String,
    /// Transient Codex save intent; never part of credential JSON or Debug.
    pub codex_persist_intent: codex_refresh::CodexPersistIntent,
    /// Unwritten rotations, oldest first; transient, never serialized or debugged.
    pub codex_pending_refresh: Vec<codex_refresh::CredentialRefreshPatch>,
    /// A partial refresh's failure. Executors return successful rotations in
    /// `Ok(Auth)` with a Refresh intent and this failure. The manager persists
    /// those rotations before reporting the active failure and retrying.
    /// Executors clear this marker on complete success and replace it on
    /// partial failure; due helpers mark the failed partition for retry.
    pub codex_refresh_failure: Option<CodexRefreshFailure>,
    /// The execution sequence watermark taken when the current auth-level
    /// cooldown was recorded; see [`ModelState::cooldown_recorded_seq`]
    /// (upstream's `cooldownRecordedSeq`). Runtime only.
    pub cooldown_recorded_seq: u64,
}

/// A transient partition failure accompanying successful Codex rotations.
#[derive(Clone)]
pub struct CodexRefreshFailure {
    /// Provider failure, scrubbed before it is logged or returned.
    pub error: crate::exec::ExecError,
    /// Which partition failed. The manager computes whether it is active
    /// against the live switch, and marks this partition due after backoff.
    pub partition: CodexCredentialPartition,
}

/// One independently refreshed Codex token set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexCredentialPartition {
    /// The ordinary outer OAuth credential.
    Outer,
    /// The nested direct-access OAuth credential.
    Siwc,
}

impl CodexCredentialPartition {
    /// Whether this partition supplies `auth`'s active bearer token.
    pub fn is_active(self, auth: &Auth) -> bool {
        match self {
            Self::Outer => !auth.use_siwc(),
            Self::Siwc => auth.use_siwc(),
        }
    }
}

/// Resolved origin equivalence, asserted only by save-serialized core callers.
#[derive(Clone, Copy)]
pub(crate) struct ResolvedOriginProof(pub(crate) bool);

impl Auth {
    /// Reapply unwritten rotations when adopting the same credential record.
    /// Only captured credential baselines match; new login identities and
    /// unrelated incoming settings are left intact. Disk-synthesized records
    /// need not carry the live registration epoch or generation. Public callers
    /// use source attributes; only core store callers can supply resolved proof.
    pub fn rebase_pending_refreshes_from(&mut self, live: &Self) {
        self.rebase_pending_refreshes_with_origin(live, None);
    }

    pub(crate) fn rebase_pending_refreshes_with_origin(
        &mut self,
        live: &Self,
        same_origin: Option<ResolvedOriginProof>,
    ) {
        let same_origin = same_origin.map(|proof| proof.0).unwrap_or_else(|| {
            match (
                self.attribute(classification::ATTRIBUTE_PATH)
                    .map(str::trim)
                    .filter(|path| !path.is_empty()),
                live.attribute(classification::ATTRIBUTE_PATH)
                    .map(str::trim)
                    .filter(|path| !path.is_empty()),
            ) {
                // FileStore resolves an explicit path before file_name. A bind
                // result can use a relative ID where synthesis uses a basename.
                (Some(incoming), Some(existing)) => incoming == existing,
                (None, None) => self.file_name.trim() == live.file_name.trim(),
                _ => false,
            }
        });
        if self.id != live.id || !equal_fold(self.provider.trim(), live.provider.trim()) {
            return;
        }
        if !same_origin {
            tracing::debug!(auth_id = %self.id, "skipping refresh rebase for a different origin");
            return;
        }
        let current = match &live.codex_persist_intent {
            codex_refresh::CodexPersistIntent::Refresh(patch)
                if !live
                    .codex_pending_refresh
                    .iter()
                    .any(|p| p.same_operation(patch)) =>
            {
                Some(patch)
            }
            _ => None,
        };
        for patch in live.codex_pending_refresh.iter().chain(current) {
            if !patch.is_written() {
                self.metadata = patch.apply(&self.metadata).0;
            }
        }
    }

    /// The metadata value at `key`, if it is a string.
    pub fn metadata_str(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).and_then(Value::as_str)
    }

    /// The attribute at `key`, if set.
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("id", &self.id)
            .field("registration_epoch", &self.registration_epoch)
            .field("generation", &self.generation)
            .field("credential_version", &self.credential_version)
            .field("provider", &self.provider)
            .field("prefix", &self.prefix)
            .field("file_name", &self.file_name)
            .field("index", &self.index)
            .field("label", &self.label)
            .field("status", &self.status)
            .field("disabled", &self.disabled)
            .field("unavailable", &self.unavailable)
            .field("attributes", &self.attributes.keys().collect::<Vec<_>>())
            .field("metadata", &self.metadata.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The identity of a credential file as a save would name it: the resolved
/// path made absolute with [`std::path::absolute`]. No trim (the store has
/// already trimmed), no lexical cleaning, no symlink resolution. It is not
/// the lock path, which [`credential_lock::credential_lock_path`] still names.
/// Equal paths prove one file; unequal paths prove nothing, and
/// [`prove`](Self::prove) decides. Never logged; `Debug` serves test asserts
/// only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexOrigin {
    path: PathBuf,
}

/// The result of comparing a pinned origin with a current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginProof {
    /// One file: lexically equal, or both opens gave equal handles.
    Same,
    /// Two files, established under the credential lock of one of the two
    /// paths: unequal handles, or the current path is `NotFound` while the
    /// pinned path opened. A file created there later is a different file.
    Different,
    /// Nothing established: any other open error, the pinned path failing to
    /// open included. A caller also uses it for a failed or panicked proof
    /// task and for an exhausted bound.
    Unproven,
}

impl CodexOrigin {
    /// No file I/O. Fails only when [`std::path::absolute`] does: an empty
    /// path, or no current directory for a relative path.
    pub fn of(path: &Path) -> io::Result<Self> {
        std::path::absolute(path).map(|path| Self { path })
    }

    /// The absolute path this origin names.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Blocking: opens both files, pinned (`self`) first, unless the paths
    /// are lexically equal. Never call it under the manager's state lock.
    /// The caller holds the credential lock of one of the two paths. A peer
    /// that names the file through a same-lock alias is serialized by that
    /// lock, so it cannot replace either file between the two opens. A caller
    /// that holds neither lock may rely only on `Same`; it must treat
    /// `Different` as `Unproven`.
    pub fn prove(&self, current: &Self) -> OriginProof {
        if self.path == current.path {
            return OriginProof::Same;
        }
        let Ok(pinned) = same_file::Handle::from_path(&self.path) else {
            return OriginProof::Unproven;
        };
        #[cfg(test)]
        if let Ok(lock) = credential_lock::credential_lock_path(&current.path) {
            credential_lock::test_event(&lock, "origin-between");
        }
        match same_file::Handle::from_path(&current.path) {
            Ok(handle) if handle == pinned => OriginProof::Same,
            Ok(_) => OriginProof::Different,
            Err(err) if err.kind() == io::ErrorKind::NotFound => OriginProof::Different,
            Err(_) => OriginProof::Unproven,
        }
    }
}

/// Owned lease on one credential file. Not `Clone`. Dropping it releases the
/// credential lock.
pub struct CodexLeaseToken {
    /// Resolved once; the only path the lease touches.
    path: PathBuf,
    guard: credential_lock::CredentialLock,
    /// `CodexOrigin::of(&path)`, computed before the lock is taken.
    origin: CodexOrigin,
}

impl CodexLeaseToken {
    /// The file the lease reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The identity of [`path`](Self::path), fixed when the lease was taken.
    pub fn origin(&self) -> &CodexOrigin {
        &self.origin
    }

    pub(crate) fn guard(&self) -> &credential_lock::CredentialLock {
        &self.guard
    }
}

/// What [`AuthStore::codex_refresh_lease`] gave.
pub enum CodexLease {
    Held(CodexLeaseToken),
    /// Not Codex, or the store has no file identity for this record.
    Unsupported,
}

/// The outcome of reading the requested record for observational adoption.
/// Capability is determined by the requested record, not newly read contents.
#[doc(hidden)]
#[allow(clippy::large_enum_variant)] // one owned record per synchronous read, as in read_back
pub enum AdoptionRead {
    Unsupported,
    Missing,
    Invalid,
    Record(Auth),
}

/// Keeps credentials (upstream's `auth.Store`).
pub trait AuthStore: Send + Sync + 'static {
    /// Every credential the store holds.
    fn list(&self) -> io::Result<Vec<Auth>>;

    /// Saves `auth`, and returns where it went, such as a file path. A
    /// store may decline to bring back a disabled credential that was
    /// removed, and return an empty string.
    fn save(&self, auth: &Auth) -> io::Result<String>;

    /// Saves a credential from a login or migration: as
    /// [`save`](Self::save), but always creating it, even when disabled.
    fn save_new(&self, auth: &Auth) -> io::Result<String> {
        self.save(auth)
    }

    /// Removes the credential with `id`. Removing one that isn't there is
    /// not an error.
    fn delete(&self, id: &str) -> io::Result<()>;

    /// Not upstream's: reads `auth`'s own record again from the store, and
    /// changes nothing there. `None` when the store can't tell, or holds no
    /// such record. A worker whose gate isn't open uses it to pick up the
    /// token the active process refreshed.
    fn read_back(&self, _auth: &Auth) -> io::Result<Option<Auth>> {
        Ok(None)
    }

    /// Reads the current record for save-serialized adoption. Opaque stores
    /// preserve compatibility: `None` means unsupported, not missing.
    /// Implement this classification to protect replacements with fresh reads.
    /// Without read-back, a stale whole-record replacement after a written save
    /// can still restore consumed tokens. With no store, replacement after an
    /// ineligible cleanup has the same compatibility limit, not a safety guarantee.
    fn read_for_adoption(&self, auth: &Auth) -> io::Result<AdoptionRead> {
        if !codex_refresh::is_codex_auth(auth) {
            return Ok(AdoptionRead::Unsupported);
        }
        Ok(match self.read_back(auth)? {
            Some(record) => AdoptionRead::Record(record),
            None => AdoptionRead::Unsupported,
        })
    }

    /// Whether saves honor Codex's scoped, baseline-checked refresh intents.
    /// Opaque stores retain the ordinary generation watermark protection.
    fn supports_codex_refresh_patches(&self) -> bool {
        false
    }

    /// Not upstream's: whether two snapshots address the same backing record.
    /// Scoped refreshes use this when a newer settings save has completed.
    /// Opaque stores keep the conservative path/name comparison; file stores
    /// compare resolved paths so equivalent source attributes are accepted.
    fn same_record(&self, a: &Auth, b: &Auth) -> io::Result<bool> {
        Ok(a.file_name == b.file_name
            && a.attributes.get(classification::ATTRIBUTE_PATH)
                == b.attributes.get(classification::ATTRIBUTE_PATH))
    }

    /// Not upstream's: a counter that changes whenever the store's mapping
    /// from an `Auth` to its record changes (a directory rebind, for a file
    /// store). A reader samples it before `read_back` and compares after, to
    /// tell that the record it read may no longer be the `Auth`'s own.
    fn binding_generation(&self) -> u64 {
        0
    }

    /// Not upstream's. Blocking. `Ok(Unsupported)` first when `auth` is not a
    /// Codex record, with no lock file created. Then resolves `auth` to its
    /// file (any resolve error is `Ok(Unsupported)`), refuses lock names,
    /// checks the file guard, fixes the origin identity, and acquires the
    /// credential lock, waiting at most `CREDENTIAL_LOCK_TIMEOUT`. Fails with
    /// `TimedOut` or a lock or guard error. The default has no file identity.
    fn codex_refresh_lease(&self, _auth: &Auth) -> io::Result<CodexLease> {
        Ok(CodexLease::Unsupported)
    }

    /// Not upstream's. Blocking. Reads the leased file's metadata object, or
    /// `None` when the file does not exist. Never re-resolves: it reads
    /// `token.path()`.
    fn codex_lease_read(&self, _token: &CodexLeaseToken) -> io::Result<Option<Map<String, Value>>> {
        Err(unsupported())
    }

    /// Not upstream's. Blocking. Applies `patch`, composed with
    /// `auth.codex_pending_refresh`, to the leased file and writes it
    /// atomically. Never re-resolves: it writes `token.path()`. Needs an
    /// existing file with `type` codex. Returns the read-back record and the
    /// per-partition outcomes.
    fn codex_lease_commit(
        &self,
        _token: &CodexLeaseToken,
        _auth: &Auth,
        _patch: &codex_refresh::CredentialRefreshPatch,
    ) -> io::Result<CodexPersisted> {
        Err(unsupported())
    }

    /// Not upstream's. Does no file I/O, so it is callable under the
    /// manager's state lock. The absolute path a save of `auth` would write
    /// now, under the store's current binding; `None` when the store has no
    /// file identity for the record. Whether two origins name one file is
    /// [`CodexOrigin::prove`]'s question, asked outside the state lock and
    /// under a credential lock.
    fn codex_origin(&self, _auth: &Auth) -> io::Result<Option<CodexOrigin>> {
        Ok(None)
    }

    /// Not upstream's. Saves `auth` only if the path it resolves to is
    /// `origin`'s file, proven under that path's credential lock. `Ok(None)`
    /// when the proof is not `Same`: nothing was written, and `refused` was
    /// called once with the resolved path's origin and the proof (`Different`
    /// or `Unproven`) while the store held its write permit and that
    /// credential lock, before the save mutex. `refused` may take the
    /// manager's state lock. It must do no I/O, and the only store method it
    /// may call is `codex_origin`. A store with no file identity saves and
    /// never calls `refused`.
    fn save_bound(
        &self,
        auth: &Auth,
        _origin: &CodexOrigin,
        _refused: &mut dyn FnMut(&CodexOrigin, OriginProof),
    ) -> io::Result<Option<String>> {
        self.save(auth).map(Some)
    }

    /// Not upstream's. The unleased save. Calls `guard` once, while the store
    /// holds its write permit and the destination's credential lock and
    /// before the save mutex, with the destination's origin (`None` for a
    /// store or record with no file identity). `Err(reason)` from `guard`:
    /// nothing is written, `Ok(Deferred(reason))`. `Ok(withheld)`: every
    /// withheld partition is written exactly as the destination file holds
    /// it, and a missing destination, or one lacking a withheld partition
    /// `auth` holds, gives `Deferred(MissingFile | MissingPartition)` with
    /// nothing created or written. `guard` has `save_bound`'s `refused`
    /// contract. The default cannot constrain fields, so it refuses a
    /// non-empty withheld set.
    fn save_guarded(
        &self,
        auth: &Auth,
        guard: &mut dyn FnMut(Option<&CodexOrigin>) -> Result<WithheldPartitions, SaveDeferral>,
    ) -> io::Result<GuardedSave> {
        match guard(None) {
            Err(reason) => Ok(GuardedSave::Deferred(reason)),
            Ok(withheld) if withheld.any() => Ok(GuardedSave::Deferred(SaveDeferral::Unsupported)),
            Ok(_) => self.save(auth).map(GuardedSave::Written),
        }
    }
}

/// The partitions an unleased save must leave exactly as the destination file
/// holds them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WithheldPartitions {
    /// The top-level `OUTER_KEYS`.
    pub outer: bool,
    /// The nested `siwc` object.
    pub siwc: bool,
}

impl WithheldPartitions {
    /// Whether any partition is withheld.
    pub fn any(self) -> bool {
        self.outer || self.siwc
    }
}

/// Why `save_guarded` wrote nothing. Carries no token, path or account data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveDeferral {
    /// Store: a partition is withheld and the destination does not exist.
    MissingFile,
    /// Store: the destination lacks a withheld partition `auth` holds.
    MissingPartition,
    /// Store: a withheld set the store cannot honour (the default body).
    Unsupported,
    /// Guard: the unresolved state now names a partition not withheld.
    StateChanged,
    /// Guard: an operation was written, or live's tokens moved.
    Stale,
    /// Guard: live no longer resolves to the destination.
    OriginMoved,
    /// Guard: the entry is gone or has a new epoch.
    RegistrationChanged,
    /// Guard: a refresh pass on this id holds its refresh mutex.
    RefreshInFlight,
}

/// The result of [`AuthStore::save_guarded`].
#[derive(Debug)]
pub enum GuardedSave {
    /// Saved; the file's path, or an empty string for a disabled credential
    /// whose file is gone.
    Written(String),
    /// Nothing was written.
    Deferred(SaveDeferral),
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "auth store: no credential lease for this store",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_leaves_out_secrets() {
        let mut auth = Auth {
            id: "codex-a.json".into(),
            provider: "codex".into(),
            ..Auth::default()
        };
        auth.metadata
            .insert("access_token".into(), Value::from("secret-token"));
        auth.attributes
            .insert("api_key".into(), "secret-key".into());
        let debug = format!("{auth:?}");
        assert!(debug.contains("access_token"), "{debug}");
        assert!(debug.contains("api_key"), "{debug}");
        assert!(!debug.contains("secret"), "{debug}");
    }
}
