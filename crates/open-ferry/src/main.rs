// Ported from CLIProxyAPI cmd/server/main.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `open-ferry` command: serves the proxy, runs a login, or runs the
//! terminal management UI.
//!
//! It loads the `.env` file in the working directory into the environment
//! (see [`dotenv`]), then the config (`-config`, or `config.yaml` in the
//! working directory), sets the log level from it, resolves the auth
//! directory, and then runs the login a flag asks for, else the TUI with
//! `-tui` (see [`tui`]), or else serves. A server started with `-password` accepts it
//! as a local management password, and stops when its keep-alive endpoint
//! isn't called (see [`keep_alive`]).
//!
//! As the first argument, `init` writes a starting config (see [`init`]),
//! `check` looks over a setup (see [`check`]), `service` installs,
//! removes or shows open-ferry as a background service (see
//! [`os_service`]), `update` checks for, installs or rolls back a release,
//! or sets whether updates are automatic (see [`update`]), and `migrate`
//! switches from CLIProxyAPI and back (see [`migrate`]); the arguments
//! after it are theirs. `status`, `config`, `keys`, `credentials`,
//! `clients` and `mcp` look at and change a setup, for people and agents
//! (see [`agent`]). `-version` prints the version.
//!
//! Deviations from upstream:
//! - The cloud-deploy, home, Postgres, object-store and git-store modes,
//!   plugins and the other providers' logins aren't ported.
//! - No model catalog is downloaded: a catalog source of the `models`
//!   section is a file, or empty for the built-in catalog, with or without
//!   `-local-model` (see `open_ferry_core::registry::catalog_sources`). So
//!   `-local-model` changes nothing but a log line. Its usage and log line
//!   say so, where upstream's say that an explicit catalog source still
//!   overrides the embedded catalogs.
//! - A working directory that can't be read, a config that won't load, or
//!   an auth directory that won't resolve exits with 1; upstream logs it and
//!   exits with 0.

mod agent;
mod browser;
mod check;
mod dotenv;
mod file_log;
mod flags;
mod init;
mod installed;
mod keep_alive;
mod logging;
mod login;
mod migrate;
mod observability;
mod os_service;
mod service;
mod tls;
mod tui;
mod update;

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use open_ferry_core::config::Config;

use crate::flags::{FlagError, Flags};
use crate::login::Login;

/// How long, at most, exiting waits for the log lines still queued.
const EXIT_FLUSH: Duration = Duration::from_secs(1);

fn main() -> ExitCode {
    let mut args = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .peekable();
    let program = args.next().unwrap_or_else(|| "open-ferry".to_owned());
    // A subcommand is recognized only as the first argument (see `flags`).
    match args.peek().map(String::as_str) {
        Some(init::NAME) => return init::main(&program, args.skip(1)),
        Some(check::NAME) => return check::main(&program, args.skip(1)),
        Some(os_service::NAME) => return os_service::main(&program, args.skip(1)),
        Some(update::NAME) => return update::main(&program, args.skip(1)),
        Some(migrate::NAME) => return migrate::main(&program, args.skip(1)),
        Some(name) if agent::is_command(name) => return agent::main(&program, args),
        _ => {}
    }
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
    if flags.version {
        // What the updater checks a downloaded binary prints.
        println!("open-ferry {}", open_ferry_update::CURRENT_VERSION);
        return ExitCode::SUCCESS;
    }
    // Upstream loads `.env` before it reads the config, so the variables it
    // sets apply to everything after. This is before `logging::init`, which
    // starts the first thread, and nothing is logged until then.
    let working_dir = std::env::current_dir();
    let dotenv = working_dir
        .as_ref()
        .ok()
        .map(|dir| load_dotenv(&dir.join(".env")));
    serve(flags, working_dir, dotenv, service::shutdown_signal())
}

/// Logs in, runs the TUI or serves, as `flags` ask, a server until `stop`
/// resolves. `working_dir` and `dotenv` are what was found, and loaded,
/// before any thread started.
fn serve(
    flags: Flags,
    working_dir: io::Result<PathBuf>,
    dotenv: Option<Result<(), dotenv::Error>>,
    stop: impl Future<Output = ()>,
) -> ExitCode {
    let log_level = logging::init();
    let file_log = log_level.file_log().clone();
    let working_dir = match working_dir {
        Ok(dir) => dir,
        Err(error) => {
            tracing::error!("failed to get working directory: {error}");
            file_log.flush(EXIT_FLUSH);
            return ExitCode::FAILURE;
        }
    };
    if let Some(Err(error)) = dotenv
        && !error.is_not_found()
    {
        tracing::warn!("failed to load .env file: {error}");
    }
    let code = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(flags, log_level, working_dir, stop)),
        Err(error) => {
            tracing::error!("failed to start the async runtime: {error}");
            ExitCode::FAILURE
        }
    };
    // The lines are written by threads of their own, which exiting stops.
    file_log.flush(EXIT_FLUSH);
    code
}

/// Sets the variables of the `.env` file at `path` that the environment
/// doesn't have, as upstream's `godotenv.Load` does. A file that doesn't
/// parse sets none. Nothing from the file is logged.
fn load_dotenv(path: &Path) -> Result<(), dotenv::Error> {
    let vars = dotenv::read(path)?;
    for (key, value) in dotenv::missing(vars, dotenv::in_environment) {
        // SAFETY: `main` calls this before it starts any thread (the log
        // writer's, the runtime's), so nothing reads or writes the
        // environment meanwhile. `missing` leaves out the names and values
        // `set_var` panics on.
        unsafe { std::env::set_var(&key, &value) };
    }
    Ok(())
}

async fn run(
    flags: Flags,
    log_level: logging::LogLevel,
    working_dir: PathBuf,
    stop: impl Future<Output = ()>,
) -> ExitCode {
    let config_path = if flags.config.is_empty() {
        working_dir.join("config.yaml")
    } else {
        PathBuf::from(&flags.config)
    };
    let (config, config_sha256) = match Config::load_with_sha256(&config_path) {
        Ok(loaded) => loaded,
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
    if flags.local_model && (!flags.tui || flags.standalone) {
        tracing::info!(
            "Local model mode: using embedded catalogs unless a catalog file is configured, as without it: no catalog is downloaded"
        );
    }
    if flags.tui {
        return tui::run(
            flags,
            config,
            config_sha256,
            config_path,
            auth_dir,
            log_level,
        )
        .await;
    }
    let options = service::Options {
        local_password: flags.password.0,
        keep_alive: true,
        announce: true,
        config_sha256: Some(config_sha256),
        self_update: true,
    };
    service::run(config, config_path, auth_dir, log_level, options, stop).await
}
