//! Finding CLIProxyAPI: its process, its binary, its command line, its
//! working directory, and what starts it.
//!
//! The binary is named as upstream's builds name it: `cli-proxy-api`
//! (`cli-proxy-api.exe` on Windows) in the release archives, `CLIProxyAPI`
//! in its Docker image, and `cliproxyapi` from Homebrew's formula. The
//! search looks for a running process of one of those names first. Then,
//! for what starts it:
//! - **Linux:** the process's control group names its systemd unit, user or
//!   system, or a container. The unit is a service switch when its main
//!   process is CLIProxyAPI; otherwise, as with cron, it is a launcher.
//!   With no process, the units upstream's community installers make
//!   (`cliproxyapi.service`, the AUR's `cli-proxy-api.service`) are looked
//!   up, the user's and the system's.
//! - **macOS:** the launchd plists in `~/Library/LaunchAgents`,
//!   `/Library/LaunchAgents` and `/Library/LaunchDaemons` whose program is
//!   the binary, Homebrew's `homebrew.mxcl.cliproxyapi` (`brew services`)
//!   among them, when `launchctl` says the job runs the process.
//! - **Windows:** a service that the process, or its parent when that is
//!   NSSM, runs; else a scheduled task whose program, arguments and working
//!   directory are the process's (more than one is a blocker), or a task
//!   whose arguments or script mention it, which is a launcher. With no
//!   process, the services `cliproxyapi`, `CLIProxyAPI` and `cli-proxy-api`
//!   (an NSSM service's command line is read from its parameters), and the
//!   tasks.
//! - **A container** from CLIProxyAPI's image (`eceasy/cli-proxy-api`),
//!   asked of Docker when no process is found, or when the process is in a
//!   container.
//! - **Anything else** is a launcher: the parent process, named, or one
//!   that has since exited.
//!
//! The command line is read for `-config` only, and for the names of the
//! other flags; their values are never kept.

use std::collections::BTreeMap;

use serde_json::Value;

use super::machine::{Machine, Proc};
use crate::os_service::{Cmd, Context, Platform, decode_output, run_checked, user_identity};

/// The file names of CLIProxyAPI's binary, compared in any case.
pub(crate) const BINARY_NAMES: [&str; 4] = [
    "cli-proxy-api",
    "cli-proxy-api.exe",
    "cliproxyapi",
    "cliproxyapi.exe",
];

/// What a launcher's arguments or script mention when it starts
/// CLIProxyAPI.
const MENTIONS: [&str; 2] = ["cli-proxy-api", "cliproxyapi"];

/// The systemd units upstream's community installers make.
const KNOWN_UNITS: [&str; 2] = ["cliproxyapi.service", "cli-proxy-api.service"];

/// Homebrew's `brew services` label for the `cliproxyapi` formula.
pub(crate) const BREW_LABEL: &str = "homebrew.mxcl.cliproxyapi";

/// The Windows service names looked up when nothing runs.
const KNOWN_SERVICES: [&str; 3] = ["cliproxyapi", "CLIProxyAPI", "cli-proxy-api"];

/// The one service wrapper `migrate` knows: NSSM, by its image name or the
/// binary of its service.
const NSSM: &str = "nssm.exe";

/// Whether `name` is NSSM's file name.
fn is_nssm(name: &str) -> bool {
    name.eq_ignore_ascii_case(NSSM)
}

/// The scripts a launcher task may run.
const SCRIPT_EXTENSIONS: [&str; 8] = [".ps1", ".psm1", ".bat", ".cmd", ".vbs", ".js", ".sh", ".py"];

/// The most of a launcher script that is read.
const SCRIPT_LIMIT: usize = 1 << 20;

/// What starts CLIProxyAPI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Starter {
    /// A systemd unit whose main process it is.
    Systemd {
        unit: String,
        user: bool,
        enabled: bool,
        active: bool,
    },
    /// A launchd job that runs it, Homebrew's among them.
    Launchd {
        label: String,
        plist: String,
        domain: String,
        brew: bool,
        loaded: bool,
    },
    /// A Windows service that runs it, itself or through a wrapper.
    WindowsService {
        name: String,
        /// `sc config`'s `start=` value: `auto`, `delayed-auto`, `demand`
        /// or `disabled`.
        start: String,
        running: bool,
        /// The wrapper's file name, when the service runs one.
        wrapper: Option<String>,
    },
    /// A scheduled task that runs the binary itself.
    Task { name: String, enabled: bool },
    /// Something `migrate` can't switch the service of: a drop-in.
    Launcher(Launcher),
    /// A container.
    Container(Container),
}

/// What starts CLIProxyAPI when no service manager runs it directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Launcher {
    /// A scheduled task that runs a launcher, `program`: a script, or
    /// another program.
    Task { name: String, program: String },
    /// A systemd unit whose main process isn't CLIProxyAPI, such as cron's.
    Unit { unit: String },
    /// A launchd job that runs a launcher.
    Job { label: String, program: String },
    /// Another program, still running.
    Program { name: String, pid: u32 },
    /// A program that has since exited.
    Gone,
}

/// A container from CLIProxyAPI's image.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Container {
    pub(crate) name: Option<String>,
    pub(crate) image: Option<String>,
    /// Docker Compose's project, service, working directory and files, from
    /// its labels.
    pub(crate) project: Option<String>,
    pub(crate) service: Option<String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) files: Option<String>,
}

impl Starter {
    /// A short name of its kind, for the JSON output and the record.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Starter::Systemd { .. } => "systemd",
            Starter::Launchd { .. } => "launchd",
            Starter::WindowsService { .. } => "windows-service",
            Starter::Task { .. } => "scheduled-task",
            Starter::Launcher(_) => "launcher",
            Starter::Container(_) => "container",
        }
    }

    /// What it is, in words.
    pub(crate) fn describe(&self) -> String {
        match self {
            Starter::Systemd { unit, user, .. } => format!(
                "the systemd {} service {unit}",
                if *user { "user" } else { "system" }
            ),
            Starter::Launchd {
                label, plist, brew, ..
            } => {
                if *brew {
                    format!("Homebrew's `brew services` (the launchd job {label}, {plist})")
                } else {
                    format!("the launchd job {label} ({plist})")
                }
            }
            Starter::WindowsService { name, wrapper, .. } => match wrapper {
                Some(wrapper) => format!("the Windows service {name} (through {wrapper})"),
                None => format!("the Windows service {name}"),
            },
            Starter::Task { name, .. } => format!("the scheduled task {name}"),
            Starter::Launcher(launcher) => launcher.describe(),
            Starter::Container(container) => {
                let mut text = String::from("a container");
                if let Some(name) = &container.name {
                    text.push_str(&format!(" ({name}"));
                    if let Some(image) = &container.image {
                        text.push_str(&format!(", from {image}"));
                    }
                    text.push(')');
                }
                text
            }
        }
    }
}

impl Launcher {
    pub(crate) fn describe(&self) -> String {
        match self {
            Launcher::Task { name, program } => {
                format!("the scheduled task {name} (through its launcher {program})")
            }
            Launcher::Unit { unit } => {
                format!("the systemd unit {unit} (which runs it through another program)")
            }
            Launcher::Job { label, program } => {
                format!("the launchd job {label} (through {program})")
            }
            Launcher::Program { name, pid } => format!("the program {name} (process {pid})"),
            Launcher::Gone => {
                "a program that has since exited (such as a launcher script or a closed terminal)"
                    .to_owned()
            }
        }
    }

    /// How to have the launcher start the binary again, in words.
    pub(crate) fn restart_hint(&self) -> String {
        match self {
            Launcher::Task { name, .. } => {
                format!(
                    "end CLIProxyAPI and run the scheduled task {name} again, or sign out and in"
                )
            }
            Launcher::Unit { unit } => format!("restart {unit}"),
            Launcher::Job { label, .. } => format!("restart the launchd job {label}"),
            Launcher::Program { name, .. } => format!("restart it from {name}"),
            Launcher::Gone => "stop it and start it again the way you started it".to_owned(),
        }
    }
}

/// CLIProxyAPI as found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    /// Its running process, if any.
    pub(crate) process: Option<Proc>,
    /// Its binary: the process's, or the service definition's.
    pub(crate) exe: Option<String>,
    /// Its arguments: the process's, or the definition's. Never shown.
    pub(crate) args: Vec<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) starter: Starter,
    /// The variables the service definition sets, when it was read. Never
    /// shown but by name.
    pub(crate) definition_env: Option<Vec<(String, String)>>,
    /// The files the definition loads variables from.
    pub(crate) env_files: Vec<String>,
    /// What couldn't be read or told, to say.
    pub(crate) notes: Vec<String>,
    /// What stops the switch, found while searching.
    pub(crate) blockers: Vec<String>,
}

impl Found {
    fn new(starter: Starter) -> Found {
        Found {
            process: None,
            exe: None,
            args: Vec::new(),
            cwd: None,
            starter,
            definition_env: None,
            env_files: Vec::new(),
            notes: Vec::new(),
            blockers: Vec::new(),
        }
    }

    fn from_process(process: Proc, starter: Starter) -> Found {
        Found {
            exe: process.exe.clone(),
            args: process.args.clone(),
            cwd: process.cwd.clone(),
            process: Some(process),
            ..Found::new(starter)
        }
    }
}

/// Upstream's flags (`cmd/server/main.go`) that take no value.
const UPSTREAM_BOOL_FLAGS: [&str; 16] = [
    "antigravity-login",
    "claude-login",
    "codex-device-login",
    "codex-login",
    "devin-login",
    "discover",
    "discover-json",
    "home-disable-cluster-discovery",
    "kimi-ai-login",
    "kimi-login",
    "local-model",
    "meta-login",
    "no-browser",
    "standalone",
    "tui",
    "xai-login",
];

/// Upstream's flags that take a value.
const UPSTREAM_VALUE_FLAGS: [&str; 11] = [
    "config",
    "discover-exclude",
    "discover-include",
    "discover-service-type",
    "discover-timeout",
    "home-jwt",
    "management-base-url",
    "oauth-callback-port",
    "password",
    "vertex-import",
    "vertex-import-prefix",
];

/// A command line's flags, read as Go's `flag` package reads upstream's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    /// The flags given, by name, in order, each once.
    pub(crate) names: Vec<String>,
    /// `-config`'s value, the last one given.
    pub(crate) config: Option<String>,
    /// Whether an argument follows the flags.
    pub(crate) rest: bool,
    /// A flag upstream doesn't know, or "a malformed flag", which stopped
    /// the reading.
    pub(crate) unknown: Option<String>,
}

/// Reads `args` as upstream's command line: one or two dashes, a value
/// after `=` or as the next argument, a stop at the first argument that
/// isn't a flag or after `--`. Only `-config`'s value is kept.
pub(crate) fn read_flags(args: &[String]) -> Flags {
    let mut flags = Flags::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let Some(stripped) = arg.strip_prefix('-').filter(|_| arg.len() >= 2) else {
            flags.rest = true;
            break;
        };
        let name = match stripped.strip_prefix('-') {
            Some("") => {
                flags.rest = rest.next().is_some();
                break;
            }
            Some(name) => name,
            None => stripped,
        };
        if name.is_empty() || name.starts_with(['-', '=']) {
            flags.unknown = Some("a malformed flag".to_owned());
            break;
        }
        let (name, value) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (name, None),
        };
        let takes_value = if UPSTREAM_VALUE_FLAGS.contains(&name) {
            true
        } else if UPSTREAM_BOOL_FLAGS.contains(&name) {
            false
        } else {
            flags.unknown = Some(format!("-{name}"));
            break;
        };
        let value = match value {
            Some(value) => Some(value),
            None if takes_value => rest.next().map(String::as_str),
            None => None,
        };
        if name == "config"
            && let Some(value) = value
        {
            flags.config = Some(value.to_owned());
        }
        if !flags.names.iter().any(|known| known == name) {
            flags.names.push(name.to_owned());
        }
    }
    flags
}

/// The file name of `path`.
pub(crate) fn file_name(platform: Platform, path: &str) -> String {
    path.rsplit(|c| platform.is_separator(c))
        .next()
        .unwrap_or(path)
        .to_owned()
}

/// Whether `name` is one of CLIProxyAPI's binaries' names.
pub(crate) fn is_binary_name(name: &str) -> bool {
    BINARY_NAMES
        .iter()
        .any(|known| name.eq_ignore_ascii_case(known))
}

/// Whether `text` mentions CLIProxyAPI's binary.
fn mentions(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    MENTIONS.iter().any(|name| lower.contains(name))
}

/// Whether two full paths are the same, in the platform's case rules.
pub(crate) fn same_path(platform: Platform, a: &str, b: &str) -> bool {
    let (a, b) = (platform.clean(a), platform.clean(b));
    match platform {
        Platform::Windows | Platform::MacOs => a.eq_ignore_ascii_case(&b),
        Platform::Linux => a == b,
    }
}

/// The config a command line names, as a full path, from `cwd`: its
/// `-config`, or upstream's default, `config.yaml` in the working directory.
pub(crate) fn config_of(platform: Platform, args: &[String], cwd: Option<&str>) -> Option<String> {
    let given = read_flags(args)
        .config
        .unwrap_or_else(|| "config.yaml".to_owned());
    if platform.is_absolute(&given) {
        return Some(platform.clean(&given));
    }
    platform.absolute(cwd?, &given).ok()
}

/// Finds CLIProxyAPI. With `config`, a running process whose config is
/// that file is preferred. `Ok(None)` when nothing is found; an error when
/// the search can't go on, such as with several processes and no way to
/// choose.
pub(crate) fn discover(
    machine: &mut dyn Machine,
    context: &Context,
    config: Option<&str>,
) -> Result<Option<Found>, String> {
    let platform = context.platform;
    let mut notes = Vec::new();
    let processes = match machine.processes(&BINARY_NAMES) {
        Ok(processes) => processes,
        Err(error) => {
            notes.push(format!("couldn't list the running processes: {error}"));
            Vec::new()
        }
    };
    let process = choose(platform, processes, config)?;
    let mut found = match process {
        Some(process) => Some(starter_of(machine, context, process)?),
        None => not_running(machine, context)?,
    };
    if let Some(found) = &mut found {
        found.notes.splice(0..0, notes);
    }
    Ok(found)
}

/// The process to switch: the only one, or the one whose config is
/// `config`.
fn choose(
    platform: Platform,
    processes: Vec<Proc>,
    config: Option<&str>,
) -> Result<Option<Proc>, String> {
    if processes.len() <= 1 {
        return Ok(processes.into_iter().next());
    }
    let pids: Vec<String> = processes
        .iter()
        .map(|process| process.pid.to_string())
        .collect();
    if let Some(config) = config {
        let mut matching: Vec<Proc> = processes
            .into_iter()
            .filter(|process| {
                config_of(platform, &process.args, process.cwd.as_deref())
                    .is_some_and(|path| same_path(platform, &path, config))
            })
            .collect();
        if matching.len() == 1 {
            return Ok(matching.pop());
        }
    }
    Err(format!(
        "Found more than one CLIProxyAPI running (processes {}). Name the config of the one to switch with -config.",
        pids.join(", ")
    ))
}

/// What starts the running `process`.
fn starter_of(
    machine: &mut dyn Machine,
    context: &Context,
    process: Proc,
) -> Result<Found, String> {
    match context.platform {
        Platform::Linux => linux_process(machine, context, process),
        Platform::MacOs => macos_process(machine, context, process),
        Platform::Windows => windows_process(machine, context, process),
    }
}

/// The process's parent as its launcher.
fn parent_launcher(process: &Proc) -> Launcher {
    match &process.parent {
        Some(super::machine::Parent {
            pid,
            name: Some(name),
        }) => Launcher::Program {
            name: name.clone(),
            pid: *pid,
        },
        _ => Launcher::Gone,
    }
}

/// The user ID `migrate` runs as, on Unix.
pub(crate) fn uid(machine: &mut dyn Machine) -> Result<u32, String> {
    let output = run_checked(machine, &Cmd::new("id", &["-u"]))?;
    let uid = output.stdout.trim();
    uid.parse()
        .map_err(|_| format!("`id -u` printed {uid:?}, not a user ID"))
}

// --- Linux ---

/// Where a control group says a process runs.
#[derive(Debug, PartialEq, Eq)]
enum Group {
    /// A systemd service unit, the user's (with their user ID) or the
    /// system's.
    Unit {
        unit: String,
        user: Option<u32>,
    },
    Container,
    /// A session, a scope, or nothing systemd knows.
    Other,
}

/// Reads `/proc/<pid>/cgroup`.
fn read_cgroup(text: &str) -> Group {
    // cgroup v2's line is `0::/path`; with v1, systemd's is
    // `N:name=systemd:/path`.
    let path = text
        .lines()
        .find(|line| line.starts_with("0::"))
        .or_else(|| text.lines().find(|line| line.contains(":name=systemd:")))
        .and_then(|line| line.splitn(3, ':').nth(2))
        .unwrap_or_default();
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let container = [
        "docker",
        "libpod",
        "containerd",
        "cri-containerd",
        "kubepods",
        "lxc",
    ];
    if segments
        .iter()
        .any(|part| container.iter().any(|prefix| part.starts_with(prefix)))
    {
        return Group::Container;
    }
    let user = segments.iter().find_map(|part| {
        part.strip_prefix("user@")
            .and_then(|rest| rest.strip_suffix(".service"))
            .and_then(|uid| uid.parse::<u32>().ok())
    });
    let unit = segments
        .iter()
        .rev()
        .find(|part| part.ends_with(".service") && !part.starts_with("user@"));
    match unit {
        Some(unit) => Group::Unit {
            unit: (*unit).to_owned(),
            user,
        },
        None => Group::Other,
    }
}

/// What `systemctl show` says of a unit.
#[derive(Debug, Default)]
struct Shown {
    values: BTreeMap<String, Vec<String>>,
}

impl Shown {
    fn read(text: &str) -> Shown {
        let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            values
                .entry(key.to_owned())
                .or_default()
                .push(value.to_owned());
        }
        Shown { values }
    }

    fn get(&self, key: &str) -> &str {
        self.values
            .get(key)
            .and_then(|values| values.last())
            .map_or("", String::as_str)
    }

    fn all(&self, key: &str) -> &[String] {
        self.values.get(key).map_or(&[], Vec::as_slice)
    }

    /// `ExecStart`'s first command line, as `systemctl show` prints it:
    /// words joined by spaces.
    fn exec_start(&self) -> Vec<String> {
        self.get("ExecStart")
            .split_once("argv[]=")
            .map(|(_, rest)| rest.split(" ;").next().unwrap_or_default())
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    /// The program of `ExecStart`'s first command, as `systemctl show`
    /// prints it.
    fn exec_path(&self) -> &str {
        self.get("ExecStart")
            .split_once("path=")
            .map(|(_, rest)| rest.split(" ;").next().unwrap_or_default())
            .unwrap_or_default()
            .trim()
    }

    fn environment(&self) -> Vec<(String, String)> {
        self.all("Environment")
            .iter()
            .flat_map(|line| line.split_whitespace())
            .filter_map(|word| {
                let (name, value) = word.split_once('=')?;
                Some((name.to_owned(), value.to_owned()))
            })
            .collect()
    }

    /// `EnvironmentFiles`, in the order systemd reads them: one per line,
    /// each `path (ignore_errors=...)`. Older systemd calls the property
    /// `EnvironmentFile`. A `-` before a file in the unit (a file that may
    /// be missing) shows as `ignore_errors=yes`, and is dropped from the
    /// path if it is there.
    fn environment_files(&self) -> Vec<String> {
        ["EnvironmentFiles", "EnvironmentFile"]
            .into_iter()
            .flat_map(|key| self.all(key))
            .filter_map(|line| {
                let line = line.trim();
                let path = match line.rsplit_once(" (ignore_errors=") {
                    Some((path, _)) => path,
                    None => line,
                };
                let path = path.strip_prefix('-').unwrap_or(path).trim();
                (!path.is_empty()).then(|| path.to_owned())
            })
            .collect()
    }
}

/// The words of a systemd command line, split as systemd splits them:
/// double and single quotes keep spaces in a word, and a backslash escapes
/// (`\n`, `\s` for a space, `\xNN`, `\NNN` octal, and a quote or backslash
/// itself). `None` when the line is malformed. A word of `;` ends the
/// command.
fn systemd_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let escaped = chars.next()?;
            let value = match escaped {
                'a' => '\u{7}',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'v' => '\u{b}',
                's' => ' ',
                '\\' | '"' | '\'' => escaped,
                'x' => {
                    let high = chars.next()?.to_digit(16)?;
                    let low = chars.next()?.to_digit(16)?;
                    char::from(u8::try_from(high * 16 + low).ok()?)
                }
                '0'..='7' => {
                    let first = escaped.to_digit(8)?;
                    let second = chars.next()?.to_digit(8)?;
                    let third = chars.next()?.to_digit(8)?;
                    char::from(u8::try_from(first * 64 + second * 8 + third).ok()?)
                }
                _ => return None,
            };
            current.push(value);
            in_word = true;
            continue;
        }
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(current);
    }
    if let Some(end) = words.iter().position(|word| word == ";") {
        words.truncate(end);
    }
    Some(words)
}

/// A command of `ExecStart=`: the program, and argv (`argv[0]` first, which
/// is the program unless the command starts with `@`).
#[derive(Debug, PartialEq, Eq)]
struct ExecCommand {
    exe: String,
    argv: Vec<String>,
    /// The `:` prefix was given: systemd leaves `$` alone, so `$$` stays `$$`.
    plain_dollars: bool,
}

/// `ExecStart=`'s value split as systemd splits it, with the prefixes `@`,
/// `-`, `:`, `+` and `!` taken off the program. After `@`, the first word
/// is the program and the second `argv[0]`. Variables (`$X`) and specifiers
/// (`%n`) are left as written.
fn exec_command(command: &str) -> Option<ExecCommand> {
    let mut rest = command;
    let mut names_argv0 = false;
    let mut plain_dollars = false;
    while let Some(first) = rest.chars().next()
        && "@-:+!".contains(first)
    {
        names_argv0 |= first == '@';
        plain_dollars |= first == ':';
        rest = rest.get(first.len_utf8()..)?;
    }
    let mut words = systemd_words(rest)?;
    let exe = words.first()?.clone();
    if names_argv0 {
        words.remove(0);
    }
    (!words.is_empty()).then_some(ExecCommand {
        exe,
        argv: words,
        plain_dollars,
    })
}

/// Adds the `ExecStart=` values of the `[Service]` section of a unit file
/// (or drop-in) `source` to `commands`, each with its file, as systemd does:
/// an empty `ExecStart=` resets the list.
fn add_exec_starts(text: &str, source: &str, commands: &mut Vec<(String, String)>) {
    let mut lines = Vec::new();
    let mut joined = String::new();
    for line in text.lines() {
        let line = line.trim();
        if joined.is_empty() && (line.starts_with('#') || line.starts_with(';')) {
            continue;
        }
        match line.strip_suffix('\\') {
            Some(start) => {
                joined.push_str(start);
                joined.push(' ');
            }
            None => {
                joined.push_str(line);
                lines.push(std::mem::take(&mut joined));
            }
        }
    }
    let mut in_service = false;
    for line in &lines {
        if line.starts_with('[') {
            in_service = line == "[Service]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if in_service && key.trim() == "ExecStart" {
            let value = value.trim();
            if value.is_empty() {
                commands.clear();
            } else {
                commands.push((value.to_owned(), source.to_owned()));
            }
        }
    }
}

const SHOW_PROPERTIES: &str = "LoadState,MainPID,FragmentPath,DropInPaths,Type,UnitFileState,ActiveState,ExecStart,WorkingDirectory,Environment,EnvironmentFiles,User,NeedDaemonReload";

/// `systemctl show`'s command for `unit`.
pub(crate) fn systemctl(user: bool, args: &[&str]) -> Cmd {
    let mut cmd = Cmd::new("systemctl", &[]);
    if user {
        cmd = cmd.arg("--user");
    }
    args.iter().fold(cmd, |cmd, arg| cmd.arg(*arg))
}

fn systemctl_show(machine: &mut dyn Machine, user: bool, unit: &str) -> Result<Shown, String> {
    let cmd = systemctl(user, &["show", unit]).arg(format!("--property={SHOW_PROPERTIES}"));
    let output = run_checked(machine, &cmd)?;
    Ok(Shown::read(&output.stdout))
}

fn linux_process(
    machine: &mut dyn Machine,
    context: &Context,
    process: Proc,
) -> Result<Found, String> {
    let group = machine
        .read(&format!("/proc/{}/cgroup", process.pid))
        .map(|data| read_cgroup(&String::from_utf8_lossy(&data)))
        .unwrap_or(Group::Other);
    match group {
        Group::Container => {
            let container = docker_container(machine).unwrap_or_default();
            Ok(Found::from_process(process, Starter::Container(container)))
        }
        Group::Unit { unit, user } => {
            if let Some(owner) = user {
                let ours = uid(machine)?;
                if owner != ours {
                    let mut found = Found::from_process(
                        process,
                        Starter::Systemd {
                            unit: unit.clone(),
                            user: true,
                            enabled: false,
                            active: true,
                        },
                    );
                    found.blockers.push(format!(
                        "its systemd user service, {unit}, is that of user ID {owner}, not yours: run `open-ferry migrate` as that user"
                    ));
                    return Ok(found);
                }
            }
            let shown = systemctl_show(machine, user.is_some(), &unit)?;
            if shown.get("MainPID") != process.pid.to_string() {
                let mut found =
                    Found::from_process(process, Starter::Launcher(Launcher::Unit { unit }));
                found.definition_env = Some(shown.environment());
                found.env_files = shown.environment_files();
                return Ok(found);
            }
            let starter = Starter::Systemd {
                unit,
                user: user.is_some(),
                enabled: shown.get("UnitFileState") == "enabled",
                active: shown.get("ActiveState") == "active",
            };
            let mut found = Found::from_process(process, starter);
            systemd_details(context, &shown, user.is_some(), &mut found);
            Ok(found)
        }
        Group::Other => {
            let launcher = parent_launcher(&process);
            Ok(Found::from_process(process, Starter::Launcher(launcher)))
        }
    }
}

/// What a systemd unit's definition adds: its variables, its working
/// directory when the process's can't be read, and a system unit's user,
/// which open-ferry's system service doesn't keep.
fn systemd_details(context: &Context, shown: &Shown, user: bool, found: &mut Found) {
    found.definition_env = Some(shown.environment());
    found.env_files = shown.environment_files();
    if found.cwd.is_none() {
        let dir = shown.get("WorkingDirectory");
        let dir = dir.strip_prefix('-').unwrap_or(dir);
        found.cwd = match dir {
            "" if user => context.var("HOME").map(str::to_owned),
            "" => Some("/".to_owned()),
            "~" => context.var("HOME").map(str::to_owned),
            dir => Some(dir.to_owned()),
        };
    }
    let run_as = shown.get("User");
    if !user && !run_as.is_empty() && run_as != "root" && run_as != "0" {
        found.blockers.push(format!(
            "its system service runs as the user {run_as}, and open-ferry's runs as root: switch by hand, as docs/migrating-from-cliproxyapi.md says"
        ));
    }
}

/// A stopped unit's command: the program, its arguments, and why the
/// command can't be relied on, if it can't.
struct UnitExec {
    exe: String,
    args: Vec<String>,
    problem: Option<String>,
}

/// The `ExecStart=` commands in effect, as systemd takes them, each with
/// the file it is in: the unit file's, then each drop-in's in order, an
/// empty `ExecStart=` clearing those before it.
fn effective_exec_starts(
    machine: &mut dyn Machine,
    shown: &Shown,
) -> Result<Vec<(String, String)>, String> {
    let fragment = shown.get("FragmentPath");
    if fragment.is_empty() {
        return Err("it has no unit file".to_owned());
    }
    let mut commands = Vec::new();
    let drop_ins = shown.get("DropInPaths").split_whitespace();
    for path in std::iter::once(fragment).chain(drop_ins) {
        let data = machine
            .read(path)
            .map_err(|error| format!("{path} can't be read: {error}"))?;
        add_exec_starts(&String::from_utf8_lossy(&data), path, &mut commands);
    }
    Ok(commands)
}

/// Whether a word of a command line has a `$` variable or a `%` specifier,
/// which systemd expands when it starts the unit. `$$` and `%%` are a
/// literal `$` and `%`.
fn expands(word: &str) -> bool {
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if (c == '$' || c == '%') && chars.next() != Some(c) {
            return true;
        }
    }
    false
}

/// A unit's command line. `systemctl show` joins the words with spaces
/// and drops their quotes, so a word with a space in it can't be told
/// apart: the unit file and its drop-ins are read and split as systemd
/// splits them, and that is the command line, when `systemctl show` prints
/// the same program and words. When it doesn't, or the files aren't what
/// systemd runs (changed since it loaded them, or using `$` variables or
/// `%` specifiers, which it expands and `migrate` doesn't), the command is
/// marked as one that can't be relied on. The problem says what kind it is,
/// and names the file, but never shows the command: it may hold a secret.
fn unit_exec(machine: &mut dyn Machine, shown: &Shown) -> Option<UnitExec> {
    let path = shown.exec_path();
    if path.is_empty() {
        return None;
    }
    let printed = shown.exec_start();
    let unreliable = |problem: String| UnitExec {
        exe: path.to_owned(),
        // The words can't be told, so none are guessed.
        args: Vec::new(),
        problem: Some(problem),
    };
    if shown.get("NeedDaemonReload") == "yes" {
        return Some(unreliable(
            "its files have changed since systemd loaded them, so they aren't what it runs"
                .to_owned(),
        ));
    }
    let commands = match effective_exec_starts(machine, shown) {
        Ok(commands) => commands,
        Err(error) => return Some(unreliable(error)),
    };
    let Some((first, file)) = commands.first() else {
        return Some(unreliable("no ExecStart= is found in its files".to_owned()));
    };
    if commands.len() > 1 && shown.get("Type") != "oneshot" {
        return Some(unreliable(format!(
            "it has {} ExecStart= commands, which only a oneshot unit may have",
            commands.len()
        )));
    }
    let Some(command) = exec_command(first) else {
        return Some(unreliable(format!(
            "the ExecStart= in {file} can't be split into words"
        )));
    };
    if std::iter::once(&command.exe)
        .chain(&command.argv)
        .any(|word| expands(word))
    {
        return Some(unreliable(format!(
            "the ExecStart= in {file} uses a $ variable or a % specifier, which systemd expands when it starts the unit"
        )));
    }
    // With the `:` prefix, `$$` is not turned into `$`, and migrate can't
    // tell what the program reads, so any `$` blocks.
    if command.plain_dollars
        && std::iter::once(&command.exe)
            .chain(&command.argv)
            .any(|word| word.contains('$'))
    {
        return Some(unreliable(format!(
            "the ExecStart= in {file} starts with : and has a $, which systemd leaves as written"
        )));
    }
    // `%%` and `$$` are one `%` and `$` to systemd. A file that `systemctl
    // show` prints differently from this is blocked below, not guessed at.
    let exe = command.exe.replace("%%", "%");
    let argv: Vec<String> = command
        .argv
        .iter()
        .map(|word| word.replace("%%", "%"))
        .collect();
    let words: Vec<&str> = argv
        .iter()
        .flat_map(|word| word.split_whitespace())
        .collect();
    let difference = if exe != path {
        Some("the program differs")
    } else if words.len() != printed.len() {
        Some("the number of words differs")
    } else if !words.iter().eq(printed.iter()) {
        Some("a word differs")
    } else {
        None
    };
    if let Some(difference) = difference {
        return Some(unreliable(format!(
            "the ExecStart= in {file} and what systemctl show prints for it don't agree: {difference}"
        )));
    }
    Some(UnitExec {
        exe: exe.replace("$$", "$"),
        args: argv
            .get(1..)
            .unwrap_or_default()
            .iter()
            .map(|word| word.replace("$$", "$"))
            .collect(),
        problem: None,
    })
}

fn linux_not_running(
    machine: &mut dyn Machine,
    context: &Context,
) -> Result<Option<Found>, String> {
    for user in [true, false] {
        for unit in KNOWN_UNITS {
            let Ok(shown) = systemctl_show(machine, user, unit) else {
                continue;
            };
            if shown.get("LoadState") != "loaded" {
                continue;
            }
            let Some(exec) = unit_exec(machine, &shown) else {
                continue;
            };
            if !is_binary_name(&file_name(Platform::Linux, shown.exec_path())) {
                continue;
            }
            let mut found = Found::new(Starter::Systemd {
                unit: unit.to_owned(),
                user,
                enabled: shown.get("UnitFileState") == "enabled",
                active: shown.get("ActiveState") == "active",
            });
            found.exe = Some(exec.exe);
            found.args = exec.args;
            if let Some(problem) = exec.problem {
                found.blockers.push(format!(
                    "the command line of the systemd unit {unit} can't be told reliably ({problem}): switch by hand, as docs/migrating-from-cliproxyapi.md says"
                ));
            }
            systemd_details(context, &shown, user, &mut found);
            found.notes.push(format!(
                "CLIProxyAPI isn't running: its command line is read from {unit}"
            ));
            return Ok(Some(found));
        }
    }
    Ok(None)
}

// --- macOS ---

/// A launchd job's plist, as far as `migrate` reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Job {
    pub(crate) label: String,
    pub(crate) path: String,
    pub(crate) domain: String,
    /// `Program`, or `ProgramArguments`' first.
    pub(crate) program: Option<String>,
    /// `ProgramArguments` after the first.
    pub(crate) args: Vec<String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) env: Vec<(String, String)>,
}

/// The XML text of the plist at `path`: binary plists are converted with
/// `plutil`.
fn plist_text(machine: &mut dyn Machine, path: &str) -> Option<String> {
    let data = machine.read(path).ok()?;
    if data.starts_with(b"bplist") {
        let cmd = Cmd::new("plutil", &["-convert", "xml1", "-o", "-"]).arg(path);
        return run_checked(machine, &cmd).ok().map(|output| output.stdout);
    }
    Some(String::from_utf8_lossy(&data).into_owned())
}

/// XML text with its five entities undone.
pub(crate) fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The text of the first `<tag>...</tag>` in `xml`, and where it ends.
fn element<'a>(xml: &'a str, tag: &str) -> Option<(&'a str, usize)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml.get(start..)?.find(&close)?;
    Some((xml.get(start..end)?, end + close.len()))
}

/// The `<string>` right after `rest`'s start, and the rest after it.
fn next_string(rest: &str) -> Option<(String, &str)> {
    if !rest.trim_start().starts_with("<string>") {
        return None;
    }
    let (value, end) = element(rest, "string")?;
    Some((unescape(value), rest.get(end..).unwrap_or_default()))
}

/// Reads the keys of a launchd plist that `migrate` needs. Only `<string>`
/// values, arrays of them and dictionaries of them are read.
pub(crate) fn read_plist(xml: &str) -> Job {
    let mut job = Job::default();
    let Some(start) = xml.find("<dict>") else {
        return job;
    };
    let mut rest = xml.get(start + "<dict>".len()..).unwrap_or_default();
    while let Some((key, end)) = element(rest, "key") {
        rest = rest.get(end..).unwrap_or_default();
        match key.trim() {
            "Label" | "Program" | "WorkingDirectory" => {
                if let Some((value, after)) = next_string(rest) {
                    match key.trim() {
                        "Label" => job.label = value,
                        "Program" => job.program = Some(value),
                        _ => job.working_dir = Some(value),
                    }
                    rest = after;
                }
            }
            "ProgramArguments" if rest.trim_start().starts_with("<array>") => {
                if let Some((array, end)) = element(rest, "array") {
                    let mut words = Vec::new();
                    let mut items = array;
                    while let Some((value, after)) = next_string(items) {
                        words.push(value);
                        items = after;
                    }
                    let mut words = words.into_iter();
                    let first = words.next();
                    if job.program.is_none() {
                        job.program = first;
                    }
                    job.args = words.collect();
                    rest = rest.get(end..).unwrap_or_default();
                }
            }
            "EnvironmentVariables" if rest.trim_start().starts_with("<dict>") => {
                if let Some((env, end)) = element(rest, "dict") {
                    let mut items = env;
                    while let Some((name, end)) = element(items, "key") {
                        items = items.get(end..).unwrap_or_default();
                        if let Some((value, after)) = next_string(items) {
                            job.env.push((unescape(name), value));
                            items = after;
                        }
                    }
                    rest = rest.get(end..).unwrap_or_default();
                }
            }
            _ => {}
        }
    }
    job
}

/// The launchd jobs that run CLIProxyAPI, or mention it.
fn launchd_jobs(machine: &mut dyn Machine, context: &Context, uid: u32) -> Vec<Job> {
    let gui = format!("gui/{uid}");
    let mut dirs = Vec::new();
    if let Some(home) = context.var("HOME") {
        dirs.push((
            Platform::MacOs.join(home, "Library/LaunchAgents"),
            gui.clone(),
        ));
    }
    dirs.push(("/Library/LaunchAgents".to_owned(), gui));
    dirs.push(("/Library/LaunchDaemons".to_owned(), "system".to_owned()));
    let mut jobs = Vec::new();
    for (dir, domain) in dirs {
        let Ok(entries) = machine.list_dir(&dir) else {
            continue;
        };
        for entry in entries {
            if !entry.name.ends_with(".plist") {
                continue;
            }
            let path = Platform::MacOs.join(&dir, &entry.name);
            let Some(text) = plist_text(machine, &path) else {
                continue;
            };
            let job = Job {
                path,
                domain: domain.clone(),
                ..read_plist(&text)
            };
            if job.label.is_empty() {
                continue;
            }
            if job_runs_binary(&job) || job.args.iter().any(|arg| mentions(arg)) {
                jobs.push(job);
            }
        }
    }
    jobs
}

/// Whether Homebrew installed the binary at `exe`, and its prefix.
pub(crate) fn brew_prefix(exe: &str) -> Option<&str> {
    exe.find("/Cellar/cliproxyapi/")
        .or_else(|| exe.find("/opt/cliproxyapi/"))
        .and_then(|at| exe.get(..at))
}

fn job_runs_binary(job: &Job) -> bool {
    job.program
        .as_deref()
        .is_some_and(|program| is_binary_name(&file_name(Platform::MacOs, program)))
}

/// What `launchctl print` says the job's process is, if it runs.
fn job_pid(machine: &mut dyn Machine, job: &Job) -> Option<u32> {
    let target = format!("{}/{}", job.domain, job.label);
    let output = machine
        .run(&Cmd::new("launchctl", &["print"]).arg(target))
        .ok()
        .filter(|output| output.success())?;
    output.stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix("pid = ")
            .and_then(|pid| pid.trim().parse().ok())
    })
}

fn launchd_starter(job: &Job, loaded: bool) -> Starter {
    Starter::Launchd {
        label: job.label.clone(),
        plist: job.path.clone(),
        domain: job.domain.clone(),
        brew: job.label == BREW_LABEL,
        loaded,
    }
}

fn macos_process(
    machine: &mut dyn Machine,
    context: &Context,
    process: Proc,
) -> Result<Found, String> {
    let uid = uid(machine)?;
    let jobs = launchd_jobs(machine, context, uid);
    for job in &jobs {
        if job_pid(machine, job) != Some(process.pid) {
            continue;
        }
        let starter = if job_runs_binary(job) {
            launchd_starter(job, true)
        } else {
            Starter::Launcher(Launcher::Job {
                label: job.label.clone(),
                program: job
                    .program
                    .as_deref()
                    .map(|program| file_name(Platform::MacOs, program))
                    .unwrap_or_default(),
            })
        };
        let mut found = Found::from_process(process, starter);
        found.definition_env = Some(job.env.clone());
        return Ok(found);
    }
    // A job whose launcher started it, and has exited or is still there.
    if let Some(job) = jobs.iter().find(|job| !job_runs_binary(job)) {
        let program = job
            .program
            .as_deref()
            .map(|program| file_name(Platform::MacOs, program))
            .unwrap_or_default();
        let mut found = Found::from_process(
            process,
            Starter::Launcher(Launcher::Job {
                label: job.label.clone(),
                program,
            }),
        );
        found.definition_env = Some(job.env.clone());
        return Ok(found);
    }
    let launcher = parent_launcher(&process);
    let mut found = Found::from_process(process, Starter::Launcher(launcher));
    if let Some(job) = jobs.iter().find(|job| job_runs_binary(job)) {
        found.notes.push(format!(
            "{} runs CLIProxyAPI too, but launchd says it doesn't run this process",
            job.path
        ));
    }
    Ok(found)
}

fn macos_not_running(
    machine: &mut dyn Machine,
    context: &Context,
) -> Result<Option<Found>, String> {
    let uid = uid(machine)?;
    let jobs = launchd_jobs(machine, context, uid);
    let Some(job) = jobs.iter().find(|job| job_runs_binary(job)) else {
        return Ok(None);
    };
    let target = format!("{}/{}", job.domain, job.label);
    let loaded = machine
        .run(&Cmd::new("launchctl", &["print"]).arg(target))
        .is_ok_and(|output| output.success());
    let mut found = Found::new(launchd_starter(job, loaded));
    found.exe = job.program.clone();
    found.args = job.args.clone();
    found.cwd = job.working_dir.clone();
    found.definition_env = Some(job.env.clone());
    found.notes.push(format!(
        "CLIProxyAPI isn't running: its command line is read from {}",
        job.path
    ));
    Ok(Some(found))
}

// --- Windows ---

/// Splits a Windows command line into its arguments, as the C runtime does.
pub(crate) fn split_windows(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut backslashes = 0usize;
    for c in line.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                in_word = true;
            }
            '"' => {
                current.extend(std::iter::repeat_n('\\', backslashes / 2));
                if backslashes % 2 == 1 {
                    current.push('"');
                } else {
                    quoted = !quoted;
                }
                backslashes = 0;
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                current.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                if in_word {
                    args.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            c => {
                current.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                current.push(c);
                in_word = true;
            }
        }
    }
    current.extend(std::iter::repeat_n('\\', backslashes));
    if in_word {
        args.push(current);
    }
    args
}

/// A service's command line, `BINARY_PATH_NAME`, as its program and
/// arguments. An unquoted program with spaces in its path runs to its
/// `.exe`.
pub(crate) fn split_service_command(line: &str) -> (String, Vec<String>) {
    let line = line.trim();
    if !line.starts_with('"')
        && let Some(at) = line.to_ascii_lowercase().find(".exe")
    {
        let end = at + 4;
        let program = line.get(..end).unwrap_or(line).to_owned();
        let rest = line.get(end..).unwrap_or_default();
        return (program, split_windows(rest));
    }
    let mut words = split_windows(line).into_iter();
    let program = words.next().unwrap_or_default();
    (program, words.collect())
}

/// The services process `pid` runs, from `tasklist /svc`.
fn services_of(machine: &mut dyn Machine, pid: u32) -> Vec<String> {
    let cmd = Cmd::new("tasklist.exe", &["/svc", "/fo", "csv", "/nh", "/fi"])
        .arg(format!("PID eq {pid}"));
    let Ok(output) = run_checked(machine, &cmd) else {
        return Vec::new();
    };
    let pid = pid.to_string();
    let mut names = Vec::new();
    for line in output.stdout.lines() {
        let fields: Vec<&str> = line.trim().trim_matches('"').split("\",\"").collect();
        if fields.get(1) != Some(&pid.as_str()) {
            continue;
        }
        let Some(services) = fields.get(2) else {
            continue;
        };
        names.extend(
            services
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty() && *name != "N/A")
                .map(str::to_owned),
        );
    }
    names
}

/// What `sc qc` says of a service.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ServiceConfig {
    /// `sc config`'s `start=` value.
    start: String,
    command: String,
}

fn service_config(machine: &mut dyn Machine, name: &str) -> Option<ServiceConfig> {
    let output = machine
        .run(&Cmd::new("sc.exe", &["qc"]).arg(name))
        .ok()
        .filter(|output| output.success())?;
    let mut config = ServiceConfig::default();
    for line in output.stdout.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "START_TYPE" => {
                let delayed = value.contains("DELAYED");
                config.start = match value.split_whitespace().next() {
                    Some("2") if delayed => "delayed-auto",
                    Some("2") => "auto",
                    Some("4") => "disabled",
                    _ => "demand",
                }
                .to_owned();
            }
            "BINARY_PATH_NAME" => config.command = value.to_owned(),
            _ => {}
        }
    }
    Some(config)
}

/// A scheduled task, as far as `migrate` reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Task {
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) command: String,
    pub(crate) arguments: String,
    pub(crate) working_dir: Option<String>,
    /// Who it runs as: its principal's `UserId`, a name or a SID.
    pub(crate) user_id: Option<String>,
    /// The principal's `LogonType`, such as `InteractiveToken`.
    pub(crate) logon_type: Option<String>,
    /// The principal's `RunLevel`; none means `LeastPrivilege`.
    pub(crate) run_level: Option<String>,
    /// The principal's `GroupId`, when it runs as a group.
    pub(crate) group_id: Option<String>,
    /// How many `Principal` elements the task has.
    pub(crate) principals: usize,
}

/// Reads `schtasks /query /xml ONE`'s output: each task's name, from the
/// comment before it or its `URI`, whether it is enabled, and its first
/// program.
pub(crate) fn read_tasks(xml: &str) -> Vec<Task> {
    // `Host::run` has decoded UTF-16 output; a byte order mark that came
    // through as text is dropped.
    let xml = xml.trim_start_matches('\u{feff}');
    let mut tasks = Vec::new();
    let mut rest = xml;
    let mut comment = None;
    loop {
        let next_comment = rest.find("<!--");
        let next_task = rest.find("<Task ").or_else(|| rest.find("<Task>"));
        match (next_comment, next_task) {
            (Some(c), Some(t)) if c < t => {
                let after = rest.get(c + 4..).unwrap_or_default();
                let end = after.find("-->").unwrap_or(after.len());
                comment = Some(after.get(..end).unwrap_or_default().trim().to_owned());
                rest = after.get(end..).unwrap_or_default();
            }
            (_, Some(t)) => {
                let after = rest.get(t..).unwrap_or_default();
                let end = after.find("</Task>").map_or(after.len(), |end| end + 7);
                tasks.push(read_task(
                    after.get(..end).unwrap_or_default(),
                    comment.take(),
                ));
                rest = after.get(end..).unwrap_or_default();
            }
            _ => break,
        }
    }
    tasks
}

fn read_task(block: &str, comment: Option<String>) -> Task {
    let text = |xml: &str, tag: &str| element(xml, tag).map(|(text, _)| unescape(text.trim()));
    let name = comment
        .filter(|name| !name.is_empty())
        .or_else(|| text(block, "URI"))
        .unwrap_or_default();
    let enabled = element(block, "Settings")
        .and_then(|(settings, _)| text(settings, "Enabled"))
        .is_none_or(|enabled| enabled != "false");
    let exec = element(block, "Exec").map_or("", |(exec, _)| exec);
    // `<Principal id="...">` has an attribute, so the first `UserId` and
    // `LogonType` inside `Principals` are read, which are the first
    // principal's.
    let principal = element(block, "Principals").map_or("", |(principals, _)| principals);
    let count = principal.matches("<Principal ").count() + principal.matches("<Principal>").count();
    Task {
        user_id: text(principal, "UserId").filter(|id| !id.is_empty()),
        logon_type: text(principal, "LogonType").filter(|kind| !kind.is_empty()),
        run_level: text(principal, "RunLevel").filter(|level| !level.is_empty()),
        group_id: text(principal, "GroupId").filter(|id| !id.is_empty()),
        principals: count,
        name,
        enabled,
        command: text(exec, "Command")
            .map(|command| command.trim_matches('"').to_owned())
            .unwrap_or_default(),
        arguments: text(exec, "Arguments").unwrap_or_default(),
        working_dir: text(exec, "WorkingDirectory").map(|dir| dir.trim_matches('"').to_owned()),
    }
}

/// A path with its `%NAME%` variables taken from `context`, where it knows
/// them.
fn expand(context: &Context, path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('%') {
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start + 1..).unwrap_or_default();
        let Some(end) = after.find('%') else {
            out.push('%');
            rest = after;
            continue;
        };
        let name = after.get(..end).unwrap_or_default();
        match context
            .env
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
        {
            Some((_, value)) => out.push_str(value),
            None => {
                out.push('%');
                out.push_str(name);
                out.push('%');
            }
        }
        rest = after.get(end + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// A task's launcher script, if it runs one: the program itself, or the
/// first of its arguments that is a script.
fn task_script(task: &Task) -> Option<String> {
    let is_script = |word: &str| {
        let lower = word.to_ascii_lowercase();
        SCRIPT_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
    };
    if is_script(&task.command) {
        return Some(task.command.clone());
    }
    split_windows(&task.arguments)
        .into_iter()
        .find(|word| is_script(word))
}

/// How a task starts CLIProxyAPI, if it does.
#[derive(Debug, PartialEq, Eq)]
enum TaskRuns {
    /// Its program is the binary.
    Binary,
    /// Through a launcher, `program`; with whether it names the binary's
    /// full path.
    Launcher { program: String, names_exe: bool },
}

fn task_runs(
    machine: &mut dyn Machine,
    context: &Context,
    task: &Task,
    exe: Option<&str>,
) -> Option<TaskRuns> {
    let platform = Platform::Windows;
    let command = expand(context, &task.command);
    if is_binary_name(&file_name(platform, &command)) {
        let matches = match exe {
            // With a variable not known, the path can't be compared; the
            // task is then blocked, not skipped.
            Some(exe) if platform.is_absolute(&command) && !has_unknown_variable(&command) => {
                same_path(platform, &command, exe)
            }
            _ => true,
        };
        return matches.then_some(TaskRuns::Binary);
    }
    let script = task_script(task);
    let mut text = task.arguments.clone();
    if let Some(script) = &script {
        let path = expand(context, script);
        let path = match &task.working_dir {
            Some(dir) if !platform.is_absolute(&path) => {
                platform.join(&expand(context, dir), &path)
            }
            _ => path,
        };
        // Read for the binary's name only; never shown.
        if let Ok(data) = machine.read(&path) {
            let data = data.get(..SCRIPT_LIMIT.min(data.len())).unwrap_or_default();
            // A script may be saved as UTF-16.
            text.push('\n');
            text.push_str(&decode_output(data));
        }
    }
    if !mentions(&text) {
        return None;
    }
    let names_exe = exe.is_some_and(|exe| {
        text.to_ascii_lowercase()
            .contains(&exe.to_ascii_lowercase())
    });
    let program = file_name(platform, script.as_deref().unwrap_or(&command));
    Some(TaskRuns::Launcher { program, names_exe })
}

fn scheduled_tasks(machine: &mut dyn Machine) -> Result<Vec<Task>, String> {
    let output = run_checked(
        machine,
        &Cmd::new("schtasks.exe", &["/query", "/xml", "ONE"]),
    )?;
    Ok(read_tasks(&output.stdout))
}

/// The Windows service that runs `process`, itself or through its parent
/// when that is NSSM: by its image name, or by the binary of the service it
/// hosts. Any other parent is not a wrapper.
fn windows_service_of(machine: &mut dyn Machine, process: &Proc) -> Option<Starter> {
    let mut hosts = vec![(process.pid, None)];
    if let Some(super::machine::Parent {
        pid,
        name: Some(name),
    }) = &process.parent
    {
        hosts.push((*pid, Some(name.clone())));
    }
    for (pid, wrapper) in hosts {
        for name in services_of(machine, pid) {
            let Some(config) = service_config(machine, &name) else {
                continue;
            };
            let (program, _) = split_service_command(&config.command);
            let program = file_name(Platform::Windows, &program);
            if program.eq_ignore_ascii_case("svchost.exe") {
                continue;
            }
            if let Some(image) = &wrapper
                && !is_nssm(image)
                && !is_nssm(&program)
            {
                continue;
            }
            return Some(Starter::WindowsService {
                name,
                start: config.start,
                running: true,
                wrapper,
            });
        }
    }
    None
}

/// The directory a task runs in: its `WorkingDirectory`, or, with none,
/// `%windir%\System32`. `None` when a variable in it isn't known.
fn task_dir(context: &Context, task: &Task) -> Option<String> {
    let dir = match task.working_dir.as_deref().filter(|dir| !dir.is_empty()) {
        Some(dir) => expand(context, dir),
        None => [r"%windir%\System32", r"%SystemRoot%\System32"]
            .iter()
            .map(|dir| expand(context, dir))
            .find(|dir| !dir.contains('%'))?,
    };
    (!dir.contains('%')).then_some(dir)
}

/// Whether `text` still has a `%NAME%` that [`expand`] didn't know.
fn has_unknown_variable(text: &str) -> bool {
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        let after = rest.get(start + 1..).unwrap_or_default();
        let Some(end) = after.find('%') else {
            return false;
        };
        let name = after.get(..end).unwrap_or_default();
        if !name.is_empty() && !name.contains(char::is_whitespace) {
            return true;
        }
        rest = after.get(end..).unwrap_or_default();
    }
    false
}

/// What a task's command, arguments and directory leave unsaid: a variable
/// in them that `migrate` doesn't know, so what the task runs can't be
/// told. Names the task and the part, never its value.
fn task_unknown(context: &Context, task: &Task) -> Option<String> {
    let part = if has_unknown_variable(&expand(context, &task.command)) {
        "command"
    } else if has_unknown_variable(&expand(context, &task.arguments)) {
        "arguments"
    } else if task_dir(context, task).is_none() {
        "working directory"
    } else {
        return None;
    };
    Some(format!(
        "the {part} of the scheduled task {} uses a variable that migrate doesn't know, so where and how it starts CLIProxyAPI can't be told: switch by hand, as docs/migrating-from-cliproxyapi.md says",
        task.name
    ))
}

/// Whether a task started a process.
#[derive(Debug, PartialEq, Eq)]
enum Started {
    Yes,
    No,
    /// It runs the same command, but whether it started this process can't
    /// be told: why.
    Unknown(String),
}

/// Whether `task` is the one that started `process`: it runs the same
/// program (see [`task_runs`]) with the same arguments, which carry the
/// config, and in the same working directory. A task with no working
/// directory runs in `%windir%\System32`. When the task's command line or
/// directory can't be told, or the process's directory can't be read, it
/// isn't known.
fn task_started(context: &Context, task: &Task, process: &Proc) -> Started {
    if let Some(why) = task_unknown(context, task) {
        return Started::Unknown(why);
    }
    let arguments = split_windows(&expand(context, &task.arguments));
    if arguments != process.args {
        return Started::No;
    }
    let Some(dir) = task_dir(context, task) else {
        return Started::No;
    };
    match process.cwd.as_deref() {
        Some(cwd) if same_path(Platform::Windows, &dir, cwd) => Started::Yes,
        Some(_) => Started::No,
        None => Started::Unknown(format!(
            "the scheduled task {} runs the same command, but CLIProxyAPI's working directory can't be read to tell whether that task started it: switch by hand, as docs/migrating-from-cliproxyapi.md says",
            task.name
        )),
    }
}

/// What `migrate` says of the scheduled tasks that run the binary: the one
/// starter, or all of them by name with a blocker when there is more than
/// one. The principal is kept: open-ferry's task runs as the user who runs
/// `migrate`, so a task of another user can't be switched.
fn task_starter(machine: &mut dyn Machine, tasks: &[&Task], blockers: &mut Vec<String>) -> Starter {
    let names: Vec<&str> = tasks.iter().map(|task| task.name.as_str()).collect();
    let starter = Starter::Task {
        name: names.join(", "),
        enabled: tasks.iter().any(|task| task.enabled),
    };
    let [task] = tasks else {
        blockers.push(format!(
            "more than one scheduled task runs CLIProxyAPI this way ({}): disable all but the one that starts it, then run `open-ferry migrate` again",
            names.join(", ")
        ));
        return starter;
    };
    // open-ferry's task is made with one principal: the user who runs
    // `migrate`, `InteractiveToken`, `LeastPrivilege`. A task that runs any
    // other way can't be replaced by it.
    let name = &task.name;
    let mut differs = |what: String| {
        blockers.push(format!(
            "the scheduled task {name} {what}, and open-ferry's task runs as you, only while you are logged on, without elevated rights: switch by hand, as docs/migrating-from-cliproxyapi.md says"
        ));
    };
    if task.principals != 1 {
        differs("doesn't have the one principal that says who it runs as".to_owned());
        return starter;
    }
    if let Some(group) = &task.group_id {
        differs(format!("runs as the group {group}"));
        return starter;
    }
    match task.logon_type.as_deref() {
        Some("InteractiveToken") => {}
        Some(kind) => differs(format!("has the logon type {kind}")),
        None => differs("has no logon type, so how it logs on can't be told".to_owned()),
    }
    match task.run_level.as_deref() {
        None | Some("LeastPrivilege") => {}
        Some(level) => differs(format!("runs with the run level {level}")),
    }
    let Some(user) = &task.user_id else {
        differs("doesn't say which user it runs as".to_owned());
        return starter;
    };
    match user_identity(machine) {
        Ok((name, sid)) => {
            if !user.eq_ignore_ascii_case(&sid) && !user.eq_ignore_ascii_case(&name) {
                blockers.push(format!(
                    "the scheduled task {} runs as {user}, and open-ferry's task would run as you ({name}): run `open-ferry migrate` as that user, or switch by hand, as docs/migrating-from-cliproxyapi.md says",
                    task.name
                ));
            }
        }
        Err(error) => blockers.push(format!(
            "the scheduled task {} runs as {user}, and who you are can't be told ({error}), so it can't be seen that open-ferry's task would run as the same user",
            task.name
        )),
    }
    starter
}

fn windows_process(
    machine: &mut dyn Machine,
    context: &Context,
    process: Proc,
) -> Result<Found, String> {
    if let Some(starter) = windows_service_of(machine, &process) {
        return Ok(Found::from_process(process, starter));
    }
    let mut notes = Vec::new();
    let tasks = scheduled_tasks(machine).unwrap_or_else(|error| {
        notes.push(format!("couldn't read the scheduled tasks: {error}"));
        Vec::new()
    });
    let exe = process.exe.clone();
    let mut direct: Vec<&Task> = Vec::new();
    let mut unknown = Vec::new();
    let mut launchers = Vec::new();
    for task in &tasks {
        match task_runs(machine, context, task, exe.as_deref()) {
            Some(TaskRuns::Binary) => match task_started(context, task, &process) {
                Started::Yes => direct.push(task),
                Started::No => {}
                Started::Unknown(why) => {
                    direct.push(task);
                    unknown.push(why);
                }
            },
            Some(TaskRuns::Launcher { program, names_exe }) => {
                launchers.push((!names_exe, !task.enabled, task.name.clone(), program));
            }
            None => {}
        }
    }
    if !direct.is_empty() {
        let mut blockers = Vec::new();
        let starter = task_starter(machine, &direct, &mut blockers);
        blockers.extend(unknown);
        let mut found = Found::from_process(process, starter);
        found.blockers = blockers;
        found.notes = notes;
        return Ok(found);
    }
    // The task that names the binary's path first, then an enabled one.
    launchers.sort();
    let launcher = match launchers.into_iter().next() {
        Some((_, _, name, program)) => Launcher::Task { name, program },
        None => parent_launcher(&process),
    };
    let mut found = Found::from_process(process, Starter::Launcher(launcher));
    found.notes = notes;
    Ok(found)
}

/// What an NSSM service's parameters (in the registry) say it runs.
struct NssmParameters {
    application: String,
    arguments: String,
    directory: Option<String>,
}

/// The parameters of the NSSM service `name`, from the registry
/// (`reg query`, a read of a key named by the service's name).
fn nssm_parameters(
    machine: &mut dyn Machine,
    context: &Context,
    name: &str,
) -> Option<NssmParameters> {
    let key = format!(r"HKLM\SYSTEM\CurrentControlSet\Services\{name}\Parameters");
    let output = machine
        .run(&Cmd::new("reg.exe", &["query"]).arg(key))
        .ok()
        .filter(|output| output.success())?;
    // `    Application    REG_SZ    C:\cpa\cli-proxy-api.exe`
    let value = |wanted: &str| {
        output.stdout.lines().find_map(|line| {
            let (key, rest) = line.trim().split_once(char::is_whitespace)?;
            if !key.eq_ignore_ascii_case(wanted) {
                return None;
            }
            let rest = rest.trim_start();
            let (kind, value) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            kind.starts_with("REG_")
                .then(|| expand(context, value.trim()))
        })
    };
    Some(NssmParameters {
        application: value("Application")?,
        arguments: value("AppParameters").unwrap_or_default(),
        directory: value("AppDirectory").filter(|dir| !dir.is_empty()),
    })
}

fn windows_not_running(
    machine: &mut dyn Machine,
    context: &Context,
) -> Result<Option<Found>, String> {
    for name in KNOWN_SERVICES {
        let Some(config) = service_config(machine, name) else {
            continue;
        };
        let (exe, args) = split_service_command(&config.command);
        let program = file_name(Platform::Windows, &exe);
        let (exe, args, cwd, wrapper, source) = if is_binary_name(&program) {
            (exe, args, None, None, format!("the service {name}"))
        } else if is_nssm(&program) {
            match nssm_parameters(machine, context, name) {
                Some(nssm) if is_binary_name(&file_name(Platform::Windows, &nssm.application)) => (
                    nssm.application,
                    split_windows(&nssm.arguments),
                    nssm.directory,
                    Some(program),
                    format!("the NSSM parameters of the service {name}"),
                ),
                _ => continue,
            }
        } else {
            continue;
        };
        let mut found = Found::new(Starter::WindowsService {
            name: name.to_owned(),
            start: config.start,
            running: false,
            wrapper,
        });
        found.exe = Some(exe);
        found.args = args;
        found.cwd = cwd;
        found.notes.push(format!(
            "CLIProxyAPI isn't running: its command line is read from {source}"
        ));
        return Ok(Some(found));
    }
    let tasks = scheduled_tasks(machine).unwrap_or_default();
    let mut direct: Vec<&Task> = Vec::new();
    let mut launcher = None;
    for task in &tasks {
        match task_runs(machine, context, task, None) {
            Some(TaskRuns::Binary) => direct.push(task),
            Some(TaskRuns::Launcher { program, .. }) => {
                launcher.get_or_insert((task, program));
            }
            None => {}
        }
    }
    if let Some(first) = direct.first() {
        let mut blockers = Vec::new();
        let starter = task_starter(machine, &direct, &mut blockers);
        blockers.extend(task_unknown(context, first));
        let mut found = Found::new(starter);
        found.blockers = blockers;
        found.exe = Some(expand(context, &first.command));
        // The task scheduler expands the variables in the arguments too,
        // before the program splits them.
        found.args = split_windows(&expand(context, &first.arguments));
        found.cwd = task_dir(context, first);
        found.notes.push(format!(
            "CLIProxyAPI isn't running: its command line is read from the task {}",
            first.name
        ));
        return Ok(Some(found));
    }
    if let Some((task, program)) = launcher {
        let mut found = Found::new(Starter::Launcher(Launcher::Task {
            name: task.name.clone(),
            program,
        }));
        found.blockers.push(format!(
            "the scheduled task {} starts CLIProxyAPI through a launcher, and CLIProxyAPI isn't running: start it, then run `open-ferry migrate` again",
            task.name
        ));
        return Ok(Some(found));
    }
    Ok(None)
}

// --- Docker ---

/// A running container from CLIProxyAPI's image, asked of Docker. `None`
/// when Docker isn't there or runs none.
fn docker_container(machine: &mut dyn Machine) -> Option<Container> {
    let cmd = Cmd::new("docker", &["ps", "--no-trunc", "--format", "{{json .}}"]);
    let output = machine.run(&cmd).ok().filter(|output| output.success())?;
    output.stdout.lines().find_map(|line| {
        let value: Value = serde_json::from_str(line).ok()?;
        let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
        let image = field("Image").unwrap_or_default();
        let name = field("Names").unwrap_or_default();
        if !mentions(&image) && !mentions(&name) {
            return None;
        }
        let labels: BTreeMap<String, String> = field("Labels")
            .unwrap_or_default()
            .split(',')
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect();
        let label = |key: &str| labels.get(key).cloned();
        Some(Container {
            name: Some(name).filter(|name| !name.is_empty()),
            image: Some(image).filter(|image| !image.is_empty()),
            project: label("com.docker.compose.project"),
            service: label("com.docker.compose.service"),
            working_dir: label("com.docker.compose.project.working_dir"),
            files: label("com.docker.compose.project.config_files"),
        })
    })
}

/// When no process runs: the service definitions, then Docker.
fn not_running(machine: &mut dyn Machine, context: &Context) -> Result<Option<Found>, String> {
    let found = match context.platform {
        Platform::Linux => linux_not_running(machine, context)?,
        Platform::MacOs => macos_not_running(machine, context)?,
        Platform::Windows => windows_not_running(machine, context)?,
    };
    if found.is_some() {
        return Ok(found);
    }
    Ok(docker_container(machine).map(|container| Found::new(Starter::Container(container))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    // Not upstream's: the command line is read as Go's flag package reads
    // upstream's, keeping only -config's value.
    #[test]
    fn reads_flags_as_go_does() {
        let flags = read_flags(&words("--config /etc/c.yaml -password=hunter2 -no-browser"));
        assert_eq!(flags.config.as_deref(), Some("/etc/c.yaml"));
        assert_eq!(flags.names, ["config", "password", "no-browser"]);
        assert!(!format!("{flags:?}").contains("hunter2"));
        let flags = read_flags(&words("-config=a.yaml -password hunter2 -tui run -x"));
        assert_eq!(flags.config.as_deref(), Some("a.yaml"));
        assert_eq!(flags.names, ["config", "password", "tui"]);
        assert!(flags.rest);
        assert!(!format!("{flags:?}").contains("hunter2"));
        let flags = read_flags(&words("-config a -- -config b"));
        assert_eq!(flags.config.as_deref(), Some("a"));
        assert!(flags.rest);
        let flags = read_flags(&words("-nope -config b"));
        assert_eq!(flags.unknown.as_deref(), Some("-nope"));
        assert_eq!(flags.config, None);
        let flags = read_flags(&words("-=secret"));
        assert_eq!(flags.unknown.as_deref(), Some("a malformed flag"));
        // The last -config wins, as with Go's flag package.
        let flags = read_flags(&words("-config a -config b"));
        assert_eq!(flags.config.as_deref(), Some("b"));
        assert_eq!(flags.names, ["config"]);
    }

    // Not upstream's: the config is -config's, from the working directory,
    // or upstream's default, config.yaml there.
    #[test]
    fn finds_the_config_of_a_command_line() {
        assert_eq!(
            config_of(
                Platform::Linux,
                &words("-config c/x.yaml"),
                Some("/srv/cpa")
            ),
            Some("/srv/cpa/c/x.yaml".to_owned())
        );
        assert_eq!(
            config_of(Platform::Linux, &[], Some("/srv/cpa")),
            Some("/srv/cpa/config.yaml".to_owned())
        );
        assert_eq!(config_of(Platform::Linux, &[], None), None);
        assert_eq!(
            config_of(
                Platform::Windows,
                &words(r"--config=C:\cpa\config.yaml"),
                None
            ),
            Some(r"C:\cpa\config.yaml".to_owned())
        );
    }

    // Not upstream's: control groups name the unit, user or system, or a
    // container.
    #[test]
    fn reads_control_groups() {
        assert_eq!(
            read_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/cliproxyapi.service\n"
            ),
            Group::Unit {
                unit: "cliproxyapi.service".to_owned(),
                user: Some(1000)
            }
        );
        assert_eq!(
            read_cgroup("0::/system.slice/cli-proxy-api.service\n"),
            Group::Unit {
                unit: "cli-proxy-api.service".to_owned(),
                user: None
            }
        );
        assert_eq!(
            read_cgroup("0::/system.slice/docker-0123abcd.scope\n"),
            Group::Container
        );
        assert_eq!(
            read_cgroup("0::/user.slice/user-1000.slice/session-2.scope\n"),
            Group::Other
        );
        assert_eq!(
            read_cgroup("12:pids:/\n1:name=systemd:/system.slice/cron.service\n"),
            Group::Unit {
                unit: "cron.service".to_owned(),
                user: None
            }
        );
    }

    // Not upstream's: `systemctl show`'s command line, variables and
    // environment files.
    #[test]
    fn reads_systemctl_show() {
        let shown = Shown::read(
            "ExecStart={ path=/usr/local/bin/cli-proxy-api ; argv[]=/usr/local/bin/cli-proxy-api --config /srv/c.yaml ; ignore_errors=no ; start_time=[n/a] }\nEnvironment=A=1 B=2\nEnvironmentFiles=/etc/default/cpa (ignore_errors=no)\nEnvironmentFiles=-/srv/.env (ignore_errors=yes)\n",
        );
        assert_eq!(
            shown.exec_start(),
            ["/usr/local/bin/cli-proxy-api", "--config", "/srv/c.yaml"]
        );
        assert_eq!(
            shown.environment(),
            [
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "2".to_owned())
            ]
        );
        assert_eq!(shown.environment_files(), ["/etc/default/cpa", "/srv/.env"]);
    }

    // Not upstream's: Windows command lines split as the C runtime splits
    // them, and a service's unquoted path runs to its `.exe`.
    #[test]
    fn splits_windows_command_lines() {
        assert_eq!(
            split_windows(r#"-NoProfile -File "C:\My Tools\start cpa.ps1" -x"#),
            ["-NoProfile", "-File", r"C:\My Tools\start cpa.ps1", "-x"]
        );
        assert_eq!(split_windows(r#"a\\"b c" d"#), [r"a\b c", "d"]);
        assert_eq!(
            split_service_command(r"C:\Program Files\CPA\cli-proxy-api.exe --config C:\cpa\c.yaml"),
            (
                r"C:\Program Files\CPA\cli-proxy-api.exe".to_owned(),
                vec!["--config".to_owned(), r"C:\cpa\c.yaml".to_owned()]
            )
        );
        assert_eq!(
            split_service_command(r#""C:\cpa\nssm.exe""#),
            (r"C:\cpa\nssm.exe".to_owned(), Vec::new())
        );
    }

    // Not upstream's: a launchd plist's program, arguments, directory and
    // variables.
    #[test]
    fn reads_launchd_plists() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>KeepAlive</key>
	<true/>
	<key>Label</key>
	<string>homebrew.mxcl.cliproxyapi</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PGSTORE_DSN</key>
		<string>postgres://x</string>
	</dict>
	<key>ProgramArguments</key>
	<array>
		<string>/opt/homebrew/opt/cliproxyapi/bin/cliproxyapi</string>
		<string>-config</string>
		<string>/a &amp; b.yaml</string>
	</array>
	<key>WorkingDirectory</key>
	<string>/opt/homebrew/var</string>
</dict>
</plist>
"#;
        let job = read_plist(xml);
        assert_eq!(job.label, BREW_LABEL);
        assert_eq!(
            job.program.as_deref(),
            Some("/opt/homebrew/opt/cliproxyapi/bin/cliproxyapi")
        );
        assert_eq!(job.args, ["-config", "/a & b.yaml"]);
        assert_eq!(job.working_dir.as_deref(), Some("/opt/homebrew/var"));
        assert_eq!(
            job.env,
            [("PGSTORE_DSN".to_owned(), "postgres://x".to_owned())]
        );
        assert_eq!(
            brew_prefix("/opt/homebrew/Cellar/cliproxyapi/8.0.20/bin/cliproxyapi"),
            Some("/opt/homebrew")
        );
        assert_eq!(
            brew_prefix("/opt/homebrew/opt/cliproxyapi/bin/cliproxyapi"),
            Some("/opt/homebrew")
        );
        assert_eq!(brew_prefix("/usr/local/bin/cli-proxy-api"), None);
    }

    // Not upstream's: `schtasks /query /xml ONE` names each task in a
    // comment before it.
    #[test]
    fn reads_scheduled_tasks() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n<Tasks>\r\n  <!-- \\CLIProxyAPI -->\r\n  <Task version=\"1.2\">\r\n    <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>S-1-5-18</UserId></LogonTrigger></Triggers>\r\n    <Principals><Principal id=\"Author\"><UserId>S-1-5-21-1-2-3-1001</UserId><LogonType>InteractiveToken</LogonType></Principal></Principals>\r\n    <Settings><Enabled>false</Enabled></Settings>\r\n    <Actions Context=\"Author\"><Exec><Command>\"C:\\cpa\\cli-proxy-api.exe\"</Command><Arguments>--config &quot;C:\\cpa\\config.yaml&quot;</Arguments><WorkingDirectory>C:\\cpa</WorkingDirectory></Exec></Actions>\r\n  </Task>\r\n  <!-- \\Other -->\r\n  <Task version=\"1.2\"><RegistrationInfo><URI>\\Other</URI></RegistrationInfo><Actions><Exec><Command>notepad.exe</Command></Exec></Actions></Task>\r\n</Tasks>\r\n";
        let tasks = read_tasks(xml);
        assert_eq!(
            tasks,
            [
                Task {
                    name: r"\CLIProxyAPI".to_owned(),
                    enabled: false,
                    command: r"C:\cpa\cli-proxy-api.exe".to_owned(),
                    arguments: r#"--config "C:\cpa\config.yaml""#.to_owned(),
                    working_dir: Some(r"C:\cpa".to_owned()),
                    user_id: Some("S-1-5-21-1-2-3-1001".to_owned()),
                    logon_type: Some("InteractiveToken".to_owned()),
                    run_level: None,
                    group_id: None,
                    principals: 1,
                },
                Task {
                    name: r"\Other".to_owned(),
                    enabled: true,
                    command: "notepad.exe".to_owned(),
                    arguments: String::new(),
                    working_dir: None,
                    user_id: None,
                    logon_type: None,
                    run_level: None,
                    group_id: None,
                    principals: 0,
                },
            ]
        );
    }

    // Not upstream's: `schtasks` prints UTF-16, which is decoded by its
    // byte order mark, or as little-endian when there is none, and the
    // tasks read the same as from UTF-8.
    #[test]
    fn reads_scheduled_tasks_from_utf16() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n<Tasks>\r\n  <!-- \\Caf\u{e9} \u{20ac} -->\r\n  <Task version=\"1.2\"><Actions><Exec><Command>C:\\cpa\\cli-proxy-api.exe</Command></Exec></Actions></Task>\r\n</Tasks>\r\n";
        let expected = read_tasks(xml);
        assert_eq!(expected[0].name, "\\Caf\u{e9} \u{20ac}");
        let le: Vec<u8> = xml.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let be: Vec<u8> = xml.encode_utf16().flat_map(u16::to_be_bytes).collect();
        let with = |mark: &[u8], body: &[u8]| [mark, body].concat();
        for bytes in [
            with(&[0xFF, 0xFE], &le),
            with(&[0xFE, 0xFF], &be),
            le.clone(),
            with(&[0xEF, 0xBB, 0xBF], xml.as_bytes()),
            xml.as_bytes().to_vec(),
        ] {
            assert_eq!(read_tasks(&decode_output(&bytes)), expected);
        }
    }

    // Not upstream's: UTF-16LE without a byte order mark is told by the
    // start of the XML, so a task whose description is nearly all CJK text
    // reads like any other, and so does the same with the mark.
    #[test]
    fn reads_cjk_tasks_from_utf16_with_and_without_a_mark() {
        let description =
            "\u{30d7}\u{30ed}\u{30ad}\u{30b7}\u{3092}\u{8d77}\u{52d5}\u{3059}\u{308b}".repeat(40);
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n<Tasks>\r\n  <!-- \\CLIProxyAPI -->\r\n  <Task version=\"1.2\"><RegistrationInfo><Description>{description}</Description></RegistrationInfo><Actions><Exec><Command>C:\\cpa\\cli-proxy-api.exe</Command></Exec></Actions></Task>\r\n</Tasks>\r\n"
        );
        let le: Vec<u8> = xml.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let marked = [&[0xFF, 0xFE][..], &le].concat();
        for bytes in [&le, &marked] {
            let tasks = read_tasks(&decode_output(bytes));
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].name, r"\CLIProxyAPI");
            assert_eq!(tasks[0].command, r"C:\cpa\cli-proxy-api.exe");
        }
        // Short text that isn't XML, with NULs at odd offsets only.
        let text: Vec<u8> = "ok\n".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_output(&text), "ok\n");
    }

    // Not upstream's: plain output isn't taken for UTF-16.
    #[test]
    fn decodes_plain_output_as_utf8() {
        assert_eq!(decode_output(b"ok\n"), "ok\n");
        assert_eq!(decode_output(b""), "");
        assert_eq!(decode_output("caf\u{e9}".as_bytes()), "caf\u{e9}");
        assert_eq!(decode_output(b"a\xffb"), "a\u{fffd}b");
        assert_eq!(decode_output(b"ab\0"), "ab\0");
    }

    // Not upstream's: a command line is split as systemd splits it, with
    // quotes and escapes, and the prefixes on the program are taken off.
    #[test]
    fn splits_systemd_command_lines() {
        assert_eq!(
            systemd_words(r#"/opt/x -config "/srv/my cpa/c.yaml" 'a b' c\sd \x41\101"#),
            Some(strings(&[
                "/opt/x",
                "-config",
                "/srv/my cpa/c.yaml",
                "a b",
                "c d",
                "AA"
            ]))
        );
        assert_eq!(systemd_words(r#"a "b c"#), None);
        assert_eq!(systemd_words(r"a \q"), None);
        assert_eq!(
            systemd_words("/bin/x one ; /bin/y"),
            Some(strings(&["/bin/x", "one"]))
        );
        let unit = "[Unit]\nDescription=x\n[Service]\nExecStart=/bin/old\nExecStart=\n# a comment\nExecStart=-@+!/opt/c/cli-proxy-api argv0 \\\n  -config \"/srv/a b/c.yaml\"\nExecStart=/bin/second\n[Install]\nExecStart=/bin/not-this\n";
        let starts = |texts: &[&str]| {
            let mut commands = Vec::new();
            for text in texts {
                add_exec_starts(text, "unit", &mut commands);
            }
            commands
                .into_iter()
                .map(|(command, _)| command)
                .collect::<Vec<_>>()
        };
        // `@` makes the first word the program and the second argv[0].
        let commands = starts(&[unit]);
        assert_eq!(commands.len(), 2);
        assert_eq!(
            exec_command(&commands[0]),
            Some(ExecCommand {
                exe: "/opt/c/cli-proxy-api".to_owned(),
                argv: strings(&["argv0", "-config", "/srv/a b/c.yaml"]),
                plain_dollars: false,
            })
        );
        assert_eq!(
            exec_command(":/bin/x -a"),
            Some(ExecCommand {
                exe: "/bin/x".to_owned(),
                argv: strings(&["/bin/x", "-a"]),
                plain_dollars: true,
            })
        );
        assert_eq!(exec_command("@/bin/x"), None);
        assert!(starts(&["[Service]\nExecStart=\n"]).is_empty());
        // A drop-in's empty `ExecStart=` resets the unit's, and its own
        // follows.
        assert_eq!(
            starts(&[
                "[Service]\nExecStart=/bin/old -config /etc/old.yaml\n",
                "[Service]\nExecStart=\nExecStart=/bin/new -config \"/etc/cpa new.yaml\"\n",
            ]),
            ["/bin/new -config \"/etc/cpa new.yaml\""]
        );
    }

    #[test]
    fn finds_variables_and_specifiers() {
        for word in ["$X", "${X}", "a$", "%h", "x%n", "%%%h", "$$$X"] {
            assert!(expands(word), "{word}");
        }
        for word in ["abc", "$$", "a$$b", "%%", "100%%"] {
            assert!(!expands(word), "{word}");
        }
        assert!(has_unknown_variable(r"C:\%NOSUCH%\x"));
        assert!(!has_unknown_variable(r"C:\x"));
        assert!(!has_unknown_variable("100% sure, 5% more"));
        assert!(!has_unknown_variable("50%"));
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }
}
