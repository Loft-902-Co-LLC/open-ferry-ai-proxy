//! `open-ferry init`: writes a starting config, so a new user never edits
//! YAML to begin.
//!
//! The config is the `config.example.yaml` built into the binary, every
//! comment kept, with these values changed:
//! - `access.api-keys`: one new client key in place of the three examples;
//! - `management.secret-key`: a new management key;
//! - `server.host`: `127.0.0.1`, or `-host` (empty for every interface);
//! - `server.port`: `-port`, or the template's.
//!
//! Each key is 32 bytes from the operating system's random generator: the
//! client key as the dashboard's "new client key" makes one, `sk-` and the
//! bytes in unpadded base64url, and the management key as 64 hex digits.
//! Both are safe in YAML and in a shell.
//!
//! The config goes to `-config`, else to the installed config path (see
//! [`installed`]), and its directory is made. An existing
//! file is replaced only with `-force`, and then kept as `<file name>.bak`.
//! The file is written by the config writer
//! ([`write_as_is`](open_ferry_core::config::save::write_as_is)): checked
//! to load, atomically, and readable by its owner only on Unix (0600). On
//! Windows it keeps the permissions it inherits from its directory, which
//! under `%APPDATA%` are the user profile's.
//!
//! It prints the path, the two keys, once, the dashboard's address, and the
//! commands that start the proxy and check the setup. It exits with 0 when
//! the config is written, 1 when it isn't, and 2 for bad usage.
//!
//! Upstream has no `init` (see [`flags`]).

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use open_ferry_core::config::{Config, save};
use rand::TryRng as _;
use rand::rngs::SysRng;

use crate::flags::{self, Definition, FlagError, Kind};
use crate::installed;

/// The subcommand's name: the first argument that runs it.
pub const NAME: &str = "init";

/// The config template, built into the binary.
const TEMPLATE: &str = include_str!("../../../config.example.yaml");

/// The host the config listens on without `-host`.
const DEFAULT_HOST: &str = "127.0.0.1";

/// How many random bytes each key holds.
const KEY_BYTES: usize = 32;

/// What the command line asks for.
struct Options {
    config: String,
    host: String,
    port: Option<i64>,
    force: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config: String::new(),
            host: DEFAULT_HOST.to_owned(),
            port: None,
            force: false,
        }
    }
}

/// The flags, sorted by name.
const DEFINITIONS: [Definition<Options>; 4] = [
    Definition {
        name: "config",
        usage: "Write the config to this path (default: the installed config path, shown below)",
        kind: Kind::String(|options, value| options.config = value),
    },
    Definition {
        name: "force",
        usage: "Replace an existing config, keeping it as <file name>.bak",
        kind: Kind::Bool(|options, value| options.force = value),
    },
    Definition {
        name: "host",
        usage: "Listen on this host (default 127.0.0.1; empty for every interface)",
        kind: Kind::String(|options, value| options.host = value),
    },
    Definition {
        name: "port",
        usage: "Listen on this port (default 8317, the template's)",
        kind: Kind::Int(|options, value| options.port = Some(value)),
    },
];

/// The usage text.
fn usage(program: &str) -> String {
    let mut out = format!(
        "Usage: {program} {NAME} [flags]\n\n\
         Writes a starting config: config.example.yaml with a new client key, a new\n\
         management key, and the proxy listening on 127.0.0.1. An existing config is\n\
         replaced only with -force.\n\nFlags:\n"
    );
    flags::write_defaults(&mut out, &DEFINITIONS, &[]);
    match installed::config_path() {
        Ok(path) => {
            let _ = writeln!(out, "\nThe installed config path is {}", path.display());
        }
        Err(error) => {
            let _ = writeln!(out, "\nThere is no installed config path: {error}");
        }
    }
    out
}

/// Runs `open-ferry init` with `args`, the arguments after `init`.
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
    let listen = match Listen::from_options(&options) {
        Ok(listen) => listen,
        Err(message) => return usage_error(program, &message),
    };
    let path = if options.config.is_empty() {
        match installed::config_path() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("{NAME}: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        PathBuf::from(&options.config)
    };
    match init(&path, &listen, options.force) {
        Ok(written) => {
            print!("{}", report(program, &written));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{NAME}: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Shows `message` and the usage, for bad usage.
fn usage_error(program: &str, message: &str) -> ExitCode {
    eprintln!("{message}");
    eprint!("{}", usage(program));
    ExitCode::from(2)
}

/// Where the config listens.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Listen {
    /// The host, possibly empty.
    host: String,
    /// The port, or `None` for the template's.
    port: Option<u16>,
}

impl Listen {
    /// `-host` and `-port`, checked: a host name or IP address without
    /// brackets or a port, so it needs no quoting in YAML, and a port from 1
    /// to 65535.
    fn from_options(options: &Options) -> Result<Self, String> {
        let host = options.host.trim();
        let allowed =
            |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%');
        if !host.chars().all(allowed) {
            return Err(format!(
                "invalid value {:?} for flag -host: give a host name or an IP address, \
                 without brackets or a port",
                options.host
            ));
        }
        let port = match options.port {
            None => None,
            Some(port) => Some(
                u16::try_from(port)
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| {
                        format!("invalid value \"{port}\" for flag -port: give 1 to 65535")
                    })?,
            ),
        };
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }
}

/// What [`init`] wrote. It has no `Debug`, so that nothing prints its keys
/// by accident.
struct Written {
    /// The config file.
    path: PathBuf,
    /// The previous file's backup, when there was one.
    backup: Option<PathBuf>,
    /// The new client key.
    client_key: String,
    /// The new management key.
    management_key: String,
    /// The host the config listens on.
    host: String,
    /// The port it listens on.
    port: i64,
}

/// Writes a new config at `path`, replacing a file there only when `force`.
fn init(path: &Path, listen: &Listen, force: bool) -> Result<Written, String> {
    let client_key = client_key(&random_bytes()?);
    let management_key = management_key(&random_bytes()?);
    let text = render(&client_key, &management_key, listen)?;
    let config = verify(&text, &client_key, &management_key, listen)?;
    let backup = write(path, &text, force)?;
    Ok(Written {
        path: path.to_path_buf(),
        backup,
        client_key,
        management_key,
        host: config.host,
        port: config.port,
    })
}

/// [`KEY_BYTES`] bytes from the operating system's random generator.
pub(crate) fn random_bytes() -> Result<[u8; KEY_BYTES], String> {
    let mut bytes = [0; KEY_BYTES];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|error| format!("the system's random generator failed: {error}"))?;
    Ok(bytes)
}

/// A client key, as the dashboard makes one: `sk-` and `bytes` in unpadded
/// base64url.
pub(crate) fn client_key(bytes: &[u8]) -> String {
    format!("sk-{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// A management key: `bytes` in lowercase hex.
fn management_key(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The template with the keys and `listen` set. Each line set is found by
/// its section and its exact text, once; a template that has moved them is
/// an error, which the tests catch.
fn render(client_key: &str, management_key: &str, listen: &Listen) -> Result<String, String> {
    let mut out = String::with_capacity(TEMPLATE.len() + 2 * KEY_BYTES);
    let mut section = "";
    let (mut host, mut port, mut secret, mut keys) = (0, 0, 0, 0);
    let mut in_keys = false;
    for line in TEMPLATE.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if in_keys {
            if text.starts_with("    - ") {
                continue;
            }
            in_keys = false;
        }
        if !text.is_empty() && !text.starts_with([' ', '#']) {
            section = text.strip_suffix(':').unwrap_or_default();
        }
        match (section, text) {
            ("server", "  host: \"\"") => {
                host += 1;
                let _ = writeln!(out, "  host: \"{}\"", listen.host);
            }
            ("server", text) if text.starts_with("  port: ") => {
                port += 1;
                match listen.port {
                    Some(value) => {
                        let _ = writeln!(out, "  port: {value}");
                    }
                    None => out.push_str(line),
                }
            }
            ("management", "  secret-key: \"\"") => {
                secret += 1;
                let _ = writeln!(out, "  secret-key: \"{management_key}\"");
            }
            ("access", "  api-keys:") => {
                keys += 1;
                in_keys = true;
                out.push_str(line);
                let _ = writeln!(out, "    - \"{client_key}\"");
            }
            _ => out.push_str(line),
        }
    }
    if [host, port, secret, keys] != [1; 4] {
        return Err(
            "the built-in config template doesn't have the lines init sets; this is a bug".into(),
        );
    }
    Ok(out)
}

/// Loads `text` as the server would, and checks that it holds the values
/// [`render`] set and isn't in safe mode.
fn verify(
    text: &str,
    client_key: &str,
    management_key: &str,
    listen: &Listen,
) -> Result<Config, String> {
    let config = Config::load_bytes(text.as_bytes())
        .map_err(|error| format!("the new config doesn't load: {error}"))?;
    let holds = config.api_keys == [client_key]
        && config.remote_management.secret_key == management_key
        && config.host == listen.host
        && listen
            .port
            .is_none_or(|port| config.port == i64::from(port))
        && !config.has_example_api_keys();
    if !holds {
        return Err("the new config doesn't hold the values init set; this is a bug".into());
    }
    Ok(config)
}

/// Writes `text` at `path`, making its directory, and returns the backup's
/// path when a file was there. A file there is an error unless `force`.
fn write(path: &Path, text: &str, force: bool) -> Result<Option<PathBuf>, String> {
    let backup = backup_path(path);
    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if existing.is_some() && !force {
        return Err(format!(
            "{} already exists; pass -force to replace it (the old file is kept as {})",
            path.display(),
            backup.display()
        ));
    }
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        create_dir(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    }
    if let Some(metadata) = &existing {
        restrict(path, metadata).map_err(|error| format!("chmod {}: {error}", path.display()))?;
    }
    save::write_as_is(path, text.as_bytes()).map_err(|error| error.to_string())?;
    Ok(existing.map(|_| backup))
}

/// Where the config writer keeps the file it replaces: `<file name>.bak`.
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// Makes `dir` and its parents; on Unix those it makes are readable by
/// their owner only.
fn create_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Makes the file being replaced readable by its owner only, on Unix, as
/// the config writer gives the new file and the backup its permissions.
#[cfg(unix)]
fn restrict(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    if !metadata.is_file() {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _metadata: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

/// The dashboard's address for a server on `host` and `port`: loopback
/// for every interface, and IPv6 in brackets.
pub fn dashboard_url(host: &str, port: i64, tls: bool) -> String {
    let host = match host.trim() {
        "" | "0.0.0.0" | "::" => "127.0.0.1",
        host => host,
    };
    let scheme = if tls { "https" } else { "http" };
    if host.contains(':') {
        format!("{scheme}://[{host}]:{port}/dashboard/")
    } else {
        format!("{scheme}://{host}:{port}/dashboard/")
    }
}

/// What `init` prints once the config is written.
fn report(program: &str, written: &Written) -> String {
    let path = quote(&written.path.display().to_string());
    let program = quote_program(program);
    let mut out = String::new();
    let _ = writeln!(out, "Wrote a new config to {}", written.path.display());
    if let Some(backup) = &written.backup {
        let _ = writeln!(out, "The old config is kept as {}", backup.display());
    }
    out.push_str(if cfg!(unix) {
        "Only you can read it (0600).\n"
    } else {
        "It has its folder's permissions: under %APPDATA%, only you, administrators and SYSTEM can read it.\n"
    });
    if written.host.is_empty() {
        out.push_str("The proxy listens on every interface, so other machines can reach it.\n");
    }
    let _ = write!(
        out,
        "\nClient key:     {}\nManagement key: {}\n\n\
         These keys aren't shown again; they are kept in the config file.\n\
         Clients send the client key as their API key.\n\
         The management key signs you in to the dashboard.\n\n\
         Dashboard: {}\n\n\
         Start the proxy with this config:\n  {program} -config {path}\n\
         Check the setup first:\n  {program} check -config {path}\n",
        written.client_key,
        written.management_key,
        dashboard_url(&written.host, written.port, false),
    );
    out
}

/// `text` quoted for a shell when it needs it.
fn quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '\\' | '.' | '_' | '-' | ':'));
    if plain {
        text.to_owned()
    } else if cfg!(windows) {
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// The program quoted for a shell; on Windows a quoted program is run with
/// PowerShell's `&`.
fn quote_program(program: &str) -> String {
    let quoted = quote(program);
    if cfg!(windows) && quoted != program {
        format!("& {quoted}")
    } else {
        quoted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listen(host: &str, port: Option<u16>) -> Listen {
        Listen {
            host: host.into(),
            port,
        }
    }

    fn written_or_panic(result: Result<Written, String>) -> Written {
        match result {
            Ok(written) => written,
            Err(error) => panic!("{error}"),
        }
    }

    fn is_client_key(key: &str) -> bool {
        key.strip_prefix("sk-").is_some_and(|rest| {
            rest.len() == 43
                && rest
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
    }

    fn is_management_key(key: &str) -> bool {
        key.len() == 64
            && key
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }

    // Not upstream's: the config is the template with four values set,
    // every comment kept, and it loads outside safe mode.
    #[test]
    fn writes_the_template_with_new_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("config.yaml");
        let written = written_or_panic(init(&path, &listen(DEFAULT_HOST, None), false));
        assert!(written.backup.is_none());
        assert!(is_client_key(&written.client_key));
        assert!(is_management_key(&written.management_key));

        let text = fs::read_to_string(&path).unwrap();
        let expected = TEMPLATE
            .replacen("\n  host: \"\"\n", "\n  host: \"127.0.0.1\"\n", 1)
            .replacen(
                "\n  secret-key: \"\"\n",
                &format!("\n  secret-key: \"{}\"\n", written.management_key),
                1,
            )
            .replacen(
                "\n    - \"your-api-key-1\"\n    - \"your-api-key-2\"\n    - \"your-api-key-3\"\n",
                &format!("\n    - \"{}\"\n", written.client_key),
                1,
            );
        assert!(
            text == expected,
            "the config isn't the template with the new values"
        );

        let config = Config::load(&path).unwrap();
        assert!(config.api_keys == [written.client_key.as_str()]);
        assert!(config.remote_management.secret_key == written.management_key);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8317);
        assert_eq!(written.port, 8317);
        assert!(!config.has_example_api_keys());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    // Not upstream's: each run makes new keys.
    #[test]
    fn makes_new_keys_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let one = written_or_panic(init(&dir.path().join("1.yaml"), &listen("", None), false));
        let two = written_or_panic(init(&dir.path().join("2.yaml"), &listen("", None), false));
        assert!(one.client_key != two.client_key);
        assert!(one.management_key != two.management_key);
        assert!(one.client_key != one.management_key);
        assert_eq!(
            client_key(&[0xfb; 32]),
            format!("sk-{}-_s", "-_v7".repeat(10))
        );
        assert_eq!(management_key(&[0x0f, 0xa0]), "0fa0");
    }

    // Not upstream's: an existing file stays unless -force, which keeps
    // it as `<file name>.bak`.
    #[test]
    fn replaces_a_config_only_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "# mine\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let Err(error) = init(&path, &listen(DEFAULT_HOST, None), false) else {
            panic!("replaced a config without -force");
        };
        assert!(error.contains("already exists; pass -force"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "# mine\n");
        assert!(!dir.path().join("config.yaml.bak").exists());

        let written = written_or_panic(init(&path, &listen(DEFAULT_HOST, None), true));
        let backup = dir.path().join("config.yaml.bak");
        assert_eq!(written.backup.as_deref(), Some(backup.as_path()));
        assert_eq!(fs::read_to_string(&backup).unwrap(), "# mine\n");
        assert!(Config::load(&path).unwrap().api_keys == [written.client_key.as_str()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for file in [&path, &backup] {
                let mode = fs::metadata(file).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{}", file.display());
            }
        }
    }

    // Not upstream's: -host and -port, and the dashboard's address.
    #[test]
    fn sets_the_host_and_port() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let written = written_or_panic(init(&path, &listen("::1", Some(18_080)), false));
        let config = Config::load(&path).unwrap();
        assert_eq!((config.host.as_str(), config.port), ("::1", 18_080));
        assert_eq!(
            dashboard_url(&written.host, written.port, false),
            "http://[::1]:18080/dashboard/"
        );

        let path = dir.path().join("every.yaml");
        written_or_panic(init(&path, &listen("", None), false));
        let config = Config::load(&path).unwrap();
        assert_eq!((config.host.as_str(), config.port), ("", 8317));
        assert_eq!(
            dashboard_url("", 8317, true),
            "https://127.0.0.1:8317/dashboard/"
        );
        assert_eq!(
            dashboard_url("proxy.local", 1, false),
            "http://proxy.local:1/dashboard/"
        );
    }

    // Not upstream's: -host and -port are checked before anything is
    // written.
    #[test]
    fn turns_down_a_bad_host_or_port() {
        let options = |host: &str, port: Option<i64>| Options {
            host: host.into(),
            port,
            ..Options::default()
        };
        assert_eq!(
            Listen::from_options(&Options::default()),
            Ok(listen("127.0.0.1", None))
        );
        assert_eq!(
            Listen::from_options(&options(" fe80::1%eth0 ", Some(65_535))),
            Ok(listen("fe80::1%eth0", Some(65_535)))
        );
        for host in ["[::1]", "a b", "x\"y", "h:1/", "#"] {
            assert!(
                Listen::from_options(&options(host, None)).is_err(),
                "{host}"
            );
        }
        for port in [0, -1, 65_536] {
            let Err(error) = Listen::from_options(&options("", Some(port))) else {
                panic!("took port {port}");
            };
            assert!(error.contains("-port"), "{error}");
        }
    }

    // Not upstream's: the report names the file, shows each key once, and
    // says how to start; its shape is checked, never printed.
    #[test]
    fn reports_the_keys_once_and_how_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "").unwrap();
        let written = written_or_panic(init(&path, &listen(DEFAULT_HOST, Some(18_081)), true));
        let out = report("open-ferry", &written);
        let lines: Vec<&str> = out.lines().collect();
        let shown = path.display().to_string();
        assert!(lines[0] == format!("Wrote a new config to {shown}"));
        assert!(lines[1].starts_with("The old config is kept as "));
        assert!(out.matches(&written.client_key).count() == 1);
        assert!(out.matches(&written.management_key).count() == 1);
        let client = lines
            .iter()
            .find_map(|line| line.strip_prefix("Client key:     "));
        assert!(client.is_some_and(is_client_key));
        let management = lines
            .iter()
            .find_map(|line| line.strip_prefix("Management key: "));
        assert!(management.is_some_and(is_management_key));
        assert!(out.contains("These keys aren't shown again"));
        assert!(out.contains("The management key signs you in to the dashboard."));
        assert!(out.contains("\nDashboard: http://127.0.0.1:18081/dashboard/\n"));
        let config = quote(&shown);
        assert!(out.contains(&format!("\n  open-ferry -config {config}\n")));
        assert!(out.contains(&format!("\n  open-ferry check -config {config}\n")));
        assert!(!out.contains("every interface"));
    }

    // Not upstream's: paths and programs are quoted for a shell only when
    // they need it.
    #[test]
    fn quotes_for_a_shell() {
        assert_eq!(
            quote("/home/u/.config/open-ferry/config.yaml"),
            "/home/u/.config/open-ferry/config.yaml"
        );
        assert_eq!(quote(r"C:\Users\u\config.yaml"), r"C:\Users\u\config.yaml");
        if cfg!(windows) {
            assert_eq!(quote(r"C:\My Files\c.yaml"), r#""C:\My Files\c.yaml""#);
            assert_eq!(
                quote_program(r"C:\My Files\open-ferry.exe"),
                r#"& "C:\My Files\open-ferry.exe""#
            );
        } else {
            assert_eq!(quote("/a b/it's"), r"'/a b/it'\''s'");
            assert_eq!(quote_program("/a b/open-ferry"), "'/a b/open-ferry'");
        }
        assert_eq!(quote_program("open-ferry"), "open-ferry");
    }

    // Not upstream's: the usage lists the flags, and bad usage exits with 2
    // before anything is written.
    #[test]
    fn lists_its_flags_and_turns_down_bad_usage() {
        let usage = usage("open-ferry");
        assert!(usage.starts_with("Usage: open-ferry init [flags]\n"));
        for flag in ["-config string", "-force\n", "-host string", "-port int"] {
            assert!(usage.contains(&format!("\n  {flag}")), "{flag}");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml").display().to_string();
        let run = |args: &[&str]| main("open-ferry", args.iter().map(|arg| (*arg).to_owned()));
        assert_eq!(run(&["-config", &path, "-port", "0"]), ExitCode::from(2));
        assert_eq!(run(&["-config", &path, "-host", "a b"]), ExitCode::from(2));
        assert_eq!(run(&["-config", &path, "extra"]), ExitCode::from(2));
        assert_eq!(run(&["-config", &path, "-nope"]), ExitCode::from(2));
        assert_eq!(run(&["-h"]), ExitCode::SUCCESS);
        assert!(!dir.path().join("config.yaml").exists());
    }
}
