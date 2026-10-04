// Ported from CLIProxyAPI internal/api/handlers/management/oauth_sessions.go
// (oauthSessionStore, newOAuthSessionStore, purgeExpiredLocked, Register,
// SetError, Complete, Get, IsPending, Cancel, GetOAuthSession,
// GetOAuthSessionDetails, guardOAuthSessionPendingForSave,
// oauthSessionErrorWithCause, ValidateOAuthState, NormalizeOAuthProvider,
// NormalizeOAuthCallbackProvider, NormalizePluginOAuthCallbackProvider,
// WriteOAuthCallbackFileForPendingSession) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The OAuth login sessions, each under the `state` its login sent to the
//! provider.
//!
//! A session is pending from its start until its login completes, fails or
//! is cancelled, or it expires: 30 minutes after it started or last failed.
//! A completed session is kept for a minute, so that a client polling the
//! status learns it completed. While a session is pending, its callback
//! (the code, or the error the provider reported) can be handed to the
//! login waiting for it.
//!
//! Deviations from upstream:
//! - A callback reaches the waiting login through a channel the session
//!   holds, instead of a `.oauth-<provider>-<state>.oauth` file in the auth
//!   directory: nothing is written, and no directory made, for a callback.
//!   A session takes the first callback it is given; a later one, while it
//!   is still pending, is answered as taken and dropped. Upstream's login
//!   reads the last file written before it next looks, twice a second.
//! - At most 1024 sessions are kept: past that, a login can't start until
//!   older sessions expire. Upstream keeps any number.
//! - Plugin sessions, their metadata and `CompleteProvider`, which only the
//!   plugin host and embedders use, aren't ported.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use open_ferry_translate::go::to_lower;
use tokio::sync::oneshot;

use super::Redacted;
use crate::go::equal_fold;

/// How long a session lasts after it starts or fails (`oauthSessionTTL`).
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// How long a completed session is kept (`oauthCompletedSessionTTL`).
const COMPLETED_TTL: Duration = Duration::from_secs(60);

/// The longest valid state, in bytes (`maxOAuthStateLength`).
const MAX_STATE_LENGTH: usize = 128;

/// The most sessions kept at once.
pub(crate) const MAX_SESSIONS: usize = 1024;

/// What came back to a login's callback, trimmed: the code, or the error
/// the provider reported. Its `Debug` hides both, as the error may quote
/// the code.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Callback {
    pub(crate) code: String,
    pub(crate) error: String,
}

impl fmt::Debug for Callback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Callback")
            .field("code", &Redacted(&self.code))
            .field("error", &Redacted(&self.error))
            .finish()
    }
}

/// A session, as [`Store::get`] shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Session {
    /// The provider, in lower case, such as `anthropic` or `codex`.
    pub(crate) provider: String,
    /// Why the login failed, or empty.
    pub(crate) status: String,
    /// Whether the login completed.
    pub(crate) completed: bool,
    /// When the session expires.
    pub(crate) expires_at: Instant,
}

impl Session {
    /// Whether the login still waits: it hasn't completed or failed.
    fn is_pending(&self) -> bool {
        !self.completed && self.status.is_empty()
    }
}

/// A callback that couldn't be handed over: there is no pending session
/// for its state and provider (`errOAuthSessionNotPending`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NotPending;

/// The sessions, by state (upstream's `oauthSessionStore`). Expired
/// sessions are dropped as the store is next used. Its `Debug` shows no
/// state.
pub(crate) struct Store {
    inner: Mutex<Inner>,
}

struct Inner {
    ttl: Duration,
    completed_ttl: Duration,
    sessions: HashMap<String, Entry>,
}

struct Entry {
    session: Session,
    /// Where the session's callback goes, until one has gone or the session
    /// stops being pending. Dropping it wakes the waiting login.
    callback: Option<oneshot::Sender<Callback>>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new(SESSION_TTL)
    }
}

impl Store {
    /// A store whose sessions last `ttl` (30 minutes for zero), and whose
    /// completed sessions are kept a minute, or `ttl` if shorter
    /// (`newOAuthSessionStore`).
    pub(crate) fn new(ttl: Duration) -> Self {
        let ttl = if ttl.is_zero() { SESSION_TTL } else { ttl };
        Self {
            inner: Mutex::new(Inner {
                ttl,
                completed_ttl: COMPLETED_TTL.min(ttl),
                sessions: HashMap::new(),
            }),
        }
    }

    /// Keeps completed sessions for `ttl` from now on.
    #[cfg(test)]
    pub(crate) fn set_completed_ttl(&self, ttl: Duration) {
        self.lock().completed_ttl = ttl;
    }

    /// How many sessions are kept.
    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        let mut inner = self.lock();
        inner.purge(Instant::now());
        inner.sessions.len()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a pending session for `state` and `provider`, after trimming,
    /// in place of any session with that state, and returns where its
    /// callback will arrive (`Register`). `None`, registering nothing, when
    /// either is empty, or [`MAX_SESSIONS`] are already kept.
    pub(crate) fn register(
        &self,
        state: &str,
        provider: &str,
    ) -> Option<oneshot::Receiver<Callback>> {
        let state = state.trim();
        let provider = to_lower(provider.trim());
        if state.is_empty() || provider.is_empty() {
            return None;
        }
        let now = Instant::now();
        let mut inner = self.lock();
        inner.purge(now);
        if inner.sessions.len() >= MAX_SESSIONS && !inner.sessions.contains_key(state) {
            return None;
        }
        let (sender, receiver) = oneshot::channel();
        let session = Session {
            provider,
            status: String::new(),
            completed: false,
            expires_at: later(now, inner.ttl),
        };
        inner.sessions.insert(
            state.to_owned(),
            Entry {
                session,
                callback: Some(sender),
            },
        );
        Some(receiver)
    }

    /// Marks the login failed with `message`, or `Authentication failed`
    /// when it is empty, unless the session is unknown or completed
    /// (`SetError`). The session then lasts the full time again.
    pub(crate) fn set_error(&self, state: &str, message: &str) {
        let state = state.trim();
        let message = match message.trim() {
            "" => "Authentication failed",
            message => message,
        };
        if state.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut inner = self.lock();
        inner.purge(now);
        let ttl = inner.ttl;
        let Some(entry) = inner.sessions.get_mut(state) else {
            return;
        };
        if entry.session.completed {
            return;
        }
        message.clone_into(&mut entry.session.status);
        entry.session.expires_at = later(now, ttl);
        entry.callback = None;
    }

    /// Marks the login completed, unless the session is unknown or already
    /// completed (`Complete`). It is then kept for the completed time.
    pub(crate) fn complete(&self, state: &str) {
        let state = state.trim();
        if state.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut inner = self.lock();
        inner.purge(now);
        let completed_ttl = inner.completed_ttl;
        let Some(entry) = inner.sessions.get_mut(state) else {
            return;
        };
        if entry.session.completed {
            return;
        }
        entry.session.status.clear();
        entry.session.completed = true;
        entry.session.expires_at = later(now, completed_ttl);
        entry.callback = None;
    }

    /// The session for `state`, completed or not (`Get`, and
    /// `GetOAuthSessionDetails`).
    pub(crate) fn get(&self, state: &str) -> Option<Session> {
        let state = state.trim();
        let mut inner = self.lock();
        inner.purge(Instant::now());
        inner.sessions.get(state).map(|entry| entry.session.clone())
    }

    /// The session for `state` unless it completed (`GetOAuthSession`).
    pub(crate) fn active(&self, state: &str) -> Option<Session> {
        self.get(state).filter(|session| !session.completed)
    }

    /// Whether the session for `state` is pending, for `provider` unless it
    /// is empty, in any case (`IsPending`, and
    /// `guardOAuthSessionPendingForSave`).
    pub(crate) fn is_pending(&self, state: &str, provider: &str) -> bool {
        let state = state.trim();
        let mut inner = self.lock();
        inner.purge(Instant::now());
        inner
            .sessions
            .get(state)
            .is_some_and(|entry| pending_for(&entry.session, provider))
    }

    /// Drops the session for `state` if it is pending, which stops its
    /// login without saving anything, and returns whether it was
    /// (`Cancel`).
    pub(crate) fn cancel(&self, state: &str) -> bool {
        let state = state.trim();
        if state.is_empty() {
            return false;
        }
        let mut inner = self.lock();
        inner.purge(Instant::now());
        let pending = inner
            .sessions
            .get(state)
            .is_some_and(|entry| entry.session.is_pending());
        if pending {
            inner.sessions.remove(state);
        }
        pending
    }

    /// Hands `callback` to the login waiting on the pending session for
    /// `state` and `provider` (upstream's
    /// `WriteOAuthCallbackFileForPendingSession`). A session takes only its
    /// first callback: a later one is dropped.
    pub(crate) fn deliver(
        &self,
        state: &str,
        provider: &str,
        callback: Callback,
    ) -> Result<(), NotPending> {
        let state = state.trim();
        let mut inner = self.lock();
        inner.purge(Instant::now());
        let entry = inner.sessions.get_mut(state).ok_or(NotPending)?;
        if !pending_for(&entry.session, provider) {
            return Err(NotPending);
        }
        if let Some(sender) = entry.callback.take() {
            // A login that stopped waiting has already ended the session or
            // left it to expire.
            let _ = sender.send(callback);
        }
        Ok(())
    }
}

impl Inner {
    /// Drops the sessions that have expired (`purgeExpiredLocked`).
    fn purge(&mut self, now: Instant) {
        self.sessions
            .retain(|_, entry| now <= entry.session.expires_at);
    }
}

/// Whether `session` is pending, for `provider` unless it is empty after
/// trimming.
fn pending_for(session: &Session, provider: &str) -> bool {
    let provider = provider.trim();
    session.is_pending() && (provider.is_empty() || equal_fold(&session.provider, provider))
}

/// `by` after `now`, or `now` if that can't be told.
fn later(now: Instant, by: Duration) -> Instant {
    now.checked_add(by).unwrap_or(now)
}

/// `message`, or `Authentication failed` when it is empty, with `cause`
/// after a colon unless it is empty, each trimmed
/// (`oauthSessionErrorWithCause`).
pub(crate) fn error_with_cause(message: &str, cause: &str) -> String {
    let message = match message.trim() {
        "" => "Authentication failed",
        message => message,
    };
    match cause.trim() {
        "" => message.to_owned(),
        cause => format!("{message}: {cause}"),
    }
}

/// Whether `state`, trimmed, can be a session's state: 1 to 128 ASCII
/// letters, digits, `-`, `_` and `.`, without `..` (`ValidateOAuthState`).
/// The callback routes, which take no key, only look up states that pass.
pub(crate) fn is_valid_state(state: &str) -> bool {
    let state = state.trim();
    !state.is_empty()
        && state.len() <= MAX_STATE_LENGTH
        && !state.contains("..")
        && state
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A login provider's canonical name, from its name or an alias
/// (`NormalizeOAuthProvider`). Only `anthropic` and `codex` logins are
/// served; the others are named as upstream names them, so a callback
/// naming one is told it doesn't match its state.
fn normalize_provider(provider: &str) -> Option<&'static str> {
    match to_lower(provider.trim()).as_str() {
        "anthropic" | "claude" => Some("anthropic"),
        "codex" | "openai" => Some("codex"),
        "antigravity" | "anti-gravity" => Some("antigravity"),
        "xai" | "x-ai" | "x.ai" | "grok" => Some("xai"),
        "devin" | "cognition" => Some("devin"),
        "meta" | "muse" => Some("meta"),
        _ => None,
    }
}

/// The provider a callback names, canonical: a login provider's name, else
/// a plugin provider's name (lower case letters, digits and `-`), else
/// `None` (`NormalizeOAuthCallbackProvider`).
pub(crate) fn normalize_callback_provider(provider: &str) -> Option<String> {
    if let Some(provider) = normalize_provider(provider) {
        return Some(provider.to_owned());
    }
    let provider = to_lower(provider.trim());
    let plugin = !provider.is_empty()
        && provider
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    plugin.then_some(provider)
}
