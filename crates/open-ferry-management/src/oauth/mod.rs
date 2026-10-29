// Ported from CLIProxyAPI internal/api/server_management.go and
// internal/api/server_management_v8.go (the OAuth login routes) and
// internal/api/server_routes.go (the /anthropic/callback and
// /codex/callback routes) (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OAuth logins started from the management API, for Claude and Codex
//! accounts, with the official OAuth flows only.
//!
//! | v0 (`/v0/management/...`) | v8 (`/v8/management/...`) | Access |
//! |---|---|---|
//! | `GET anthropic-auth-url` | `GET oauth/auth-url?provider=claude` | key |
//! | `GET codex-auth-url` | `GET oauth/auth-url?provider=codex` | key |
//! | `GET get-auth-status` | `GET oauth/status` | key |
//! | `DELETE oauth-session` | `DELETE oauth/session` | key |
//! | `GET`, `POST oauth-callback` | `GET`, `POST oauth/callback` | a key set |
//!
//! The main server also serves `GET /anthropic/callback` and
//! `GET /codex/callback`, which need nothing: the page a callback
//! forwarder sends the browser to, which says how the login ended.
//!
//! The `flows` module starts and follows a login, the `callback` module
//! takes its callback, the `sessions` module keeps the sessions and the
//! `forwarder` module runs the callback forwarders.
//!
//! Deviations from upstream, besides those noted on each module:
//! - Only Claude and Codex logins are served. The logins of other
//!   providers (Antigravity, Kimi, xAI, Devin, Meta, plugins) aren't, and
//!   neither are their callback pages (`/antigravity/callback`,
//!   `/devin/callback`, `/callback`).

mod bind_workers;
mod callback;
mod flows;
mod forwarder;
pub(crate) mod mutation_path;
pub(crate) mod sessions;
pub(crate) mod siwc_bind;
mod siwc_status;

use std::fmt;
use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::routing::{delete, get};
use open_ferry_providers::claude::oauth as claude;
use open_ferry_providers::codex::oauth as codex;
use tokio::task::JoinSet;

use crate::Route;
use forwarder::Forwarders;
use sessions::Store;

#[cfg(feature = "test-support")]
pub mod test_support;

/// A provider whose logins are served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Claude,
    Codex,
}

impl Provider {
    /// The provider's name in a session.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Claude => "anthropic",
            Self::Codex => "codex",
        }
    }

    /// The main server's callback page.
    fn page(self) -> &'static str {
        match self {
            Self::Claude => "/anthropic/callback",
            Self::Codex => "/codex/callback",
        }
    }

    /// The port of the provider's redirect URI, where its callback
    /// forwarder listens (`anthropicCallbackPort`, `codexCallbackPort`).
    pub(crate) fn callback_port(self) -> u16 {
        match self {
            Self::Claude => claude::DEFAULT_CALLBACK_PORT,
            Self::Codex => codex::DEFAULT_CALLBACK_PORT,
        }
    }

    /// The status of a login whose callback reported an error.
    fn callback_error(self) -> &'static str {
        match self {
            Self::Claude => "Bad request",
            Self::Codex => "Bad Request",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        })
    }
}

/// The OAuth logins in progress: their sessions (upstream's
/// `oauthSessionStore`), callback forwarders and tasks.
#[derive(Debug, Default)]
pub(crate) struct Sessions {
    store: Store,
    forwarders: Forwarders,
    logins: Mutex<Logins>,
    pub(super) bind_workers: std::sync::Arc<bind_workers::BindWorkers>,
    #[cfg(any(test, feature = "test-support"))]
    overrides: Mutex<Overrides>,
}

/// The logins' tasks.
#[derive(Debug, Default)]
struct Logins {
    /// The tasks, some perhaps ended.
    tasks: JoinSet<()>,
    /// Whether the logins were shut down: no login starts after that.
    shut_down: bool,
}

/// What a test changes: where the providers' endpoints are, the ports the
/// forwarders take, and how long a login waits.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub(crate) struct Overrides {
    /// The base URL of both providers' endpoints: a closed local port
    /// unless a test sets its own, so that a test never reaches a real
    /// provider.
    pub(crate) endpoints: Option<String>,
    /// The forwarders' ports for Claude and Codex logins: 0, any free
    /// port, unless a test sets others.
    pub(crate) ports: Option<(u16, u16)>,
    /// How long a login waits for its callback.
    pub(crate) wait: Option<Duration>,
    /// How long a login's code exchange may take.
    pub(crate) exchange: Option<Duration>,
    /// How long a callback page waits for its login to end.
    pub(crate) page: Option<Duration>,
    /// Notified only after acquiring the bind's credential lock polls Pending.
    #[cfg(test)]
    pub(crate) bind_lock_wait: Option<std::sync::Arc<tokio::sync::Notify>>,
    /// Notified after a busy save releases both locks and reserves a writer.
    #[cfg(test)]
    pub(crate) bind_admission_wait: Option<std::sync::Arc<tokio::sync::Notify>>,
    /// Holds a persisted bind after revision allocation, before releasing its lock.
    #[cfg(test)]
    pub(crate) bind_after_sync: Option<std::sync::Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    /// Notified after worker RAII has released its reservation and FIFO turn.
    #[cfg(test)]
    pub(crate) bind_worker_finished: Option<std::sync::Arc<tokio::sync::Notify>>,
    /// Enlarged only by signal tests, separating cancellation from the deadline.
    #[cfg(test)]
    pub(crate) bind_drain_budget: Option<Duration>,
    /// Records actual reservation starts for retry-spacing regressions.
    #[cfg(test)]
    pub(crate) bind_admission_times: Option<BindAdmissionTimes>,
    /// Pause after the last session fence, before writing.
    pub(crate) bind_before_write: Option<BindWritePause>,
    /// Pause a live worker in filesystem work before the final session fence.
    pub(crate) bind_before_file: Option<BindWritePause>,
    #[cfg(test)]
    pub(crate) bind_worker_starts: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    pub(crate) bind_shutdown_grace: Option<Duration>,
    pub(crate) bind_before_complete:
        Option<std::sync::Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    #[cfg(test)]
    pub(crate) bind_after_file_miss: Option<BindWritePause>,
    #[cfg(test)]
    pub(crate) bind_turn_started: Option<std::sync::Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    pub(crate) bind_file_misses: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    /// Restricts the file-miss counter and pause to one credential.
    #[cfg(test)]
    pub(crate) bind_file_miss_target: Option<std::path::PathBuf>,
    #[cfg(test)]
    pub(crate) bind_worker_error: Option<std::io::ErrorKind>,
    #[cfg(test)]
    pub(crate) bind_cancel_observed: Option<std::sync::Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    pub(crate) bind_shutdown_events: Option<BindShutdownEvents>,
    #[cfg(test)]
    pub(crate) bind_retry_records: Option<BindRetryRecords>,
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) type BindWritePause = std::sync::Arc<(
    tokio::sync::Notify,
    std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
)>;

#[cfg(test)]
type BindShutdownEvents = std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>;
#[cfg(test)]
type BindRetryRecords = std::sync::Arc<(
    tokio::sync::Notify,
    std::sync::Mutex<Vec<(bool, u32, Duration)>>,
)>;

#[cfg(test)]
type BindAdmissionTimes = std::sync::Arc<std::sync::Mutex<Vec<std::time::Instant>>>;

#[cfg(any(test, feature = "test-support"))]
impl Default for Overrides {
    fn default() -> Self {
        Self {
            endpoints: Some("http://127.0.0.1:9".to_owned()),
            ports: Some((0, 0)),
            wait: None,
            exchange: None,
            page: None,
            #[cfg(test)]
            bind_lock_wait: None,
            #[cfg(test)]
            bind_admission_wait: None,
            #[cfg(test)]
            bind_after_sync: None,
            bind_before_write: None,
            bind_before_file: None,
            #[cfg(test)]
            bind_worker_starts: None,
            bind_shutdown_grace: None,
            bind_before_complete: None,
            #[cfg(test)]
            bind_after_file_miss: None,
            #[cfg(test)]
            bind_turn_started: None,
            #[cfg(test)]
            bind_file_misses: None,
            #[cfg(test)]
            bind_file_miss_target: None,
            #[cfg(test)]
            bind_worker_error: None,
            #[cfg(test)]
            bind_cancel_observed: None,
            #[cfg(test)]
            bind_shutdown_events: None,
            #[cfg(test)]
            bind_retry_records: None,
            #[cfg(test)]
            bind_drain_budget: None,
            #[cfg(test)]
            bind_admission_times: None,
            #[cfg(test)]
            bind_worker_finished: None,
        }
    }
}

impl Sessions {
    fn try_bind_worker(&self) -> Result<bind_workers::Worker, siwc_bind::SaveError> {
        let logins = self.logins();
        if logins.shut_down {
            return Err(siwc_bind::SaveError::ShuttingDown);
        }
        self.bind_workers.try_worker()
    }

    fn check_bind_worker(&self) -> Result<(), siwc_bind::SaveError> {
        let logins = self.logins();
        if logins.shut_down {
            Err(siwc_bind::SaveError::ShuttingDown)
        } else if !self.bind_workers.available() {
            Err(siwc_bind::SaveError::Busy)
        } else {
            Ok(())
        }
    }

    fn admit_bind(&self, session: &str, registration: u64) -> Result<(), siwc_bind::CancelCause> {
        use siwc_bind::CancelCause;
        let logins = self.logins();
        // Check session end first so its recorded cause wins over shutdown.
        if !self.store.is_pending(session, "codex") {
            Err(CancelCause::SessionEnded)
        } else if logins.shut_down {
            Err(CancelCause::Shutdown)
        } else if !self.store.admit_bind(session, registration) {
            Err(CancelCause::SessionEnded)
        } else {
            Ok(())
        }
    }

    /// Serialize collector registration with the shutdown cutoff. Its guard
    /// lasts through session finalization, not merely the worker's result.
    fn bind_completion(&self) -> Option<bind_workers::Completion> {
        let logins = self.logins();
        (!logins.shut_down).then(|| self.bind_workers.completion())
    }

    pub(super) fn is_shut_down(&self) -> bool {
        self.logins().shut_down
    }

    /// The sessions.
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    fn forwarders(&self) -> &Forwarders {
        &self.forwarders
    }

    fn logins(&self) -> MutexGuard<'_, Logins> {
        self.logins.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `login` as a task of its own, until it ends or the logins are
    /// shut down. Once they are, returns false and drops `login`.
    fn spawn(&self, login: impl Future<Output = ()> + Send + 'static) -> bool {
        let mut logins = self.logins();
        if logins.shut_down {
            return false;
        }
        // Forget the logins that have ended.
        while logins.tasks.try_join_next().is_some() {}
        // The wait for a callback holds nothing of the gate and writes
        // nothing; once a callback came, the login that saves a credential
        // begins an operation of its own (`flows`).
        #[allow(clippy::disallowed_methods)]
        logins.tasks.spawn(login);
        true
    }

    /// Refuses new logins and gives bind workers and session finalization ten
    /// seconds to settle before aborting login tasks. A post-fence collector
    /// survives that abort if the grace expires; process exit can still end it.
    pub(crate) async fn shutdown(&self) {
        let mut tasks = {
            let mut logins = self.logins();
            logins.shut_down = true;
            std::mem::take(&mut logins.tasks)
        };
        let grace = bind_workers::SHUTDOWN_GRACE;
        #[cfg(any(test, feature = "test-support"))]
        let grace = self.overrides().bind_shutdown_grace.unwrap_or(grace);
        if tokio::time::timeout(grace, self.bind_workers.idle())
            .await
            .is_err()
        {
            let (workers, completions) = self.bind_workers.counts();
            if workers != 0 {
                tracing::warn!(
                    workers,
                    completions,
                    "SIWC bind shutdown grace expired; workers may still be writing"
                );
            } else if completions != 0 {
                tracing::warn!(
                    workers,
                    completions,
                    "SIWC bind shutdown grace expired; sessions are still finalizing"
                );
            }
        }
        #[cfg(test)]
        if let Some(events) = self.overrides().bind_shutdown_events.clone() {
            events.lock().unwrap().push("shutdown passed idle");
        }
        tasks.shutdown().await;
    }

    /// How many logins are still running.
    #[cfg(test)]
    pub(crate) fn running(&self) -> usize {
        let mut logins = self.logins();
        while logins.tasks.try_join_next().is_some() {}
        logins.tasks.len()
    }

    /// What a test changes.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn overrides(&self) -> MutexGuard<'_, Overrides> {
        self.overrides
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the forwarder for `provider` listens, while it runs.
    #[cfg(test)]
    pub(crate) fn forwarder_addr(&self, provider: Provider) -> Option<std::net::SocketAddr> {
        self.forwarders.addr(self.callback_port(provider))
    }

    /// Anthropic's endpoints.
    fn claude_endpoints(&self) -> claude::Endpoints {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(base) = self.overrides().endpoints.clone() {
            return claude::Endpoints::with_base(&base);
        }
        claude::Endpoints::default()
    }

    /// OpenAI's endpoints.
    fn codex_endpoints(&self) -> codex::Endpoints {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(base) = self.overrides().endpoints.clone() {
            return codex::Endpoints::with_base(&base);
        }
        codex::Endpoints::default()
    }

    fn siwc_endpoints(&self) -> open_ferry_providers::codex::siwc::Endpoints {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(base) = self.overrides().endpoints.clone() {
            return open_ferry_providers::codex::siwc::Endpoints::with_base(&base);
        }
        open_ferry_providers::codex::siwc::Endpoints::default()
    }

    /// The port `provider`'s forwarder listens on.
    fn callback_port(&self, provider: Provider) -> u16 {
        #[cfg(any(test, feature = "test-support"))]
        if let Some((claude, codex)) = self.overrides().ports {
            return match provider {
                Provider::Claude => claude,
                Provider::Codex => codex,
            };
        }
        provider.callback_port()
    }

    /// How long a login waits for its callback.
    fn wait_timeout(&self) -> Duration {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(wait) = self.overrides().wait {
            return wait;
        }
        flows::WAIT_TIMEOUT
    }

    /// How long a login's code exchange may take.
    fn exchange_timeout(&self) -> Duration {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(exchange) = self.overrides().exchange {
            return exchange;
        }
        flows::EXCHANGE_TIMEOUT
    }

    /// How long a callback page waits for its login to end.
    fn page_wait(&self) -> Duration {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(page) = self.overrides().page {
            return page;
        }
        callback::PAGE_WAIT
    }
}

/// A secret as `Debug` shows it: `"[redacted]"`, or `""` when it is empty.
struct Redacted<'a>(&'a str);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() {
            "\"\""
        } else {
            "\"[redacted]\""
        })
    }
}

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    let callback = || get(callback::get).post(callback::post);
    vec![
        Route::key(
            "/v0/management/anthropic-auth-url",
            get(flows::anthropic_auth_url),
        ),
        Route::key("/v0/management/codex-auth-url", get(flows::codex_auth_url)),
        Route::key("/v8/management/oauth/auth-url", get(flows::auth_url)),
        Route::key(
            "/v8/management/oauth/siwc/bind",
            axum::routing::post(flows::siwc_bind),
        ),
        Route::key("/v0/management/get-auth-status", get(flows::status)),
        Route::key("/v8/management/oauth/status", get(flows::status)),
        Route::key("/v8/management/oauth/siwc/status", get(siwc_status::status)),
        Route::key(
            "/v8/management/credentials/siwc/status",
            get(siwc_status::status),
        ),
        Route::key("/v0/management/oauth-session", delete(flows::cancel)),
        Route::key("/v8/management/oauth/session", delete(flows::cancel)),
        Route::availability("/v0/management/oauth-callback", callback()),
        Route::availability(
            "/v8/management/oauth/callback",
            get(callback::get_v8).post(callback::post_v8),
        ),
        Route::open("/anthropic/callback", get(callback::anthropic_page)),
        Route::open("/codex/callback", get(callback::codex_page)),
    ]
}
