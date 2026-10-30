// Ported from CLIProxyAPI internal/api/handlers/management/oauth_sessions.go
// (oauthSessionStore, newOAuthSessionStore, purgeExpiredLocked, Register,
// SetError, Complete, Get, IsPending, Cancel, GetOAuthSession,
// GetOAuthSessionDetails, guardOAuthSessionPendingForSave,
// oauthSessionErrorWithCause, ValidateOAuthState, NormalizeOAuthProvider,
// NormalizeOAuthCallbackProvider, NormalizePluginOAuthCallbackProvider,
// WriteOAuthCallbackFileForPendingSession) (v8.0.42, MIT).
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
//! A login learns that its session was dropped (cancelled, replaced or
//! expired) through the session's [`Lifeline`], whatever it is doing; the
//! callback channel tells it only while it waits for the callback. A
//! callback page that handed a session its callback can wait for the login
//! to end with [`Store::ended`].
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
use tokio::sync::{oneshot, watch};

use super::Redacted;
use super::mutation_path::BindMutationPath;
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
    pub(crate) client_id: String,
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

/// What a login gets for its session from [`Store::register`].
pub(crate) struct Registration {
    /// Where its callback arrives. It closes, without one, when the
    /// session stops being pending before a callback came.
    pub(crate) callback: oneshot::Receiver<Callback>,
    /// What tells the login its session was dropped.
    pub(crate) lifeline: Lifeline,
    /// Which registration of the state this is, for [`Store::release`].
    pub(crate) id: u64,
}

/// What tells a login its session was dropped: cancelled, replaced by a
/// session with the same state, or expired. Unlike the callback channel,
/// it lasts the session's whole life.
pub(crate) struct Lifeline(watch::Receiver<()>);

impl Lifeline {
    /// Waits until the session is dropped; at once if it already was.
    pub(crate) async fn cut(&mut self) {
        // The session's channel also says when the login completes or
        // fails, for the callback pages: only its end stops this.
        while self.0.changed().await.is_ok() {}
    }
}

/// A callback that [`Store::deliver`] handed to a pending session.
#[derive(Debug)]
pub(crate) struct Delivered {
    /// Whether the login took this callback: false when it took an earlier
    /// one, or had stopped waiting.
    pub(crate) taken: bool,
    /// The registration whose session it went to.
    id: u64,
    /// Wakes when the login completes or fails, or the session is dropped.
    changes: watch::Receiver<()>,
}

/// How the login of a session ended, as [`Store::ended`] tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// It completed: the credential is saved.
    Completed,
    /// It failed, with this status.
    Failed(String),
    /// Its session was dropped first: cancelled, replaced or expired, or
    /// the login stopped.
    Dropped,
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
    /// The ID of the next registration.
    next_id: u64,
}

struct Entry {
    session: Session,
    bind: bool,
    bind_target: Option<BindMutationPath>,
    bind_redirect: String,
    committing: bool,
    proxy_url: String,
    /// Where the session's callback goes, until one has gone or the session
    /// stops being pending. Dropping it wakes the waiting login.
    callback: Option<oneshot::Sender<Callback>>,
    /// Cuts the login's [`Lifeline`] when the session is dropped, and wakes
    /// the callback pages waiting in [`Store::ended`] when the login
    /// completes or fails too.
    changes: watch::Sender<()>,
    /// The registration's ID.
    id: u64,
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
                next_id: 0,
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
    /// in place of any session with that state, and returns what its login
    /// needs (`Register`). `None`, registering nothing, when either is
    /// empty, or [`MAX_SESSIONS`] are already kept.
    pub(crate) fn register(&self, state: &str, provider: &str) -> Option<Registration> {
        self.register_with_proxy(state, provider, "")
    }

    pub(crate) fn register_with_proxy(
        &self,
        state: &str,
        provider: &str,
        proxy_url: &str,
    ) -> Option<Registration> {
        let state = state.trim();
        let provider = to_lower(provider.trim());
        if state.is_empty() || provider.is_empty() {
            return None;
        }
        let now = Instant::now();
        let mut inner = self.lock();
        inner.purge(now);
        if inner
            .sessions
            .get(state)
            .is_some_and(|entry| entry.committing)
            || (inner.sessions.len() >= MAX_SESSIONS && !inner.sessions.contains_key(state))
        {
            return None;
        }
        let (sender, callback) = oneshot::channel();
        let (changes, login_end) = watch::channel(());
        let id = inner.next_id;
        inner.next_id = id.wrapping_add(1);
        let session = Session {
            provider,
            status: String::new(),
            completed: false,
            expires_at: later(now, inner.ttl),
        };
        inner.sessions.insert(
            state.to_owned(),
            Entry {
                bind: false,
                bind_target: None,
                bind_redirect: String::new(),
                committing: false,
                proxy_url: proxy_url.trim().to_owned(),
                session,
                callback: Some(sender),
                changes,
                id,
            },
        );
        Some(Registration {
            callback,
            lifeline: Lifeline(login_end),
            id,
        })
    }

    /// Marks the login failed with `message`, or `Authentication failed`
    /// when it is empty, unless the session is unknown or completed
    /// (`SetError`). The session then lasts the full time again.
    pub(crate) fn set_error(&self, state: &str, message: &str) {
        self.fail(state, message, false, false);
    }

    /// The owner of a claimed save may fail it; polls and callbacks may not.
    pub(super) fn fail_commit(&self, state: &str, message: &str) {
        self.fail(state, message, false, true);
    }

    /// Fails a pending session; returns whether its status changed.
    pub(super) fn set_pending_error(&self, state: &str, message: &str) -> bool {
        self.fail(state, message, true, false)
    }

    fn fail(&self, state: &str, message: &str, only_pending: bool, allow_committing: bool) -> bool {
        let state = state.trim();
        let message = match message.trim() {
            "" => "Authentication failed",
            message => message,
        };
        if state.is_empty() {
            return false;
        }
        let now = Instant::now();
        let mut inner = self.lock();
        inner.purge(now);
        let ttl = inner.ttl;
        let Some(entry) = inner.sessions.get_mut(state) else {
            return false;
        };
        if entry.session.completed
            || (entry.committing && !allow_committing)
            || (only_pending && !entry.session.is_pending())
        {
            return false;
        }
        entry.session.status = redact_session_proxy(message, &entry.proxy_url);
        entry.proxy_url.clear();
        entry.session.expires_at = later(now, ttl);
        entry.callback = None;
        entry.committing = false;
        entry.changes.send_replace(());
        true
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
        entry.proxy_url.clear();
        entry.session.completed = true;
        entry.session.expires_at = later(now, completed_ttl);
        entry.callback = None;
        entry.committing = false;
        entry.changes.send_replace(());
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

    #[cfg(test)]
    pub(super) fn mark_bind(&self, state: &str) {
        if let Some(entry) = self.lock().sessions.get_mut(state) {
            entry.bind = true;
        }
    }

    pub(super) fn mark_bind_target(
        &self,
        state: &str,
        target: BindMutationPath,
        redirect: &str,
    ) -> bool {
        let mut inner = self.lock();
        let now = Instant::now();
        inner.purge(now);
        if inner.sessions.iter().any(|(key, entry)| {
            key != state
                && entry
                    .bind_target
                    .as_ref()
                    .is_some_and(|bound| bound.matches(&target))
                && entry.session.is_pending()
        }) {
            return false;
        }
        let Some(entry) = inner.sessions.get_mut(state) else {
            return false;
        };
        entry.bind = true;
        entry.bind_target = Some(target);
        entry.bind_redirect = redirect.to_owned();
        entry.session.expires_at = later(now, Duration::from_secs(300));
        true
    }

    pub(super) fn bind_redirect(&self, state: &str) -> Option<String> {
        self.lock()
            .sessions
            .get(state)
            .filter(|entry| entry.bind)
            .map(|entry| entry.bind_redirect.clone())
    }

    pub(crate) fn invalidate_bind_target(&self, target: &BindMutationPath) {
        let mut inner = self.lock();
        let expiry = later(Instant::now(), inner.completed_ttl);
        for entry in inner.sessions.values_mut().filter(|e| {
            e.bind_target
                .as_ref()
                .is_some_and(|bound| bound.current().matches(target))
                && e.session.is_pending()
                && !e.committing
        }) {
            entry.session.status = "SIWC binding target changed; start a new binding".to_owned();
            entry.session.expires_at = expiry;
            entry.callback = None;
            entry.changes.send_replace(());
        }
    }

    pub(super) fn is_bind(&self, state: &str) -> bool {
        self.lock()
            .sessions
            .get(state)
            .is_some_and(|entry| entry.bind)
    }

    /// Checks the selected registration immediately before a bind commits.
    pub(super) fn admit_bind(&self, state: &str, id: u64) -> bool {
        let mut inner = self.lock();
        inner.purge(Instant::now());
        let Some(entry) = inner.sessions.get_mut(state) else {
            return false;
        };
        if entry.id != id
            || !entry.bind
            || entry.committing
            || !pending_for(&entry.session, "codex")
        {
            return false;
        }
        entry.committing = true;
        true
    }

    /// Claims an ordinary login's save under the same lock as cancellation.
    /// The registration fence prevents a replaced login claiming its successor.
    pub(super) fn claim_commit(&self, state: &str, provider: &str, id: u64) -> bool {
        let mut inner = self.lock();
        inner.purge(Instant::now());
        let Some(entry) = inner.sessions.get_mut(state) else {
            return false;
        };
        if entry.id != id
            || entry.bind
            || entry.committing
            || !pending_for(&entry.session, provider)
        {
            return false;
        }
        entry.committing = true;
        true
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

    pub(crate) fn is_committing(&self, state: &str) -> bool {
        self.lock()
            .sessions
            .get(state)
            .is_some_and(|entry| entry.committing)
    }

    #[cfg(test)]
    pub(crate) fn expire_bind_for_test(&self, state: &str) {
        let mut inner = self.lock();
        let entry = inner.sessions.get_mut(state).expect("a bind session");
        assert!(entry.bind && !entry.committing);
        entry.session.expires_at = Instant::now();
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
            .is_some_and(|entry| entry.session.is_pending() && !entry.committing);
        if pending {
            inner.sessions.remove(state);
        }
        pending
    }

    /// Drops the unclaimed pending session of registration `id` when its
    /// login ends. A claimed save retains its session until its gate-owned
    /// finalizer records the durable outcome.
    pub(crate) fn release(&self, state: &str, id: u64) {
        let mut inner = self.lock();
        let ours = inner
            .sessions
            .get(state)
            .is_some_and(|entry| entry.id == id && entry.session.is_pending() && !entry.committing);
        if ours {
            inner.sessions.remove(state);
        }
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
    ) -> Result<Delivered, NotPending> {
        let state = state.trim();
        let mut inner = self.lock();
        inner.purge(Instant::now());
        let entry = inner.sessions.get_mut(state).ok_or(NotPending)?;
        if !pending_for(&entry.session, provider) || (entry.bind && entry.callback.is_none()) {
            return Err(NotPending);
        }
        // A login that stopped waiting has already ended the session or
        // left it to expire.
        let taken = entry
            .callback
            .take()
            .is_some_and(|sender| sender.send(callback).is_ok());
        Ok(Delivered {
            taken,
            id: entry.id,
            changes: entry.changes.subscribe(),
        })
    }

    /// Waits up to `limit` for the login whose session `delivered` went to,
    /// under `state`, to end, and tells how it did; `None` if it hadn't
    /// ended by then.
    pub(crate) async fn ended(
        &self,
        state: &str,
        mut delivered: Delivered,
        limit: Duration,
    ) -> Option<Ended> {
        let state = state.trim();
        let ended = async {
            loop {
                // The receiver was made under the lock, as the session was
                // then: any change since has marked it changed.
                if let Some(ended) = self.outcome(state, delivered.id) {
                    return ended;
                }
                if delivered.changes.changed().await.is_err() {
                    return self.outcome(state, delivered.id).unwrap_or(Ended::Dropped);
                }
            }
        };
        tokio::time::timeout(limit, ended).await.ok()
    }

    /// How the login of registration `id` for `state` ended, or `None`
    /// while its session is pending.
    fn outcome(&self, state: &str, id: u64) -> Option<Ended> {
        let mut inner = self.lock();
        inner.purge(Instant::now());
        let Some(entry) = inner.sessions.get(state).filter(|entry| entry.id == id) else {
            return Some(Ended::Dropped);
        };
        let session = &entry.session;
        if session.completed {
            Some(Ended::Completed)
        } else if !session.status.is_empty() {
            Some(Ended::Failed(session.status.clone()))
        } else {
            None
        }
    }
}

impl Inner {
    /// Drops the sessions that have expired (`purgeExpiredLocked`).
    fn purge(&mut self, now: Instant) {
        self.sessions
            .retain(|_, entry| entry.committing || now <= entry.session.expires_at);
    }
}

/// Provider and transport errors may quote a proxy or just its credentials.
/// Hide the encoded and decoded forms before retaining a terminal status.
fn redact_session_proxy(text: &str, proxy_url: &str) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    if proxy_url.is_empty() {
        return text.to_owned();
    }
    let Some(url) = crate::go_url::parse(proxy_url.as_bytes()) else {
        return text.to_owned();
    };
    let Some((user, password)) = url.user else {
        return text.to_owned();
    };
    let sanitized = format!(
        "{}://[redacted]@{}",
        url.scheme,
        String::from_utf8_lossy(&url.host)
    );
    let mut secrets = Vec::new();
    if let Some((_, authority)) = proxy_url.split_once("://")
        && let Some((userinfo, _)) = authority.split('/').next().unwrap_or("").rsplit_once('@')
        && let Some((_, password)) = userinfo.split_once(':')
    {
        secrets.push(userinfo.to_owned());
        secrets.push(password.to_owned());
    }
    if let Some(password) = password {
        let user = String::from_utf8_lossy(&user);
        let password = String::from_utf8_lossy(&password).into_owned();
        secrets.push(STANDARD.encode(format!("{user}:{password}")));
        secrets.push(format!("{user}:{password}"));
        secrets.push(password);
    }
    secrets.retain(|secret| !secret.is_empty());
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    // Match only the original text, longest first at each position. In
    // particular a password must never match inside an inserted marker or
    // the sanitized URL, whose host remains useful for diagnosing failures.
    let mut redacted = String::with_capacity(text.len());
    let mut from = 0;
    for (index, _) in text.char_indices() {
        if index < from {
            continue;
        }
        let rest = &text[index..];
        let (length, replacement) = if rest.starts_with(proxy_url) {
            (proxy_url.len(), sanitized.as_str())
        } else if let Some(secret) = secrets
            .iter()
            .find(|secret| rest.starts_with(secret.as_str()))
        {
            (secret.len(), "[redacted]")
        } else {
            continue;
        };
        redacted.push_str(&text[from..index]);
        redacted.push_str(replacement);
        from = index + length;
    }
    redacted.push_str(&text[from..]);
    redacted
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

#[cfg(test)]
mod bind_tests {
    use super::*;

    #[test]
    fn session_proxy_commit_claim_is_registration_and_provider_fenced() {
        let store = Store::default();
        let old = store.register("session-test", "codex").unwrap();
        let current = store.register("session-test", "codex").unwrap();
        assert!(!store.claim_commit("session-test", "codex", old.id));
        assert!(!store.claim_commit("session-test", "anthropic", current.id));
        assert!(store.claim_commit("session-test", "CODEX", current.id));
        assert!(!store.claim_commit("session-test", "codex", current.id));
        assert!(!store.cancel("session-test"));
        assert!(store.register("session-test", "codex").is_none());
        store.set_error("session-test", "late error");
        assert!(!store.set_pending_error("session-test", "late poll"));
        assert!(store.get("session-test").unwrap().status.is_empty());
        store.fail_commit("session-test", "save failed");
        assert!(store.get("session-test").unwrap().status == "save failed");
        assert!(!store.is_committing("session-test"));
        let cancelled = store.register("cancel-test", "codex").unwrap();
        assert!(store.cancel("cancel-test"));
        assert!(!store.claim_commit("cancel-test", "codex", cancelled.id));
        let bound = store.register("bind-test", "codex").unwrap();
        store.mark_bind("bind-test");
        assert!(!store.claim_commit("bind-test", "codex", bound.id));
        assert!(store.admit_bind("bind-test", bound.id));
    }

    #[test]
    fn session_proxy_release_retains_claimed_saves_until_their_real_outcome() {
        for failed in [false, true] {
            let store = Store::default();
            let registration = store.register("save-test", "codex").unwrap();
            assert!(store.claim_commit("save-test", "codex", registration.id));
            store.release("save-test", registration.id);
            assert!(store.is_committing("save-test"));
            assert!(store.get("save-test").unwrap().is_pending());
            if failed {
                store.fail_commit("save-test", "save failed");
            } else {
                store.complete("save-test");
            }
            store.release("save-test", registration.id);
            let session = store.get("save-test").unwrap();
            assert_eq!(session.completed, !failed);
            assert_eq!(session.status, if failed { "save failed" } else { "" });
            assert!(!store.is_committing("save-test"));
        }
        let store = Store::default();
        let registration = store.register("unsaved-test", "codex").unwrap();
        store.release("unsaved-test", registration.id);
        assert!(store.get("unsaved-test").is_none());
    }

    #[test]
    fn session_proxy_native_commit_claim_is_fenced_by_shutdown() {
        let sessions = super::super::Sessions::default();
        let before = sessions.store.register("before-shutdown", "codex").unwrap();
        let during = sessions.store.register("during-grace", "codex").unwrap();
        let after = sessions.store.register("after-shutdown", "codex").unwrap();
        assert!(sessions.claim_login_commit("before-shutdown", "codex", before.id));
        sessions.logins().shut_down = true;
        assert!(sessions.claim_login_commit("during-grace", "codex", during.id));
        sessions.logins().aborting = true;
        assert!(!sessions.claim_login_commit("after-shutdown", "codex", after.id));
        assert!(!sessions.store.is_committing("after-shutdown"));
        sessions.store.release("after-shutdown", after.id);
        assert!(sessions.store.get("after-shutdown").is_none());
        assert!(sessions.store.is_committing("before-shutdown"));
        sessions.store.complete("before-shutdown");
        assert!(sessions.store.get("before-shutdown").unwrap().completed);
    }

    #[test]
    fn session_proxy_terminal_state_redacts_and_releases_transient_proxy() {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD;
        let user = "proxy user-test";
        let password = "proxy@password-test";
        let proxy = "http://proxy%20user-test:proxy%40password-test@127.0.0.1:12345";
        let basic = STANDARD.encode(format!("{user}:{password}"));
        for terminal in ["error", "poll", "save", "complete"] {
            let store = Store::default();
            let registration = store
                .register_with_proxy("state-test", "codex", proxy)
                .unwrap();
            assert!(store.lock().sessions["state-test"].proxy_url == proxy);
            let message = format!(
                "failed {proxy} {user} {password} {basic} proxy%20user-test proxy%40password-test"
            );
            match terminal {
                "error" => store.set_error("state-test", &message),
                "poll" => assert!(store.set_pending_error("state-test", &message)),
                "save" => {
                    assert!(store.claim_commit("state-test", "codex", registration.id));
                    store.fail_commit("state-test", &message);
                }
                _ => {
                    assert!(store.claim_commit("state-test", "codex", registration.id));
                    store.complete("state-test");
                }
            }
            let session = store.get("state-test").unwrap();
            for secret in [proxy, password, &basic, "proxy%40password-test"] {
                assert!(!session.status.contains(secret));
                assert!(!format!("{session:?} {store:?}").contains(secret));
            }
            if terminal != "complete" {
                assert!(session.status.contains("http://[redacted]@127.0.0.1:12345"));
                assert!(session.status.contains(user));
                assert!(session.status.contains("proxy%20user-test"));
            }
            assert!(store.lock().sessions["state-test"].proxy_url.is_empty());
            assert!(!store.is_committing("state-test"));
            assert!(!store.set_pending_error("state-test", "late poll"));
        }
    }

    #[test]
    fn session_proxy_redaction_is_single_pass_for_short_and_overlapping_credentials() {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD;
        for (proxy, user, password, host) in [
            ("http://u:d@bridge.test:8080", "u", "d", "bridge.test:8080"),
            (
                "http://redacted:acted@redacted.test:8080",
                "redacted",
                "acted",
                "redacted.test:8080",
            ),
        ] {
            let basic = STANDARD.encode(format!("{user}:{password}"));
            let text =
                format!("unable to connect {proxy} {user}:{password} {password} Basic {basic}");
            let expected = format!(
                "unable to connect http://[redacted]@{host} [redacted] [redacted] Basic [redacted]"
            );
            assert!(redact_session_proxy(&text, proxy) == expected);
        }
        assert!(
            redact_session_proxy("unable to connect u", "http://u:d@bridge.test:8080")
                == "unable to connect u"
        );
        assert!(
            redact_session_proxy("http://user@proxy.test user", "http://user@proxy.test")
                == "http://[redacted]@proxy.test user"
        );
        assert!(
            redact_session_proxy("http://proxy.test", "http://proxy.test") == "http://proxy.test"
        );
    }

    #[test]
    fn session_proxy_redacts_encoded_and_decoded_userinfo_and_passwords() {
        let proxy = "http://proxy%20user-test:proxy%40password-test@127.0.0.1:12345";
        let text = "failed proxy%20user-test:proxy%40password-test proxy user-test:proxy@password-test proxy%40password-test proxy@password-test";
        assert!(
            redact_session_proxy(text, proxy)
                == "failed [redacted] [redacted] [redacted] [redacted]"
        );
    }

    #[test]
    fn session_proxy_claimed_sessions_survive_expiry_until_finalization() {
        for failed in [false, true] {
            let store = Store::default();
            let registration = store
                .register_with_proxy("state-test", "codex", "http://127.0.0.1:12345")
                .unwrap();
            assert!(store.claim_commit("state-test", "codex", registration.id));
            store
                .lock()
                .sessions
                .get_mut("state-test")
                .unwrap()
                .session
                .expires_at = Instant::now() - Duration::from_secs(1);
            assert!(store.get("state-test").is_some());
            assert!(!store.cancel("state-test"));
            if failed {
                store.fail_commit("state-test", "save failed");
            } else {
                store.complete("state-test");
            }
            assert!(store.lock().sessions["state-test"].proxy_url.is_empty());
            store
                .lock()
                .sessions
                .get_mut("state-test")
                .unwrap()
                .session
                .expires_at = Instant::now() - Duration::from_secs(1);
            assert!(store.get("state-test").is_none());
        }
    }

    #[test]
    fn admitted_sessions_expire_after_completion_or_failure() {
        for failed in [false, true] {
            let store = Store::new(Duration::from_nanos(1));
            // Give admission a real window, then make its pending expiry past.
            store.lock().ttl = Duration::from_secs(60);
            let registration = store.register("bind-expiry-test", "codex").unwrap();
            store.mark_bind("bind-expiry-test");
            assert!(store.admit_bind("bind-expiry-test", registration.id));
            assert!(!store.cancel("bind-expiry-test"));
            store
                .lock()
                .sessions
                .get_mut("bind-expiry-test")
                .unwrap()
                .session
                .expires_at = Instant::now() - Duration::from_secs(1);
            assert_eq!(store.count(), 1);
            if failed {
                store.fail_commit("bind-expiry-test", "test failure");
            } else {
                store.complete("bind-expiry-test");
            }
            store
                .lock()
                .sessions
                .get_mut("bind-expiry-test")
                .unwrap()
                .session
                .expires_at = Instant::now() - Duration::from_secs(1);
            assert_eq!(store.count(), 0);
        }
    }
}
