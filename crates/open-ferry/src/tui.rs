// Ported from CLIProxyAPI cmd/server/main.go (main's -tui branch,
// resolveManagementBaseURL) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The terminal management UI (`-tui`, see [`open_ferry_tui`]).
//!
//! On its own it is a client of a server already running: the management
//! API at `-management-base-url`, else at the config's
//! `management.base-url`, else at open-ferry's
//! `management.separate-address`, else on 127.0.0.1 at the config's port,
//! signed in with `-password` if given and otherwise with the key the user
//! types.
//!
//! With `-standalone` it runs the server itself, in the background, with a
//! local management password, `-password` or else one made up, that the TUI
//! signs in with. While the TUI has the terminal, the server's log lines go
//! to the TUI's logs tab and not to standard output, and the server prints
//! nothing. The TUI starts once the server answers a config request (see
//! [`open_ferry_tui::wait_ready`]); if it never does, the server is stopped
//! and the TUI doesn't start. When the TUI ends, the server stops. With
//! `management.separate-address` set, the TUI uses the management
//! address, as the management API is served only there.
//!
//! Errors are written to standard error, and the exit code is 0, as
//! upstream's.
//!
//! Deviations from upstream:
//! - The local management password made up for standalone mode is `tui-`
//!   and 128 random bits in hex. Upstream's is `tui-`, the process ID and
//!   the time in nanoseconds, which another local process could guess.
//! - While the TUI runs, the log lines still go to `main.log` with
//!   `logging-to-file`. Upstream discards every log line then, `main.log`'s
//!   too, until a reload changes `logging-to-file`.
//! - A config port that isn't a TCP port fails the readiness check at once,
//!   where upstream asks 30 times.
//! - With open-ferry's `management.separate-address`, the TUI's default
//!   and standalone mode use that address, with `https` while `tls` is
//!   on (see [`management_address_url`]). Upstream has no such setting.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use open_ferry_core::config::Config;
use open_ferry_tui::LogHook;
use tokio::sync::oneshot;

use crate::browser;
use crate::flags::Flags;
use crate::logging::LogLevel;
use crate::service;

/// How many log lines the logs tab may fall behind by in standalone mode.
const HOOK_LINES: usize = 2000;

/// The port the management API is on when the config doesn't say.
const DEFAULT_PORT: i64 = 8317;

/// Runs the TUI as `flags` ask, until the user quits.
pub async fn run(
    flags: Flags,
    config: Config,
    config_path: PathBuf,
    auth_dir: PathBuf,
    log_level: LogLevel,
) -> ExitCode {
    if flags.standalone {
        standalone(flags, config, config_path, auth_dir, log_level).await;
    } else {
        let base_url = resolve_management_base_url(&flags.management_base_url, Some(&config));
        if let Err(error) =
            open_ferry_tui::run_with_base_url(&base_url, &flags.password.0, None, open_url).await
        {
            eprintln!("TUI error: {error}");
        }
    }
    ExitCode::SUCCESS
}

/// Runs the server in the background and the TUI against it.
async fn standalone(
    flags: Flags,
    config: Config,
    config_path: PathBuf,
    auth_dir: PathBuf,
    log_level: LogLevel,
) {
    let file_log = log_level.file_log().clone();
    let hook = LogHook::new(HOOK_LINES);
    let tap = hook.clone();
    file_log.set_tap(Some(Arc::new(move |line: &[u8]| {
        tap.send(&String::from_utf8_lossy(line));
    })));
    file_log.mute_console(true);
    let restore = || {
        file_log.mute_console(false);
        file_log.set_tap(None);
    };

    let password = if flags.password.0.is_empty() {
        local_password()
    } else {
        flags.password.0
    };
    let port = u16::try_from(config.port).ok();
    let management_url = management_address_url(&config);
    let (cancel, cancelled) = oneshot::channel::<()>();
    let options = service::Options {
        local_password: password.clone(),
        keep_alive: false,
        announce: false,
    };
    let server = tokio::spawn(service::run(
        config,
        config_path,
        auth_dir,
        log_level,
        options,
        async move {
            let _ = cancelled.await;
        },
    ));

    let ready = match (&management_url, port) {
        (Some(url), _) => open_ferry_tui::wait_ready_at(url, &password).await,
        (None, Some(port)) => open_ferry_tui::wait_ready(port, &password).await,
        (None, None) => false,
    };
    match (management_url, port) {
        (Some(url), _) if ready => {
            let result =
                open_ferry_tui::run_with_base_url(&url, &password, Some(hook), open_url).await;
            restore();
            if let Err(error) = result {
                eprintln!("TUI error: {error}");
            }
        }
        (None, Some(port)) if ready => {
            let result = open_ferry_tui::run(port, &password, Some(hook), open_url).await;
            restore();
            if let Err(error) = result {
                eprintln!("TUI error: {error}");
            }
        }
        _ => {
            restore();
            let _ = cancel.send(());
            let _ = server.await;
            eprintln!("TUI error: embedded server is not ready");
            return;
        }
    }
    let _ = cancel.send(());
    let _ = server.await;
}

/// A local management password for standalone mode.
fn local_password() -> String {
    let bytes: [u8; 16] = rand::random();
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("tui-{hex}")
}

/// Opens a sign-in URL from the OAuth tab, as upstream's `openBrowser`,
/// whose error the tab ignores.
fn open_url(url: &str) {
    let _ = browser::open(url);
}

/// The management API the TUI uses on its own (`resolveManagementBaseURL`):
/// `flag_url`, else the config's `management.base-url`, else its
/// `management.separate-address` (see [`management_address_url`]), else
/// 127.0.0.1 at the config's port, or 8317 when that isn't positive; each
/// URL trimmed.
pub fn resolve_management_base_url(flag_url: &str, config: Option<&Config>) -> String {
    let base_url = flag_url.trim();
    if !base_url.is_empty() {
        return base_url.to_owned();
    }
    if let Some(config) = config {
        let base_url = config.remote_management.base_url.trim();
        if !base_url.is_empty() {
            return base_url.to_owned();
        }
        if let Some(url) = management_address_url(config) {
            return url;
        }
    }
    let port = config
        .map(|config| config.port)
        .filter(|&port| port > 0)
        .unwrap_or(DEFAULT_PORT);
    format!("http://127.0.0.1:{port}")
}

/// The URL of `config`'s management address (`management.separate-address`),
/// when it has one: on 127.0.0.1 when it is every interface, and with
/// `https` while `tls` is on.
fn management_address_url(config: &Config) -> Option<String> {
    let address = config.remote_management.separate_address().ok()??;
    Some(address.base_url(config.tls.enable))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ports TestResolveManagementBaseURL.
    #[test]
    fn resolves_the_management_base_url() {
        let config = |base_url: &str, port: i64| {
            let mut config = Config::default();
            config.remote_management.base_url = base_url.to_owned();
            config.port = port;
            config
        };
        let cases = [
            (
                "flag takes highest precedence",
                "https://flag.example.com",
                Some(config("https://cfg.example.com", 9000)),
                "https://flag.example.com",
            ),
            (
                "config base url used when flag is empty",
                "",
                Some(config("https://cfg.example.com", 9000)),
                "https://cfg.example.com",
            ),
            (
                "config port used when neither flag nor base url provided",
                "",
                Some(config("", 9090)),
                "http://127.0.0.1:9090",
            ),
            (
                "nil config falls back to default port 8317",
                "",
                None,
                "http://127.0.0.1:8317",
            ),
            (
                "zero port falls back to default port 8317",
                "",
                Some(config("", 0)),
                "http://127.0.0.1:8317",
            ),
        ];
        for (name, flag_url, config, want) in cases {
            assert_eq!(
                resolve_management_base_url(flag_url, config.as_ref()),
                want,
                "{name}"
            );
        }
    }

    // Not upstream's: the management address is the default after
    // management.base-url, with https while tls is on.
    #[test]
    fn defaults_to_the_management_address() {
        let config = |address: &str, base_url: &str, tls: bool| {
            let mut config = Config::default();
            config.port = 9000;
            config.remote_management.separate_address = address.to_owned();
            config.remote_management.base_url = base_url.to_owned();
            config.tls.enable = tls;
            config
        };
        for (address, base_url, tls, want) in [
            ("127.0.0.1:8318", "", false, "http://127.0.0.1:8318"),
            ("[::1]:8318", "", false, "http://[::1]:8318"),
            (":8318", "", false, "http://127.0.0.1:8318"),
            ("0.0.0.0:8318", "", true, "https://127.0.0.1:8318"),
            (
                "admin.internal:8318",
                "",
                false,
                "http://admin.internal:8318",
            ),
            (
                "127.0.0.1:8318",
                "https://cfg.example.com",
                false,
                "https://cfg.example.com",
            ),
            ("", "", false, "http://127.0.0.1:9000"),
            // Not one the loader takes: the port, as upstream.
            ("127.0.0.1", "", false, "http://127.0.0.1:9000"),
        ] {
            let config = config(address, base_url, tls);
            assert_eq!(
                resolve_management_base_url("", Some(&config)),
                want,
                "{address:?} {base_url:?} {tls}"
            );
        }
        let config = config("127.0.0.1:8318", "", false);
        assert_eq!(
            resolve_management_base_url("http://flag.example.com", Some(&config)),
            "http://flag.example.com"
        );
    }

    // Not upstream's: the URLs are trimmed, and the made-up password is
    // 128 random bits.
    #[test]
    fn trims_urls_and_makes_up_passwords() {
        let mut config = Config::default();
        config.remote_management.base_url = " https://cfg.example.com\t".into();
        assert_eq!(
            resolve_management_base_url(" \n", Some(&config)),
            "https://cfg.example.com"
        );
        assert_eq!(
            resolve_management_base_url(" https://flag.example.com ", None),
            "https://flag.example.com"
        );
        let password = local_password();
        assert_eq!(password.len(), 4 + 32);
        assert!(password.starts_with("tui-"));
        assert_ne!(password, local_password());
    }
}
