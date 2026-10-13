//! `open-ferry service`: runs open-ferry in the background, started by the
//! operating system's own service manager.
//!
//! - `open-ferry service install [-config PATH] [-system] [-dry-run]` writes
//!   a service definition that runs the binary that ran it, with `-config`
//!   and the config's absolute path, and starts it. The service manager
//!   starts it again when it fails. Without `-config` the config is the
//!   installed config path (see [`installed`]). `install` refuses a config
//!   that doesn't exist or doesn't load, and a service that is already
//!   installed.
//! - `open-ferry service uninstall [-system] [-dry-run]` stops the service
//!   and removes what `install` made, and nothing else: the config and the
//!   auth directory stay.
//! - `open-ferry service status [-system]` says where the service is
//!   defined, and shows what the service manager says about it.
//!
//! A user service runs supervised when the binary it starts can update
//! itself (it has an install receipt; see [`Updating`]): the definition
//! starts `-supervise -config <path> -data-dir <path>`, with the data
//! directory pinned so that the service and `open-ferry update` agree on it.
//! A system service also runs supervised for an explicit machine install on
//! Linux or Windows. Otherwise it runs the server plainly. `install` says
//! which, and why. A user install, and a supervised system install, write
//! `service.json` in the data directory, removed after a complete uninstall.
//! A user record also names the directory when the definition can't.
//!
//! With `-dry-run`, `install` and `uninstall` print what they would write
//! and run, and change nothing. `service` exits with 0 when it did what it
//! was asked, 1 when it didn't, and 2 for bad usage.
//!
//! By default the service is the user's, and runs as them, so the config's
//! `~` paths, the auth directory's among them, are theirs: a systemd user
//! unit on Linux (see [`systemd`]), a launchd agent on macOS (see
//! [`launchd`]), and on Windows a scheduled task at the user's logon (see
//! [`windows`]). With `-system` it is the machine's, started at boot as
//! root: a systemd system unit, a launchd daemon, or a Windows service
//! running as LocalSystem. As `~` would then be that account's home,
//! `install -system` refuses a config whose auth directory is under `~`;
//! and as the service runs with root's rights, it refuses a binary or a
//! config that anyone but root (on Windows, the administrators) can change.
//! Installing or removing a system service needs root, or an administrator.
//!
//! Every command `service` runs, and every file it reads or writes, goes
//! through [`System`], so the tests drive it with a recording fake and never
//! touch a real service manager. The definitions are made by pure
//! functions, for every platform whichever one `service` runs on, and are
//! tested against their text.
//!
//! Upstream can't install itself as a service, and has no `service` (see
//! [`flags`]).

pub(crate) mod hint;
mod launchd;
#[cfg(windows)]
mod run;
mod state;
mod systemd;
#[cfg(test)]
mod tests;
mod windows;
pub(crate) use state::{
    ServiceState, launchd_state, service_state, systemd_state, windows_service_state,
};
pub(crate) use windows::{SERVICE_RUNNING, service_state_number, user_identity};

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use open_ferry_core::config::{Config, DEFAULT_AUTH_DIR};
use open_ferry_supervisor::client::{self, StopOutcome};
use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
use open_ferry_supervisor::timings::STOP_BUDGET;
use open_ferry_supervisor::trust;
use open_ferry_update::DataDir;
use open_ferry_update::install::{self as update_install, Install};
use open_ferry_update::receipt::Receipt;

use crate::flags::{self, FlagError, Kind};
use crate::{installed, scope};

/// The subcommand's name: the first argument that runs it.
pub const NAME: &str = "service";

/// The service's name: the systemd unit's, the scheduled task's and the
/// Windows service's.
pub(crate) const SERVICE_NAME: &str = "open-ferry";

/// Where the service definitions point for help.
const DOCS: &str = "https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy";

/// Runs `open-ferry service` with `args`, the arguments after `service`.
pub fn main<I>(program: &str, args: I) -> ExitCode
where
    I: IntoIterator<Item = String>,
{
    let request = match parse(args) {
        Ok(request) => request,
        Err(FlagError::Help) => {
            eprint!("{}", usage(program, &installed_config()));
            return ExitCode::SUCCESS;
        }
        Err(FlagError::Invalid(message)) => {
            eprintln!("{message}");
            eprint!("{}", usage(program, &installed_config()));
            return ExitCode::from(2);
        }
    };
    if request.action == Action::Run {
        return run_for_service_manager(&request);
    }
    let context = match Context::host() {
        Ok(context) => context,
        Err(message) => {
            eprintln!("{NAME}: {message}");
            return ExitCode::FAILURE;
        }
    };
    let updating = if request.action == Action::Install {
        if request.system {
            Updating::assess_system(
                &scope::Host,
                &update_install::RealSystem::default(),
                &context,
            )
        } else {
            Updating::assess(&context)
        }
    } else {
        Updating::plain(data_dir(&context), "no install was requested")
    };
    let mut out = io::stdout().lock();
    match execute(&mut Host, &context, &request, &updating, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = out.flush();
            eprintln!("{NAME}: {message}");
            ExitCode::FAILURE
        }
    }
}

/// `open-ferry service run`, which only the service definitions start.
#[cfg(windows)]
fn run_for_service_manager(request: &Request) -> ExitCode {
    let Some(config) = request.config.as_deref() else {
        eprintln!("service run needs -config");
        return ExitCode::from(2);
    };
    run::main(
        config,
        request.dir.as_deref(),
        request.system,
        request
            .supervise
            .then_some(request.data_dir.as_deref())
            .flatten(),
    )
}

/// `open-ferry service run`, which only the service definitions start.
#[cfg(not(windows))]
fn run_for_service_manager(_request: &Request) -> ExitCode {
    eprintln!("service run is for the Windows service manager only");
    ExitCode::from(2)
}

/// What `service` is asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Install,
    Uninstall,
    Status,
    /// Run the server for the service manager (Windows only; not in the
    /// usage).
    Run,
}

impl Action {
    /// The flags this action takes.
    fn definitions(self) -> &'static [flags::Definition<Options>] {
        match self {
            Action::Install => &DEFINITIONS,
            Action::Uninstall => &[DRY_RUN, SYSTEM],
            Action::Status => &[SYSTEM],
            Action::Run => &[CONFIG, DATA_DIR, DIR, SUPERVISE, SYSTEM],
        }
    }
}

/// The command line after `service`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    action: Action,
    /// `-config`, as given.
    config: Option<String>,
    /// `-dir`, which only `run` takes: the directory to run in.
    dir: Option<String>,
    /// `-system`.
    system: bool,
    /// `-dry-run`.
    dry_run: bool,
    /// `-supervise`, which only `run` takes: run the blue-green supervisor,
    /// not the server.
    supervise: bool,
    /// `-data-dir`, which only `run -supervise` takes: the supervisor's data
    /// directory, a full path.
    data_dir: Option<String>,
}

/// What the flags ask for.
#[derive(Default)]
struct Options {
    config: String,
    dir: String,
    system: bool,
    dry_run: bool,
    supervise: bool,
    data_dir: String,
}

const CONFIG: flags::Definition<Options> = flags::Definition {
    name: "config",
    usage: "The config the service runs with (default: the installed config path, shown below)",
    kind: Kind::String(|options, value| options.config = value),
};

const DIR: flags::Definition<Options> = flags::Definition {
    name: "dir",
    usage: "The directory the server runs in (default: the config's directory)",
    kind: Kind::String(|options, value| options.dir = value),
};

const DRY_RUN: flags::Definition<Options> = flags::Definition {
    name: "dry-run",
    usage: "Print what would be written and run, and change nothing",
    kind: Kind::Bool(|options, value| options.dry_run = value),
};

const SYSTEM: flags::Definition<Options> = flags::Definition {
    name: "system",
    usage: "For the whole machine, started at boot as root (LocalSystem on Windows); needs root or an administrator",
    kind: Kind::Bool(|options, value| options.system = value),
};

const SUPERVISE: flags::Definition<Options> = flags::Definition {
    name: "supervise",
    usage: "Run the blue-green supervisor, not the server",
    kind: Kind::Bool(|options, value| options.supervise = value),
};

const DATA_DIR: flags::Definition<Options> = flags::Definition {
    name: "data-dir",
    usage: "With -supervise: the supervisor's data directory, a full path",
    kind: Kind::String(|options, value| options.data_dir = value),
};

/// Every flag, sorted by name: `install`'s.
const DEFINITIONS: [flags::Definition<Options>; 3] = [CONFIG, DRY_RUN, SYSTEM];

/// Reads the arguments after `service`: the action, then its flags, read
/// as the server's are (see [`flags`]). An argument after the flags is an
/// error.
fn parse<I>(args: I) -> Result<Request, FlagError>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Err(FlagError::Invalid(
            "service needs a command: install, uninstall or status".to_owned(),
        ));
    };
    let action = match first.as_str() {
        "install" => Action::Install,
        "uninstall" => Action::Uninstall,
        "status" => Action::Status,
        "run" => Action::Run,
        "-h" | "-help" | "--h" | "--help" => return Err(FlagError::Help),
        other if other.starts_with('-') => {
            return Err(FlagError::Invalid(
                "service needs a command before its flags: install, uninstall or status".to_owned(),
            ));
        }
        other => {
            return Err(FlagError::Invalid(format!(
                "unknown service command: {other}"
            )));
        }
    };
    let (options, rest) = flags::parse_with(action.definitions(), args)?;
    if let Some(arg) = rest.first() {
        return Err(FlagError::Invalid(format!("unexpected argument: {arg}")));
    }
    if action == Action::Run {
        if options.supervise && options.data_dir.is_empty() {
            return Err(FlagError::Invalid("-supervise needs -data-dir".to_owned()));
        }
        if !options.data_dir.is_empty() && !options.supervise {
            return Err(FlagError::Invalid("-data-dir needs -supervise".to_owned()));
        }
        if !options.data_dir.is_empty() && !Path::new(&options.data_dir).is_absolute() {
            return Err(FlagError::Invalid(
                "-data-dir needs an absolute path".to_owned(),
            ));
        }
    }
    Ok(Request {
        action,
        config: Some(options.config).filter(|config| !config.is_empty()),
        dir: Some(options.dir).filter(|dir| !dir.is_empty()),
        system: options.system,
        dry_run: options.dry_run,
        supervise: options.supervise,
        data_dir: Some(options.data_dir).filter(|dir| !dir.is_empty()),
    })
}

/// The usage text; `installed` is the installed config path, or why there
/// is none.
fn usage(program: &str, installed: &Result<String, String>) -> String {
    let mut out = format!(
        "Usage: {program} {NAME} install [-config PATH] [-system] [-dry-run]\n       \
         {program} {NAME} uninstall [-system] [-dry-run]\n       \
         {program} {NAME} status [-system]\n\n\
         Runs open-ferry in the background with the system's service manager: as a\n\
         systemd unit on Linux, a launchd job on macOS, and a scheduled task at logon\n\
         on Windows, as you; with -system, as a service started at boot.\n\nFlags:\n"
    );
    flags::write_defaults(&mut out, &DEFINITIONS, &[]);
    match installed {
        Ok(path) => {
            let _ = writeln!(out, "\nThe installed config path is {path}");
        }
        Err(error) => {
            let _ = writeln!(out, "\nThere is no installed config path: {error}");
        }
    }
    out
}

/// The installed config path (see [`installed`]).
fn installed_config() -> Result<String, String> {
    installed::config_path()?
        .into_os_string()
        .into_string()
        .map_err(|_| "the installed config path isn't valid UTF-8".to_owned())
}

/// The platform whose service manager is used, and whose path rules apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    Linux,
    MacOs,
    Windows,
}

impl Platform {
    /// The platform this build runs on, if `service` supports it.
    pub(crate) const HOST: Option<Platform> = if cfg!(target_os = "linux") {
        Some(Platform::Linux)
    } else if cfg!(target_os = "macos") {
        Some(Platform::MacOs)
    } else if cfg!(windows) {
        Some(Platform::Windows)
    } else {
        None
    };

    pub(crate) fn separator(self) -> char {
        match self {
            Platform::Windows => '\\',
            Platform::Linux | Platform::MacOs => '/',
        }
    }

    pub(crate) fn is_separator(self, c: char) -> bool {
        c == '/' || (self == Platform::Windows && c == '\\')
    }

    /// Whether `path` is a full path: from `/`, or on Windows from a drive's
    /// root or a UNC share.
    pub(crate) fn is_absolute(self, path: &str) -> bool {
        match self {
            Platform::Windows => {
                matches!(
                    path.as_bytes(),
                    [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic()
                ) || matches!(path.as_bytes(), [b'/' | b'\\', b'/' | b'\\', ..])
            }
            Platform::Linux | Platform::MacOs => path.starts_with('/'),
        }
    }

    /// `path` as a full path from `cwd`, with `.` and `..` folded away.
    pub(crate) fn absolute(self, cwd: &str, path: &str) -> Result<String, String> {
        let full = if self.is_absolute(path) {
            path.to_owned()
        } else if self == Platform::Windows && path.starts_with(['/', '\\']) {
            match cwd.as_bytes() {
                [drive, b':', ..] => format!("{}:{path}", char::from(*drive)),
                _ => return Err(format!("{path} isn't a full path: give one with a drive")),
            }
        } else if self == Platform::Windows && matches!(path.as_bytes(), [_, b':', ..]) {
            return Err(format!(
                "{path} is relative to a drive's current directory: give a full path"
            ));
        } else {
            self.join(cwd, path)
        };
        Ok(self.clean(&full))
    }

    /// A full path with `.`, `..` and repeated separators folded away, and
    /// on Windows `/` written as `\`.
    pub(crate) fn clean(self, path: &str) -> String {
        let sep = self.separator();
        let (mut out, rest) = match self {
            Platform::Linux | Platform::MacOs => ("/".to_owned(), path),
            Platform::Windows => match path.as_bytes() {
                [b'/' | b'\\', b'/' | b'\\', ..] => {
                    let rest = path.trim_start_matches(['/', '\\']);
                    let mut parts = rest.splitn(3, ['/', '\\']);
                    let server = parts.next().unwrap_or_default();
                    let share = parts.next().unwrap_or_default();
                    (
                        format!("\\\\{server}\\{share}\\"),
                        parts.next().unwrap_or_default(),
                    )
                }
                [drive, b':', ..] => (
                    format!("{}:\\", char::from(*drive)),
                    path.get(2..).unwrap_or_default(),
                ),
                _ => (String::new(), path),
            },
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in rest.split(|c| self.is_separator(c)) {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        let mut first = true;
        for part in parts {
            if !first {
                out.push(sep);
            }
            out.push_str(part);
            first = false;
        }
        out
    }

    /// `name` under `dir`.
    pub(crate) fn join(self, dir: &str, name: &str) -> String {
        if dir.ends_with(|c| self.is_separator(c)) {
            format!("{dir}{name}")
        } else {
            format!("{dir}{}{name}", self.separator())
        }
    }

    /// The directory of the full path `path`, or `None` at the root.
    pub(crate) fn parent(self, path: &str) -> Option<String> {
        let path = self.clean(path);
        // The root's length: `/`, `C:\`, or `\\server\share\`.
        let root = match self {
            Platform::Linux | Platform::MacOs => 1,
            Platform::Windows => {
                let nth = if path.starts_with(r"\\") { 3 } else { 0 };
                path.match_indices('\\')
                    .nth(nth)
                    .map_or(path.len(), |(index, _)| index + 1)
            }
        };
        if path.len() <= root {
            return None;
        }
        let index = path.rfind(self.separator())?;
        path.get(..index.max(root)).map(str::to_owned)
    }

    /// Whether the full path `path` is `dir` or under it; on Windows
    /// without regard to case.
    pub(crate) fn is_within(self, path: &str, dir: &str) -> bool {
        let (path, dir) = match self {
            Platform::Windows => (
                self.clean(path).to_lowercase(),
                self.clean(dir).to_lowercase(),
            ),
            Platform::Linux | Platform::MacOs => (self.clean(path), self.clean(dir)),
        };
        match path.strip_prefix(&dir) {
            Some(rest) => {
                rest.is_empty()
                    || dir.ends_with(self.separator())
                    || rest.starts_with(self.separator())
            }
            None => false,
        }
    }

    /// `word` as a shell would need it typed, for the commands `service`
    /// suggests.
    pub(crate) fn quote(self, word: &str) -> String {
        match self {
            Platform::Windows => {
                if word.is_empty() || word.contains([' ', '\t', '"']) {
                    format!("\"{}\"", word.replace('"', "\\\""))
                } else {
                    word.to_owned()
                }
            }
            Platform::Linux | Platform::MacOs => {
                let safe = |c: char| c.is_ascii_alphanumeric() || "/._+:@%=,-".contains(c);
                if !word.is_empty() && word.chars().all(safe) {
                    word.to_owned()
                } else {
                    format!("'{}'", word.replace('\'', "'\\''"))
                }
            }
        }
    }
}

/// What `service` knows of where it runs.
#[derive(Clone, Debug)]
pub(crate) struct Context {
    pub(crate) platform: Platform,
    /// The running binary's full path.
    pub(crate) exe: String,
    /// The working directory.
    pub(crate) cwd: String,
    /// The environment variables of [`ENV_VARS`] that are set and not empty.
    pub(crate) env: BTreeMap<String, String>,
    /// The installed config path, or why there is none: the config when
    /// `-config` isn't given.
    pub(crate) installed_config: Result<String, String>,
    /// The machine config from the platform's known location, never its environment.
    pub(crate) machine_config: Result<Option<String>, String>,
}

/// The environment variables `service` and `migrate` read.
const ENV_VARS: [&str; 16] = [
    "HOME",
    "USERPROFILE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "TEMP",
    "TMP",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "SystemRoot",
    "windir",
    "LOCALAPPDATA",
    "APPDATA",
    "ProgramData",
    "USERNAME",
    "PUBLIC",
];

impl Context {
    /// This process's.
    pub(crate) fn host() -> Result<Context, String> {
        let platform =
            Platform::HOST.ok_or("open-ferry service supports Linux, macOS and Windows only")?;
        let cwd = std::env::current_dir()
            .map_err(|error| format!("failed to get the working directory: {error}"))?
            .into_os_string()
            .into_string()
            .map_err(|_| "the working directory's path isn't valid UTF-8".to_owned())?;
        let exe = std::env::current_exe()
            .map_err(|error| format!("failed to find the running open-ferry binary: {error}"))?
            .into_os_string()
            .into_string()
            .map_err(|_| {
                "the path of the running open-ferry binary isn't valid UTF-8".to_owned()
            })?;
        let exe = platform.absolute(&cwd, &exe)?;
        let env = ENV_VARS
            .iter()
            .filter_map(|name| {
                let value = std::env::var(name).ok().filter(|value| !value.is_empty())?;
                Some(((*name).to_owned(), value))
            })
            .collect();
        Ok(Context {
            platform,
            exe,
            cwd,
            env,
            installed_config: installed_config(),
            machine_config: scope::machine_config(&scope::Host),
        })
    }

    pub(crate) fn var(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }

    pub(crate) fn home(&self) -> Result<&str, String> {
        self.var("HOME")
            .ok_or_else(|| "HOME isn't set, so the service has nowhere to go".to_owned())
    }

    /// `$XDG_CONFIG_HOME` when it is a full path, else `~/.config`.
    fn config_home(&self) -> Result<String, String> {
        match self
            .var("XDG_CONFIG_HOME")
            .filter(|dir| self.platform.is_absolute(dir))
        {
            Some(dir) => Ok(dir.to_owned()),
            None => Ok(self.platform.join(self.home()?, ".config")),
        }
    }
}

/// A path's owner and permissions, on Unix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Owner {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) mode: u32,
    pub(crate) dir: bool,
}

/// A command `service` runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cmd {
    pub(crate) program: &'static str,
    pub(crate) args: Vec<String>,
}

impl Cmd {
    pub(crate) fn new(program: &'static str, args: &[&str]) -> Cmd {
        Cmd {
            program,
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    pub(crate) fn arg(mut self, arg: impl Into<String>) -> Cmd {
        self.args.push(arg.into());
        self
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.program)?;
        for arg in &self.args {
            if arg.is_empty() || arg.contains([' ', '\t', '"']) {
                write!(f, " \"{}\"", arg.replace('"', "\\\""))?;
            } else {
                write!(f, " {arg}")?;
            }
        }
        Ok(())
    }
}

/// What a command that ran said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Output {
    /// The exit code, or `None` when a signal ended it.
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Output {
    pub(crate) fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// How it failed, for a message.
    pub(crate) fn failure(&self) -> String {
        let status = match self.code {
            Some(code) => format!("exit code {code}"),
            None => "ended by a signal".to_owned(),
        };
        let said = [self.stderr.trim(), self.stdout.trim()]
            .into_iter()
            .find(|text| !text.is_empty());
        match said {
            Some(text) => format!("{status}: {text}"),
            None => status,
        }
    }
}

/// The commands and files `service` uses; [`Host`]'s are real.
pub(crate) trait System {
    /// Runs `cmd`, its output captured.
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output>;
    /// Runs `cmd` with its output shown, and gives its exit code.
    fn show(&mut self, cmd: &Cmd) -> io::Result<Option<i32>>;
    fn exists(&self, path: &str) -> bool;
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;
    /// `path` with every symbolic link resolved.
    fn real_path(&self, path: &str) -> io::Result<String>;
    /// Who owns `path`, on Unix.
    fn owner(&self, path: &str) -> io::Result<Owner>;
    fn create_dir_all(&mut self, path: &str) -> io::Result<()>;
    fn write(&mut self, path: &str, data: &[u8]) -> io::Result<()>;
    fn remove(&mut self, path: &str) -> io::Result<()>;
    /// Writes `data` to `path` through a temporary file beside it, so a
    /// reader finds the old file or the new one, never part of one.
    fn write_atomic(&mut self, path: &str, data: &[u8]) -> io::Result<()>;
    /// Asks the supervisor of the data directory `data_dir`, a user's or
    /// the system's as `kind` says, to stop, and waits up to `budget` for
    /// it to end (`client::stop`). The system's is refused unless
    /// `data_dir` passes [`System::trusted_dir`].
    fn stop_supervisor(
        &mut self,
        data_dir: &str,
        budget: Duration,
        kind: ServiceKind,
    ) -> StopOutcome;
    /// Whether the directory `path` and every directory above it pass the
    /// supervisor's trust check with the system rules: nothing another
    /// account can change (`trust::check_dirs`). A link anywhere in it is
    /// refused, so give a real path. The error says why.
    fn trusted_dir(&self, path: &str) -> Result<(), String>;
    /// Whether the file `path` and its directories pass the same check
    /// (`trust::check_file`).
    fn trusted_file(&self, path: &str) -> Result<(), String>;
    /// Waits `time`, between looks at what a service manager says.
    fn pause(&mut self, time: Duration);
}

/// A command's output as text. Windows tools such as `schtasks /xml` may
/// print UTF-16: with a byte order mark (little- or big-endian), or
/// little-endian without one, which is told by the start of the output.
/// Anything else is read as UTF-8, with replacement characters for what
/// isn't.
pub(crate) fn decode_output(bytes: &[u8]) -> String {
    let units = |data: &[u8], big: bool| -> String {
        let units: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if big {
                    u16::from_be_bytes(*pair)
                } else {
                    u16::from_le_bytes(*pair)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return units(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return units(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    // No mark: UTF-16LE is told by its start, whatever the text after it
    // (a task's description may be all CJK). XML starts with `<?xml` or
    // `<Tag`, an ASCII character and a NUL, each; failing that, the first
    // bytes have NULs, all at odd offsets, which UTF-8 text never has.
    let head = bytes.get(..bytes.len().min(64) & !1).unwrap_or_default();
    let xml = matches!(head, [b'<', 0, ascii, 0, ..] if ascii.is_ascii() && *ascii != 0);
    let nuls = head.iter().filter(|byte| **byte == 0).count();
    let odd_nuls = head
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|byte| **byte == 0)
        .count();
    let utf16 = xml || (nuls >= 2 && nuls == odd_nuls && head.first().is_some_and(|b| *b != 0));
    if utf16 && bytes.len().is_multiple_of(2) {
        return units(bytes, false);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Windows gives a real path in the verbatim form, `\\?\C:\dir` or
/// `\\?\UNC\server\share`; this is the usual form of it. Any other path is
/// returned as it is.
fn plain_windows_path(path: String) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let mut chars = rest.chars();
        if chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some(':') {
            return rest.to_owned();
        }
    }
    path
}

/// The most [`Host`] reads of a file: a config, a service definition or
/// `service.json` is far smaller.
const MAX_READ: u64 = 8 * 1024 * 1024;

/// The real system.
pub(crate) struct Host;

impl System for Host {
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
        let output = std::process::Command::new(cmd.program)
            .args(&cmd.args)
            .stdin(Stdio::null())
            .output()?;
        Ok(Output {
            code: output.status.code(),
            stdout: decode_output(&output.stdout),
            stderr: decode_output(&output.stderr),
        })
    }

    fn show(&mut self, cmd: &Cmd) -> io::Result<Option<i32>> {
        let status = std::process::Command::new(cmd.program)
            .args(&cmd.args)
            .stdin(Stdio::null())
            .status()?;
        Ok(status.code())
    }

    fn exists(&self, path: &str) -> bool {
        std::fs::exists(path).unwrap_or(true)
    }

    /// Reads a regular file of at most [`MAX_READ`] bytes, links followed
    /// (a config may be a link into a dotfiles repository). Opening never
    /// waits on a pipe somebody put there.
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        use io::Read as _;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path} isn't a regular file"),
            ));
        }
        let mut data = Vec::new();
        file.take(MAX_READ + 1).read_to_end(&mut data)?;
        if data.len() as u64 > MAX_READ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{path} is larger than {MAX_READ} bytes"),
            ));
        }
        Ok(data)
    }

    fn real_path(&self, path: &str) -> io::Result<String> {
        let real = std::fs::canonicalize(path)?
            .into_os_string()
            .into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not valid UTF-8"))?;
        Ok(plain_windows_path(real))
    }

    #[cfg(unix)]
    fn owner(&self, path: &str) -> io::Result<Owner> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path)?;
        Ok(Owner {
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode(),
            dir: metadata.is_dir(),
        })
    }

    #[cfg(not(unix))]
    fn owner(&self, _path: &str) -> io::Result<Owner> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "owners are read on Unix only",
        ))
    }

    fn create_dir_all(&mut self, path: &str) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn write(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        std::fs::write(path, data)
    }

    fn remove(&mut self, path: &str) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn write_atomic(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        open_ferry_update::journal::atomic_write(Path::new(path), data, true)
    }

    fn stop_supervisor(
        &mut self,
        data_dir: &str,
        budget: Duration,
        kind: ServiceKind,
    ) -> StopOutcome {
        // The system supervisor is found through its data directory: only
        // one root or the administrators alone can change will do.
        if kind == ServiceKind::System
            && let Err(reason) = self.trusted_dir(data_dir)
        {
            return StopOutcome::Refused(reason);
        }
        let identity = match Identity::current(kind) {
            Ok(identity) => identity,
            Err(error) => {
                return StopOutcome::Refused(format!(
                    "failed to find the service's account: {error}"
                ));
            }
        };
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                return StopOutcome::Refused(format!("failed to start an async runtime: {error}"));
            }
        };
        let deadline = Instant::now() + budget;
        runtime.block_on(client::stop(Path::new(data_dir), &identity, deadline))
    }

    fn trusted_dir(&self, path: &str) -> Result<(), String> {
        trust::check_dirs(&[Path::new(path)], &system_trust_rules()?)
            .map_err(|error| error.to_string())
    }

    fn trusted_file(&self, path: &str) -> Result<(), String> {
        trust::check_file(Path::new(path), &system_trust_rules()?)
            .map_err(|error| error.to_string())
    }

    fn pause(&mut self, time: Duration) {
        std::thread::sleep(time);
    }
}

/// The trust check's rules for what the system's service may run: owned
/// by root, or SYSTEM and Administrators, and changed by no one else.
pub(crate) fn system_trust_rules() -> Result<trust::TrustRules, String> {
    Identity::current(ServiceKind::System)
        .map(|identity| trust::TrustRules::for_identity(&identity))
        .map_err(|error| format!("failed to find the system service's account: {error}"))
}

/// Runs `cmd`, and fails unless it succeeds.
pub(crate) fn run_checked(system: &mut dyn System, cmd: &Cmd) -> Result<Output, String> {
    let output = system
        .run(cmd)
        .map_err(|error| format!("failed to run `{cmd}`: {error}"))?;
    if output.success() {
        Ok(output)
    } else {
        Err(format!("`{cmd}` failed with {}", output.failure()))
    }
}

/// One change `install` or `uninstall` makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    CreateDir(String),
    Write {
        path: String,
        text: String,
        /// Written as UTF-16 with a byte order mark, as Task Scheduler
        /// reads its XML; else as UTF-8.
        utf16: bool,
    },
    Remove(String),
    /// Runs a command that must succeed.
    Run(Cmd),
    /// Runs a command whose failure is reported, and doesn't stop the rest.
    TryRun(Cmd),
    /// Writes a record, as UTF-8, through [`System::write_atomic`].
    WriteAtomic {
        path: String,
        text: String,
    },
    /// Asks the supervisor of a data directory, a user's or the system's as
    /// `kind` says, to stop, waiting up to `STOP_BUDGET`, before the service
    /// manager ends the service. How that went is said, and doesn't stop the
    /// rest.
    StopSupervisor {
        data_dir: String,
        kind: ServiceKind,
    },
}

/// How `uninstall` stops a user service that may run the supervisor: it runs
/// `stop`; where that fails, `query` asks the manager whether the service is
/// still there, and unless that proves it is not, `uninstall` fails before
/// it removes anything. (Not a [`Step`], which `migrate` matches on.)
pub(crate) struct StopCheck {
    pub(crate) stop: Cmd,
    pub(crate) query: Cmd,
    pub(crate) gone: Gone,
}

/// How to tell from the answer to a [`StopCheck`] query that the service is
/// not running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gone {
    /// `systemctl is-active` prints `inactive`, `failed` or `unknown`.
    Systemd,
    /// `launchctl print` can't find the job (exit code 113).
    Launchd,
}

impl Gone {
    fn proves(self, answer: &Output) -> bool {
        if answer.success() {
            return false;
        }
        match self {
            Gone::Systemd => matches!(answer.stdout.trim(), "inactive" | "failed" | "unknown"),
            Gone::Launchd => {
                answer.code == Some(113)
                    || format!("{}{}", answer.stdout, answer.stderr)
                        .contains("Could not find service")
            }
        }
    }
}

/// What `install` or `uninstall` does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) steps: Vec<Step>,
    /// Files removed once the steps are done, or have failed.
    pub(crate) cleanup: Vec<String>,
}

/// A text's bytes as [`Step::Write`] writes them.
fn encode(text: &str, utf16: bool) -> Vec<u8> {
    if utf16 {
        [0xFF, 0xFE]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect()
    } else {
        text.as_bytes().to_vec()
    }
}

/// A plan that failed part way.
pub(crate) struct Failed {
    pub(crate) message: String,
    /// Whether a step before the failed one changed something.
    pub(crate) changed: bool,
}

/// Carries out `plan`, saying what it did.
pub(crate) fn apply(
    system: &mut dyn System,
    plan: &Plan,
    out: &mut dyn Write,
) -> Result<(), Failed> {
    let mut changed = false;
    let mut result = Ok(());
    for step in &plan.steps {
        match apply_step(system, step, out) {
            // A file that is cleaned up doesn't stay.
            Ok(()) => {
                changed |= !matches!(
                    step,
                    Step::Write { path, .. } if plan.cleanup.contains(path)
                );
            }
            Err(message) => {
                result = Err(Failed { message, changed });
                break;
            }
        }
    }
    for path in &plan.cleanup {
        if system.exists(path) {
            match system.remove(path) {
                Ok(()) => say(out, format_args!("Removed {path}")),
                Err(error) => say(out, format_args!("Failed to remove {path}: {error}")),
            }
        }
    }
    result
}

fn apply_step(system: &mut dyn System, step: &Step, out: &mut dyn Write) -> Result<(), String> {
    match step {
        Step::CreateDir(dir) => {
            system
                .create_dir_all(dir)
                .map_err(|error| format!("failed to create {dir}: {error}"))?;
            say(out, format_args!("Created {dir}"));
        }
        Step::Write { path, text, utf16 } => {
            system
                .write(path, &encode(text, *utf16))
                .map_err(|error| format!("failed to write {path}: {error}"))?;
            say(out, format_args!("Wrote {path}"));
        }
        Step::Remove(path) => {
            system
                .remove(path)
                .map_err(|error| format!("failed to remove {path}: {error}"))?;
            say(out, format_args!("Removed {path}"));
        }
        Step::Run(cmd) => {
            run_checked(system, cmd)?;
            say(out, format_args!("Ran: {cmd}"));
        }
        Step::TryRun(cmd) => match system.run(cmd) {
            Ok(output) if output.success() => say(out, format_args!("Ran: {cmd}")),
            Ok(output) => say(
                out,
                format_args!("Ran: {cmd} (failed with {}; going on)", output.failure()),
            ),
            Err(error) => say(
                out,
                format_args!("Failed to run `{cmd}`: {error}; going on"),
            ),
        },
        Step::WriteAtomic { path, text } => {
            system
                .write_atomic(path, text.as_bytes())
                .map_err(|error| format!("failed to write {path}: {error}"))?;
            say(out, format_args!("Wrote {path}"));
        }
        Step::StopSupervisor { data_dir, kind } => {
            match system.stop_supervisor(data_dir, STOP_BUDGET, *kind) {
                StopOutcome::Stopped => say(out, format_args!("Stopped the supervisor")),
                StopOutcome::NotRunning => say(out, format_args!("No supervisor was running")),
                StopOutcome::TimedOut(seen) => say(
                    out,
                    format_args!(
                        "The supervisor didn't stop within {} s ({seen}); going on",
                        STOP_BUDGET.as_secs()
                    ),
                ),
                StopOutcome::Refused(why) => say(
                    out,
                    format_args!("Failed to ask the supervisor to stop ({why}); going on"),
                ),
            }
        }
    }
    Ok(())
}

/// Runs `check.stop`, saying how it went. Where it fails and the manager
/// doesn't say the service is gone, that is an error, so nothing is removed.
fn stop_checked(
    system: &mut dyn System,
    check: &StopCheck,
    out: &mut dyn Write,
) -> Result<(), String> {
    let StopCheck { stop, query, gone } = check;
    let why = match system.run(stop) {
        Ok(output) if output.success() => {
            say(out, format_args!("Ran: {stop}"));
            return Ok(());
        }
        Ok(output) => output.failure(),
        Err(error) => error.to_string(),
    };
    let down = matches!(system.run(query), Ok(answer) if gone.proves(&answer));
    if !down {
        return Err(format!(
            "`{stop}` failed ({why}), and open-ferry may still be running, so nothing was removed. Stop it by hand with `{stop}`, then run the uninstall again; that is safe."
        ));
    }
    say(
        out,
        format_args!("Ran: {stop} (failed with {why}; the service is not running, going on)"),
    );
    Ok(())
}

/// Prints what `plan` would do.
pub(crate) fn describe(plan: &Plan, out: &mut dyn Write) {
    describe_steps(&plan.steps, out);
    for path in &plan.cleanup {
        say(out, format_args!("Would remove {path} afterwards"));
    }
    say(out, format_args!("Dry run: nothing was changed."));
}

fn describe_steps(steps: &[Step], out: &mut dyn Write) {
    for step in steps {
        match step {
            Step::CreateDir(dir) => say(out, format_args!("Would create {dir}")),
            Step::Write { path, text, utf16 } => {
                let encoding = if *utf16 {
                    " (in UTF-16, with a byte order mark)"
                } else {
                    ""
                };
                say(out, format_args!("Would write {path}{encoding}:"));
                for line in text.lines() {
                    say(out, format_args!("    {line}"));
                }
            }
            Step::Remove(path) => say(out, format_args!("Would remove {path}")),
            Step::Run(cmd) | Step::TryRun(cmd) => say(out, format_args!("Would run: {cmd}")),
            Step::WriteAtomic { path, text } => {
                say(out, format_args!("Would write {path}:"));
                for line in text.lines() {
                    say(out, format_args!("    {line}"));
                }
            }
            Step::StopSupervisor { data_dir, .. } => say(
                out,
                format_args!(
                    "Would ask the supervisor of {data_dir} to stop, and wait up to {} s",
                    STOP_BUDGET.as_secs()
                ),
            ),
        }
    }
}

/// Prints a line; a closed standard output doesn't stop the work.
pub(crate) fn say(out: &mut dyn Write, line: fmt::Arguments<'_>) {
    let _ = writeln!(out, "{line}");
}

/// Which service manager, and whose service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    SystemdUser,
    SystemdSystem,
    LaunchAgent,
    LaunchDaemon,
    ScheduledTask,
    WindowsService,
}

impl Target {
    pub(crate) fn new(platform: Platform, system: bool) -> Target {
        match (platform, system) {
            (Platform::Linux, false) => Target::SystemdUser,
            (Platform::Linux, true) => Target::SystemdSystem,
            (Platform::MacOs, false) => Target::LaunchAgent,
            (Platform::MacOs, true) => Target::LaunchDaemon,
            (Platform::Windows, false) => Target::ScheduledTask,
            (Platform::Windows, true) => Target::WindowsService,
        }
    }

    pub(crate) fn system(self) -> bool {
        matches!(
            self,
            Target::SystemdSystem | Target::LaunchDaemon | Target::WindowsService
        )
    }

    /// `-system` when it is the machine's, for the commands `service`
    /// suggests.
    pub(crate) fn flag(self) -> &'static str {
        if self.system() { " -system" } else { "" }
    }

    pub(crate) fn kind(self) -> &'static str {
        match self {
            Target::SystemdUser => "a systemd user service",
            Target::SystemdSystem => "a systemd system service",
            Target::LaunchAgent => "a launchd agent",
            Target::LaunchDaemon => "a launchd daemon",
            Target::ScheduledTask => "a scheduled task",
            Target::WindowsService => "a Windows service",
        }
    }

    /// The file that defines the service, where there is one.
    fn definition_path(self, context: &Context) -> Result<Option<String>, String> {
        match self {
            Target::SystemdUser | Target::SystemdSystem => {
                systemd::unit_path(context, self.system()).map(Some)
            }
            Target::LaunchAgent | Target::LaunchDaemon => {
                launchd::plist_path(context, self.system()).map(Some)
            }
            Target::ScheduledTask | Target::WindowsService => Ok(None),
        }
    }

    /// Where the service is defined, for messages.
    pub(crate) fn location(self, context: &Context) -> Result<String, String> {
        Ok(match self.definition_path(context)? {
            Some(path) => path,
            None if self == Target::ScheduledTask => format!("the task named {SERVICE_NAME}"),
            None => format!("the service named {SERVICE_NAME}"),
        })
    }

    /// Whether the service is installed.
    pub(crate) fn installed(
        self,
        system: &mut dyn System,
        context: &Context,
    ) -> Result<bool, String> {
        if let Some(path) = self.definition_path(context)? {
            return Ok(system.exists(&path));
        }
        let cmd = match self {
            Target::ScheduledTask => windows::query_task(),
            _ => windows::query_service(),
        };
        let output = system
            .run(&cmd)
            .map_err(|error| format!("failed to run `{cmd}`: {error}"))?;
        match output.code {
            Some(0) => Ok(true),
            // The task scheduler says only that the query failed; its list
            // of tasks says whether the task is there. Any other failure
            // is not "no such task".
            _ if self == Target::ScheduledTask => {
                let list = windows::list_tasks();
                match system.run(&list) {
                    Ok(listed) if listed.success() => Ok(windows::task_listed(&listed.stdout)),
                    _ => Err(format!("`{cmd}` failed with {}", output.failure())),
                }
            }
            Some(windows::SERVICE_DOES_NOT_EXIST) => Ok(false),
            _ => Err(format!("`{cmd}` failed with {}", output.failure())),
        }
    }
}

/// Who runs `service`, as far as it matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Account {
    /// On Unix: the user ID.
    Unix { uid: u32 },
    /// On Windows: whether the process is elevated.
    Windows { elevated: bool },
}

impl Account {
    pub(crate) fn probe(
        system: &mut dyn System,
        platform: Platform,
        target: Target,
    ) -> Result<Account, String> {
        match platform {
            Platform::Linux | Platform::MacOs => {
                let output = run_checked(system, &Cmd::new("id", &["-u"]))?;
                let uid = output.stdout.trim();
                let uid = uid
                    .parse()
                    .map_err(|_| format!("`id -u` printed {uid:?}, not a user ID"))?;
                Ok(Account::Unix { uid })
            }
            // Only a Windows service needs an administrator.
            Platform::Windows if target.system() => Ok(Account::Windows {
                elevated: windows::elevated(system)?,
            }),
            Platform::Windows => Ok(Account::Windows { elevated: false }),
        }
    }

    /// Whether it may install or remove a system service.
    pub(crate) fn privileged(&self) -> bool {
        match self {
            Account::Unix { uid } => *uid == 0,
            Account::Windows { elevated } => *elevated,
        }
    }

    fn uid(&self) -> u32 {
        match self {
            Account::Unix { uid } => *uid,
            Account::Windows { .. } => 0,
        }
    }
}

/// Why a system service needs root or an administrator, with `command`
/// run so.
fn needs_admin(platform: Platform, doing: &str, command: &str) -> String {
    match platform {
        Platform::Windows => format!(
            "{doing} a Windows service needs an administrator. Run this in a terminal opened with \"Run as administrator\":\n  {command}"
        ),
        Platform::Linux | Platform::MacOs => {
            format!("{doing} a system service needs root. Run:\n  sudo {command}")
        }
    }
}

/// What a service runs: `exe -config config`, in `dir`: the config's
/// directory, unless `open-ferry migrate` keeps CLIProxyAPI's. With a
/// `data_dir` it runs `exe -supervise -config config -data-dir data_dir`
/// instead, the blue-green supervisor on that data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Definition {
    pub(crate) exe: String,
    pub(crate) config: String,
    pub(crate) dir: String,
    /// The supervisor's data directory, a full path; `None` for the plain
    /// run.
    pub(crate) data_dir: Option<String>,
}

const NO_MACHINE_INSTALL: &str = "no machine-wide install of this binary: install it with install.sh --system / install.ps1 -System";
const MACOS_SYSTEM_PLAIN: &str =
    "machine-wide installs on macOS come later; install and update open-ferry as the user";

/// The data directory `open-ferry update` would use for the service's
/// account, from the starting environment of `context` (before any `.env`),
/// as a full path in `context`'s platform's form.
pub(crate) fn data_dir(context: &Context) -> Result<String, String> {
    let platform = context.platform;
    let located = DataDir::locate(
        |name| context.var(name).map(OsString::from),
        platform == Platform::Windows,
    )
    .map_err(|error| error.to_string())?;
    // `DataDir` joins with the separators of the machine running this, which
    // a test of a Unix context on Windows isn't. Anywhere else its path is
    // kept as it is: on Unix a backslash can be part of a name.
    let root = located.root().to_string_lossy().into_owned();
    let root = if cfg!(windows) && platform != Platform::Windows {
        root.replace('\\', "/")
    } else {
        root
    };
    Ok(platform.clean(&root))
}

/// Whether a service runs supervised or plain, and the data directory
/// it pins. It is decided once, from the real machine ([`Updating::assess`]),
/// and handed to the pure builders as an input, so the tests cover both
/// forms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Updating {
    /// The data directory, or why there is none.
    pub(crate) data_dir: Result<String, String>,
    /// Why the service runs plain, or `None` when it is supervised.
    pub(crate) why_plain: Option<String>,
}

impl Updating {
    /// The plain run, for this reason.
    pub(crate) fn plain(data_dir: Result<String, String>, why: impl Into<String>) -> Updating {
        Updating {
            data_dir,
            why_plain: Some(why.into()),
        }
    }

    /// The supervised run, on `data_dir`.
    #[cfg(test)]
    pub(crate) fn supervised(data_dir: impl Into<String>) -> Updating {
        Updating {
            data_dir: Ok(data_dir.into()),
            why_plain: None,
        }
    }

    /// The data directory the definition pins, when it is supervised.
    fn pinned(&self) -> Option<&str> {
        match (&self.why_plain, &self.data_dir) {
            (None, Ok(dir)) => Some(dir),
            _ => None,
        }
    }

    /// Supervised when `install` says the binary updates itself and there is
    /// a data directory, else plain.
    pub(crate) fn from_install(data_dir: Result<String, String>, install: &Install) -> Updating {
        if let Err(error) = &data_dir {
            let why = format!("there is no data directory ({error})");
            return Updating::plain(data_dir, why);
        }
        match install.why_not() {
            None => Updating {
                data_dir,
                why_plain: None,
            },
            Some(why) => {
                let why = why.to_string();
                Updating::plain(data_dir, why)
            }
        }
    }

    /// Looks at this machine: the install receipt in the data directory, and
    /// whether the binary `context` runs can update itself, which is what
    /// the definition would start.
    pub(crate) fn assess(context: &Context) -> Updating {
        let data_dir = data_dir(context);
        let receipt = match &data_dir {
            Ok(dir) => Receipt::load(&DataDir::at(dir).receipt_file()),
            Err(_) => Ok(None),
        };
        let install = update_install::assess_for(
            &update_install::RealSystem::default(),
            receipt,
            Path::new(&context.exe),
        );
        Updating::from_install(data_dir, &install)
    }

    /// A system service supervises only the machine install of this binary.
    /// Lookup checks its receipt and trust; authorization also checks the
    /// update records before assessment reads the receipt again.
    fn assess_system(
        probe: &dyn scope::Probe,
        machine: &dyn update_install::System,
        context: &Context,
    ) -> Updating {
        let unavailable = |why: String| Updating::plain(Err(why.clone()), why);
        if context.platform == Platform::MacOs {
            return unavailable(MACOS_SYSTEM_PLAIN.to_owned());
        }
        let home = match scope::lookup(probe, &context.exe) {
            Ok(home) if home.scope == scope::Scope::Machine => home,
            Ok(_) => return unavailable(NO_MACHINE_INSTALL.to_owned()),
            Err(why) => return unavailable(why.to_string()),
        };
        let root = home.data.root().to_string_lossy().into_owned();
        if let Err(why) = scope::authorize(probe, &root) {
            return unavailable(why.to_string());
        }
        let receipt_path = context
            .platform
            .join(&root, open_ferry_update::data_dir::RECEIPT_FILE);
        let install = update_install::assess_for(
            machine,
            probe.receipt(&receipt_path),
            Path::new(&context.exe),
        );
        Updating::from_install(Ok(root), &install)
    }

    /// Says which form is installed, and why.
    fn form(&self) -> String {
        match (&self.why_plain, &self.data_dir) {
            (None, Ok(dir)) => format!(
                "Installing the supervised form: this binary can update itself, so the service runs the supervisor and `open-ferry update` can switch versions without downtime. Its data directory is {dir}."
            ),
            (None, Err(_)) | (Some(_), _) => format!(
                "Installing the plain form, without the supervisor: {}.",
                self.why_plain
                    .as_deref()
                    .unwrap_or("there is no data directory")
            ),
        }
    }

    fn system_form(&self) -> String {
        match self.pinned() {
            Some(dir) => format!(
                "Installing the supervised form: this binary has a machine-wide install and can update itself. Its data directory is {dir}. Stop the system service before running `open-ferry update` in this version."
            ),
            None => self.form(),
        }
    }
}

/// Carries out `request`.
fn execute(
    system: &mut dyn System,
    context: &Context,
    request: &Request,
    updating: &Updating,
    out: &mut dyn Write,
) -> Result<(), String> {
    let target = Target::new(context.platform, request.system);
    match request.action {
        Action::Install => install(system, context, target, request, updating, out),
        Action::Uninstall => uninstall(system, context, target, request.dry_run, out),
        Action::Status => status(system, context, target, out),
        Action::Run => Err("service run is for the service manager only".to_owned()),
    }
}

fn install(
    system: &mut dyn System,
    context: &Context,
    target: Target,
    request: &Request,
    updating: &Updating,
    out: &mut dyn Write,
) -> Result<(), String> {
    let platform = context.platform;
    let config = match &request.config {
        Some(path) => platform.absolute(&context.cwd, path)?,
        None if target.system() && platform != Platform::MacOs => context
            .machine_config
            .as_ref()?
            .clone()
            .ok_or("there is no machine config: pass -config")?,
        None => platform.absolute(&context.cwd, context.installed_config.as_ref()?)?,
    };
    let Prepared {
        plan,
        definition,
        account,
    } = prepare_install_with(
        system,
        context,
        target,
        &config,
        None,
        request.dry_run,
        updating,
    )?;
    let form = if target.system() {
        if target == Target::LaunchDaemon {
            Updating::plain(updating.data_dir.clone(), MACOS_SYSTEM_PLAIN).form()
        } else {
            updating.system_form()
        }
    } else {
        updating.form()
    };
    say(out, format_args!("{form}"));
    if request.dry_run {
        describe(&plan, out);
        if target.system() && !account.privileged() {
            say(
                out,
                format_args!(
                    "Without -dry-run, this needs {}.",
                    if platform == Platform::Windows {
                        "an administrator"
                    } else {
                        "root"
                    }
                ),
            );
        }
        return Ok(());
    }
    if let Err(failed) = apply(system, &plan, out) {
        let mut message = failed.message;
        if failed.changed {
            message.push_str(&format!(
                "\nWhat was done so far is still in place: `open-ferry {NAME} uninstall{}` removes it.",
                target.flag()
            ));
        }
        return Err(message);
    }
    say(
        out,
        format_args!("{}", installed_notes(context, target, &definition)),
    );
    say(
        out,
        format_args!(
            "`open-ferry {NAME} status{flag}` shows it; `open-ferry {NAME} uninstall{flag}` removes it.",
            flag = target.flag()
        ),
    );
    Ok(())
}

/// What installing open-ferry as `target` would do, and the account doing
/// it.
pub(crate) struct Prepared {
    pub(crate) plan: Plan,
    pub(crate) definition: Definition,
    pub(crate) account: Account,
}

/// Checks that open-ferry can be installed as `target` with `config`, a
/// full path, and makes the plan that installs it, changing nothing. The
/// service runs in `dir` when it is given (`open-ferry migrate` keeps
/// CLIProxyAPI's working directory), else in the config's directory. With
/// `dry_run`, a system service is planned without root or an
/// administrator. The service runs plain: `open-ferry migrate` installs it
/// so (see [`prepare_install_with`]).
pub(crate) fn prepare_install(
    system: &mut dyn System,
    context: &Context,
    target: Target,
    config: &str,
    dir: Option<&str>,
    dry_run: bool,
) -> Result<Prepared, String> {
    // No data directory, so no `service.json` either: this install is
    // `open-ferry migrate`'s, which has no use for one.
    let updating = Updating::plain(
        Err("the install is the plain run".to_owned()),
        "this install isn't supervised",
    );
    prepare_install_with(system, context, target, config, dir, dry_run, &updating)
}

/// [`prepare_install`], with the form `updating` chose for the service.
pub(crate) fn prepare_install_with(
    system: &mut dyn System,
    context: &Context,
    target: Target,
    config: &str,
    dir: Option<&str>,
    dry_run: bool,
    updating: &Updating,
) -> Result<Prepared, String> {
    let platform = context.platform;
    let config = config.to_owned();
    check_path(platform, "The open-ferry binary's path", &context.exe)?;
    check_path(platform, "The config's path", &config)?;
    if let Some(dir) = dir {
        check_path(platform, "The working directory's path", dir)?;
    }
    // The supervised form puts the data directory in the definition too.
    if let Some(data) = updating.pinned() {
        check_path(platform, "The data directory's path", data)?;
    }
    let account = Account::probe(system, platform, target)?;
    let command = format!(
        "{} {NAME} install{} -config {}",
        platform.quote(&context.exe),
        target.flag(),
        platform.quote(&config)
    );
    if let Account::Unix { uid: 0 } = account
        && !target.system()
    {
        return Err(format!(
            "You are root, so this would install a service for root. Run it as the user the service is for, or add -system for a service of the whole machine:\n  {command}"
        ));
    }
    if target.system() && !account.privileged() && !dry_run {
        return Err(needs_admin(platform, "Installing", &command));
    }
    if !system.exists(&config) {
        return Err(format!(
            "There is no config at {config}. Make one with `open-ferry init`, or name another with -config."
        ));
    }
    let data = system
        .read(&config)
        .map_err(|error| format!("failed to read the config at {config}: {error}"))?;
    let loaded = Config::load_bytes(&data)
        .map_err(|error| format!("The config at {config} doesn't load: {error}"))?;
    let definition = if target.system() {
        check_auth_dir(platform, &loaded)?;
        let mut definition = protected(system, context, &config, dir)?;
        // Check the paths as given too: resolving a link in `protected`
        // must not hide it from the real system trust check.
        for path in [&context.exe, &config] {
            system.trusted_file(path).map_err(|why| {
                format!("Refusing the system service: {path} failed the system trust check: {why}")
            })?;
        }
        let working = dir
            .map(str::to_owned)
            .unwrap_or_else(|| platform.parent(&config).unwrap_or_else(|| config.clone()));
        system.trusted_dir(&working).map_err(|why| {
            format!("Refusing the system service: {working} failed the system trust check: {why}")
        })?;
        let dotenv = platform.join(&working, ".env");
        if system.exists(&dotenv) {
            system.trusted_file(&dotenv).map_err(|why| {
                format!("Refusing the system service: {dotenv} failed the system trust check: {why}. As root or an administrator, make the file and its directories writable only by root or administrators, or remove it.")
            })?;
        }
        if platform != Platform::MacOs {
            definition.data_dir = updating.pinned().map(str::to_owned);
        }
        definition
    } else {
        Definition {
            exe: context.exe.clone(),
            dir: match dir {
                Some(dir) => dir.to_owned(),
                None => platform.parent(&config).unwrap_or_else(|| config.clone()),
            },
            config,
            data_dir: updating.pinned().map(str::to_owned),
        }
    };
    if target.installed(system, context)? {
        return Err(format!(
            "open-ferry is already installed as {} ({}). To install it again, run `open-ferry {NAME} uninstall{}` first.",
            target.kind(),
            target.location(context)?,
            target.flag()
        ));
    }
    let record = updating.data_dir.as_ref().ok().map(String::as_str);
    let plan = install_plan(system, context, target, &definition, &account, record)?;
    Ok(Prepared {
        plan,
        definition,
        account,
    })
}

/// Refuses a path a service definition can't hold.
fn check_path(platform: Platform, what: &str, path: &str) -> Result<(), String> {
    if path.chars().any(char::is_control) {
        return Err(format!(
            "{what}, {path:?}, has a control character, which a service definition can't hold."
        ));
    }
    if platform == Platform::Linux && path.contains('\\') {
        return Err(format!(
            "{what}, {path}, has a backslash, which a systemd unit would read as an escape."
        ));
    }
    if platform == Platform::Windows && path.contains('%') {
        return Err(format!(
            "{what}, {path}, has a %, which Windows would read as part of a variable."
        ));
    }
    Ok(())
}

/// Refuses an auth directory under `~`, which a system service would look
/// for in root's home, or LocalSystem's profile.
fn check_auth_dir(platform: Platform, config: &Config) -> Result<(), String> {
    let (shown, value) = if config.auth_dir.is_empty() {
        (format!("not set, so {DEFAULT_AUTH_DIR}"), DEFAULT_AUTH_DIR)
    } else {
        (config.auth_dir.clone(), config.auth_dir.as_str())
    };
    if !value.starts_with('~') {
        return Ok(());
    }
    let account = match platform {
        Platform::Windows => "LocalSystem, and ~ would be its profile",
        Platform::Linux | Platform::MacOs => "root, and ~ would be root's home",
    };
    Err(format!(
        "The config's auth-dir is {shown}. A system service runs as {account}, not yours. Set auth-dir to a full path, or install without -system."
    ))
}

/// The definition of a system service, refused unless only root, or the
/// administrators, can change its binary and its config: the service runs
/// them with root's rights.
fn protected(
    system: &mut dyn System,
    context: &Context,
    config: &str,
    dir: Option<&str>,
) -> Result<Definition, String> {
    let platform = context.platform;
    let (exe, config, dir) = match platform {
        Platform::Windows => {
            let mut roots: Vec<&str> = [
                "ProgramFiles",
                "ProgramFiles(x86)",
                "ProgramW6432",
                "SystemRoot",
            ]
            .into_iter()
            .filter_map(|name| context.var(name))
            .filter(|dir| platform.is_absolute(dir))
            .collect();
            let machine_dir = context
                .machine_config
                .as_ref()
                .ok()
                .and_then(|config| config.as_deref())
                .and_then(|config| platform.parent(config));
            if let Some(dir) = machine_dir.as_deref() {
                roots.push(dir);
            }
            let program_files = context.var("ProgramFiles").unwrap_or(r"C:\Program Files");
            for path in [Some(context.exe.as_str()), Some(config), dir]
                .into_iter()
                .flatten()
            {
                if !roots.iter().any(|root| platform.is_within(path, root)) {
                    return Err(format!(
                        "A Windows service runs as LocalSystem, so open-ferry's binary and its config must be where only administrators can change them, such as {program_files}; {path} isn't. Copy them to {}, and pass -config.",
                        platform.join(program_files, SERVICE_NAME)
                    ));
                }
            }
            (
                context.exe.clone(),
                config.to_owned(),
                dir.map(str::to_owned),
            )
        }
        Platform::Linux | Platform::MacOs => {
            let real = |system: &mut dyn System, path: &str| {
                system
                    .real_path(path)
                    .map_err(|error| format!("failed to resolve {path}: {error}"))
            };
            let exe = real(system, &context.exe)?;
            let config = real(system, config)?;
            let dir = dir.map(|dir| real(system, dir)).transpose()?;
            for path in [Some(&exe), Some(&config), dir.as_ref()]
                .into_iter()
                .flatten()
            {
                if let Some(problem) = unprotected(system, platform, path)? {
                    return Err(format!(
                        "A system service runs as root, so open-ferry's binary and its config, and the directories above them, must be owned by root and writable by no one else; {problem}. Copy them where only root can change them, such as /usr/local/bin/open-ferry and /etc/open-ferry/config.yaml, and pass -config."
                    ));
                }
            }
            (exe, config, dir)
        }
    };
    Ok(Definition {
        exe,
        dir: dir.unwrap_or_else(|| platform.parent(&config).unwrap_or_else(|| config.clone())),
        config,
        data_dir: None,
    })
}

/// What, if anything, lets someone other than root change `path`: it, or
/// a directory above it, owned by another user, or writable by another
/// group or by everyone. A directory everyone may write to with the sticky
/// bit, such as `/tmp`, doesn't count: others can't replace what root put
/// there.
fn unprotected(
    system: &mut dyn System,
    platform: Platform,
    path: &str,
) -> Result<Option<String>, String> {
    let mut current = Some(path.to_owned());
    while let Some(path) = current {
        let owner = system
            .owner(&path)
            .map_err(|error| format!("failed to read who owns {path}: {error}"))?;
        let sticky = owner.dir && owner.mode & 0o1000 != 0;
        if owner.uid != 0 {
            return Ok(Some(format!("{path} is owned by user {}", owner.uid)));
        }
        if owner.mode & 0o002 != 0 && !sticky {
            return Ok(Some(format!("everyone may write to {path}")));
        }
        if owner.mode & 0o020 != 0 && owner.gid != 0 && !sticky {
            return Ok(Some(format!("group {} may write to {path}", owner.gid)));
        }
        current = platform.parent(&path);
    }
    Ok(None)
}

/// The service install's record, in its data directory.
pub(crate) const RECORD_FILE: &str = "service.json";

/// `service.json`'s text: the definition's binary, config and data directory.
#[derive(serde::Serialize)]
struct ServiceRecord<'a> {
    format: u32,
    kind: &'a str,
    manager: &'a str,
    supervised: bool,
    data_dir: &'a str,
    exe: &'a str,
    config: &'a str,
}

/// The name the record gives the service manager of `target`.
fn manager_name(target: Target) -> &'static str {
    match target {
        Target::SystemdUser | Target::SystemdSystem => "systemd",
        Target::LaunchAgent | Target::LaunchDaemon => "launchd",
        Target::ScheduledTask => "task",
        Target::WindowsService => "scm",
    }
}

fn record_text(target: Target, definition: &Definition, data_dir: &str) -> String {
    let record = ServiceRecord {
        format: 1,
        kind: if target.system() { "system" } else { "user" },
        manager: manager_name(target),
        supervised: definition.data_dir.is_some(),
        data_dir,
        exe: &definition.exe,
        config: &definition.config,
    };
    let mut text = serde_json::to_string_pretty(&record).unwrap_or_default();
    text.push('\n');
    text
}

/// The steps that install `definition` as `target`. A user service, or a
/// supervised system service, records the install in `record` last, once
/// the definition is in place and the service is started.
fn install_plan(
    system: &mut dyn System,
    context: &Context,
    target: Target,
    definition: &Definition,
    account: &Account,
    record: Option<&str>,
) -> Result<Plan, String> {
    let platform = context.platform;
    let mut plan = Plan::default();
    if let Some(path) = target.definition_path(context)?
        && let Some(dir) = platform.parent(&path)
        && !system.exists(&dir)
    {
        plan.steps.push(Step::CreateDir(dir));
    }
    match target {
        Target::SystemdUser | Target::SystemdSystem => {
            let path = systemd::unit_path(context, target.system())?;
            plan.steps
                .extend(systemd::install_steps(&path, definition, target.system()));
        }
        Target::LaunchAgent | Target::LaunchDaemon => {
            let path = launchd::plist_path(context, target.system())?;
            let log = launchd::log_path(context, target.system())?;
            if let Some(dir) = platform.parent(&log)
                && !system.exists(&dir)
            {
                plan.steps.push(Step::CreateDir(dir));
            }
            let domain = launchd::domain(target.system(), account.uid());
            plan.steps
                .extend(launchd::install_steps(&path, &domain, definition, &log));
        }
        Target::ScheduledTask => {
            let temp = context
                .var("TEMP")
                .or_else(|| context.var("TMP"))
                .ok_or("TEMP isn't set, so there is nowhere to write the task's definition")?;
            let file = platform.join(temp, "open-ferry-task.xml");
            let sid = windows::user_sid(system)?;
            plan.steps
                .extend(windows::task_install_steps(&file, definition, &sid));
            plan.cleanup.push(file);
        }
        Target::WindowsService => {
            plan.steps
                .extend(windows::service_install_steps(definition));
        }
    }
    if (!target.system() || definition.data_dir.is_some())
        && let Some(data_dir) = record
    {
        plan.steps.push(Step::WriteAtomic {
            path: platform.join(data_dir, RECORD_FILE),
            text: record_text(target, definition, data_dir),
        });
    }
    Ok(plan)
}

/// What to say once `install` is done.
pub(crate) fn installed_notes(
    context: &Context,
    target: Target,
    definition: &Definition,
) -> String {
    let windows_log = || context.platform.join(&definition.dir, windows::LOG_FILE);
    match target {
        Target::SystemdUser | Target::SystemdSystem => systemd::notes(target.system()),
        Target::LaunchAgent | Target::LaunchDaemon => {
            let log = launchd::log_path(context, target.system()).unwrap_or_default();
            launchd::notes(target.system(), &log)
        }
        Target::ScheduledTask => windows::task_notes(&windows_log()),
        Target::WindowsService => windows::service_notes(&windows_log()),
    }
}

pub(crate) fn uninstall(
    system: &mut dyn System,
    context: &Context,
    target: Target,
    dry_run: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let platform = context.platform;
    if !target.installed(system, context)? {
        return Err(not_installed(context, target)?);
    }
    let account = Account::probe(system, platform, target)?;
    if target.system() && !account.privileged() && !dry_run {
        let command = format!("{} {NAME} uninstall -system", platform.quote(&context.exe));
        return Err(needs_admin(platform, "Removing", &command));
    }
    let mut steps = match target {
        Target::SystemdUser | Target::SystemdSystem => systemd::uninstall_steps(
            &systemd::unit_path(context, target.system())?,
            target.system(),
        ),
        Target::LaunchAgent | Target::LaunchDaemon => launchd::uninstall_steps(
            &launchd::plist_path(context, target.system())?,
            &launchd::domain(target.system(), account.uid()),
        ),
        Target::ScheduledTask => windows::task_uninstall_steps(),
        Target::WindowsService => windows::service_uninstall_steps(),
    };
    let mut stop_check = match target {
        Target::SystemdSystem => Some(systemd::stop_check(true)),
        Target::LaunchDaemon => Some(launchd::stop_check("system")),
        _ => None,
    };
    let mut wait_for_scm = false;
    if target.system() {
        let found = find_system_install(system, context, target);
        if let Some(why) = &found.ignored {
            say(out, format_args!("{why}"));
        }
        wait_for_scm = target == Target::WindowsService && found.stop;
        if let Some(dir) = found.data_dir {
            say(
                out,
                format_args!("The service's data directory is {dir} ({}).", found.source),
            );
            if wait_for_scm {
                steps.insert(
                    0,
                    Step::StopSupervisor {
                        data_dir: dir,
                        kind: ServiceKind::System,
                    },
                );
            }
        }
        if let Some(record) = found.record {
            steps.push(Step::Remove(record));
        }
    }
    if !target.system() {
        let found = find_install(system, context, target);
        if let Some(why) = &found.ignored {
            say(out, format_args!("{why}"));
        }
        if let Some(dir) = &found.data_dir
            && (found.stop || found.record.is_some())
        {
            say(
                out,
                format_args!("The service's data directory is {dir} ({}).", found.source),
            );
        }
        if let Some(why) = &found.missing
            && found.stop
            && target == Target::ScheduledTask
        {
            say(
                out,
                format_args!(
                    "There is no data directory to ask a supervisor in ({why}), so none is asked to stop."
                ),
            );
        }
        // Only a task needs the supervisor asked to stop: the task's end
        // would otherwise cut it off mid-stop. systemd and launchd stop the
        // whole unit or job themselves, within `TimeoutStopSec` and
        // `ExitTimeOut`.
        if found.stop
            && target == Target::ScheduledTask
            && let Some(dir) = &found.data_dir
        {
            steps.insert(
                0,
                Step::StopSupervisor {
                    data_dir: dir.clone(),
                    kind: ServiceKind::User,
                },
            );
        }
        // A user service that may run the supervisor must be known to be
        // stopped before anything is removed: where `disable --now` or
        // `bootout` fails, the manager must say the service is not there.
        // (A dry run still lists the plain step.)
        if found.stop {
            stop_check = match target {
                Target::SystemdUser => Some(systemd::stop_check(false)),
                Target::LaunchAgent => {
                    Some(launchd::stop_check(&launchd::domain(false, account.uid())))
                }
                _ => None,
            };
        }
        // Last, so a failed uninstall keeps the record that names the data
        // directory. Only a record found to be this service's is removed.
        if let Some(record) = found.record {
            steps.push(Step::Remove(record));
        }
    }
    let mut plan = Plan {
        steps,
        cleanup: Vec::new(),
    };
    if dry_run {
        if wait_for_scm {
            let before_delete = scm_delete_index(&plan.steps);
            describe_steps(&plan.steps[..before_delete], out);
            say(
                out,
                format_args!(
                    "Would wait up to {} s for sc.exe query open-ferry to say STOPPED",
                    scm_stop_budget().as_secs()
                ),
            );
            describe_steps(&plan.steps[before_delete..], out);
            say(out, format_args!("Dry run: nothing was changed."));
        } else {
            describe(&plan, out);
        }
        return Ok(());
    }
    if let Some(check) = &stop_check {
        // The checked stop takes the plain stop's place.
        plan.steps
            .retain(|step| *step != Step::TryRun(check.stop.clone()));
        stop_checked(system, check, out)?;
    }
    if wait_for_scm {
        let before_delete = scm_delete_index(&plan.steps);
        for step in plan.steps.drain(..before_delete) {
            apply_step(system, &step, out)?;
        }
        wait_service_stopped(system, out)?;
    }
    apply(system, &plan, out).map_err(|failed| failed.message)?;
    say(
        out,
        format_args!(
            "open-ferry is no longer installed as {}. The config and the auth directory are left as they are.",
            target.kind()
        ),
    );
    Ok(())
}

fn scm_delete_index(steps: &[Step]) -> usize {
    steps.iter().position(|step| matches!(step, Step::Run(cmd) if cmd.program == "sc.exe" && cmd.args.first().is_some_and(|arg| arg == "delete"))).unwrap_or(steps.len())
}

fn scm_stop_budget() -> Duration {
    STOP_BUDGET + Duration::from_secs(10)
}

/// SCM's stop command only asks: keep the service and record until its
/// query positively says STOPPED. The accounted pauses bound fake runs too.
fn wait_service_stopped(system: &mut dyn System, out: &mut dyn Write) -> Result<(), String> {
    let started = Instant::now();
    wait_service_stopped_with_clock(system, out, || started.elapsed())
}

fn wait_service_stopped_with_clock(
    system: &mut dyn System,
    out: &mut dyn Write,
    elapsed: impl Fn() -> Duration,
) -> Result<(), String> {
    let budget = scm_stop_budget();
    let mut waited = Duration::ZERO;
    loop {
        let cmd = windows::query_service();
        let state = match system.run(&cmd) {
            Ok(answer) => windows_service_state(&cmd, &answer),
            Err(error) => ServiceState::Unknown(error.to_string()),
        };
        let elapsed = elapsed().max(waited);
        if state == ServiceState::Stopped {
            say(out, format_args!("The Windows service is STOPPED."));
            return Ok(());
        }
        if elapsed >= budget {
            return Err(format!(
                "Uninstall incomplete: the Windows service is {} after {} s; the service and service.json were kept. Stop it by hand, then run the uninstall again.",
                state.describe(),
                budget.as_secs()
            ));
        }
        let pause = Duration::from_secs(1).min(budget - elapsed);
        system.pause(pause);
        waited += pause;
    }
}

/// What an installed service's definition says of how it runs.
#[derive(Debug, Default, PartialEq, Eq)]
struct Flagged {
    /// Whether it runs the supervisor.
    supervised: bool,
    /// The `-data-dir` it pins.
    data_dir: Option<String>,
    /// The binary it runs, when the definition says.
    exe: Option<String>,
    /// The config it names with `-config`.
    config: Option<String>,
}

/// What `-supervise`, `-data-dir` and `-config` say in a definition's
/// command line, and `exe` is the binary it runs.
fn flagged(words: &[String], exe: Option<String>) -> Flagged {
    let after = |flag: &str| {
        words
            .iter()
            .position(|word| word == flag)
            .and_then(|index| words.get(index + 1))
            .cloned()
    };
    Flagged {
        supervised: words.iter().any(|word| word == "-supervise"),
        data_dir: after("-data-dir"),
        exe,
        config: after("-config"),
    }
}

/// `service.json`, read back.
#[derive(Debug, PartialEq, Eq)]
struct Record {
    manager: String,
    supervised: bool,
    data_dir: String,
    exe: String,
    config: String,
}

/// The record in `bytes`, if it is a user service's. Its paths are as
/// written; `record_rejected` says whether they are full paths.
fn parse_record(bytes: &[u8]) -> Option<Record> {
    parse_record_kind(bytes, "user")
}

fn parse_record_kind(bytes: &[u8], kind: &str) -> Option<Record> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if value["format"].as_u64() != Some(1) || value["kind"].as_str() != Some(kind) {
        return None;
    }
    Some(Record {
        manager: value["manager"].as_str()?.to_owned(),
        supervised: value["supervised"].as_bool()?,
        data_dir: value["data_dir"].as_str()?.to_owned(),
        exe: value["exe"].as_str()?.to_owned(),
        config: value["config"].as_str()?.to_owned(),
    })
}

/// Whether two paths are the same as `platform` compares them.
fn same_path(platform: Platform, a: &str, b: &str) -> bool {
    let (a, b) = (platform.clean(a), platform.clean(b));
    match platform {
        Platform::Windows | Platform::MacOs => a.eq_ignore_ascii_case(&b),
        Platform::Linux => a == b,
    }
}

/// Why `record`, found in `held_in`, is not the installed service's: it
/// must give full paths, name the directory it is in, be its manager's, and,
/// where the definition can be read, name its binary and config (full paths
/// too). `None` when it is.
fn record_rejected(
    platform: Platform,
    record: &Record,
    held_in: &str,
    target: Target,
    defined: Option<&Flagged>,
) -> Option<String> {
    for (what, path) in [
        ("data directory", &record.data_dir),
        ("binary", &record.exe),
        ("config", &record.config),
    ] {
        if !platform.is_absolute(path) {
            return Some(format!("its {what}, {path}, isn't a full path"));
        }
    }
    if !same_path(platform, &record.data_dir, held_in) {
        return Some(format!(
            "it names {}, not the directory it is in",
            record.data_dir
        ));
    }
    if record.manager != manager_name(target) {
        return Some(format!(
            "it is of a {} service, not a {} one",
            record.manager,
            manager_name(target)
        ));
    }
    let defined = defined?;
    for (what, recorded, installed) in [
        ("binary", &record.exe, &defined.exe),
        ("config", &record.config, &defined.config),
    ] {
        let Some(installed) = installed else {
            return Some(format!(
                "the installed service's definition doesn't name its {what}"
            ));
        };
        if !platform.is_absolute(installed) {
            return Some(format!(
                "the installed service's {what}, {installed}, isn't a full path"
            ));
        }
        if !same_path(platform, recorded, installed) {
            return Some(format!(
                "it names {recorded} as the {what}, but the installed service runs {installed}"
            ));
        }
    }
    None
}

/// What the installed service's own definition says: the unit, the plist,
/// the task's XML, or SCM's registered command line.
enum Installed {
    /// There is none to read: no unit or plist file, or a kind that isn't
    /// looked at.
    Absent,
    /// A unit or plist file is there and can't be read or has no command
    /// line, or the installed task's XML can't be queried. Nothing is known
    /// of how the service runs, and no record can be checked against it.
    Unreadable,
    Read(Flagged),
}

fn installed_flags(system: &mut dyn System, context: &Context, target: Target) -> Installed {
    // A unit's and a plist's first word is the binary; a task's is its own
    // element.
    /// `None` when there is no file; else its text, or `None` inside when
    /// it can't be read.
    fn file(system: &dyn System, path: Result<String, String>) -> Option<Option<String>> {
        let path = path.ok()?;
        system.exists(&path).then(|| {
            system
                .read(&path)
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        })
    }
    let (words, exe) = match target {
        Target::SystemdUser | Target::SystemdSystem => {
            let path = systemd::unit_path(context, target.system());
            // A drop-in directory beside the unit can change its `ExecStart`.
            let drop_in = path
                .as_ref()
                .is_ok_and(|path| system.exists(&format!("{path}.d")));
            let text = match file(system, path) {
                None => return Installed::Absent,
                Some(text) => text,
            };
            if drop_in {
                return Installed::Unreadable;
            }
            let Some(words) = text.and_then(|text| systemd::exec_words(&text)) else {
                return Installed::Unreadable;
            };
            let exe = words.first().cloned();
            (words, exe)
        }
        Target::LaunchAgent | Target::LaunchDaemon => {
            let text = match file(system, launchd::plist_path(context, target.system())) {
                None => return Installed::Absent,
                Some(text) => text,
            };
            let Some(words) = text.and_then(|text| launchd::program_arguments(&text)) else {
                return Installed::Unreadable;
            };
            let exe = words.first().cloned();
            (words, exe)
        }
        Target::ScheduledTask => {
            // `installed` has confirmed the task, so a query that fails now
            // leaves its definition unknown, not absent. No record can then
            // be checked, and the stop asks the supervisor of the
            // environment's data directory, which may not be the task's own.
            let Ok(output) = system.run(&windows::task_xml_query()) else {
                return Installed::Unreadable;
            };
            if !output.success() {
                return Installed::Unreadable;
            }
            // Only a task of our shape is read; any other is unreadable.
            let Some(definition) = windows::task_definition(&output.stdout) else {
                return Installed::Unreadable;
            };
            definition
        }
        Target::WindowsService => {
            let Ok(answer) = system.run(&windows::service_config_query()) else {
                return Installed::Unreadable;
            };
            let Some(words) = answer
                .success()
                .then(|| windows::service_definition(&answer.stdout))
                .flatten()
            else {
                return Installed::Unreadable;
            };
            let exe = words.first().cloned();
            (words, exe)
        }
    };
    Installed::Read(flagged(&words, exe))
}

/// What `uninstall` found of a user service's data directory.
struct Found {
    /// The data directory, when anything names it.
    data_dir: Option<String>,
    /// Where `data_dir` came from, for the message.
    source: String,
    /// Whether a supervisor may be running: the definition or the record
    /// says so, or neither can be read.
    stop: bool,
    /// Why there is no data directory.
    missing: Option<String>,
    /// The `service.json` found to be this service's: the only file
    /// `uninstall` removes.
    record: Option<String>,
    /// What to say of a `service.json` that is there and isn't.
    ignored: Option<String>,
}

/// System uninstalls use only the directory their definition pins, never
/// the environment or an unchecked record. Check trust before looking in it.
fn find_system_install(system: &mut dyn System, context: &Context, target: Target) -> Found {
    let mut found = Found {
        data_dir: None,
        source: "from its definition".to_owned(),
        stop: true,
        missing: None,
        record: None,
        ignored: None,
    };
    let Installed::Read(defined) = installed_flags(system, context, target) else {
        return found;
    };
    found.stop = defined.supervised;
    let Some(dir) = &defined.data_dir else {
        return found;
    };
    let trust = if context.platform.is_absolute(dir) {
        system.trusted_dir(dir)
    } else {
        Err("it isn't a full path".to_owned())
    };
    if let Err(why) = trust {
        found.ignored = Some(format!(
            "Not using the service's data directory {dir}: {why}. Its records are left alone and no supervisor is asked to stop through it."
        ));
        return found;
    }
    found.data_dir = Some(dir.clone());
    let path = context.platform.join(dir, RECORD_FILE);
    if !system.exists(&path) {
        return found;
    }
    let checked = system.trusted_file(&path).and_then(|()| {
        let bytes = system.read(&path).map_err(|error| error.to_string())?;
        let record = parse_record_kind(&bytes, "system")
            .ok_or("it isn't a system service's record in format 1")?;
        if let Some(why) = record_rejected(context.platform, &record, dir, target, Some(&defined)) {
            return Err(why);
        }
        if !record.supervised || !defined.supervised {
            return Err("it isn't the supervised service's record".to_owned());
        }
        Ok(())
    });
    match checked {
        Ok(()) => found.record = Some(path),
        Err(why) => found.ignored = Some(format!("Leaving {path} alone and not using it: {why}.")),
    }
    found
}

/// Finds the data directory of the installed user service: the `-data-dir`
/// of its own definition first, else the `service.json` in the directory
/// the environment gives, else that directory itself. The record looked at
/// is the one in the directory the definition names, when it names one (the
/// environment's directory then isn't this service's), else the
/// environment's.
fn find_install(system: &mut dyn System, context: &Context, target: Target) -> Found {
    let platform = context.platform;
    let computed = data_dir(context);
    let installed = installed_flags(system, context, target);
    let unreadable = matches!(installed, Installed::Unreadable);
    let defined = match installed {
        Installed::Read(flagged) => Some(flagged),
        Installed::Absent | Installed::Unreadable => None,
    };
    let named = defined
        .as_ref()
        .and_then(|flagged| flagged.data_dir.clone());
    let record_dir = match &named {
        // A directory that isn't a full path can't be told from another.
        Some(dir) => Some(dir.clone()).filter(|dir| platform.is_absolute(dir)),
        None => computed.as_ref().ok().cloned(),
    };
    let record_path = record_dir
        .as_ref()
        .map(|dir| platform.join(dir, RECORD_FILE));
    let mut ignored = None;
    let mut record = None;
    if let (Some(path), Some(held_in)) = (record_path.as_deref(), record_dir.as_deref())
        && system.exists(path)
    {
        let why = match system.read(path) {
            Err(error) => Some(format!("it can't be read: {error}")),
            Ok(bytes) => match parse_record(&bytes) {
                None => Some("it isn't a user service's record in format 1".to_owned()),
                // A definition that is there and can't be read says nothing
                // of the service, so nothing can be checked against it.
                Some(_) if unreadable => Some(
                    "the installed service's definition can't be read, so the record can't be checked against it"
                        .to_owned(),
                ),
                Some(parsed) => {
                    let why = record_rejected(platform, &parsed, held_in, target, defined.as_ref());
                    if why.is_none() {
                        record = Some(parsed);
                    }
                    why
                }
            },
        };
        if let Some(why) = why {
            ignored = Some(format!("Leaving {path} alone and not using it: {why}."));
        }
    }
    let supervised = defined
        .as_ref()
        .map(|flagged| flagged.supervised)
        .or_else(|| record.as_ref().map(|record| record.supervised));
    let stop = supervised != Some(false);
    let record_file = record.as_ref().and_then(|_| record_path.clone());
    let (data_dir, source) = match (named, record, computed) {
        // A relative `-data-dir` is read from whatever directory this runs
        // in, so it is never a place to ask a supervisor to stop, and
        // neither the record nor the environment's directory stands in for
        // it. (Nor can the task be running a supervisor from it: the
        // startup parser refuses a relative `-data-dir`.)
        (Some(dir), _, _) if !platform.is_absolute(&dir) => {
            return Found {
                data_dir: None,
                source: String::new(),
                stop,
                missing: Some(format!(
                    "its definition names {dir} as its data directory, which isn't a full path"
                )),
                record: None,
                ignored,
            };
        }
        (Some(dir), _, _) => (Some(dir), "from its definition".to_owned()),
        (None, Some(record), _) => (
            Some(platform.clean(&record.data_dir)),
            format!("from {}", record_path.unwrap_or_default()),
        ),
        (None, None, Ok(dir)) if unreadable && target == Target::ScheduledTask => (
            Some(dir),
            "worked out from the environment because the task's definition couldn't be read, so the supervisor of this directory is asked to stop, which may not be the task's own".to_owned(),
        ),
        (None, None, Ok(dir)) => (
            Some(dir),
            "worked out from the environment, as nothing else names it".to_owned(),
        ),
        (None, None, Err(error)) => {
            return Found {
                data_dir: None,
                source: String::new(),
                stop,
                missing: Some(error),
                record: record_file,
                ignored,
            };
        }
    };
    Found {
        data_dir,
        source,
        stop,
        missing: None,
        record: record_file,
        ignored,
    }
}

/// The message for a service that isn't installed.
fn not_installed(context: &Context, target: Target) -> Result<String, String> {
    let other = if target.system() {
        "your own service, leave out -system"
    } else {
        "the whole machine's service, add -system"
    };
    Ok(format!(
        "open-ferry isn't installed as {} ({} doesn't exist). For {other}.",
        target.kind(),
        target.location(context)?
    ))
}

fn status(
    system: &mut dyn System,
    context: &Context,
    target: Target,
    out: &mut dyn Write,
) -> Result<(), String> {
    if !target.installed(system, context)? {
        return Err(not_installed(context, target)?);
    }
    say(
        out,
        format_args!(
            "open-ferry is installed as {}: {}",
            target.kind(),
            target.location(context)?
        ),
    );
    let cmd = match target {
        Target::SystemdUser | Target::SystemdSystem => systemd::status(target.system()),
        Target::LaunchAgent | Target::LaunchDaemon => {
            let account = Account::probe(system, context.platform, target)?;
            launchd::status(&launchd::domain(target.system(), account.uid()))
        }
        Target::ScheduledTask => windows::task_status(),
        Target::WindowsService => windows::query_service(),
    };
    let _ = out.flush();
    // What the service manager says is shown whatever its exit code:
    // `systemctl status`, for one, fails for a service that isn't running.
    system
        .show(&cmd)
        .map_err(|error| format!("failed to run `{cmd}`: {error}"))?;
    Ok(())
}
