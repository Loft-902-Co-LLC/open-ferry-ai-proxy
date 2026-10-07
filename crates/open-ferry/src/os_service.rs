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

mod launchd;
#[cfg(windows)]
mod run;
mod systemd;
#[cfg(test)]
mod tests;
mod windows;

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::process::{ExitCode, Stdio};

use open_ferry_core::config::{Config, DEFAULT_AUTH_DIR};

use crate::flags::{self, FlagError, Kind};
use crate::installed;

/// The subcommand's name: the first argument that runs it.
pub const NAME: &str = "service";

/// The service's name: the systemd unit's, the scheduled task's and the
/// Windows service's.
const SERVICE_NAME: &str = "open-ferry";

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
    let mut out = io::stdout().lock();
    match execute(&mut Host, &context, &request, &mut out) {
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
    run::main(config, request.system)
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
            Action::Run => &[CONFIG, SYSTEM],
        }
    }
}

/// The command line after `service`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    action: Action,
    /// `-config`, as given.
    config: Option<String>,
    /// `-system`.
    system: bool,
    /// `-dry-run`.
    dry_run: bool,
}

/// What the flags ask for.
#[derive(Default)]
struct Options {
    config: String,
    system: bool,
    dry_run: bool,
}

const CONFIG: flags::Definition<Options> = flags::Definition {
    name: "config",
    usage: "The config the service runs with (default: the installed config path, shown below)",
    kind: Kind::String(|options, value| options.config = value),
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
    Ok(Request {
        action,
        config: Some(options.config).filter(|config| !config.is_empty()),
        system: options.system,
        dry_run: options.dry_run,
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
enum Platform {
    Linux,
    MacOs,
    Windows,
}

impl Platform {
    /// The platform this build runs on, if `service` supports it.
    const HOST: Option<Platform> = if cfg!(target_os = "linux") {
        Some(Platform::Linux)
    } else if cfg!(target_os = "macos") {
        Some(Platform::MacOs)
    } else if cfg!(windows) {
        Some(Platform::Windows)
    } else {
        None
    };

    fn separator(self) -> char {
        match self {
            Platform::Windows => '\\',
            Platform::Linux | Platform::MacOs => '/',
        }
    }

    fn is_separator(self, c: char) -> bool {
        c == '/' || (self == Platform::Windows && c == '\\')
    }

    /// Whether `path` is a full path: from `/`, or on Windows from a drive's
    /// root or a UNC share.
    fn is_absolute(self, path: &str) -> bool {
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
    fn absolute(self, cwd: &str, path: &str) -> Result<String, String> {
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
    fn clean(self, path: &str) -> String {
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
    fn join(self, dir: &str, name: &str) -> String {
        if dir.ends_with(|c| self.is_separator(c)) {
            format!("{dir}{name}")
        } else {
            format!("{dir}{}{name}", self.separator())
        }
    }

    /// The directory of the full path `path`, or `None` at the root.
    fn parent(self, path: &str) -> Option<String> {
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
    fn is_within(self, path: &str, dir: &str) -> bool {
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
    fn quote(self, word: &str) -> String {
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
struct Context {
    platform: Platform,
    /// The running binary's full path.
    exe: String,
    /// The working directory.
    cwd: String,
    /// The environment variables of [`ENV_VARS`] that are set and not empty.
    env: BTreeMap<String, String>,
    /// The installed config path, or why there is none: the config when
    /// `-config` isn't given.
    installed_config: Result<String, String>,
}

/// The environment variables `service` reads.
const ENV_VARS: [&str; 8] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "TEMP",
    "TMP",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "SystemRoot",
];

impl Context {
    /// This process's.
    fn host() -> Result<Context, String> {
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
        })
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }

    fn home(&self) -> Result<&str, String> {
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
struct Owner {
    uid: u32,
    gid: u32,
    mode: u32,
    dir: bool,
}

/// A command `service` runs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cmd {
    program: &'static str,
    args: Vec<String>,
}

impl Cmd {
    fn new(program: &'static str, args: &[&str]) -> Cmd {
        Cmd {
            program,
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    fn arg(mut self, arg: impl Into<String>) -> Cmd {
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
struct Output {
    /// The exit code, or `None` when a signal ended it.
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Output {
    fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// How it failed, for a message.
    fn failure(&self) -> String {
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
trait System {
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
}

/// The real system.
struct Host;

impl System for Host {
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
        let output = std::process::Command::new(cmd.program)
            .args(&cmd.args)
            .stdin(Stdio::null())
            .output()?;
        Ok(Output {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
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

    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn real_path(&self, path: &str) -> io::Result<String> {
        std::fs::canonicalize(path)?
            .into_os_string()
            .into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not valid UTF-8"))
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
}

/// Runs `cmd`, and fails unless it succeeds.
fn run_checked(system: &mut dyn System, cmd: &Cmd) -> Result<Output, String> {
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
enum Step {
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
}

/// What `install` or `uninstall` does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Plan {
    steps: Vec<Step>,
    /// Files removed once the steps are done, or have failed.
    cleanup: Vec<String>,
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
struct Failed {
    message: String,
    /// Whether a step before the failed one changed something.
    changed: bool,
}

/// Carries out `plan`, saying what it did.
fn apply(system: &mut dyn System, plan: &Plan, out: &mut dyn Write) -> Result<(), Failed> {
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
    }
    Ok(())
}

/// Prints what `plan` would do.
fn describe(plan: &Plan, out: &mut dyn Write) {
    for step in &plan.steps {
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
        }
    }
    for path in &plan.cleanup {
        say(out, format_args!("Would remove {path} afterwards"));
    }
    say(out, format_args!("Dry run: nothing was changed."));
}

/// Prints a line; a closed standard output doesn't stop the work.
fn say(out: &mut dyn Write, line: fmt::Arguments<'_>) {
    let _ = writeln!(out, "{line}");
}

/// Which service manager, and whose service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    SystemdUser,
    SystemdSystem,
    LaunchAgent,
    LaunchDaemon,
    ScheduledTask,
    WindowsService,
}

impl Target {
    fn new(platform: Platform, system: bool) -> Target {
        match (platform, system) {
            (Platform::Linux, false) => Target::SystemdUser,
            (Platform::Linux, true) => Target::SystemdSystem,
            (Platform::MacOs, false) => Target::LaunchAgent,
            (Platform::MacOs, true) => Target::LaunchDaemon,
            (Platform::Windows, false) => Target::ScheduledTask,
            (Platform::Windows, true) => Target::WindowsService,
        }
    }

    fn system(self) -> bool {
        matches!(
            self,
            Target::SystemdSystem | Target::LaunchDaemon | Target::WindowsService
        )
    }

    /// `-system` when it is the machine's, for the commands `service`
    /// suggests.
    fn flag(self) -> &'static str {
        if self.system() { " -system" } else { "" }
    }

    fn kind(self) -> &'static str {
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
    fn location(self, context: &Context) -> Result<String, String> {
        Ok(match self.definition_path(context)? {
            Some(path) => path,
            None if self == Target::ScheduledTask => format!("the task named {SERVICE_NAME}"),
            None => format!("the service named {SERVICE_NAME}"),
        })
    }

    /// Whether the service is installed.
    fn installed(self, system: &mut dyn System, context: &Context) -> Result<bool, String> {
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
            // The task scheduler says only that it failed; the service
            // manager that the service doesn't exist.
            _ if self == Target::ScheduledTask => Ok(false),
            Some(windows::SERVICE_DOES_NOT_EXIST) => Ok(false),
            _ => Err(format!("`{cmd}` failed with {}", output.failure())),
        }
    }
}

/// Who runs `service`, as far as it matters.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Account {
    /// On Unix: the user ID.
    Unix { uid: u32 },
    /// On Windows: whether the process is elevated.
    Windows { elevated: bool },
}

impl Account {
    fn probe(
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
    fn privileged(&self) -> bool {
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

/// What a service runs: `exe -config config`, in `dir`, the config's
/// directory.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Definition {
    exe: String,
    config: String,
    dir: String,
}

/// Carries out `request`.
fn execute(
    system: &mut dyn System,
    context: &Context,
    request: &Request,
    out: &mut dyn Write,
) -> Result<(), String> {
    let target = Target::new(context.platform, request.system);
    match request.action {
        Action::Install => install(system, context, target, request, out),
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
    out: &mut dyn Write,
) -> Result<(), String> {
    let platform = context.platform;
    let config = match &request.config {
        Some(path) => platform.absolute(&context.cwd, path)?,
        None => platform.absolute(&context.cwd, context.installed_config.as_ref()?)?,
    };
    check_path(platform, "The open-ferry binary's path", &context.exe)?;
    check_path(platform, "The config's path", &config)?;
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
    if target.system() && !account.privileged() && !request.dry_run {
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
        protected(system, context, &config)?
    } else {
        Definition {
            exe: context.exe.clone(),
            dir: platform.parent(&config).unwrap_or_else(|| config.clone()),
            config,
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
    let plan = install_plan(system, context, target, &definition, &account)?;
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
) -> Result<Definition, String> {
    let platform = context.platform;
    let (exe, config) = match platform {
        Platform::Windows => {
            let roots: Vec<&str> = [
                "ProgramFiles",
                "ProgramFiles(x86)",
                "ProgramW6432",
                "SystemRoot",
            ]
            .into_iter()
            .filter_map(|name| context.var(name))
            .filter(|dir| platform.is_absolute(dir))
            .collect();
            let program_files = context.var("ProgramFiles").unwrap_or(r"C:\Program Files");
            for path in [&context.exe, config] {
                if !roots.iter().any(|root| platform.is_within(path, root)) {
                    return Err(format!(
                        "A Windows service runs as LocalSystem, so open-ferry's binary and its config must be where only administrators can change them, such as {program_files}; {path} isn't. Copy them to {}, and pass -config.",
                        platform.join(program_files, SERVICE_NAME)
                    ));
                }
            }
            (context.exe.clone(), config.to_owned())
        }
        Platform::Linux | Platform::MacOs => {
            let real = |system: &mut dyn System, path: &str| {
                system
                    .real_path(path)
                    .map_err(|error| format!("failed to resolve {path}: {error}"))
            };
            let exe = real(system, &context.exe)?;
            let config = real(system, config)?;
            for path in [&exe, &config] {
                if let Some(problem) = unprotected(system, platform, path)? {
                    return Err(format!(
                        "A system service runs as root, so open-ferry's binary and its config, and the directories above them, must be owned by root and writable by no one else; {problem}. Copy them where only root can change them, such as /usr/local/bin/open-ferry and /etc/open-ferry/config.yaml, and pass -config."
                    ));
                }
            }
            (exe, config)
        }
    };
    Ok(Definition {
        exe,
        dir: platform.parent(&config).unwrap_or_else(|| config.clone()),
        config,
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

/// The steps that install `definition` as `target`.
fn install_plan(
    system: &mut dyn System,
    context: &Context,
    target: Target,
    definition: &Definition,
    account: &Account,
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
    Ok(plan)
}

/// What to say once `install` is done.
fn installed_notes(context: &Context, target: Target, definition: &Definition) -> String {
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

fn uninstall(
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
    let steps = match target {
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
    let plan = Plan {
        steps,
        cleanup: Vec::new(),
    };
    if dry_run {
        describe(&plan, out);
        return Ok(());
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
