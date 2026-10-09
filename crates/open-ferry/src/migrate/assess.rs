//! What carries over from CLIProxyAPI, what doesn't, and what stops the
//! switch.
//!
//! The config is loaded with open-ferry's loader and looked over with
//! `open-ferry check`'s checks. Credential files are read for their `type`
//! only, and counted by it; nothing else in them is kept or shown. The
//! environment that matters, the remote stores and cloud mode
//! (`PGSTORE_*`, `GITSTORE_*`, `OBJECTSTORE_*`, `DEPLOY=cloud`) and Home
//! mode (`HOME_JWT`), is looked for in CLIProxyAPI's process, or else its
//! service definition and the files it names, and in the `.env` file
//! upstream loads from its working directory (`cmd/server/main.go`). Only
//! the variables' names are shown.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use open_ferry_core::auth::file_store::MAX_AUTH_FILE_SIZE;
use open_ferry_core::config::{AnyValue, Config, DEFAULT_AUTH_DIR, V8Document};
use serde_json::Value;

use super::discover::{self, Flags, Found, Starter};
use super::machine::{EntryKind, Machine};
use crate::check::{Finding, Level};
use crate::dotenv;
use crate::os_service::{Context, Platform, Target};

/// How CLIProxyAPI is switched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Its service manager's service is stopped and disabled, and
    /// open-ferry's installed as `Target`.
    Service(Target),
    /// open-ferry's binary takes the place of CLIProxyAPI's.
    DropIn,
    /// The steps to take are printed.
    Container,
}

impl Kind {
    pub(crate) fn of(platform: Platform, starter: &Starter) -> Kind {
        match starter {
            Starter::Systemd { user: true, .. } => Kind::Service(Target::SystemdUser),
            Starter::Systemd { user: false, .. } => Kind::Service(Target::SystemdSystem),
            Starter::Launchd { domain, .. } if domain == "system" => {
                Kind::Service(Target::LaunchDaemon)
            }
            Starter::Launchd { .. } => Kind::Service(Target::LaunchAgent),
            Starter::WindowsService { .. } => Kind::Service(Target::WindowsService),
            Starter::Task { .. } => Kind::Service(Target::new(platform, false)),
            Starter::Launcher(_) => Kind::DropIn,
            Starter::Container(_) => Kind::Container,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::Service(_) => "service",
            Kind::DropIn => "drop-in",
            Kind::Container => "container",
        }
    }
}

/// Where the proxy listens, from the config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listen {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) tls: bool,
    /// Where to ask whether open-ferry answers: the host's address, or the
    /// loopback address for all addresses. `None` for a host name.
    pub(crate) probe: Option<IpAddr>,
}

impl Listen {
    pub(crate) fn describe(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        match self.host.as_str() {
            "" => format!("port {} on every address ({scheme})", self.port),
            host if host.contains(':') => format!("[{host}]:{} ({scheme})", self.port),
            host => format!("{host}:{} ({scheme})", self.port),
        }
    }
}

/// Credential files, counted by type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Credentials {
    /// The types open-ferry serves, by their provider's name.
    pub(crate) served: BTreeMap<String, usize>,
    /// The types it doesn't, by their type.
    pub(crate) not_served: BTreeMap<String, usize>,
    /// `*.json` files that hold no credential: empty, not JSON, too large,
    /// or with no type.
    pub(crate) other_files: usize,
    /// Files that couldn't be read.
    pub(crate) unreadable: usize,
}

impl Credentials {
    pub(crate) fn served_total(&self) -> usize {
        self.served.values().sum()
    }
}

/// What the switch finds, says and is stopped by.
#[derive(Clone, Debug, Default)]
pub(crate) struct Assessment {
    /// The config's full path, when it can be told.
    pub(crate) config: Option<String>,
    /// How the config was found: `-config`, upstream's default, and so on.
    pub(crate) config_from: String,
    pub(crate) working_dir: Option<String>,
    pub(crate) auth_dir: Option<String>,
    pub(crate) listen: Option<Listen>,
    pub(crate) credentials: Credentials,
    pub(crate) claude_sign_ins: usize,
    pub(crate) carries_over: Vec<String>,
    pub(crate) does_not_carry_over: Vec<String>,
    /// `open-ferry check`'s findings that matter for the switch.
    pub(crate) check: Vec<Finding>,
    pub(crate) blockers: Vec<String>,
    pub(crate) warnings: Vec<String>,
    /// The `.env` files to back up: beside the config, and in the working
    /// directory when that is elsewhere.
    pub(crate) env_files: Vec<String>,
    /// The flags of the command line.
    pub(crate) flags: Flags,
    /// Whether the config loaded.
    pub(crate) loaded: bool,
    /// Whether a management password is set in the environment
    /// (`MANAGEMENT_PASSWORD`).
    pub(crate) management_password: bool,
}

/// What `migrate` says of Claude sign-ins.
pub(crate) fn claude_warning(count: usize) -> String {
    format!(
        "{} from CLIProxyAPI's Claude sign-in. open-ferry serves them, but in our testing they get only the Haiku models, because open-ferry doesn't pose as Claude Code. If your clients use Claude Opus or Sonnet through them, they will lose those models. For the rest, use `claude-cli`, which runs your own Claude Code: see docs/claude-subscription.md.",
        plural(count, "credential file", "credential files")
    )
}

pub(crate) fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// The variables upstream reads for its remote stores, cloud mode and Home
/// mode (`cmd/server/main.go`).
const STORE_PREFIXES: [&str; 3] = ["PGSTORE_", "GITSTORE_", "OBJECTSTORE_"];

/// The environment `migrate` could read: the process's, or its service
/// definition's and the files it names, and `.env` in the working
/// directory.
#[derive(Debug, Default)]
struct Environment {
    /// Each variable, with where it came from.
    vars: Vec<(String, String, String)>,
    /// Whether the process's own environment, or its definition's, was
    /// read.
    known: bool,
    /// Where it couldn't read.
    unreadable: Vec<String>,
}

impl Environment {
    /// The value of `name`, the first source's, compared in any case on
    /// Windows.
    fn get(&self, platform: Platform, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(known, _, _)| same_name(platform, known, name))
            .map(|(_, value, _)| value.as_str())
    }
}

fn same_name(platform: Platform, a: &str, b: &str) -> bool {
    match platform {
        Platform::Windows => a.eq_ignore_ascii_case(b),
        Platform::Linux | Platform::MacOs => a == b,
    }
}

/// Reads a `.env`-style file's variables: names and values, never shown.
fn read_env_file(machine: &dyn Machine, path: &str) -> Result<Vec<(String, String)>, String> {
    let data = machine.read(path).map_err(|error| error.to_string())?;
    let vars = dotenv::parse(&data).map_err(|error| error.to_string())?;
    Ok(vars
        .into_iter()
        .map(|(name, value)| {
            (
                String::from_utf8_lossy(&name).into_owned(),
                String::from_utf8_lossy(&value).into_owned(),
            )
        })
        .collect())
}

fn environment(
    machine: &dyn Machine,
    found: &Found,
    working_dir: Option<&str>,
    platform: Platform,
) -> Environment {
    let mut env = Environment::default();
    let push = |env: &mut Environment, vars: &[(String, String)], source: &str| {
        for (name, value) in vars {
            env.vars
                .push((name.clone(), value.clone(), source.to_owned()));
        }
    };
    if let Some(vars) = found
        .process
        .as_ref()
        .and_then(|process| process.env.as_ref())
    {
        push(&mut env, vars, "its environment");
        env.known = true;
    } else if let Some(vars) = &found.definition_env {
        env.known = true;
        // The first of a name is the one that counts. systemd lets a later
        // `EnvironmentFile=` override an earlier one, and the files override
        // `Environment=`: so the last file comes first, and the definition's
        // own variables last. A file that is missing is skipped, as with the
        // `-` prefix.
        for file in found.env_files.iter().rev() {
            match read_env_file(machine, file) {
                Ok(vars) => push(&mut env, &vars, file),
                Err(_) if !machine.exists(file) => {}
                Err(error) => env.unreadable.push(format!("{file} ({error})")),
            }
        }
        push(&mut env, vars, "its service definition");
    }
    if let Some(dir) = working_dir {
        let file = platform.join(dir, ".env");
        if machine.exists(&file) {
            match read_env_file(machine, &file) {
                Ok(vars) => push(&mut env, &vars, &file),
                Err(error) => env.unreadable.push(format!("{file} ({error})")),
            }
        }
    }
    env
}

/// The home directory CLIProxyAPI resolves `~` with, as Go's
/// `os.UserHomeDir` finds it: `USERPROFILE` on Windows, else `HOME`. Its
/// own environment says, when it could be read. A service definition
/// seldom does, as the service manager sets it: then it is a system
/// service's account's home, or else the home of the user whose service or
/// process it is, the one running `migrate`.
fn home_of(context: &Context, found: &Found, env: &Environment) -> Option<String> {
    let platform = context.platform;
    let name = match platform {
        Platform::Windows => "USERPROFILE",
        Platform::Linux | Platform::MacOs => "HOME",
    };
    let set = env
        .vars
        .iter()
        // CLIProxyAPI's own `.env` files don't set the home it resolves
        // `~` with; a service's `EnvironmentFile=` named `x.env` does.
        .filter(|(_, _, source)| !source.ends_with("/.env") && !source.ends_with("\\.env"))
        .find(|(known, _, _)| same_name(platform, known, name))
        .map(|(_, value, _)| value.clone())
        .filter(|value| !value.is_empty());
    let own_env = found
        .process
        .as_ref()
        .is_some_and(|process| process.env.is_some());
    let set = set.or_else(|| system_home(platform, &found.starter));
    if own_env {
        return set;
    }
    set.or_else(|| context.var(name).map(str::to_owned))
}

/// The home of a system service's account, where it has one.
fn system_home(platform: Platform, starter: &Starter) -> Option<String> {
    match (platform, starter) {
        (Platform::Linux, Starter::Systemd { user: false, .. }) => Some("/root".to_owned()),
        (Platform::MacOs, Starter::Launchd { domain, .. }) if domain == "system" => {
            Some("/var/root".to_owned())
        }
        (Platform::Windows, Starter::WindowsService { .. }) => {
            Some(r"C:\Windows\System32\config\systemprofile".to_owned())
        }
        _ => None,
    }
}

/// The auth directory as upstream resolves it (`open_ferry_core`'s
/// `resolve_auth_dir`), with `home` for `~` and a relative path from the
/// working directory.
pub(crate) fn resolve_auth_dir(
    platform: Platform,
    auth_dir: &str,
    home: Option<&str>,
    working_dir: Option<&str>,
) -> Result<String, String> {
    let auth_dir = if auth_dir.is_empty() {
        DEFAULT_AUTH_DIR
    } else {
        auth_dir
    };
    let path = match auth_dir.strip_prefix('~') {
        Some(rest) => {
            let home = home.ok_or_else(|| {
                format!(
                    "its auth-dir, {auth_dir}, starts with ~, and its home directory can't be told"
                )
            })?;
            let rest = rest.trim_start_matches(|c| platform.is_separator(c));
            if rest.is_empty() {
                home.to_owned()
            } else {
                platform.join(home, &rest.replace('\\', "/"))
            }
        }
        None => auth_dir.to_owned(),
    };
    if platform.is_absolute(&path) {
        return Ok(platform.clean(&path));
    }
    let dir = working_dir.ok_or_else(|| {
        format!("its auth-dir, {path}, is relative, and its working directory can't be told")
    })?;
    platform.absolute(dir, &path)
}

/// Whether a value, as YAML decodes it, sets something.
/// Whether an `api-keys` group sets `key`, for the group or for one of
/// its `keys`, where a legacy key's own settings move.
fn holds(group: &AnyValue, key: &str) -> bool {
    let AnyValue::Map(group) = group else {
        return false;
    };
    group.get(key).is_some_and(is_set)
        || matches!(group.get("keys"), Some(AnyValue::Seq(keys)) if keys.iter().any(|entry| {
            matches!(entry, AnyValue::Map(entry) if entry.get(key).is_some_and(is_set))
        }))
}

fn is_set(value: &AnyValue) -> bool {
    match value {
        AnyValue::Null => false,
        AnyValue::Bool(value) => *value,
        AnyValue::Int(value) => *value != 0,
        AnyValue::Uint(value) => *value != 0,
        AnyValue::Float(value) => *value != 0.0,
        AnyValue::Str(value) => !value.trim().is_empty(),
        AnyValue::Seq(items) => !items.is_empty(),
        AnyValue::Map(map) => map.values().any(is_set),
        AnyValue::Time(..) | AnyValue::AnyMap => true,
    }
}

/// The settings open-ferry reads and ignores that the config sets, in the
/// v8 layout's names, or `None` when the file can't be read so.
pub(crate) fn ignored_settings(data: &[u8]) -> Option<Vec<String>> {
    let document = V8Document::migrate(data).ok()?;
    let value = |path: &[&str]| document.value(path).and_then(Result::ok);
    let set = |path: &[&str]| value(path).is_some_and(|value| is_set(&value));
    let mut found = Vec::new();
    // Client impersonation.
    let mut cloaking = Vec::new();
    for (path, shown) in [
        (
            &["upstream", "claude", "header-defaults"][..],
            "upstream.claude.header-defaults",
        ),
        (
            &[
                "oauth",
                "providers",
                "codex",
                "header-defaults",
                "user-agent",
            ][..],
            "oauth.providers.codex.header-defaults.user-agent",
        ),
    ] {
        if set(path) {
            cloaking.push(shown.to_owned());
        }
    }
    if let Some(AnyValue::Map(groups)) = value(&["api-keys"]) {
        for (group, entries) in &groups {
            let AnyValue::Seq(entries) = entries else {
                continue;
            };
            for key in ["cloak", "fingerprint-profile"] {
                if entries.iter().any(|entry| holds(entry, key)) {
                    cloaking.push(format!("{key} in api-keys.{group}"));
                }
            }
        }
    }
    if !cloaking.is_empty() {
        found.push(format!(
            "cloaking, the client impersonation open-ferry doesn't do ({})",
            cloaking.join(", ")
        ));
    }
    for (path, shown) in [
        (&["plugins", "enabled"][..], "plugins (plugins.enabled)"),
        (
            &["observability", "pprof", "enable"][..],
            "pprof (observability.pprof.enable)",
        ),
        (
            &["server", "discovery", "enabled"][..],
            "LAN discovery (server.discovery.enabled)",
        ),
        (
            &["oauth", "providers", "codex", "live-media-relay", "enabled"][..],
            "the Codex live media relay (oauth.providers.codex.live-media-relay.enabled)",
        ),
        (
            &["oauth", "providers", "antigravity"][..],
            "the Antigravity section (oauth.providers.antigravity)",
        ),
        (
            &["oauth", "providers", "devin"][..],
            "the Devin section (oauth.providers.devin)",
        ),
        (
            &["credentials", "concurrency"][..],
            "the per-credential concurrency limits (credentials.concurrency)",
        ),
        (
            &["credentials", "in-flight"][..],
            "the per-credential in-flight limits (credentials.in-flight)",
        ),
    ] {
        if set(path) {
            found.push(shown.to_owned());
        }
    }
    Some(found)
}

/// What a credential file's type means for open-ferry.
#[derive(Debug, PartialEq, Eq)]
enum Served {
    Yes(&'static str),
    Claude,
    /// Meta's sign-in: served only while its access token lasts.
    Meta,
    /// xAI's sign-in: open-ferry serves xAI's API keys only.
    Xai,
    No(String),
}

fn classify(kind: &str) -> Served {
    match kind {
        "codex" => Served::Yes("Codex"),
        "vertex" => Served::Yes("Vertex AI"),
        "claude" => Served::Claude,
        "meta" => Served::Meta,
        "xai" => Served::Xai,
        "gemini" | "gemini-cli" => Served::No("gemini-cli".to_owned()),
        kind if !kind.is_empty()
            && kind.len() <= 32
            && kind
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)) =>
        {
            Served::No(kind.to_owned())
        }
        _ => Served::No("another type".to_owned()),
    }
}

/// The type of a credential file's contents, lower-cased, as the server
/// reads it; `None` for contents that hold none.
fn credential_type(data: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(data).ok()?;
    let kind = value.get("type")?.as_str()?.trim().to_ascii_lowercase();
    (!kind.is_empty()).then_some(kind)
}

/// Counts the credential files in `dir`, as the server finds them: `*.json`
/// directly in it.
fn count_credentials(
    machine: &dyn Machine,
    platform: Platform,
    dir: &str,
) -> Result<(Credentials, usize, usize, usize), String> {
    let entries = machine.list_dir(dir).map_err(|error| error.to_string())?;
    let mut credentials = Credentials::default();
    let (mut claude, mut meta, mut xai) = (0, 0, 0);
    for entry in entries {
        if entry.kind == EntryKind::Dir || !entry.name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }
        let Ok(data) = machine.read(&platform.join(dir, &entry.name)) else {
            credentials.unreadable += 1;
            continue;
        };
        let too_large = u64::try_from(data.len()).unwrap_or(u64::MAX) > MAX_AUTH_FILE_SIZE;
        let Some(kind) = (!too_large).then(|| credential_type(&data)).flatten() else {
            credentials.other_files += 1;
            continue;
        };
        let (name, count) = match classify(&kind) {
            Served::Yes(name) => (name.to_owned(), &mut credentials.served),
            Served::Claude => {
                claude += 1;
                ("Claude".to_owned(), &mut credentials.served)
            }
            Served::Meta => {
                meta += 1;
                ("meta".to_owned(), &mut credentials.not_served)
            }
            Served::Xai => {
                xai += 1;
                ("xai".to_owned(), &mut credentials.not_served)
            }
            Served::No(kind) => (kind, &mut credentials.not_served),
        };
        *count.entry(name).or_default() += 1;
    }
    Ok((credentials, claude, meta, xai))
}

/// Where the proxy listens, from the config's `host`, `port` and `tls`.
fn listen(config: &Config) -> Result<Listen, String> {
    let port = u16::try_from(config.port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            format!(
                "its port, {}, isn't one open-ferry can listen on",
                config.port
            )
        })?;
    let host = config.host.trim().to_owned();
    Ok(Listen {
        probe: probe_address(&host),
        host,
        port,
        tls: config.tls.enable,
    })
}

/// Where to ask whether the proxy answers, for the config's `host`: the
/// loopback address for every address, `None` for a host name.
pub(crate) fn probe_address(host: &str) -> Option<IpAddr> {
    match host.trim().trim_start_matches('[').trim_end_matches(']') {
        "" | "localhost" => Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        host => match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) if ip.is_unspecified() => Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Ok(IpAddr::V6(ip)) if ip.is_unspecified() => Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            Ok(ip) => Some(ip),
            Err(_) => None,
        },
    }
}

/// Whether a finding of `open-ferry check` matters for the switch: its
/// warnings and errors, but not those of the files counted here, nor its
/// address check, since CLIProxyAPI holds the port.
fn matters(finding: &Finding) -> bool {
    if finding.level < Level::Warning {
        return false;
    }
    let check = finding.check.as_str();
    if check == "auth directory" || check == "address" || check.starts_with("credential ") {
        return false;
    }
    if check == "clock" {
        return finding.message.starts_with("the system time");
    }
    true
}

/// Looks over what `found` uses, for a switch of `kind`. `given` is
/// `migrate`'s own `-config`, a full path.
pub(crate) fn assess(
    machine: &mut dyn Machine,
    context: &Context,
    found: &Found,
    kind: Kind,
    given: Option<&str>,
) -> Assessment {
    let platform = context.platform;
    let mut out = Assessment {
        working_dir: found.cwd.clone(),
        flags: discover::read_flags(&found.args),
        ..Assessment::default()
    };
    out.blockers.extend(found.blockers.iter().cloned());
    out.warnings.extend(found.notes.iter().cloned());
    if kind == Kind::Container {
        // Its config and credentials are the container's, in its image or
        // its volumes: open-ferry reads neither, and changes neither.
        out.config_from = "the container's own, which open-ferry doesn't read".to_owned();
        return out;
    }

    // The config.
    let brew = found
        .exe
        .as_deref()
        .and_then(discover::brew_prefix)
        .map(|prefix| format!("{prefix}/etc/cliproxyapi.conf"));
    let (config, from) = match (given, &out.flags.config, brew) {
        (Some(given), _, _) => (Some(given.to_owned()), "migrate's -config"),
        (None, Some(_), _) => (
            discover::config_of(platform, &found.args, found.cwd.as_deref()),
            "its -config",
        ),
        (None, None, Some(brew)) => (Some(brew), "the Homebrew build's default"),
        (None, None, None) => (
            discover::config_of(platform, &[], found.cwd.as_deref()),
            "upstream's default, config.yaml in its working directory",
        ),
    };
    out.config_from = from.to_owned();
    let Some(config) = config else {
        out.blockers.push(
            "Can't tell which config it uses: its working directory can't be read. Name the config with -config."
                .to_owned(),
        );
        return out;
    };
    out.config = Some(config.clone());
    let data = match machine.read(&config) {
        Ok(data) => data,
        Err(error) => {
            out.blockers
                .push(format!("Can't read its config, {config}: {error}"));
            return out;
        }
    };
    let loaded = match Config::load_bytes(&data) {
        Ok(loaded) => {
            out.loaded = true;
            loaded
        }
        Err(error) => {
            out.blockers.push(format!(
                "Its config, {config}, doesn't load in open-ferry: {error}"
            ));
            return out;
        }
    };

    // The environment.
    let env = environment(machine, found, found.cwd.as_deref(), platform);
    if !env.known {
        out.warnings.push(
            "Can't read CLIProxyAPI's environment, so whether it sets PGSTORE_*, GITSTORE_*, OBJECTSTORE_*, DEPLOY or HOME_JWT can't be told, except from .env in its working directory.".to_owned(),
        );
    }
    if found.cwd.is_none() {
        out.warnings.push(
            "Can't read CLIProxyAPI's working directory, so its .env file can't be looked at."
                .to_owned(),
        );
    }
    for file in &env.unreadable {
        out.warnings.push(format!(
            "Can't read {file}, so the variables it sets can't be told."
        ));
    }
    let mut stores: Vec<String> = Vec::new();
    for (name, value, source) in &env.vars {
        let upper = name.to_ascii_uppercase();
        let store = STORE_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
            && !value.trim().is_empty();
        let cloud = same_name(platform, name, "DEPLOY") && value == "cloud";
        let home = upper == "HOME_JWT" && !value.trim().is_empty();
        if store || cloud || home {
            let shown = if cloud {
                "DEPLOY=cloud".to_owned()
            } else {
                name.clone()
            };
            let entry = format!("{shown} (in {source})");
            if !stores.contains(&entry) {
                stores.push(entry);
            }
        }
    }
    if out.flags.names.iter().any(|name| name == "home-jwt") {
        stores.push("-home-jwt (in its command line)".to_owned());
    }
    if !stores.is_empty() {
        out.blockers.push(format!(
            "It uses remote storage, cloud mode or Home mode, which open-ferry doesn't have: {}. open-ferry needs the config and credentials as local files.",
            stores.join(", ")
        ));
    }

    // The auth directory.
    let home = home_of(context, found, &env);
    match resolve_auth_dir(
        platform,
        &loaded.auth_dir,
        home.as_deref(),
        found.cwd.as_deref(),
    ) {
        Ok(dir) => {
            out.auth_dir = Some(dir.clone());
            match count_credentials(machine, platform, &dir) {
                Ok((credentials, claude, meta, xai)) => {
                    out.credentials = credentials;
                    out.claude_sign_ins = claude;
                    if meta > 0 {
                        out.does_not_carry_over.push(format!(
                            "{} from Meta's sign-in: open-ferry can't refresh them, so it serves one only until its access token expires. Add a meta-api-key entry instead.",
                            plural(meta, "credential file", "credential files")
                        ));
                    }
                    if xai > 0 {
                        out.does_not_carry_over.push(format!(
                            "{} from xAI's sign-in: open-ferry serves xAI through API keys only. Add an xai-api-key entry instead.",
                            plural(xai, "credential file", "credential files")
                        ));
                    }
                }
                Err(_) if !machine.exists(&dir) => {
                    out.warnings.push(format!(
                        "Its auth directory, {dir}, doesn't exist, so it has no credential files."
                    ));
                }
                Err(error) => out
                    .blockers
                    .push(format!("Its auth directory, {dir}, can't be read: {error}")),
            }
        }
        Err(error) => out
            .blockers
            .push(format!("Can't find its auth directory: {error}.")),
    }
    if out.credentials.unreadable > 0 {
        out.blockers.push(format!(
            "{} in its auth directory can't be read, so it can't be backed up.",
            plural(out.credentials.unreadable, "file", "files")
        ));
    }

    // The listening address.
    match listen(&loaded) {
        Ok(listen) => {
            if listen.probe.is_none() {
                out.warnings.push(format!(
                    "Its host, {}, is a name, not an address, so the switch can't check that open-ferry answers there.",
                    listen.host
                ));
            }
            out.listen = Some(listen);
        }
        Err(error) => out.blockers.push(format!("{error}.")),
    }

    // What carries over.
    let management_password = env
        .get(platform, "MANAGEMENT_PASSWORD")
        .is_some_and(|value| !value.is_empty());
    out.management_password = management_password;
    if let Some(listen) = &out.listen {
        out.carries_over
            .push(format!("The address and port: {}", listen.describe()));
    }
    out.carries_over.push(format!(
        "The client keys: {} in api-keys",
        plural(loaded.api_keys.len(), "key", "keys")
    ));
    if out.credentials.served_total() > 0 {
        let served: Vec<String> = out
            .credentials
            .served
            .iter()
            .map(|(name, count)| format!("{count} {name}"))
            .collect();
        out.carries_over.push(format!(
            "The credential files open-ferry serves: {}",
            served.join(", ")
        ));
    }
    out.carries_over.push(
        "The API keys and providers in the config that open-ferry serves (`open-ferry check` lists any it can't)"
            .to_owned(),
    );
    match (
        !loaded.remote_management.secret_key.is_empty(),
        management_password,
    ) {
        (true, _) => out
            .carries_over
            .push("The management key (management.secret-key)".to_owned()),
        (false, true) => out
            .carries_over
            .push("The management key (MANAGEMENT_PASSWORD)".to_owned()),
        (false, false) => {}
    }
    let writable = env
        .get(platform, "WRITABLE_PATH")
        .filter(|value| !value.is_empty());
    let logs = match (writable, &out.working_dir, &out.auth_dir) {
        (Some(path), _, _) => platform.join(path, "logs"),
        (None, Some(dir), Some(auth)) => format!(
            "{}, or {} if that can't be written to",
            platform.join(dir, "logs"),
            platform.join(auth, "logs")
        ),
        (None, Some(dir), None) => platform.join(dir, "logs"),
        (None, None, _) => "the same place, by the same rule".to_owned(),
    };
    out.carries_over.push(format!("The logs directory: {logs}"));
    out.carries_over.push(
        "The config and the auth directory themselves, used in place: nothing is copied or rewritten"
            .to_owned(),
    );

    // What doesn't.
    let not_served: Vec<String> = out
        .credentials
        .not_served
        .iter()
        .filter(|(name, _)| *name != "meta" && *name != "xai")
        .map(|(name, count)| format!("{count} {name}"))
        .collect();
    if !not_served.is_empty() {
        out.does_not_carry_over.push(format!(
            "Credential files of providers open-ferry doesn't serve: {}. They stay in the auth directory, unused.",
            not_served.join(", ")
        ));
    }
    if out.credentials.other_files > 0 {
        out.does_not_carry_over.push(format!(
            "{} in the auth directory that {} no credential open-ferry reads",
            plural(
                out.credentials.other_files,
                "other .json file",
                "other .json files"
            ),
            if out.credentials.other_files == 1 {
                "holds"
            } else {
                "hold"
            }
        ));
    }
    match ignored_settings(&data) {
        Some(ignored) if !ignored.is_empty() => out.does_not_carry_over.push(format!(
            "Settings open-ferry reads and ignores: {}",
            ignored.join("; ")
        )),
        Some(_) => {}
        None => out.warnings.push(
            "Can't tell which settings open-ferry would ignore: the config doesn't read in the v8 layout.".to_owned(),
        ),
    }
    flag_notes(&mut out, found, kind);
    if let (Kind::Service(_), Some(vars)) = (kind, &found.definition_env) {
        let mut names: Vec<&str> = vars.iter().map(|(name, _)| name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        if !names.is_empty() || !found.env_files.is_empty() {
            let mut what = Vec::new();
            if !names.is_empty() {
                what.push(names.join(", "));
            }
            what.extend(found.env_files.iter().cloned());
            let dir = out
                .working_dir
                .as_deref()
                .unwrap_or("its working directory");
            out.does_not_carry_over.push(format!(
                "The variables its service sets ({}): open-ferry's service doesn't set them. Put those it needs in .env in {dir}.",
                what.join(", ")
            ));
        }
    }

    // `open-ferry check`'s findings.
    out.check = machine
        .check(&config, out.working_dir.as_deref(), management_password)
        .into_iter()
        .filter(matters)
        .collect();

    // The `.env` files to back up.
    let mut env_files = Vec::new();
    if let Some(dir) = platform.parent(&config) {
        env_files.push(platform.join(&dir, ".env"));
    }
    if let Some(dir) = &out.working_dir {
        let file = platform.join(dir, ".env");
        if !env_files
            .iter()
            .any(|known| discover::same_path(platform, known, &file))
        {
            env_files.push(file);
        }
    }
    out.env_files = env_files
        .into_iter()
        .filter(|file| machine.exists(file))
        .collect();
    out
}

/// What the start command's flags mean for the switch.
fn flag_notes(out: &mut Assessment, found: &Found, kind: Kind) {
    let flags = &out.flags;
    match kind {
        Kind::Service(_) => {
            let dropped: Vec<String> = flags
                .names
                .iter()
                .filter(|name| *name != "config")
                .map(|name| format!("-{name}"))
                .collect();
            if !dropped.is_empty() {
                out.does_not_carry_over.push(format!(
                    "The start command's {}: open-ferry's service runs with -config only.",
                    dropped.join(", ")
                ));
            }
            if flags.names.iter().any(|name| name == "password") {
                out.does_not_carry_over.push(
                    "-password: for a management key, set management.secret-key in the config, or MANAGEMENT_PASSWORD in .env in the working directory.".to_owned(),
                );
            }
            if let Some(unknown) = &flags.unknown {
                out.warnings.push(format!(
                    "Its command line has {unknown}, which CLIProxyAPI doesn't take either; the rest of it isn't read."
                ));
            }
        }
        Kind::DropIn => {
            let unsupported: Vec<String> = flags
                .names
                .iter()
                .filter(|name| !crate::flags::takes(name))
                .map(|name| format!("-{name}"))
                .collect();
            if !unsupported.is_empty() {
                out.blockers.push(format!(
                    "Its start command has {}, which open-ferry doesn't take: it would stop with its usage. Take {} out of what starts CLIProxyAPI first.",
                    unsupported.join(", "),
                    if unsupported.len() == 1 { "it" } else { "them" }
                ));
            }
            if let Some(unknown) = &flags.unknown {
                out.blockers.push(format!(
                    "Its start command has {unknown}, which open-ferry doesn't take: it would stop with its usage."
                ));
            }
            let subcommands = [
                crate::init::NAME,
                crate::check::NAME,
                crate::os_service::NAME,
                super::NAME,
            ];
            if let Some(first) = found.args.first()
                && subcommands.contains(&first.as_str())
            {
                out.blockers.push(format!(
                    "Its start command's first argument is {first}, which open-ferry would run as its subcommand."
                ));
            }
            if !flags.names.is_empty() {
                let names: Vec<String> = flags
                    .names
                    .iter()
                    .filter(|name| crate::flags::takes(name))
                    .map(|name| format!("-{name}"))
                    .collect();
                if !names.is_empty() {
                    out.carries_over.push(format!(
                        "The start command and its flags ({}), which open-ferry takes as CLIProxyAPI does",
                        names.join(", ")
                    ));
                }
            }
        }
        Kind::Container => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the auth directory resolves as the server resolves
    // it, from CLIProxyAPI's home and working directory.
    #[test]
    fn resolves_the_auth_directory() {
        let linux = Platform::Linux;
        assert_eq!(
            resolve_auth_dir(linux, "", Some("/home/me"), None),
            Ok("/home/me/.cli-proxy-api".to_owned())
        );
        assert_eq!(
            resolve_auth_dir(linux, "~", Some("/home/me"), None),
            Ok("/home/me".to_owned())
        );
        assert_eq!(
            resolve_auth_dir(linux, "auths", None, Some("/srv/cpa")),
            Ok("/srv/cpa/auths".to_owned())
        );
        assert!(resolve_auth_dir(linux, "~/x", None, None).is_err());
        assert_eq!(
            resolve_auth_dir(
                Platform::Windows,
                r"~\.cli-proxy-api",
                Some(r"C:\Users\me"),
                None
            ),
            Ok(r"C:\Users\me\.cli-proxy-api".to_owned())
        );
    }

    // Not upstream's: types are counted under open-ferry's names, and an
    // odd type isn't shown as it is.
    #[test]
    fn classifies_credential_types() {
        assert_eq!(classify("codex"), Served::Yes("Codex"));
        assert_eq!(classify("claude"), Served::Claude);
        assert_eq!(classify("gemini"), Served::No("gemini-cli".to_owned()));
        assert_eq!(
            classify("antigravity"),
            Served::No("antigravity".to_owned())
        );
        assert_eq!(
            classify("someone@example.com"),
            Served::No("another type".to_owned())
        );
        assert_eq!(
            credential_type(br#"{"type":" Codex "}"#),
            Some("codex".to_owned())
        );
        assert_eq!(credential_type(br#"{"email":"x"}"#), None);
        assert_eq!(credential_type(b"not json"), None);
    }

    // Not upstream's: the settings open-ferry ignores, in both layouts.
    #[test]
    fn finds_ignored_settings() {
        let legacy = b"port: 8317\npprof:\n  enable: true\n  addr: 127.0.0.1:6060\ndiscovery:\n  enabled: false\nclaude-header-defaults:\n  user-agent: x\nclaude-api-key:\n  - api-key: k\n    cloak:\n      mode: always\nantigravity-signature-cache-enabled: true\n";
        let ignored = ignored_settings(legacy).unwrap_or_default();
        assert_eq!(
            ignored,
            [
                "cloaking, the client impersonation open-ferry doesn't do (upstream.claude.header-defaults, cloak in api-keys.claude)",
                "pprof (observability.pprof.enable)",
                "the Antigravity section (oauth.providers.antigravity)",
            ]
        );
        assert_eq!(ignored_settings(b"port: 8317\n"), Some(Vec::new()));
    }

    // Not upstream's: an unspecified host is checked on the loopback
    // address.
    #[test]
    fn listens_where_the_config_says() {
        let mut config = Config::default();
        config.port = 8317;
        assert_eq!(
            listen(&config).map(|listen| listen.probe),
            Ok(Some(IpAddr::V4(Ipv4Addr::LOCALHOST)))
        );
        config.host = "::".to_owned();
        assert_eq!(
            listen(&config).map(|listen| listen.probe),
            Ok(Some(IpAddr::V6(Ipv6Addr::LOCALHOST)))
        );
        config.host = "proxy.lan".to_owned();
        assert_eq!(listen(&config).map(|listen| listen.probe), Ok(None));
        config.port = 70000;
        assert!(listen(&config).is_err());
    }
}
