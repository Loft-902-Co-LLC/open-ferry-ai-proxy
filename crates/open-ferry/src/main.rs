// Ported from CLIProxyAPI cmd/server/main.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `open-ferry` command: serves the proxy, or runs a login.
//!
//! It loads the config (`-config`, or `config.yaml` in the working
//! directory), sets the log level from it, resolves the auth directory, and
//! then runs the login a flag asks for or else serves.
//!
//! Deviations from upstream:
//! - The cloud-deploy, home, Postgres, object-store and git-store modes, the
//!   TUI, plugins and the other providers' logins aren't ported.
//! - Remote model catalog updates aren't ported, so `-local-model` changes
//!   nothing but a log line: the built-in catalog is always the one used.
//! - A config that won't load, or an auth directory that won't resolve,
//!   exits with 1; upstream logs it and exits with 0.

mod browser;
mod flags;
mod logging;
mod login;
mod service;
mod tls;

use std::path::PathBuf;
use std::process::ExitCode;

use open_ferry_core::config::Config;

use crate::flags::{FlagError, Flags};
use crate::login::Login;

fn main() -> ExitCode {
    let mut args = std::env::args_os().map(|arg| arg.to_string_lossy().into_owned());
    let program = args.next().unwrap_or_else(|| "open-ferry".to_owned());
    let flags = match flags::parse(args) {
        Ok(flags) => flags,
        Err(FlagError::Help) => {
            eprint!("{}", flags::usage(&program));
            return ExitCode::SUCCESS;
        }
        Err(FlagError::Invalid(message)) => {
            eprintln!("{message}");
            eprint!("{}", flags::usage(&program));
            return ExitCode::from(2);
        }
    };
    let log_level = logging::init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!("failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(flags, log_level))
}

async fn run(flags: Flags, log_level: logging::LogLevel) -> ExitCode {
    let config_path = if flags.config.is_empty() {
        match std::env::current_dir() {
            Ok(dir) => dir.join("config.yaml"),
            Err(error) => {
                tracing::error!("failed to get working directory: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        PathBuf::from(&flags.config)
    };
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!("failed to load config: {error}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("open-ferry Version: {}", env!("CARGO_PKG_VERSION"));
    log_level.set_debug(config.debug);
    let auth_dir = match config.resolve_auth_dir() {
        Ok(dir) => dir,
        Err(error) => {
            tracing::error!("failed to resolve auth directory: {error}");
            return ExitCode::FAILURE;
        }
    };

    let login = if flags.codex_login {
        Some(Login::Codex)
    } else if flags.codex_device_login {
        Some(Login::CodexDevice)
    } else if flags.claude_login {
        Some(Login::Claude)
    } else {
        None
    };
    if let Some(login) = login {
        let options = login::Options {
            no_browser: flags.no_browser,
            callback_port: flags.oauth_callback_port,
        };
        return login::run(login, &config, &auth_dir, options).await;
    }
    if flags.local_model {
        tracing::info!(
            "Local model mode: using embedded model catalogs, remote model updates disabled"
        );
    }
    service::run(config, config_path, auth_dir, log_level).await
}
