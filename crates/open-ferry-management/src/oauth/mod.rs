// Ported from CLIProxyAPI internal/api/server_management.go and
// internal/api/server_management_v8.go (the OAuth login routes) and
// internal/api/server_routes.go (the /anthropic/callback and
// /codex/callback routes) (v8.0.15, MIT).
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
//! forwarder sends the browser to.
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

mod callback;
mod flows;
mod forwarder;
pub(crate) mod sessions;

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
    #[cfg(test)]
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
#[cfg(test)]
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
}

#[cfg(test)]
impl Default for Overrides {
    fn default() -> Self {
        Self {
            endpoints: Some("http://127.0.0.1:9".to_owned()),
            ports: Some((0, 0)),
            wait: None,
            exchange: None,
        }
    }
}

impl Sessions {
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
        logins.tasks.spawn(login);
        true
    }

    /// Stops every login and waits for their tasks to end; no login starts
    /// after this. A login stopped here stops its forwarder and frees its
    /// session as when it ends otherwise.
    pub(crate) async fn shutdown(&self) {
        let mut tasks = {
            let mut logins = self.logins();
            logins.shut_down = true;
            std::mem::take(&mut logins.tasks)
        };
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
    #[cfg(test)]
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
        #[cfg(test)]
        if let Some(base) = self.overrides().endpoints.clone() {
            return claude::Endpoints::with_base(&base);
        }
        claude::Endpoints::default()
    }

    /// OpenAI's endpoints.
    fn codex_endpoints(&self) -> codex::Endpoints {
        #[cfg(test)]
        if let Some(base) = self.overrides().endpoints.clone() {
            return codex::Endpoints::with_base(&base);
        }
        codex::Endpoints::default()
    }

    /// The port `provider`'s forwarder listens on.
    fn callback_port(&self, provider: Provider) -> u16 {
        #[cfg(test)]
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
        #[cfg(test)]
        if let Some(wait) = self.overrides().wait {
            return wait;
        }
        flows::WAIT_TIMEOUT
    }

    /// How long a login's code exchange may take.
    fn exchange_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(exchange) = self.overrides().exchange {
            return exchange;
        }
        flows::EXCHANGE_TIMEOUT
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
        Route::key("/v0/management/get-auth-status", get(flows::status)),
        Route::key("/v8/management/oauth/status", get(flows::status)),
        Route::key("/v0/management/oauth-session", delete(flows::cancel)),
        Route::key("/v8/management/oauth/session", delete(flows::cancel)),
        Route::availability("/v0/management/oauth-callback", callback()),
        Route::availability("/v8/management/oauth/callback", callback()),
        Route::open("/anthropic/callback", get(callback::anthropic_page)),
        Route::open("/codex/callback", get(callback::codex_page)),
    ]
}
