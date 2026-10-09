//! The machine `migrate` works on, behind [`Machine`]: its processes, its
//! files, its service managers (through [`System`], as `service` runs
//! them), the proxy's port and the person at the terminal. [`Host`] is the
//! real one; the tests drive `migrate` with a fake.
//!
//! [`Host`] reads processes with `sysinfo`: first every process's name and
//! parent, then the command line, working directory and environment of
//! those named as CLIProxyAPI's binaries only. Those three can hold
//! secrets, such as upstream's `-password`: they are kept in memory, read
//! for the flags and variables `migrate` needs, and never printed, logged
//! or saved ([`Proc`]'s `Debug` leaves them out).

use std::fmt;
use std::io::{self, BufRead as _, IsTerminal as _, Write as _};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::check::{self, Finding};
use crate::os_service::{self, Cmd, Output, Owner, System};

/// A running process, as far as `migrate` reads it. Its `Debug` leaves out
/// its arguments and environment, which can hold secrets.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Proc {
    pub(crate) pid: u32,
    /// Its executable's file name, as the system names the process.
    pub(crate) name: String,
    /// Its executable's full path, when it can be read.
    pub(crate) exe: Option<String>,
    /// Its arguments, without the program's own name.
    pub(crate) args: Vec<String>,
    /// Its working directory, when it can be read.
    pub(crate) cwd: Option<String>,
    /// Its environment, when it can be read.
    pub(crate) env: Option<Vec<(String, String)>>,
    /// The process that started it, when it is known.
    pub(crate) parent: Option<Parent>,
    /// When it started, in seconds since 1970: a process ID that is used
    /// again is a different process, with a different start time.
    pub(crate) started: Option<u64>,
}

impl fmt::Debug for Proc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proc")
            .field("pid", &self.pid)
            .field("name", &self.name)
            .field("exe", &self.exe)
            .field("args", &self.args.len())
            .field("cwd", &self.cwd)
            .field("env", &self.env.as_ref().map(Vec::len))
            .field("parent", &self.parent)
            .field("started", &self.started)
            .finish()
    }
}

/// A process's parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Parent {
    pub(crate) pid: u32,
    /// Its name, or `None` when it no longer runs.
    pub(crate) name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Dir,
    /// A symbolic link.
    Link,
    /// Anything else that is neither.
    Other,
}

/// An entry of a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) kind: EntryKind,
}

/// A program to start in the background, apart from `migrate`. Its `Debug`
/// leaves out its arguments and environment.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    pub(crate) exe: String,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: String,
    /// Its whole environment, or `None` for `migrate`'s own.
    pub(crate) env: Option<Vec<(String, String)>>,
    /// Where what it prints goes.
    pub(crate) log: String,
}

impl fmt::Debug for Launch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Launch")
            .field("exe", &self.exe)
            .field("args", &self.args.len())
            .field("cwd", &self.cwd)
            .field("env", &self.env.as_ref().map(Vec::len))
            .field("log", &self.log)
            .finish()
    }
}

/// What answered on the proxy's port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    OpenFerry,
    /// CLIProxyAPI, by the message it answers `/` with.
    CliProxyApi,
    /// Something else, described.
    Other(String),
    /// Nothing, and why.
    Nothing(String),
}

/// A lock that is held until this is dropped.
pub(crate) type Held = Box<dyn std::any::Any>;

/// Everything `migrate` does to the machine, besides running commands and
/// reading and writing files through [`System`].
pub(crate) trait Machine: System {
    /// The running processes whose name is one of `names`, in any case,
    /// but for this one.
    fn processes(&mut self, names: &[&str]) -> io::Result<Vec<Proc>>;
    /// Whether process `pid` runs.
    fn running(&mut self, pid: u32) -> bool;
    /// When process `pid` started (see [`Proc::started`]), if it runs.
    fn started(&mut self, pid: u32) -> Option<u64>;
    /// Stops process `pid`, if it is still named `name` and, when `started`
    /// is given, started then: asks it to stop (on Windows, ends it at
    /// once), and waits for it, ending it after a while.
    fn stop(&mut self, pid: u32, name: &str, started: Option<u64>) -> io::Result<()>;
    /// Stops process `pid` if it started at `started`, whatever its name or
    /// the file it runs: for a process whose identity was taken before its
    /// file was moved.
    fn stop_exact(&mut self, pid: u32, started: u64) -> io::Result<()>;
    /// Takes the lock file `path`, made if it is not there, for as long as
    /// the answer is held. `None` when another process holds it.
    fn lock(&mut self, path: &str) -> io::Result<Option<Held>>;
    /// The name process `pid` was started under (its first argument), if it
    /// runs and it can be read. Not the file it runs: a process started
    /// through a symbolic link names the link.
    fn argv0(&mut self, pid: u32) -> Option<String>;
    /// Starts `launch` in the background, apart from this process, and
    /// gives its process ID.
    fn spawn(&mut self, launch: &Launch) -> io::Result<u32>;
    fn list_dir(&self, path: &str) -> io::Result<Vec<Entry>>;
    fn copy(&mut self, from: &str, to: &str) -> io::Result<()>;
    fn rename(&mut self, from: &str, to: &str) -> io::Result<()>;
    /// Where the symbolic link `path` points, as it is written; `None` when
    /// `path` is not a symbolic link.
    fn link_target(&self, path: &str) -> Option<String>;
    /// Makes `link` a symbolic link to `target`. Only on Unix.
    fn symlink(&mut self, target: &str, link: &str) -> io::Result<()>;
    /// The binary the install receipt names, if there is a receipt.
    fn installed_binary(&mut self) -> Option<String>;
    /// Creates `path`, which only its owner can open on Unix.
    fn create_private_dir(&mut self, path: &str) -> io::Result<()>;
    /// Makes the new file `path` hold `data`. It fails if anything is at
    /// `path`, a symbolic link too, which is never followed; on Unix only its
    /// owner can open the file.
    fn write_new(&mut self, path: &str, data: &[u8]) -> io::Result<()>;
    /// What answers `GET /` on `ip`, a loopback address, and `port`.
    fn probe(&mut self, ip: IpAddr, port: u16, tls: bool) -> Answer;
    /// `open-ferry check`'s findings for `config`, run in `working_dir`.
    fn check(
        &mut self,
        config: &str,
        working_dir: Option<&str>,
        management_password: bool,
    ) -> Vec<Finding>;
    fn sleep(&mut self, duration: Duration);
    fn now(&self) -> DateTime<Utc>;
    /// Whether a person can answer questions: the standard input and output
    /// are a terminal.
    fn terminal(&self) -> bool;
    /// Asks `question`, which ends with `[y/N]`, and whether the answer is
    /// yes.
    fn ask(&mut self, question: &str) -> bool;
}

/// How long a stopped process has to exit before it is ended.
const STOP_WAIT: Duration = Duration::from_secs(15);

/// How often a stopped process is looked at.
const STOP_STEP: Duration = Duration::from_millis(200);

/// How long one probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The most of an answer a probe reads.
const PROBE_LIMIT: usize = 64 * 1024;

/// The real machine.
pub(crate) struct Host {
    system: os_service::Host,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Host {
    pub(crate) fn new() -> Host {
        Host {
            system: os_service::Host,
            runtime: None,
        }
    }

    fn block_on<F: Future>(&mut self, future: F) -> io::Result<F::Output> {
        if self.runtime.is_none() {
            self.runtime = Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            );
        }
        match &self.runtime {
            Some(runtime) => Ok(runtime.block_on(future)),
            None => Err(io::Error::other("no async runtime")),
        }
    }
}

impl System for Host {
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
        self.system.run(cmd)
    }

    fn show(&mut self, cmd: &Cmd) -> io::Result<Option<i32>> {
        self.system.show(cmd)
    }

    fn exists(&self, path: &str) -> bool {
        self.system.exists(path)
    }

    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.system.read(path)
    }

    fn real_path(&self, path: &str) -> io::Result<String> {
        self.system.real_path(path)
    }

    fn owner(&self, path: &str) -> io::Result<Owner> {
        self.system.owner(path)
    }

    fn create_dir_all(&mut self, path: &str) -> io::Result<()> {
        self.system.create_dir_all(path)
    }

    fn write(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        self.system.write(path, data)
    }

    fn remove(&mut self, path: &str) -> io::Result<()> {
        self.system.remove(path)
    }
}

fn lossy(text: &std::ffi::OsStr) -> String {
    text.to_string_lossy().into_owned()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The process `pid`, refreshed with `kind`, if it runs and isn't a zombie.
fn one_process(pid: u32, kind: sysinfo::ProcessRefreshKind) -> Option<(sysinfo::System, String)> {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, kind);
    let name = sys
        .process(pid)
        .filter(|process| process.status() != sysinfo::ProcessStatus::Zombie)
        .map(|process| lossy(process.name()))?;
    Some((sys, name))
}

/// Whether process `pid` runs and is still the process named `name` (any
/// name when `None`) that started at `started`: an ID that is used again is
/// another process.
fn same_process_now(pid: u32, name: Option<&str>, started: u64) -> bool {
    let kind = sysinfo::ProcessRefreshKind::nothing();
    let Some((sys, found)) = one_process(pid, kind) else {
        return false;
    };
    name.is_none_or(|name| found.eq_ignore_ascii_case(name))
        && sys
            .process(sysinfo::Pid::from_u32(pid))
            .is_some_and(|process| process.start_time() == started)
}

/// Waits for the process to exit, as `same` tells it still runs, then ends
/// it. `same` is asked again just before `end`, and after every nap: once it
/// says no, the process asked to stop has exited, and whatever has its ID
/// now is left alone.
fn wait_then_end(
    pid: u32,
    mut same: impl FnMut() -> bool,
    mut end: impl FnMut(),
    mut nap: impl FnMut(),
) -> io::Result<()> {
    let waits = STOP_WAIT.as_millis() / STOP_STEP.as_millis();
    for _ in 0..waits {
        if !same() {
            return Ok(());
        }
        nap();
    }
    // It didn't stop when asked: end it, if it is still the same.
    if !same() {
        return Ok(());
    }
    end();
    for _ in 0..25 {
        if !same() {
            return Ok(());
        }
        nap();
    }
    Err(io::Error::other(format!(
        "process {pid} still runs after it was asked to stop"
    )))
}

/// Ends process `pid` at once, if it started at `started`. On Windows the
/// start time is read from the handle that is then ended, so the ID can't
/// be used again in between.
#[cfg(windows)]
fn end_exact(pid: u32, started: u64) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        TerminateProcess,
    };
    // SAFETY: the call takes plain values, and gives a handle or null.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return false;
    }
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the handle is open, and the four structures are valid for
    // writes.
    let read =
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    // The same count of seconds since 1970 that sysinfo reports.
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    let start = (ticks / 10_000_000).saturating_sub(11_644_473_600);
    // SAFETY: the handle is open, and is closed once, here.
    let ended = read != 0 && start == started && unsafe { TerminateProcess(handle, 1) } != 0;
    // SAFETY: as above.
    unsafe { CloseHandle(handle) };
    ended
}

/// Asks process `pid` to stop, if it is the process named `name` (any name
/// when `None`) that started at `started` (any time when `None`), waits for
/// it, and ends it after a while.
fn stop_process(pid: u32, name: Option<&str>, started: Option<u64>) -> io::Result<()> {
    let Some((sys, found)) = one_process(pid, sysinfo::ProcessRefreshKind::nothing()) else {
        return Ok(());
    };
    if let Some(name) = name
        && !found.eq_ignore_ascii_case(name)
    {
        return Err(io::Error::other(format!(
            "process {pid} is now {found}, not {name}, so it was left alone"
        )));
    }
    let Some(process) = sys.process(sysinfo::Pid::from_u32(pid)) else {
        return Ok(());
    };
    if let Some(want) = started
        && process.start_time() != want
    {
        return Err(io::Error::other(format!(
            "process {pid} started at a different time than the one that was seen, so it is another process and was left alone"
        )));
    }
    // What every later look at the ID is held to: the same name and the
    // same start time, else the process asked to stop has exited, and
    // the ID is another's.
    let start = started.unwrap_or_else(|| process.start_time());
    // On Unix, SIGTERM, which CLIProxyAPI and open-ferry both stop
    // gracefully on. Windows has no such signal for a program without a
    // window: it is ended at once, through a handle that checks the start
    // time.
    #[cfg(unix)]
    let asked = process.kill_with(sysinfo::Signal::Term).unwrap_or(false);
    #[cfg(windows)]
    let asked = end_exact(pid, start);
    #[cfg(not(any(unix, windows)))]
    let asked = process.kill();
    if !asked && same_process_now(pid, name, start) {
        return Err(io::Error::other(format!("failed to stop process {pid}")));
    }
    wait_then_end(
        pid,
        || same_process_now(pid, name, start),
        // Looks again at the moment it ends it. On Windows the handle
        // that checks the start time is the one that ends the process. On
        // Unix a gap is left between this look and the signal: a pidfd
        // would close it, and sysinfo has none.
        || {
            #[cfg(windows)]
            {
                if name.is_none_or(|name| same_process_now(pid, Some(name), start)) {
                    end_exact(pid, start);
                }
            }
            #[cfg(not(windows))]
            if let Some((sys, found)) = one_process(pid, sysinfo::ProcessRefreshKind::nothing())
                && name.is_none_or(|name| found.eq_ignore_ascii_case(name))
                && let Some(process) = sys.process(sysinfo::Pid::from_u32(pid))
                && process.start_time() == start
            {
                process.kill();
            }
        },
        || std::thread::sleep(STOP_STEP),
    )
}

impl Machine for Host {
    fn processes(&mut self, names: &[&str]) -> io::Result<Vec<Proc>> {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
        let mut sys = sysinfo::System::new();
        // Names and parents only, of every process.
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        // Never this process: open-ferry run under CLIProxyAPI's name, as a
        // drop-in is, isn't CLIProxyAPI.
        let own = sysinfo::Pid::from_u32(std::process::id());
        let pids: Vec<sysinfo::Pid> = sys
            .processes()
            .iter()
            .filter(|(pid, process)| {
                let name = lossy(process.name());
                **pid != own && names.iter().any(|want| name.eq_ignore_ascii_case(want))
            })
            .map(|(pid, _)| *pid)
            .collect();
        if pids.is_empty() {
            return Ok(Vec::new());
        }
        // The rest, of those only.
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            false,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::Always)
                .with_cmd(UpdateKind::Always)
                .with_cwd(UpdateKind::Always)
                .with_environ(UpdateKind::Always),
        );
        let mut found = Vec::new();
        for pid in pids {
            let Some(process) = sys.process(pid) else {
                continue;
            };
            if process.status() == sysinfo::ProcessStatus::Zombie {
                continue;
            }
            let env: Vec<(String, String)> = process
                .environ()
                .iter()
                .filter_map(|entry| {
                    let entry = lossy(entry);
                    let (name, value) = entry.split_once('=')?;
                    (!name.is_empty()).then(|| (name.to_owned(), value.to_owned()))
                })
                .collect();
            let parent = process.parent().map(|parent| Parent {
                pid: parent.as_u32(),
                name: sys.process(parent).map(|process| lossy(process.name())),
            });
            found.push(Proc {
                pid: pid.as_u32(),
                name: lossy(process.name()),
                exe: process.exe().map(path_string),
                args: process.cmd().iter().skip(1).map(|arg| lossy(arg)).collect(),
                cwd: process.cwd().map(path_string),
                env: (!env.is_empty()).then_some(env),
                parent,
                started: Some(process.start_time()),
            });
        }
        found.sort_by_key(|proc| proc.pid);
        Ok(found)
    }

    fn running(&mut self, pid: u32) -> bool {
        one_process(pid, sysinfo::ProcessRefreshKind::nothing()).is_some()
    }

    fn started(&mut self, pid: u32) -> Option<u64> {
        let (sys, _) = one_process(pid, sysinfo::ProcessRefreshKind::nothing())?;
        sys.process(sysinfo::Pid::from_u32(pid))
            .map(sysinfo::Process::start_time)
    }

    fn stop(&mut self, pid: u32, name: &str, started: Option<u64>) -> io::Result<()> {
        stop_process(pid, Some(name), started)
    }

    fn stop_exact(&mut self, pid: u32, started: u64) -> io::Result<()> {
        stop_process(pid, None, Some(started))
    }

    fn lock(&mut self, path: &str) -> io::Result<Option<Held>> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Box::new(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    fn argv0(&mut self, pid: u32) -> Option<String> {
        let kind = sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always);
        let (sys, _) = one_process(pid, kind)?;
        sys.process(sysinfo::Pid::from_u32(pid))?
            .cmd()
            .first()
            .map(|arg| lossy(arg))
    }

    fn spawn(&mut self, launch: &Launch) -> io::Result<u32> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&launch.log)?;
        let command = || -> io::Result<std::process::Command> {
            let mut command = std::process::Command::new(&launch.exe);
            command
                .args(&launch.args)
                .current_dir(&launch.cwd)
                .stdin(std::process::Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log.try_clone()?);
            if let Some(env) = &launch.env {
                command.env_clear();
                command.envs(env.iter().map(|(name, value)| (name, value)));
            }
            Ok(command)
        };
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // Its own process group, so that a signal to the terminal's
            // doesn't reach it.
            let child = command()?.process_group(0).spawn()?;
            Ok(child.id())
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            use windows_sys::Win32::System::Threading::{
                CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
            };
            let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
            // Out of this process's job, if it is in one that allows it, so
            // that it outlives a terminal that ends the job as it closes.
            match command()?
                .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
                .spawn()
            {
                Ok(child) => Ok(child.id()),
                Err(_) => Ok(command()?.creation_flags(flags).spawn()?.id()),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Ok(command()?.spawn()?.id())
        }
    }

    fn list_dir(&self, path: &str) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = match entry.file_type() {
                Ok(kind) if kind.is_file() => EntryKind::File,
                Ok(kind) if kind.is_dir() => EntryKind::Dir,
                Ok(kind) if kind.is_symlink() => EntryKind::Link,
                _ => EntryKind::Other,
            };
            entries.push(Entry {
                name: lossy(&entry.file_name()),
                kind,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn copy(&mut self, from: &str, to: &str) -> io::Result<()> {
        std::fs::copy(from, to).map(|_| ())
    }

    fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn link_target(&self, path: &str) -> Option<String> {
        std::fs::read_link(path)
            .ok()
            .map(|target| path_string(&target))
    }

    fn symlink(&mut self, target: &str, link: &str) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(not(unix))]
        {
            let _ = (target, link);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "symbolic links are made on Linux and macOS only",
            ))
        }
    }

    fn installed_binary(&mut self) -> Option<String> {
        let data = open_ferry_update::DataDir::for_this_user().ok()?;
        match open_ferry_update::receipt::Receipt::load(&data.receipt_file()) {
            Ok(Some(receipt)) => Some(receipt.binary),
            _ => None,
        }
    }

    fn create_private_dir(&mut self, path: &str) -> io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        // A new directory: one already there is an error.
        builder.recursive(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(path)
    }

    fn write_new(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        // `create_new` fails on anything at the path, and never follows a
        // link there.
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(data)?;
        file.sync_all()
    }

    fn probe(&mut self, ip: IpAddr, port: u16, tls: bool) -> Answer {
        let address = SocketAddr::new(ip, port);
        match self.block_on(probe(address, tls, PROBE_TIMEOUT)) {
            Ok(answer) => answer,
            Err(error) => Answer::Nothing(format!("failed to start the async runtime: {error}")),
        }
    }

    fn check(
        &mut self,
        config: &str,
        working_dir: Option<&str>,
        management_password: bool,
    ) -> Vec<Finding> {
        // The checks read relative paths from the working directory, as the
        // proxy does: CLIProxyAPI's, which open-ferry keeps.
        let previous = std::env::current_dir().ok();
        if let Some(dir) = working_dir {
            let _ = std::env::set_current_dir(dir);
        }
        let mut env = check::Environment::current();
        env.management_password = management_password;
        // The config may not be ours, and this may run as root: it must not
        // run a program the config names.
        env.run_config_programs = false;
        let findings = self
            .block_on(check::run(Path::new(config), &env))
            .unwrap_or_default();
        if let Some(dir) = previous {
            let _ = std::env::set_current_dir(dir);
        }
        findings
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn terminal(&self) -> bool {
        io::stdin().is_terminal() && io::stdout().is_terminal()
    }

    fn ask(&mut self, question: &str) -> bool {
        let mut out = io::stdout().lock();
        let _ = write!(out, "{question} ");
        let _ = out.flush();
        let mut answer = String::new();
        if io::stdin().lock().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

/// What answers `GET /` at `address`, over TLS when `tls`, within
/// `timeout`. open-ferry answers it with `"message":"open-ferry-ai-proxy"`,
/// upstream with `"message":"CLI Proxy API Server"`. The route needs no
/// key, calls no provider, and is open in safe mode. Over TLS the
/// certificate isn't checked: the address is loopback, and nothing secret
/// is sent.
pub(crate) async fn probe(address: SocketAddr, tls: bool, timeout: Duration) -> Answer {
    match tokio::time::timeout(timeout, fetch_root(address, tls)).await {
        Ok(Ok(response)) => read_answer(&response),
        Ok(Err(error)) => Answer::Nothing(error.to_string()),
        Err(_) => Answer::Nothing(format!("no answer within {} seconds", timeout.as_secs())),
    }
}

async fn fetch_root(address: SocketAddr, tls: bool) -> io::Result<Vec<u8>> {
    let stream = tokio::net::TcpStream::connect(address).await?;
    if !tls {
        return exchange(stream, address).await;
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = rustls::pki_types::ServerName::IpAddress(address.ip().into());
    let stream = connector.connect(name, stream).await?;
    exchange(stream, address).await
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    address: SocketAddr,
) -> io::Result<Vec<u8>> {
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {address}\r\nUser-Agent: open-ferry-migrate\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    while response.len() < PROBE_LIMIT {
        let read = stream.read(&mut chunk).await;
        match read {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(chunk.get(..n).unwrap_or_default()),
            // A TLS peer that closes without a close_notify still sent its
            // answer.
            Err(_) if !response.is_empty() => break,
            Err(error) => return Err(error),
        }
    }
    Ok(response)
}

/// Who `response` says it is.
fn read_answer(response: &[u8]) -> Answer {
    let text = String::from_utf8_lossy(response);
    let status = text
        .lines()
        .next()
        .filter(|line| line.starts_with("HTTP/"))
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_owned);
    let Some(status) = status else {
        return Answer::Other("something that doesn't speak HTTP".to_owned());
    };
    let body = text.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    if status == "200" && compact.contains(r#""message":"open-ferry-ai-proxy""#) {
        Answer::OpenFerry
    } else if compact.contains(r#""message":"CLIProxyAPIServer""#) {
        Answer::CliProxyApi
    } else {
        Answer::Other(format!(
            "a server that isn't open-ferry (HTTP status {status})"
        ))
    }
}

/// Takes any certificate: for a probe of loopback that sends nothing
/// secret.
#[derive(Debug)]
struct AnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves `answer` once on loopback, and gives the address.
    async fn serve_once(answer: &'static str) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await;
            let _ = stream.write_all(answer.as_bytes()).await;
        });
        address
    }

    // Not upstream's: the probe tells open-ferry from CLIProxyAPI by `GET
    // /`, against servers on loopback.
    #[tokio::test]
    async fn probe_tells_open_ferry_from_cli_proxy_api() {
        let open_ferry = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"endpoints\":[],\"message\":\"open-ferry-ai-proxy\"}",
        )
        .await;
        assert_eq!(
            probe(open_ferry, false, Duration::from_secs(5)).await,
            Answer::OpenFerry
        );
        let upstream = serve_once(
            "HTTP/1.1 200 OK\r\n\r\n{\"message\": \"CLI Proxy API Server\", \"endpoints\": []}",
        )
        .await;
        assert_eq!(
            probe(upstream, false, Duration::from_secs(5)).await,
            Answer::CliProxyApi
        );
        let other = serve_once("HTTP/1.1 404 Not Found\r\n\r\nnope").await;
        assert_eq!(
            probe(other, false, Duration::from_secs(5)).await,
            Answer::Other("a server that isn't open-ferry (HTTP status 404)".to_owned())
        );
    }

    // Not upstream's: a closed port is nothing.
    #[tokio::test]
    async fn probe_of_a_closed_port_is_nothing() {
        // Bound but not listening, so nothing else takes the port.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = socket.local_addr().unwrap();
        assert!(matches!(
            probe(address, false, Duration::from_secs(5)).await,
            Answer::Nothing(_)
        ));
    }

    // Not upstream's: the backup's directory is new, and on Unix only its
    // owner can open it.
    #[test]
    fn the_backup_directory_is_new_and_private() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("open-ferry-migrate-20261008T120000Z");
        let path = dir.to_str().unwrap();
        let mut host = Host::new();
        host.create_private_dir(path).unwrap();
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let again = host.create_private_dir(path).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        // Its parent isn't made for it.
        let deeper = temp.path().join("missing").join("backup");
        assert!(host.create_private_dir(deeper.to_str().unwrap()).is_err());
    }

    // Not upstream's: a process ID that another process takes while one is
    // waited on is never ended: the identity is asked on every poll and again
    // just before the kill.
    #[test]
    fn a_reused_process_id_is_never_ended() {
        use std::cell::Cell;
        // The process exits during the wait, and its ID is reused: the same
        // question then says no, and nothing is ended.
        let asked = Cell::new(0);
        let ended = Cell::new(false);
        let result = wait_then_end(
            4242,
            || {
                asked.set(asked.get() + 1);
                asked.get() < 4
            },
            || ended.set(true),
            || {},
        );
        assert!(result.is_ok());
        assert!(!ended.get());

        // It was another process already by the time the wait ran out: the
        // check before the kill says no, and nothing is ended.
        let waits = STOP_WAIT.as_millis() / STOP_STEP.as_millis();
        let asked: Cell<u128> = Cell::new(0);
        let result = wait_then_end(
            4242,
            || {
                asked.set(asked.get() + 1);
                asked.get() <= waits
            },
            || ended.set(true),
            || {},
        );
        assert!(result.is_ok());
        assert!(!ended.get());

        // One that stays is ended, and then it must be gone.
        let result = wait_then_end(4242, || true, || ended.set(true), || {});
        assert!(ended.get());
        assert!(result.is_err());
        let gone = Cell::new(false);
        let result = wait_then_end(4242, || !gone.get(), || gone.set(true), || {});
        assert!(result.is_ok());
    }

    // Not upstream's: a new file is created and nothing else: an existing
    // file, or a link (even to nothing), is an error and is not written
    // through; on Unix only its owner can open it.
    #[test]
    fn write_new_never_follows_or_replaces() {
        let temp = tempfile::tempdir().unwrap();
        let mut host = Host::new();
        let file = temp.path().join("migration.json.1.2.3.tmp");
        host.write_new(file.to_str().unwrap(), b"record").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"record");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let again = host
            .write_new(file.to_str().unwrap(), b"other")
            .unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&file).unwrap(), b"record");

        #[cfg(unix)]
        {
            let target = temp.path().join("target");
            std::fs::write(&target, b"somebody else's").unwrap();
            let link = temp.path().join("link.tmp");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(host.write_new(link.to_str().unwrap(), b"record").is_err());
            assert_eq!(std::fs::read(&target).unwrap(), b"somebody else's");
            let dangling = temp.path().join("dangling.tmp");
            std::os::unix::fs::symlink(temp.path().join("nowhere"), &dangling).unwrap();
            assert!(
                host.write_new(dangling.to_str().unwrap(), b"record")
                    .is_err()
            );
            assert!(!temp.path().join("nowhere").exists());
        }
    }

    // Not upstream's: a process whose start time is not the one asked for is
    // left alone, on the one handle that would have ended it. (The test's own
    // process stands in: it must still be here after the call.)
    #[cfg(windows)]
    #[test]
    fn end_exact_leaves_a_process_with_another_start_time() {
        assert!(!end_exact(std::process::id(), 1));
        assert!(!end_exact(u32::MAX - 1, 1));
    }
}
