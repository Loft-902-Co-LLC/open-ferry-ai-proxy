// Ported from CLIProxyAPI internal/tui/app.go (Run, RunWithBaseURL) and
// cmd/server/main.go (the embedded server's readiness check) (v8.0.20,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Terminal management UI: a client of the management API, in tabs for the
//! dashboard, the config, auth files, API keys, OAuth sign-in and logs.
//!
//! [`run_with_base_url`] runs it against any server; [`run`] against one
//! on a loopback port, which in standalone mode is the embedded server,
//! whose log lines come through a [`LogHook`].
//!
//! Deviations from upstream:
//! - The run functions take what opens a URL in a browser, rather than
//!   opening it themselves, and write to standard output only.
//! - [`wait_ready_at`] checks a server at any base URL, for open-ferry's
//!   `management.separate-address`; upstream checks a loopback port only.

mod ansi;
mod app;
mod auth_tab;
mod client;
mod config_tab;
mod dashboard;
mod i18n;
mod keys;
mod keys_tab;
mod loghook;
mod logs_tab;
mod oauth_tab;
mod style;
mod styles;
mod tea;
mod terminal;
#[cfg(test)]
mod testing;
mod textinput;
mod viewport;

#[cfg(test)]
mod screens;

use std::io;
use std::sync::Arc;
use std::time::Duration;

pub use loghook::LogHook;

use crate::app::App;
use crate::client::Client;

/// How many times [`wait_ready`] asks.
const READY_TRIES: usize = 30;

/// `RunWithBaseURL`: runs the TUI in the terminal against the management
/// API at `base_url` until the user quits. With a hook it is in standalone
/// mode, signed in with `secret`; otherwise it asks for the management
/// key, with `secret` typed in. `open_url` opens a sign-in URL in a
/// browser.
///
/// # Errors
///
/// When the terminal can't be set up or drawn to.
pub async fn run_with_base_url(
    base_url: &str,
    secret: &str,
    hook: Option<LogHook>,
    open_url: impl Fn(&str) + Send + Sync + 'static,
) -> io::Result<()> {
    let platform = terminal::platform(Arc::new(open_url));
    terminal::run(App::new(base_url, secret, hook, platform)).await
}

/// `Run`: [`run_with_base_url`] for the server on loopback port `port`.
///
/// # Errors
///
/// When the terminal can't be set up or drawn to.
pub async fn run(
    port: u16,
    secret: &str,
    hook: Option<LogHook>,
    open_url: impl Fn(&str) + Send + Sync + 'static,
) -> io::Result<()> {
    run_with_base_url(&format!("http://127.0.0.1:{port}"), secret, hook, open_url).await
}

/// Whether the server on loopback port `port` answers a config request
/// with `secret`, asking up to 30 times, 100 ms apart at first and then
/// half as long again each time until a second apart, as upstream checks
/// the embedded server before it runs the TUI.
pub async fn wait_ready(port: u16, secret: &str) -> bool {
    ready(&Client::local(i64::from(port), secret), READY_TRIES).await
}

/// [`wait_ready`] for the server at `base_url`, read as
/// [`run_with_base_url`] reads it.
pub async fn wait_ready_at(base_url: &str, secret: &str) -> bool {
    ready(&Client::new(base_url, secret), READY_TRIES).await
}

async fn ready(client: &Client, tries: usize) -> bool {
    let mut backoff = Duration::from_millis(100);
    for _ in 0..tries {
        if client.get_config().await.is_ok() {
            return true;
        }
        tokio::time::sleep(backoff).await;
        if backoff < Duration::from_secs(1) {
            backoff = backoff.mul_f64(1.5);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the readiness check succeeds against a server that
    // answers, and gives up on one that keeps failing.
    #[tokio::test]
    async fn waits_for_the_server() {
        let server = testing::Server::start(&[("GET /v0/management/config", "{}")]).await;
        assert!(wait_ready(server.port(), "pw").await);
        let requests = server.take_requests();
        assert_eq!(requests, ["GET /v0/management/config auth=Bearer pw body="]);

        let failing = testing::Server::start(&[]).await;
        let client = Client::local(i64::from(failing.port()), "pw");
        assert!(!ready(&client, 3).await);
        assert_eq!(failing.take_requests().len(), 3);
    }

    // Not upstream's: the readiness check at a base URL, with or without
    // its scheme.
    #[tokio::test]
    async fn waits_for_the_server_at_a_base_url() {
        let server = testing::Server::start(&[("GET /v0/management/config", "{}")]).await;
        let port = server.port();
        for base_url in [
            format!("http://127.0.0.1:{port}/"),
            format!("127.0.0.1:{port}"),
        ] {
            assert!(wait_ready_at(&base_url, "pw").await, "{base_url}");
        }
        assert_eq!(server.take_requests().len(), 2);
    }
}
