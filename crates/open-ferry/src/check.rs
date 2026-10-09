//! `open-ferry check`: looks over a setup before a start, says how to fix
//! what it finds, and exits with a code a script or a service manager can
//! act on.
//!
//! It finds the config as the server does: `-config`, else `config.yaml`
//! in the working directory, after loading the working directory's `.env`.
//! Each finding is one line: its level (`ok`, `warning` or `error`), what
//! was checked, what was found and, when it isn't ok, what to do. A last
//! line counts the errors and warnings. `-json` prints one JSON object
//! instead. It exits with 0 when nothing is an error, warnings allowed, 1
//! when something is, and 2 when it couldn't check (bad usage).
//!
//! What it checks:
//! - the config exists and loads with the server's loader; the template's
//!   example client keys (safe mode) are an error, and no client key a
//!   warning;
//! - the management key: none, and no `MANAGEMENT_PASSWORD`, is a warning;
//! - the auth directory can be read, and each `*.json` credential file in
//!   it reads as the server reads it; a bad one is named, never shown;
//! - TLS, when on: the certificate and key load;
//! - the address: whether something already listens on the host and port,
//!   found by connecting to loopback, never by binding. A host name or an
//!   address that isn't loopback isn't checked, as that would be a network
//!   call;
//! - the management address, when `management.separate-address` sets one:
//!   its port, as for the address, and whether `management.allow-remote`
//!   fits it (see [`management`]);
//! - the dashboard app is built into this binary, unless it is turned off;
//! - the clock, with no network: the system time against this binary's
//!   build date (release builds know it), and against each credential's
//!   last refresh and expiry;
//! - each enabled `claude-cli` entry's Claude Code, with `claude --version`
//!   as the server checks it at start. It never runs `claude auth status`;
//! - whether open-ferry updates itself, and can (see [`self_update`]).
//!
//! It writes nothing, and its only connections are those to loopback.
//!
//! Upstream has no `check` (see [`flags`]).

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use open_ferry_core::auth::Auth;
use open_ferry_core::auth::file_store::read_capped;
use open_ferry_core::auth::synthesizer::SynthesisContext;
use open_ferry_core::auth::synthesizer::file::synthesize_auth_file;
use open_ferry_core::config::Config;
use open_ferry_core::manager::last_refresh_timestamp;
use open_ferry_providers::claude_cli::{self, MIN_VERSION, VersionCheck};
use serde_json::{Map, Value, json};

use crate::flags::{self, Definition, FlagError, Kind};

mod management;
mod self_update;

/// The subcommand's name: the first argument that runs it.
pub const NAME: &str = "check";

/// How long a connection to the address may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// How far ahead of the system time a credential's last refresh may be.
const REFRESH_SKEW_MINUTES: i64 = 10;

/// How far ahead a credential's expiry may be; Claude Code's long-lived
/// tokens last a year.
const EXPIRY_HORIZON_DAYS: i64 = 400;

/// What the command line asks for.
#[derive(Default)]
struct Options {
    config: String,
    json: bool,
}

/// The flags, sorted by name.
const DEFINITIONS: [Definition<Options>; 2] = [
    Definition {
        name: "config",
        usage: "Check this config file (default: config.yaml in the working directory, as the server reads it)",
        kind: Kind::String(|options, value| options.config = value),
    },
    Definition {
        name: "json",
        usage: "Print one JSON object rather than a line for each finding",
        kind: Kind::Bool(|options, value| options.json = value),
    },
];

/// The usage text.
fn usage(program: &str) -> String {
    let mut out = format!(
        "Usage: {program} {NAME} [flags]\n\n\
         Checks a setup before a start: the config, the keys, the auth directory and\n\
         its credential files, TLS, the address, the dashboard, the clock and each\n\
         claude-cli entry's Claude Code. It writes nothing, and its only connections\n\
         are to loopback, to see whether the port is taken.\n\
         Exit codes: 0 when nothing is an error, 1 when something is, 2 when it\n\
         couldn't check.\n\nFlags:\n"
    );
    flags::write_defaults(&mut out, &DEFINITIONS, &[]);
    out
}

/// Runs `open-ferry check` with `args`, the arguments after `check`.
/// `main` calls it before any thread starts, as `.env` is loaded here.
pub fn main<I>(program: &str, args: I) -> ExitCode
where
    I: IntoIterator<Item = String>,
{
    let options = match flags::parse_with(&DEFINITIONS, args) {
        Ok((options, rest)) => match rest.first() {
            None => options,
            Some(arg) => return usage_error(program, &format!("unexpected argument: {arg}")),
        },
        Err(FlagError::Help) => {
            eprint!("{}", usage(program));
            return ExitCode::SUCCESS;
        }
        Err(FlagError::Invalid(message)) => return usage_error(program, &message),
    };
    let working_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("{NAME}: failed to get working directory: {error}");
            return ExitCode::from(2);
        }
    };
    let path = if options.config.is_empty() {
        working_dir.join("config.yaml")
    } else {
        PathBuf::from(&options.config)
    };
    let mut findings = Vec::new();
    let dotenv = working_dir.join(".env");
    if let Err(error) = crate::load_dotenv(&dotenv)
        && !error.is_not_found()
    {
        findings.push(Finding::warning(
            ".env",
            format!(
                "{} doesn't load, so none of it is set: {error}",
                dotenv.display()
            ),
            "fix the line it names",
        ));
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{NAME}: failed to start the async runtime: {error}");
            return ExitCode::from(2);
        }
    };
    findings.extend(runtime.block_on(run(&path, &Environment::current())));
    if options.json {
        println!("{}", render_json(&path, &findings));
    } else {
        print!("{}", render_text(&findings));
    }
    exit_code(&findings)
}

/// Shows `message` and the usage, for bad usage.
fn usage_error(program: &str, message: &str) -> ExitCode {
    eprintln!("{message}");
    eprint!("{}", usage(program));
    ExitCode::from(2)
}

/// How much a finding matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Ok,
    Warning,
    Error,
}

impl Level {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

/// One finding: its level, what was checked, what was found, and what to
/// do when it isn't ok. None of it holds a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) level: Level,
    pub(crate) check: String,
    pub(crate) message: String,
    pub(crate) fix: String,
}

impl Finding {
    fn ok(check: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Level::Ok, check, message, "")
    }

    fn warning(
        check: impl Into<String>,
        message: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(Level::Warning, check, message, fix)
    }

    fn error(check: impl Into<String>, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(Level::Error, check, message, fix)
    }

    fn new(
        level: Level,
        check: impl Into<String>,
        message: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            level,
            check: check.into(),
            message: message.into(),
            fix: fix.into(),
        }
    }
}

/// What the checks read besides the config.
pub(crate) struct Environment {
    /// The system time.
    pub(crate) now: DateTime<Utc>,
    /// When this binary was built, when the build said
    /// (`OPEN_FERRY_BUILD_DATE`).
    pub(crate) build_date: Option<DateTime<Utc>>,
    /// Whether the dashboard app is built in.
    pub(crate) dashboard_built: bool,
    /// Whether `MANAGEMENT_PASSWORD` sets a management key.
    pub(crate) management_password: bool,
    /// Whether the checks may run a program the config names (a
    /// `claude-cli` entry's command). `open-ferry migrate` sets it false:
    /// it reads configs it didn't write, maybe as another user's root.
    pub(crate) run_config_programs: bool,
    /// What the `self-update` finding reads.
    updates: self_update::Updates,
}

impl Environment {
    pub(crate) fn current() -> Self {
        Self {
            now: Utc::now(),
            build_date: option_env!("OPEN_FERRY_BUILD_DATE").and_then(|date| {
                DateTime::parse_from_rfc3339(date)
                    .ok()
                    .map(|date| date.with_timezone(&Utc))
            }),
            dashboard_built: open_ferry_dashboard::app_built(),
            management_password: open_ferry_management::management_password_from_env()
                .is_some_and(|password| !password.is_empty()),
            run_config_programs: true,
            updates: self_update::Updates::current(),
        }
    }
}

/// Checks the setup of the config at `path`. Without a config that loads,
/// that is the only finding.
pub(crate) async fn run(path: &Path, env: &Environment) -> Vec<Finding> {
    let mut findings = Vec::new();
    let Some(config) = check_config(path, &mut findings) else {
        return findings;
    };
    check_client_keys(&config, &mut findings);
    check_management_key(&config, env, &mut findings);
    let auths = check_auth_dir(&config, env.now, &mut findings);
    check_tls(&config, &mut findings);
    check_address(&config, &mut findings).await;
    management::check_management_address(&config, env, &mut findings).await;
    check_dashboard(&config, env, &mut findings);
    check_clock(env, &auths, &mut findings);
    if env.run_config_programs {
        check_claude_cli(&config, &mut findings).await;
    } else {
        skip_claude_cli(&config, &mut findings);
    }
    self_update::check_self_update(&config, env, &mut findings);
    findings
}

/// The config exists and loads.
fn check_config(path: &Path, findings: &mut Vec<Finding>) -> Option<Config> {
    const CHECK: &str = "config";
    let shown = path.display();
    match fs::metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            findings.push(Finding::error(
                CHECK,
                format!("{shown} doesn't exist"),
                format!(
                    "write one with `open-ferry init -config {shown}`, or pass -config with your config's path"
                ),
            ));
            return None;
        }
        Ok(metadata) if metadata.is_dir() => {
            findings.push(Finding::error(
                CHECK,
                format!("{shown} is a directory"),
                "pass -config with the config file's path",
            ));
            return None;
        }
        _ => {}
    }
    match Config::load(path) {
        Ok(config) => {
            findings.push(Finding::ok(CHECK, format!("{shown} loads")));
            Some(config)
        }
        Err(error) => {
            findings.push(Finding::error(
                CHECK,
                format!("{shown} doesn't load: {error}"),
                "fix what the error names; the server won't start with this config",
            ));
            None
        }
    }
}

/// The client keys: the template's examples put the proxy in safe mode,
/// and none lets any client in.
fn check_client_keys(config: &Config, findings: &mut Vec<Finding>) {
    const CHECK: &str = "client keys";
    let examples = config.example_api_keys();
    let finding = if !examples.is_empty() {
        Finding::error(
            CHECK,
            format!(
                "the template's example keys are still listed ({}), so the proxy refuses service (safe mode)",
                examples.join(", ")
            ),
            "replace them in access.api-keys with your own keys, or write a new config with `open-ferry init`",
        )
    } else if config.api_keys.is_empty() {
        Finding::warning(
            CHECK,
            "access.api-keys is empty, so any client that reaches the proxy can use it",
            "add a key to access.api-keys",
        )
    } else {
        Finding::ok(CHECK, plural(config.api_keys.len(), "key", "keys") + " set")
    };
    findings.push(finding);
}

/// The management key: without one the management API and the dashboard's
/// sign-in are off.
fn check_management_key(config: &Config, env: &Environment, findings: &mut Vec<Finding>) {
    const CHECK: &str = "management key";
    let finding = if !config.remote_management.secret_key.trim().is_empty() {
        Finding::ok(CHECK, "set")
    } else if env.management_password {
        Finding::ok(
            CHECK,
            "management.secret-key is empty; MANAGEMENT_PASSWORD sets one",
        )
    } else {
        Finding::warning(
            CHECK,
            "management.secret-key is empty, so the management API and the dashboard's sign-in are off",
            "set management.secret-key to a long random key (`open-ferry init` writes a config with one)",
        )
    };
    findings.push(finding);
}

/// The auth directory can be read, and each credential file in it reads
/// as the server reads it. Returns the credentials that do, by file name.
fn check_auth_dir(
    config: &Config,
    now: DateTime<Utc>,
    findings: &mut Vec<Finding>,
) -> Vec<(String, Auth)> {
    const CHECK: &str = "auth directory";
    const READABLE: &str = "make it a directory the user the proxy runs as can read";
    let dir = match config.resolve_auth_dir() {
        Ok(dir) => dir,
        Err(error) => {
            findings.push(Finding::error(
                CHECK,
                error.to_string(),
                "set oauth.auth-dir to a directory",
            ));
            return Vec::new();
        }
    };
    if dir.as_os_str().is_empty() {
        findings.push(Finding::ok(
            CHECK,
            "none is set, so no credential file is read",
        ));
        return Vec::new();
    }
    let shown = dir.display();
    match fs::metadata(&dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            findings.push(Finding::error(
                CHECK,
                format!("{shown} isn't a directory"),
                "set oauth.auth-dir to a directory",
            ));
            return Vec::new();
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            findings.push(Finding::ok(
                CHECK,
                format!("{shown} doesn't exist yet; the proxy makes it when it starts"),
            ));
            return Vec::new();
        }
        Err(error) => {
            findings.push(Finding::error(CHECK, format!("{shown}: {error}"), READABLE));
            return Vec::new();
        }
    }
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) => {
            findings.push(Finding::error(
                CHECK,
                format!("{shown} can't be read: {error}"),
                READABLE,
            ));
            return Vec::new();
        }
    };
    // The files the server reads: `*.json` directly in the directory.
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| open_ferry_translate::go::to_lower(name).ends_with(".json"))
        .collect();
    names.sort();
    let ctx = SynthesisContext::new(&dir, now);
    let mut auths = Vec::new();
    let mut files = Vec::new();
    for name in names {
        let check = format!("credential {name}");
        let full = dir.join(&name);
        let data = match read_capped(&full) {
            Ok(data) => data,
            Err(error) => {
                files.push(Finding::error(
                    check,
                    format!("can't be read: {error}"),
                    "make it readable, or remove it",
                ));
                continue;
            }
        };
        if data.is_empty() {
            files.push(Finding::warning(
                check,
                "is empty, so the proxy skips it",
                "remove it, or sign in again",
            ));
            continue;
        }
        match synthesize_auth_file(&ctx, &full, &data) {
            Ok(Some(auth)) => auths.push((name, auth)),
            Ok(None) => files.push(skipped(check, &data)),
            Err(error) => files.push(Finding::error(
                check,
                error.to_string(),
                "fix or remove the setting it names",
            )),
        }
    }
    findings.push(Finding::ok(
        CHECK,
        format!(
            "{shown} can be read; {} {}",
            plural(
                auths.len(),
                "credential file loads",
                "credential files load"
            ),
            if files.is_empty() {
                ""
            } else {
                "(see below for the others)"
            }
        )
        .trim_end()
        .to_owned(),
    ));
    findings.extend(files);
    auths
}

/// Why the server skips a credential file's contents, `data`, without
/// showing any of them.
fn skipped(check: String, data: &[u8]) -> Finding {
    const FIX: &str = "remove it, or sign in again to write a new one";
    match serde_json::from_slice::<Value>(data) {
        Err(error) => Finding::error(
            check,
            format!(
                "isn't valid JSON (line {}, column {})",
                error.line(),
                error.column()
            ),
            FIX,
        ),
        Ok(Value::Object(map)) => match credential_type(&map).as_str() {
            "" => Finding::warning(
                check,
                "has no type, so the proxy skips it",
                "remove it if it isn't a credential",
            ),
            "gemini" | "gemini-cli" => Finding::warning(
                check,
                "is a Gemini CLI credential, which open-ferry doesn't serve",
                "nothing, unless you meant to use it: open-ferry skips it",
            ),
            _ => Finding::warning(check, "the proxy skips it", FIX),
        },
        Ok(_) => Finding::error(check, "isn't a JSON object", FIX),
    }
}

/// A credential's `type`, trimmed and lowercased.
fn credential_type(map: &Map<String, Value>) -> String {
    open_ferry_translate::go::to_lower(
        map.get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim(),
    )
}

/// TLS, when on: the certificate and key load.
fn check_tls(config: &Config, findings: &mut Vec<Finding>) {
    const CHECK: &str = "tls";
    if !config.tls.enable {
        return;
    }
    findings.push(match crate::tls::load(&config.tls.cert, &config.tls.key) {
        Ok(_) => Finding::ok(CHECK, "the certificate and key load"),
        Err(error) => Finding::error(
            CHECK,
            error,
            "fix tls.cert and tls.key, or set tls.enable to false",
        ),
    });
}

/// Whether something already listens on the server's host and port,
/// found by connecting to loopback addresses only.
async fn check_address(config: &Config, findings: &mut Vec<Finding>) {
    const CHECK: &str = "address";
    let Ok(port) = u16::try_from(config.port) else {
        findings.push(Finding::error(
            CHECK,
            format!("server.port {} isn't a port", config.port),
            "set server.port to a port from 1 to 65535",
        ));
        return;
    };
    if port == 0 {
        findings.push(Finding::warning(
            CHECK,
            "server.port isn't set, so the proxy listens on a port the system picks",
            "set server.port",
        ));
        return;
    }
    let host = config.host.trim();
    findings.push(match check_port(host, port).await {
        PortCheck::NotChecked(what) => Finding::warning(
            CHECK,
            format!(
                "server.host {host} {what}, so whether port {port} is free there isn't checked (check makes no network call)"
            ),
            format!(
                "if the proxy fails to start because the address is in use, free port {port} or set server.port"
            ),
        ),
        PortCheck::Listening(address) => Finding::error(
            CHECK,
            format!("something already listens on {address}"),
            "stop it (if it is this proxy, it is already running), or set server.port to a free port",
        ),
        PortCheck::Unknown(unknown) => Finding::warning(
            CHECK,
            format!(
                "couldn't tell whether something listens on {}",
                unknown.join("; ")
            ),
            format!(
                "run check again; if the proxy fails to start because the address is in use, free port {port} or set server.port"
            ),
        ),
        PortCheck::Free(shown) => Finding::ok(CHECK, format!("nothing listens on {shown}")),
    });
}

/// What connecting to loopback says of `port` on `host`.
#[derive(Debug, PartialEq)]
enum PortCheck {
    /// The host isn't checked, as that would be a network call: `what`
    /// says why ("isn't a loopback address" or "is a host name").
    NotChecked(&'static str),
    /// Something listens on the address.
    Listening(SocketAddr),
    /// Some connections said nothing of the port: each address and its
    /// error, sorted.
    Unknown(Vec<String>),
    /// Nothing listens on the addresses checked, joined with "or".
    Free(String),
}

/// Whether something listens on `host`:`port`, found by connecting to the
/// loopback addresses it covers: both for every interface and
/// `localhost`. A host name or an address that isn't loopback isn't
/// checked.
async fn check_port(host: &str, port: u16) -> PortCheck {
    let bare = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let targets = match bare {
        "" | "localhost" => vec![v4, v6],
        _ => match bare.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) if ip.is_unspecified() => vec![v4],
            Ok(IpAddr::V6(ip)) if ip.is_unspecified() => vec![v6, v4],
            Ok(ip) if ip.is_loopback() => vec![ip],
            Ok(_) => return PortCheck::NotChecked("isn't a loopback address"),
            Err(_) => return PortCheck::NotChecked("is a host name"),
        },
    };
    let addresses: Vec<SocketAddr> = targets
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    // At once: a connection to a closed port can take seconds to fail, on
    // Windows.
    let mut connects = tokio::task::JoinSet::new();
    for address in addresses.iter().copied() {
        connects.spawn(async move {
            let connect = tokio::net::TcpStream::connect(address);
            let result = tokio::time::timeout(CONNECT_TIMEOUT, connect)
                .await
                .ok()
                .map(|connected| connected.map(drop));
            (address, probe(result))
        });
    }
    let mut unknown = Vec::new();
    while let Some(connected) = connects.join_next().await {
        match connected {
            Ok((address, Probe::Listening)) => return PortCheck::Listening(address),
            Ok((address, Probe::Unknown(error))) => unknown.push(format!("{address}: {error}")),
            Ok((_, Probe::Free)) | Err(_) => {}
        }
    }
    if !unknown.is_empty() {
        unknown.sort();
        return PortCheck::Unknown(unknown);
    }
    let shown: Vec<String> = addresses.iter().map(SocketAddr::to_string).collect();
    PortCheck::Free(shown.join(" or "))
}

/// What a connection to a loopback address says of its port.
#[derive(Debug, PartialEq)]
enum Probe {
    Listening,
    Free,
    /// The connection failed for another reason, such as the machine
    /// running out of local ports, so it says nothing of the port.
    Unknown(String),
}

/// Reads the result of connecting to a loopback address, `None` when the
/// connection timed out.
fn probe(result: Option<io::Result<()>>) -> Probe {
    match result {
        Some(Ok(())) => Probe::Listening,
        // Windows takes about two seconds to refuse a connection to a
        // closed loopback port, longer than the timeout.
        None => Probe::Free,
        // Refused: nothing listens. Not available, unreachable, down or an
        // unsupported family: the machine has no such address, such as ::1
        // where IPv6 is off, so nothing can listen there.
        Some(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::AddrNotAvailable
                    | io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::HostUnreachable
                    | io::ErrorKind::NetworkDown
            ) || error.raw_os_error() == Some(EAFNOSUPPORT) =>
        {
            Probe::Free
        }
        Some(Err(error)) => Probe::Unknown(error.to_string()),
    }
}

/// The error for an address family the system doesn't have, as a Linux
/// started with IPv6 off gives for ::1 (`WSAEAFNOSUPPORT` on Windows). No
/// `io::ErrorKind` names it.
const EAFNOSUPPORT: i32 = if cfg!(windows) {
    10047
} else if cfg!(any(target_os = "linux", target_os = "android")) {
    97
} else {
    47
};

/// The dashboard app is built in, unless the config turns it off. Its URL
/// is the management address's when there is one.
fn check_dashboard(config: &Config, env: &Environment, findings: &mut Vec<Finding>) {
    const CHECK: &str = "dashboard";
    findings.push(if config.remote_management.disable_control_panel {
        Finding::ok(
            CHECK,
            "turned off by management.disable-control-panel",
        )
    } else if env.dashboard_built {
        Finding::ok(
            CHECK,
            match management::dashboard_url(config) {
                Some(url) => format!("built in, at {url}, on the management address alone"),
                None => format!(
                    "built in, at {}",
                    crate::init::dashboard_url(&config.host, config.port, config.tls.enable)
                ),
            },
        )
    } else {
        Finding::warning(
            CHECK,
            "this binary was built without the dashboard app, so /dashboard/ shows a page that says so",
            "use a release binary, or build the app in dashboard/ before building open-ferry",
        )
    });
}

/// The system time, against the build date and the credentials' times.
fn check_clock(env: &Environment, auths: &[(String, Auth)], findings: &mut Vec<Finding>) {
    const CHECK: &str = "clock";
    const FIX: &str = "check the system clock";
    let start = findings.len();
    let now = env.now;
    if let Some(build) = env.build_date
        && now < build
    {
        findings.push(Finding::warning(
            CHECK,
            format!(
                "the system time, {}, is before this binary's build date, {}",
                time(now),
                time(build)
            ),
            "set the system clock; signing in and TLS need it right",
        ));
    }
    let refresh_limit = now + TimeDelta::minutes(REFRESH_SKEW_MINUTES);
    let expiry_limit = now + TimeDelta::days(EXPIRY_HORIZON_DAYS);
    for (name, auth) in auths {
        if let Some(at) = last_refresh_timestamp(auth)
            && at > refresh_limit
        {
            findings.push(Finding::warning(
                CHECK,
                format!(
                    "{name} last refreshed at {}, after the system time",
                    time(at)
                ),
                FIX,
            ));
        }
        if let Some(at) = auth.expiration_time()
            && at > expiry_limit
        {
            findings.push(Finding::warning(
                CHECK,
                format!(
                    "{name} expires at {}, more than {EXPIRY_HORIZON_DAYS} days after the system time",
                    time(at)
                ),
                FIX,
            ));
        }
    }
    if findings.len() == start {
        let against = match (env.build_date.is_some(), auths.is_empty()) {
            (true, false) => "agrees with this binary's build date and the credentials' times",
            (true, true) => "agrees with this binary's build date",
            (false, false) => "agrees with the credentials' times",
            (false, true) => "has no build date or credential time to be checked against",
        };
        findings.push(Finding::ok(
            CHECK,
            format!("the system time, {}, {against}", time(now)),
        ));
    }
}

/// Each enabled `claude-cli` entry's Claude Code, once for each command.
async fn check_claude_cli(config: &Config, findings: &mut Vec<Finding>) {
    for (names, check) in claude_cli::check_versions(&config.claude_cli).await {
        findings.push(claude_cli_finding(&names, check));
    }
}

/// Says the `claude-cli` entries weren't checked, without running their
/// commands.
fn skip_claude_cli(config: &Config, findings: &mut Vec<Finding>) {
    for (_, names) in claude_cli::by_command(&config.claude_cli) {
        findings.push(Finding::warning(
            format!("claude-cli {}", names.join(", ")),
            "not checked: this check doesn't run the program a config names",
            "run `open-ferry check` once open-ferry is running",
        ));
    }
}

/// What a `claude-cli` version check for the entries `names` found.
fn claude_cli_finding(names: &[String], check: VersionCheck) -> Finding {
    let name = format!("claude-cli {}", names.join(", "));
    let (major, minor, patch) = MIN_VERSION;
    match check {
        VersionCheck::Supported(version) => Finding::ok(name, format!("Claude Code {version}")),
        VersionCheck::Outdated(version) => Finding::error(
            name,
            format!(
                "Claude Code {version} is older than {major}.{minor}.{patch}, which claude-cli needs"
            ),
            "update it with `claude update`",
        ),
        VersionCheck::Unknown(output) => Finding::warning(
            name,
            format!("Claude Code gave no version (it printed {output:?})"),
            "check that the entry's command runs Claude Code",
        ),
        VersionCheck::Failed(error) => Finding::error(
            name,
            format!("couldn't run Claude Code: {error}"),
            "install Claude Code, or set the entry's command to the claude executable",
        ),
    }
}

/// A time as RFC 3339, to the second.
fn time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `count` and the noun for it.
fn plural(count: usize, one: &str, many: &str) -> String {
    match count {
        0 => format!("no {many}"),
        1 => format!("1 {one}"),
        count => format!("{count} {many}"),
    }
}

/// How many findings are at `level`.
fn count(findings: &[Finding], level: Level) -> usize {
    findings
        .iter()
        .filter(|finding| finding.level == level)
        .count()
}

/// A line for each finding, then the count of errors and warnings.
fn render_text(findings: &[Finding]) -> String {
    let mut out = String::new();
    for finding in findings {
        let _ = write!(
            out,
            "{:<7} {}: {}",
            finding.level.name(),
            finding.check,
            finding.message
        );
        if !finding.fix.is_empty() {
            let _ = write!(out, ". Fix: {}", finding.fix);
        }
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "{}, {}",
        plural(count(findings, Level::Error), "error", "errors"),
        plural(count(findings, Level::Warning), "warning", "warnings")
    );
    out
}

/// One JSON object: the config's path, the worst level, the counts, and
/// the findings.
fn render_json(path: &Path, findings: &[Finding]) -> Value {
    let status = findings
        .iter()
        .map(|finding| finding.level)
        .max()
        .unwrap_or(Level::Ok);
    let errors = count(findings, Level::Error);
    let warnings = count(findings, Level::Warning);
    let findings: Vec<Value> = findings
        .iter()
        .map(|finding| {
            let mut value = json!({
                "level": finding.level.name(),
                "check": finding.check,
                "message": finding.message,
            });
            if !finding.fix.is_empty()
                && let Value::Object(map) = &mut value
            {
                map.insert("fix".into(), Value::String(finding.fix.clone()));
            }
            value
        })
        .collect();
    json!({
        "config": path.display().to_string(),
        "status": status.name(),
        "errors": errors,
        "warnings": warnings,
        "findings": findings,
    })
}

/// 1 when a finding is an error, else 0.
fn exit_code(findings: &[Finding]) -> ExitCode {
    if count(findings, Level::Error) > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests;
