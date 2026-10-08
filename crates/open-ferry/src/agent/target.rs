//! Where a command finds the config, the management key, and the server
//! running for the config.
//!
//! - **The config**: `--config`, else `config.yaml` in the working
//!   directory, else the installed config (see [`crate::installed`]),
//!   whichever exists first.
//! - **The management key**, in this order: the config's
//!   `management.secret-key` when it is plain, not a bcrypt hash;
//!   `MANAGEMENT_PASSWORD`; the first line of the key file
//!   `--management-key-file` names, else `OPEN_FERRY_MANAGEMENT_KEY_FILE`.
//!   Never one from the command line, where other users and the shell's
//!   history could read it.
//! - **The server**: at the management address, `management.separate-address`
//!   when set, else the proxy's `server.host` and `server.port`, over
//!   `https` when `tls.enable` is set. Only loopback is tried: an empty
//!   host, `0.0.0.0` or `::` as loopback, `localhost` as `127.0.0.1` and
//!   `::1`. A host name isn't looked up and an address that isn't loopback
//!   isn't tried, so the key is never sent off this machine. A server is
//!   running when something accepts a connection there within a second;
//!   it is reached when it answers `GET /v0/management/debug` with the key.
//!   It is used only when it runs this config: the file it runs, as
//!   `GET /v0/management/config.yaml` gives it, must hold the same bytes as
//!   the config here. A server that runs another config (one on the same
//!   port, from another file) is never changed or asked about.
//!
//! A server that refuses the key stops the command: each refusal counts
//! towards the server's ban of five failed attempts in thirty minutes.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::Method;
use open_ferry_core::config::{Config, V8Document};
use serde::Serialize;

use super::api::{Remote, answer_failure};
use super::values::load_error;
use super::{Context, Failure};

/// The environment variable that names a management key file.
pub(crate) const KEY_FILE_VAR: &str = "OPEN_FERRY_MANAGEMENT_KEY_FILE";

/// The environment variable that sets a management key.
pub(crate) const PASSWORD_VAR: &str = "MANAGEMENT_PASSWORD";

/// How long a connection to the server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// The most of a key file read.
const KEY_FILE_LIMIT: usize = 4096;

/// The route that tells whether the key is good.
const PROBE: &str = "/v0/management/debug";

/// The route that gives the config file the server runs, as it is.
const CONFIG_ROUTE: &str = "/v0/management/config.yaml";

/// How many times the server's config and this one are compared before
/// they count as different: a write between the two reads changes one.
const CONFIG_READS: usize = 3;

/// The config file `flag` names, else `config.yaml` in `working_dir`,
/// else `installed`, whichever exists first.
pub(crate) fn config_path(
    flag: &str,
    working_dir: &Path,
    installed: Result<PathBuf, String>,
) -> Result<PathBuf, Failure> {
    if !flag.is_empty() {
        return Ok(working_dir.join(flag));
    }
    let local = working_dir.join("config.yaml");
    if local.is_file() {
        return Ok(local);
    }
    let hint = "pass --config with your config's path, or write one with `open-ferry init`";
    match installed {
        Ok(path) if path.is_file() => Ok(path),
        Ok(path) => Err(Failure::new(
            "not_found",
            format!(
                "no config found: neither {} nor {} exists",
                local.display(),
                path.display()
            ),
        )
        .hint(hint)),
        Err(error) => Err(Failure::new(
            "not_found",
            format!(
                "no config found: {} doesn't exist, and {error}",
                local.display()
            ),
        )
        .hint(hint)),
    }
}

/// What the environment says of the management key.
#[derive(Clone, Default)]
pub(crate) struct Env {
    /// `MANAGEMENT_PASSWORD`, when set and not blank.
    pub(crate) password: Option<String>,
    /// The key file `OPEN_FERRY_MANAGEMENT_KEY_FILE` names.
    pub(crate) key_file: Option<PathBuf>,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("password", &self.password.as_ref().map(|_| ".."))
            .field("key_file", &self.key_file)
            .finish()
    }
}

impl Env {
    /// The process's.
    pub(crate) fn current() -> Self {
        Self {
            password: std::env::var_os(PASSWORD_VAR)
                .map(|value| value.to_string_lossy().into_owned())
                .filter(|value| !value.trim().is_empty()),
            key_file: std::env::var_os(KEY_FILE_VAR)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        }
    }
}

/// Where the management key came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum KeySource {
    /// The config's plain `management.secret-key`.
    #[serde(rename = "config")]
    Config,
    /// `MANAGEMENT_PASSWORD`.
    #[serde(rename = "MANAGEMENT_PASSWORD")]
    Password,
    /// A key file.
    #[serde(rename = "key-file")]
    File,
}

impl KeySource {
    /// For people.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Config => "the config's management.secret-key",
            Self::Password => "MANAGEMENT_PASSWORD",
            Self::File => "the management key file",
        }
    }
}

/// A management key, which `Debug` doesn't show.
#[derive(Clone)]
pub(crate) struct Key {
    pub(crate) secret: String,
    pub(crate) source: KeySource,
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// The management key, in order: `plain`, the config's plain key;
/// `MANAGEMENT_PASSWORD`; the key file `flag` names, else the one the
/// environment names. `None` when there is none. A key file that can't be
/// read, or is empty, is an error.
pub(crate) fn find_key(
    plain: Option<String>,
    env: &Env,
    flag: Option<&Path>,
) -> Result<Option<Key>, Failure> {
    if let Some(secret) = plain.filter(|key| !key.trim().is_empty()) {
        return Ok(Some(Key {
            secret,
            source: KeySource::Config,
        }));
    }
    if let Some(secret) = env.password.clone() {
        return Ok(Some(Key {
            secret,
            source: KeySource::Password,
        }));
    }
    let Some(path) = flag.or(env.key_file.as_deref()) else {
        return Ok(None);
    };
    read_key_file(path).map(|secret| {
        Some(Key {
            secret,
            source: KeySource::File,
        })
    })
}

/// The first line of the key file at `path`, trimmed.
pub(crate) fn read_key_file(path: &Path) -> Result<String, Failure> {
    let unreadable = |why: String| {
        Failure::new(
            "failed",
            format!(
                "can't read the management key file {}: {why}",
                path.display()
            ),
        )
    };
    let data = std::fs::read(path).map_err(|error| unreadable(error.to_string()))?;
    let data = data.get(..KEY_FILE_LIMIT).unwrap_or(&data);
    let text = std::str::from_utf8(data).map_err(|_| unreadable("it isn't UTF-8".to_owned()))?;
    let key = text.lines().next().unwrap_or_default().trim();
    if key.is_empty() {
        return Err(unreadable("it is empty".to_owned()));
    }
    Ok(key.to_owned())
}

/// The config and the server running for it.
#[derive(Debug)]
pub(crate) struct Target {
    /// The config file's bytes, as read: what the server runs, when it
    /// was reached.
    pub(crate) data: Vec<u8>,
    /// The config, or why it doesn't load, as [`load_error`] says it.
    pub(crate) config: Result<Config, String>,
    /// The proxy's root URL, without a path, when the config sets a port.
    pub(crate) proxy_url: Option<String>,
    /// The management API's root URL.
    pub(crate) management_url: Option<String>,
    /// How the server was reached.
    pub(crate) reach: Reach,
}

/// Whether, and how, the server running for the config was reached.
#[derive(Debug)]
pub(crate) enum Reach {
    /// Nothing answers at the management address, or there is none to try:
    /// why.
    NotRunning(String),
    /// A server answers, but there is no management key to call it with.
    NoKey,
    /// A server answers, but its management API is off: it has no key.
    ManagementOff,
    /// A server answers, and takes the key.
    Running(Server),
    /// A server answers, but refused the key, or answered something else.
    Refused(Failure),
    /// A server answers and takes the key, but runs another config: the
    /// failure that says so.
    OtherConfig(Failure),
}

/// A server reached with the management key.
#[derive(Debug)]
pub(crate) struct Server {
    pub(crate) remote: Remote,
    /// Its version, as its answers give it.
    pub(crate) version: Option<String>,
    /// Where the key came from.
    pub(crate) key_source: KeySource,
}

impl Target {
    /// The server, for a command that needs it: a failure saying why when
    /// it wasn't reached.
    pub(crate) fn server(&self) -> Result<&Server, Failure> {
        match &self.reach {
            Reach::Running(server) => Ok(server),
            Reach::Refused(failure) | Reach::OtherConfig(failure) => Err(failure.clone()),
            Reach::NotRunning(why) => Err(Failure::new(
                "not_running",
                format!("no open-ferry server is running for this config: {why}"),
            )
            .hint(
                "start it with `open-ferry --config <your config>`, or as a service (`open-ferry service install`)",
            )),
            Reach::NoKey => Err(Failure::new(
                "no_management_key",
                "a server is running, but there is no management key to call it with",
            )
            .hint(KEY_HINT)),
            Reach::ManagementOff => Err(Failure::new(
                "management_off",
                "a server is running, but its management API is off: it has no management key",
            )
            .hint(
                "set management.secret-key (`open-ferry config set management.secret-key --from-stdin --yes`) or MANAGEMENT_PASSWORD for the server, then use the same key here",
            )),
        }
    }
}

/// Where the management key is looked for.
pub(crate) const KEY_HINT: &str = "the key is looked for in the config's management.secret-key (when it isn't a bcrypt hash), then MANAGEMENT_PASSWORD, then the file --management-key-file or OPEN_FERRY_MANAGEMENT_KEY_FILE names; it is never taken from the command line";

/// Reads the config at `ctx.path` and looks for the server running for
/// it.
pub(crate) async fn probe(ctx: &Context) -> Result<Target, Failure> {
    let data = std::fs::read(&ctx.path).map_err(|error| {
        Failure::new(
            "not_found",
            format!("can't read {}: {error}", ctx.path.display()),
        )
    })?;
    let config = Config::load_bytes(&data).map_err(|error| load_error(&error));
    let Ok(loaded) = &config else {
        return Ok(Target {
            data,
            config,
            proxy_url: None,
            management_url: None,
            reach: Reach::NotRunning("the config doesn't load".to_owned()),
        });
    };
    let proxy = proxy_address(loaded);
    let management = management_address(loaded);
    let proxy_url = proxy
        .as_ref()
        .map(|(host, port)| root_url(host, *port, loaded.tls.enable));
    let management_url = management
        .as_ref()
        .map(|(host, port)| root_url(host, *port, loaded.tls.enable));
    let plain = V8Document::migrate(&data)
        .ok()
        .and_then(|document| document.plain_management_key());
    let mut data = data;
    let reach = match management {
        None => Reach::NotRunning("the config sets no server.port".to_owned()),
        Some((host, port)) => {
            let found = reach(ctx, &host, port, loaded.tls.enable, plain).await?;
            match found {
                Reach::Running(server) => match runs_this_config(ctx, &server, &mut data).await? {
                    None => Reach::Running(server),
                    Some(other) => other,
                },
                other => other,
            }
        }
    };
    Ok(Target {
        data,
        config,
        proxy_url,
        management_url,
        reach,
    })
}

/// Whether a server answers at `host`:`port`, and takes the key.
async fn reach(
    ctx: &Context,
    host: &str,
    port: u16,
    tls: bool,
    plain: Option<String>,
) -> Result<Reach, Failure> {
    let targets = match loopback_targets(host) {
        Ok(targets) => targets,
        Err(why) => return Ok(Reach::NotRunning(why)),
    };
    let Some(address) = connect_any(&targets, port).await else {
        return Ok(Reach::NotRunning(format!(
            "nothing answers at {}",
            root_url(host, port, tls)
        )));
    };
    let Some(key) = find_key(plain, &ctx.env, ctx.key_file.as_deref())? else {
        return Ok(Reach::NoKey);
    };
    let url = root_url(&address.ip().to_string(), port, tls);
    let remote = Remote::new(&url, &key.secret);
    let reply = match remote.send(Method::GET, PROBE, None).await {
        Ok(reply) => reply,
        Err(failure) => return Ok(Reach::Refused(failure)),
    };
    Ok(match reply.status {
        200 => Reach::Running(Server {
            remote,
            version: reply.version,
            key_source: key.source,
        }),
        404 if reply.body.iter().all(u8::is_ascii_whitespace) => Reach::ManagementOff,
        401 | 403 => Reach::Refused(refused(&key, &reply.body)),
        status => Reach::Refused(answer_failure(status, &reply.body).hint(format!(
            "is it open-ferry that answers at {url}? Its management API answered {status} to {PROBE}"
        ))),
    })
}

/// Whether `server` runs the config at `ctx.path`, whose bytes are
/// `data`: `None` when the file it runs holds the same bytes, with `data`
/// read again when this file changed while they were compared; else what
/// it is instead.
async fn runs_this_config(
    ctx: &Context,
    server: &Server,
    data: &mut Vec<u8>,
) -> Result<Option<Reach>, Failure> {
    let url = server.remote.url();
    for _ in 0..CONFIG_READS {
        let reply = server.remote.send(Method::GET, CONFIG_ROUTE, None).await?;
        match reply.status {
            200 => {}
            404 => {
                return Ok(Some(Reach::OtherConfig(
                    Failure::new(
                        "other_config",
                        format!(
                            "a server at {url} takes this config's management key but runs no config file, so it isn't the one running {}; nothing was changed",
                            ctx.path.display()
                        ),
                    )
                    .hint(OTHER_CONFIG_HINT),
                )));
            }
            status => {
                return Ok(Some(Reach::Refused(answer_failure(status, &reply.body).hint(
                    format!(
                        "is it open-ferry that answers at {url}? Its management API answered {status} to {CONFIG_ROUTE}"
                    ),
                ))));
            }
        }
        if reply.body == *data {
            return Ok(None);
        }
        let again = std::fs::read(&ctx.path).map_err(|error| {
            Failure::new(
                "not_found",
                format!("can't read {}: {error}", ctx.path.display()),
            )
        })?;
        let same = reply.body == again;
        *data = again;
        if same {
            return Ok(None);
        }
    }
    Ok(Some(Reach::OtherConfig(
        Failure::new(
            "other_config",
            format!(
                "a server at {url} runs another config, not {}; nothing was changed",
                ctx.path.display()
            ),
        )
        .hint(OTHER_CONFIG_HINT),
    )))
}

/// What to do about a server that runs another config.
const OTHER_CONFIG_HINT: &str = "pass --config with the path of the config that server runs, or give this config a server.port (or management.separate-address) nothing else uses";

/// The failure for a key the server refused.
fn refused(key: &Key, body: &[u8]) -> Failure {
    let why = super::api::error_text(body).unwrap_or_else(|| "refused".to_owned());
    Failure::new(
        "unauthorized",
        format!(
            "the server refused the management key from {}: {why}",
            key.source.describe()
        ),
    )
    .hint(format!(
        "{KEY_HINT}. Every refused key counts: after five in thirty minutes the server refuses this address for thirty minutes"
    ))
}

/// The proxy's host and port, when the config sets a port.
pub(crate) fn proxy_address(config: &Config) -> Option<(String, u16)> {
    let port = u16::try_from(config.port).ok().filter(|port| *port != 0)?;
    Some((config.host.trim().to_owned(), port))
}

/// The management API's host and port: `management.separate-address`,
/// else the proxy's.
pub(crate) fn management_address(config: &Config) -> Option<(String, u16)> {
    match config.remote_management.separate_address() {
        Ok(Some(address)) => Some((address.host, address.port)),
        _ => proxy_address(config),
    }
}

/// The root URL a client on this machine reaches `host`:`port` at:
/// loopback for every interface, and IPv6 in brackets.
pub(crate) fn root_url(host: &str, port: u16, tls: bool) -> String {
    let url = crate::init::dashboard_url(host, i64::from(port), tls);
    url.strip_suffix("/dashboard/").unwrap_or(&url).to_owned()
}

/// The loopback addresses `host` covers, or why it isn't tried.
fn loopback_targets(host: &str) -> Result<Vec<IpAddr>, String> {
    let bare = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    if bare.is_empty() || bare.eq_ignore_ascii_case("localhost") {
        return Ok(vec![v4, v6]);
    }
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if ip.is_unspecified() => Ok(vec![v4]),
        Ok(IpAddr::V6(ip)) if ip.is_unspecified() => Ok(vec![v6, v4]),
        Ok(ip) if ip.is_loopback() => Ok(vec![ip]),
        Ok(_) => Err(format!(
            "its address {host} isn't loopback, and the management key is only sent over loopback"
        )),
        Err(_) => Err(format!(
            "its host {host} is a name, which isn't looked up; set server.host to an address"
        )),
    }
}

/// The first of `targets` that accepts a connection on `port`.
async fn connect_any(targets: &[IpAddr], port: u16) -> Option<SocketAddr> {
    for ip in targets {
        let address = SocketAddr::new(*ip, port);
        let connect = tokio::net::TcpStream::connect(address);
        if let Ok(Ok(_)) = tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            return Some(address);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: --config wins, then config.yaml in the working
    // directory, then the installed config; with none, the answer says so.
    #[test]
    fn finds_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("installed.yaml");
        let local = dir.path().join("config.yaml");
        let missing = || Ok(dir.path().join("nope.yaml"));

        assert_eq!(
            config_path("other.yaml", dir.path(), missing()).unwrap(),
            dir.path().join("other.yaml")
        );
        let failure = config_path("", dir.path(), missing()).unwrap_err();
        assert_eq!(failure.error, "not_found");
        assert!(failure.hint.unwrap().contains("--config"));
        let failure = config_path("", dir.path(), Err("HOME isn't set".to_owned())).unwrap_err();
        assert!(failure.message.contains("HOME isn't set"));

        std::fs::write(&installed, "").unwrap();
        assert_eq!(
            config_path("", dir.path(), Ok(installed.clone())).unwrap(),
            installed
        );
        std::fs::write(&local, "").unwrap();
        assert_eq!(config_path("", dir.path(), Ok(installed)).unwrap(), local);
    }

    // Not upstream's: the key is the config's plain one, then
    // MANAGEMENT_PASSWORD, then the key file the flag names, then the one
    // the environment names.
    #[test]
    fn looks_for_the_key_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let flag_file = dir.path().join("flag-key");
        let env_file = dir.path().join("env-key");
        std::fs::write(&flag_file, "from-flag-file\nignored\n").unwrap();
        std::fs::write(&env_file, "  from-env-file  ").unwrap();
        let env = Env {
            password: Some("from-password".to_owned()),
            key_file: Some(env_file.clone()),
        };
        let found = |plain: Option<&str>, env: &Env, flag: Option<&Path>| {
            find_key(plain.map(str::to_owned), env, flag)
                .unwrap()
                .map(|key| (key.secret, key.source))
        };
        assert_eq!(
            found(Some("from-config"), &env, Some(&flag_file)),
            Some(("from-config".to_owned(), KeySource::Config))
        );
        assert_eq!(
            found(None, &env, Some(&flag_file)),
            Some(("from-password".to_owned(), KeySource::Password))
        );
        let no_password = Env {
            password: None,
            ..env.clone()
        };
        assert_eq!(
            found(Some(" "), &no_password, Some(&flag_file)),
            Some(("from-flag-file".to_owned(), KeySource::File))
        );
        assert_eq!(
            found(None, &no_password, None),
            Some(("from-env-file".to_owned(), KeySource::File))
        );
        assert_eq!(found(None, &Env::default(), None), None);

        std::fs::write(&flag_file, "\n").unwrap();
        let failure = find_key(None, &no_password, Some(&flag_file)).unwrap_err();
        assert!(failure.message.contains("it is empty"));
        let failure = find_key(None, &no_password, Some(&dir.path().join("nope"))).unwrap_err();
        assert!(
            failure
                .message
                .contains("can't read the management key file")
        );
        // The key never shows in Debug.
        let key = find_key(Some("hidden-key".to_owned()), &env, None)
            .unwrap()
            .unwrap();
        assert!(!format!("{key:?}{env:?}").contains("hidden-key"));
        assert!(!format!("{env:?}").contains("from-password"));
    }

    // Not upstream's: only loopback is tried.
    #[test]
    fn tries_loopback_only() {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(loopback_targets("").unwrap(), vec![v4, v6]);
        assert_eq!(loopback_targets("localhost").unwrap(), vec![v4, v6]);
        assert_eq!(loopback_targets("0.0.0.0").unwrap(), vec![v4]);
        assert_eq!(loopback_targets("::").unwrap(), vec![v6, v4]);
        assert_eq!(loopback_targets("[::1]").unwrap(), vec![v6]);
        assert_eq!(
            loopback_targets("127.0.0.2").unwrap(),
            vec!["127.0.0.2".parse::<IpAddr>().unwrap()]
        );
        assert!(
            loopback_targets("192.0.2.1")
                .unwrap_err()
                .contains("isn't loopback")
        );
        assert!(
            loopback_targets("proxy.example.com")
                .unwrap_err()
                .contains("is a name")
        );
        assert_eq!(root_url("", 8, false), "http://127.0.0.1:8");
        assert_eq!(root_url("::1", 8, true), "https://[::1]:8");
    }
}
