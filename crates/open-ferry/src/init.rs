//! `open-ferry init`: writes a starting config, so a new user never edits
//! YAML to begin.
//!
//! The config is the `config.example.yaml` built into the binary, every
//! comment kept, with these values changed:
//! - `access.api-keys`: one new client key in place of the three examples;
//! - `management.secret-key`: a new management key;
//! - `server.host`: `127.0.0.1`, or `-host` (empty for every interface);
//! - `server.port`: `-port`, or the template's;
//! - `self-update`: uncommented, so the file says that open-ferry updates
//!   itself (`mode: auto`) and, in the comment above it, how to turn that
//!   off (`open-ferry update -mode off`).
//!
//! Each key is 32 bytes from the operating system's random generator: the
//! client key as the dashboard's "new client key" makes one, `sk-` and the
//! bytes in unpadded base64url, and the management key as 64 hex digits.
//! Both are safe in YAML and in a shell.
//!
//! The config goes to `-config`, else to the installed config path (see
//! [`installed`]), and its directory is made. An existing
//! file is replaced only with `-force`, and then kept as `<file name>.bak`.
//! With `-system`, the default is the machine config. Its data directory must
//! be trusted; on Windows a missing one is created with a protected DACL.
//! Its auth directory replaces the template's, and existing configs stay.
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
use crate::scope;

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
    system: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config: String::new(),
            host: DEFAULT_HOST.to_owned(),
            port: None,
            force: false,
            system: false,
        }
    }
}

/// The flags, sorted by name.
const DEFINITIONS: [Definition<Options>; 5] = [
    Definition {
        name: "config",
        usage: "Write the config to this path (default: the installed config path, or the machine config with -system)",
        kind: Kind::String(|options, value| options.config = value),
    },
    Definition {
        name: "force",
        usage: "Replace an existing config, keeping it as <file name>.bak (except with -system)",
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
    Definition {
        name: "system",
        usage: "Use the machine's config and auth directory; needs root or an administrator (creates a missing protected machine directory on Windows)",
        kind: Kind::Bool(|options, value| options.system = value),
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
    let path = match config_path(&options, &scope::Host) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{NAME}: {error}");
            return ExitCode::FAILURE;
        }
    };
    match init_scoped(
        &path,
        &listen,
        options.force,
        options.system,
        &mut scope::Host,
    ) {
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

/// Explicit config, else the config for the requested scope.
fn config_path(options: &Options, probe: &dyn scope::Probe) -> Result<PathBuf, String> {
    if !options.config.is_empty() {
        return Ok(PathBuf::from(&options.config));
    }
    if options.system {
        return scope::machine_config(probe)?
            .map(PathBuf::from)
            .ok_or_else(|| {
                "machine-wide installs on macOS come in a later version; run init without -system"
                    .to_owned()
            });
    }
    installed::config_path()
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
    init_config(path, listen, force, None)
}

/// Checks the machine scope before writing; existing machine configs stay
/// as they are, even with `-force`, so their credentials never move.
fn init_scoped(
    path: &Path,
    listen: &Listen,
    force: bool,
    system: bool,
    probe: &mut dyn scope::Probe,
) -> Result<Written, String> {
    if !system {
        return init(path, listen, force);
    }
    if probe.platform() == crate::os_service::Platform::Windows
        && let Some(root) = scope::machine_root(probe)?
        && probe.state(&root) == scope::State::Missing
    {
        scope::provision(probe).map_err(|refusal| refusal.to_string())?;
    }
    let auth_dir = scope::machine_auth_dir(probe).map_err(|refusal| refusal.to_string())?;
    if let Some(root) = scope::machine_root(probe)? {
        crate::os_service::authorize_auth_dir_with(
            probe.platform(),
            &root,
            &auth_dir,
            &|path| probe.state(path),
            &|path| probe.trusted_dir(path),
            &|path| probe.canonical(path),
        )?;
    }
    if fs::symlink_metadata(path).is_ok() {
        return Err(format!(
            "{} already exists; init -system leaves an existing config and its credentials alone",
            path.display()
        ));
    }
    init_config(path, listen, false, Some(&auth_dir))
}

/// Writes the template with an optional machine auth directory.
fn init_config(
    path: &Path,
    listen: &Listen,
    force: bool,
    auth_dir: Option<&str>,
) -> Result<Written, String> {
    let client_key = client_key(&random_bytes()?);
    let management_key = management_key(&random_bytes()?);
    let text = render(&client_key, &management_key, listen)?;
    let text = match auth_dir {
        Some(auth_dir) => {
            let old = "  auth-dir: \"~/.cli-proxy-api\"";
            if text.lines().filter(|line| *line == old).count() != 1 {
                return Err("the built-in config template doesn't have the auth-dir init sets; this is a bug".into());
            }
            // JSON strings are YAML strings too, including escaped Windows
            // separators, quotes and control characters.
            let quoted = serde_json::to_string(auth_dir).map_err(|error| error.to_string())?;
            text.replacen(old, &format!("  auth-dir: {quoted}"), 1)
        }
        None => text,
    };
    let config = verify(&text, &client_key, &management_key, listen)?;
    if auth_dir.is_some_and(|auth_dir| config.auth_dir != auth_dir) {
        return Err("the new config doesn't hold the auth-dir init set; this is a bug".into());
    }
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
    let (mut updates, mut in_updates) = (0, false);
    let mut in_keys = false;
    for line in TEMPLATE.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        if in_keys {
            if text.starts_with("    - ") {
                continue;
            }
            in_keys = false;
        }
        // The commented `self-update` block, uncommented.
        if text == "# self-update:" {
            updates += 1;
            in_updates = true;
            out.push_str(line.get(2..).unwrap_or_default());
            continue;
        }
        if in_updates {
            if let Some(rest) = line.strip_prefix("#   ") {
                if rest.starts_with("mode: auto ") || rest.starts_with("check-every: 6h ") {
                    updates += 1;
                }
                let _ = write!(out, "  {rest}");
                continue;
            }
            in_updates = false;
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
    if [host, port, secret, keys, updates] != [1, 1, 1, 1, 3] {
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
        && !config.has_example_api_keys()
        && config.self_update.mode == "auto";
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
    use crate::os_service::Platform;
    use open_ferry_update::receipt::{Receipt, ReceiptError};

    struct Fake {
        platform: Platform,
        privileged: Result<bool, String>,
        state: scope::State,
        trust: Result<(), String>,
        auth_trust: Result<(), String>,
        program_data: String,
        created: Vec<String>,
        create_error: Option<io::ErrorKind>,
    }

    impl Fake {
        fn new(platform: Platform) -> Self {
            Self {
                platform,
                privileged: Ok(true),
                state: scope::State::Directory,
                trust: Ok(()),
                auth_trust: Ok(()),
                program_data: r"C:\ProgramData".into(),
                created: Vec::new(),
                create_error: None,
            }
        }
    }

    impl scope::Probe for Fake {
        fn platform(&self) -> Platform {
            self.platform
        }
        fn privileged(&self) -> Result<bool, String> {
            self.privileged.clone()
        }
        fn program_data(&self) -> Result<String, String> {
            Ok(self.program_data.clone())
        }
        fn state(&self, _path: &str) -> scope::State {
            self.state.clone()
        }
        fn entries(&self, _dir: &str, _limit: usize) -> io::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn trusted_dir(&self, path: &str) -> Result<(), String> {
            if path.ends_with("auth") {
                return self.auth_trust.clone();
            }
            self.trust.clone()
        }
        fn account_dir(&self) -> Result<String, String> {
            panic!("init does not look up account records")
        }
        fn trusted_file(&self, _path: &str) -> Result<(), String> {
            panic!("the fake machine directory is empty")
        }
        fn readable_file(&self, _path: &str) -> io::Result<()> {
            Ok(())
        }

        fn receipt(&self, _path: &str) -> Result<Option<Receipt>, ReceiptError> {
            panic!("init does not read receipts")
        }
        fn canonical(&self, path: &str) -> io::Result<String> {
            Ok(path.to_owned())
        }
        fn create_machine_dir(&mut self, path: &str) -> io::Result<()> {
            if let Some(error) = self.create_error {
                return Err(error.into());
            }
            self.created.push(path.to_owned());
            self.state = scope::State::Directory;
            Ok(())
        }
    }

    #[test]
    fn writes_the_machine_auth_directory_for_linux_and_windows() {
        for (platform, expected) in [
            (Platform::Linux, "/var/lib/open-ferry/auth"),
            (Platform::Windows, r"C:\ProgramData\open-ferry\auth"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.yaml");
            written_or_panic(init_scoped(
                &path,
                &listen(DEFAULT_HOST, None),
                false,
                true,
                &mut Fake::new(platform),
            ));
            let config = Config::load(&path).unwrap();
            assert_eq!(config.auth_dir, expected);
            // Equivalent to os_service::check_auth_dir: a nonempty auth-dir
            // passes precisely when it does not start with `~`.
            assert!(!config.auth_dir.is_empty());
            assert!(!config.auth_dir.starts_with('~'));
            let root = scope::machine_root(&Fake::new(platform)).unwrap().unwrap();
            assert_eq!(
                scope::check_auth_dir_placement(platform, &root, &config.auth_dir),
                Ok(())
            );
        }
    }

    #[test]
    fn system_init_checks_auth_directory_trust_before_writing_the_config() {
        for platform in [Platform::Linux, Platform::Windows] {
            let dir = crate::scope::runtime_test_probe::scratch_dir();
            let path = dir.path().join("config.yaml");
            let mut fake = Fake::new(platform);
            fake.auth_trust = Err("the auth directory is writable by another account".to_owned());
            let error =
                match init_scoped(&path, &listen(DEFAULT_HOST, None), false, true, &mut fake) {
                    Ok(_) => panic!("an untrusted auth directory must refuse"),
                    Err(error) => error,
                };
            assert!(error.contains("Refusing the auth directory"));
            // The refused directory is the default, so it isn't suggested.
            assert!(!error.contains("set auth-dir"), "{error}");
            assert!(!path.exists());
        }
    }

    #[test]
    fn system_init_defaults_to_the_machine_config_and_keeps_an_explicit_path() {
        let mut options = Options {
            system: true,
            ..Options::default()
        };
        for (platform, expected) in [
            (Platform::Linux, "/etc/open-ferry/config.yaml"),
            (Platform::Windows, r"C:\ProgramData\open-ferry\config.yaml"),
        ] {
            assert_eq!(
                config_path(&options, &Fake::new(platform)).unwrap(),
                Path::new(expected)
            );
        }
        assert!(
            config_path(&options, &Fake::new(Platform::MacOs))
                .unwrap_err()
                .contains("macOS")
        );
        options.config = "explicit.yaml".to_owned();
        assert_eq!(
            config_path(&options, &Fake::new(Platform::Windows)).unwrap(),
            Path::new("explicit.yaml")
        );
    }

    #[test]
    fn windows_system_init_creates_a_missing_machine_directory_through_the_scope_probe() {
        for privileged in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.yaml");
            let mut probe = Fake::new(Platform::Windows);
            probe.state = scope::State::Missing;
            probe.privileged = Ok(privileged);
            let result = init_scoped(&path, &listen(DEFAULT_HOST, None), false, true, &mut probe);
            if privileged {
                written_or_panic(result);
                assert_eq!(probe.created, [r"C:\ProgramData\open-ferry"]);
            } else {
                assert!(result.is_err());
                assert!(probe.created.is_empty());
                assert!(!path.exists());
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut probe = Fake::new(Platform::Windows);
        probe.state = scope::State::Missing;
        probe.create_error = Some(io::ErrorKind::PermissionDenied);
        assert!(init_scoped(&path, &listen(DEFAULT_HOST, None), false, true, &mut probe).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn quotes_the_windows_machine_auth_directory_as_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut fake = Fake::new(Platform::Windows);
        fake.program_data = r#"C:\Data with "quotes" and 'apostrophes'"#.into();
        written_or_panic(init_scoped(
            &path,
            &listen(DEFAULT_HOST, None),
            false,
            true,
            &mut fake,
        ));
        assert_eq!(
            Config::load(&path).unwrap().auth_dir,
            format!(r"{}\open-ferry\auth", fake.program_data)
        );
    }

    #[test]
    fn refuses_system_init_with_the_scope_refusal_before_writing() {
        let mut cases = vec![Fake::new(Platform::MacOs)];
        for platform in [Platform::Linux, Platform::Windows] {
            let mut unprivileged = Fake::new(platform);
            unprivileged.privileged = Ok(false);
            cases.push(unprivileged);
            let mut unknown = Fake::new(platform);
            unknown.privileged = Err("no token".into());
            cases.push(unknown);
            for state in [
                scope::State::Missing,
                scope::State::Other,
                scope::State::Inaccessible("access denied".into()),
            ] {
                let mut fake = Fake::new(platform);
                if platform == Platform::Windows && state == scope::State::Missing {
                    continue;
                }
                fake.state = state;
                cases.push(fake);
            }
            let mut untrusted = Fake::new(platform);
            untrusted.trust = Err("machine directory is writable by another account".into());
            cases.push(untrusted);
        }
        for mut fake in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("new").join("config.yaml");
            let expected = scope::machine_auth_dir(&fake).unwrap_err().to_string();
            let Err(error) =
                init_scoped(&path, &listen(DEFAULT_HOST, None), false, true, &mut fake)
            else {
                panic!("wrote a config despite a scope refusal");
            };
            assert_eq!(error, expected);
            assert!(!path.parent().unwrap().exists());
        }
    }

    #[test]
    fn system_init_never_rewrites_an_existing_config_even_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "oauth:\n  auth-dir: /old/credentials\n").unwrap();
        for force in [false, true] {
            let Err(error) = init_scoped(
                &path,
                &listen(DEFAULT_HOST, None),
                force,
                true,
                &mut Fake::new(Platform::Linux),
            ) else {
                panic!("system init rewrote an existing config");
            };
            assert!(
                error.contains("init -system leaves an existing config and its credentials alone")
            );
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                "oauth:\n  auth-dir: /old/credentials\n"
            );
            assert!(!backup_path(&path).exists());
        }
    }

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
            )
            .replacen("\n# self-update:\n#   mode:", "\nself-update:\n  mode:", 1)
            .replacen("\n#   check-every:", "\n  check-every:", 1);
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
        assert_eq!(config.self_update.mode, "auto");
        assert!(config.self_update.channel.is_empty());
        assert!(!text.contains("channel:"));
        assert_eq!(
            config.self_update.channel(),
            open_ferry_core::config::SelfUpdateChannel::Stable
        );
        assert_eq!(config.self_update.check_every, "6h");
        assert!(text.contains("open-ferry update -mode off"), "{text}");
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
        for flag in [
            "-config string",
            "-force\n",
            "-host string",
            "-port int",
            "-system\n",
        ] {
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
