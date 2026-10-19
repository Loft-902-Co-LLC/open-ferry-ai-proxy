//! `open-ferry install-self`: what the install scripts run, from the binary
//! they downloaded and staged beside its destination, to put it in place.
//! Under the data directory's update lock it checks that no supervisor runs,
//! moves itself over the destination and writes the install receipt. Not for
//! people: the scripts run it.
//!
//! The replacement keeps rollback material until the receipt is published,
//! so a failure leaves the binary and the receipt as they were. The exit
//! codes: 0 installed; 1 failed, nothing changed or all restored (a path that
//! can't be looked at or resolved included); 2 usage (the arguments are wrong);
//! 3 a supervisor runs; 4 whether one runs can't be told; 5 the wait ran
//! out; 6 partly changed and the restore failed.
//! Scope, privilege and source/destination trust refusals exit 1, before any lock
//! or replacement. `-system` provisions the machine's update records.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use open_ferry_supervisor::client::{self, Detection, Running};
use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
use open_ferry_update::data_dir::DataDir;
use open_ferry_update::receipt::{Receipt, WriteError};

use crate::os_service::hint;
use crate::scope;

/// The subcommand's name: the first argument that runs it.
pub const NAME: &str = "install-self";

/// How long to wait for another update or install, when `-wait` isn't given.
const DEFAULT_WAIT: Duration = Duration::from_secs(120);
/// The most `-wait` takes: a day.
const MAX_WAIT_SECS: u64 = 24 * 60 * 60;
/// How often a busy lock is tried again.
const RETRY_EVERY: Duration = Duration::from_millis(250);
/// The most time detection gets in one attempt, so a peer that accepts and
/// never answers can't stretch the wait.
const DETECT_BUDGET: Duration = Duration::from_secs(3);

/// How the run ended: the process's exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exit {
    Installed = 0,
    Failed = 1,
    Usage = 2,
    Supervised = 3,
    Unknown = 4,
    WaitedOut = 5,
    PartlyChanged = 6,
}

/// The installer that runs `install-self`, as the receipt names it.
const INSTALLERS: [&str; 2] = ["install.sh", "install.ps1"];

/// What the arguments say.
#[derive(Debug, PartialEq, Eq)]
struct Options {
    from: PathBuf,
    to: PathBuf,
    installer: String,
    wait: Duration,
}

fn usage(program: &str) -> String {
    format!(
        "usage: {program} {NAME} -from <file> -to <file> -installer install.sh|install.ps1 [-wait <seconds>] [-system]\n\
         Not for people: the install scripts run it, from the binary they staged.\n"
    )
}

/// The scope flag is separate from the replacement's file options.
fn parse_with_scope(args: impl IntoIterator<Item = String>) -> Result<(Options, bool), String> {
    let mut from = None;
    let mut to = None;
    let mut installer = None;
    let mut wait = DEFAULT_WAIT;
    let mut system = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let name = arg.trim_start_matches('-');
        let dashes = arg.len() - name.len();
        if !matches!(name, "from" | "to" | "installer" | "wait" | "system")
            || !(1..=2).contains(&dashes)
        {
            return Err(format!("unknown argument: {arg}"));
        }
        if name == "system" {
            system = true;
            continue;
        }
        let Some(value) = args.next().filter(|value| !value.is_empty()) else {
            return Err(format!("{arg} needs a value"));
        };
        match name {
            "from" => from = Some(PathBuf::from(value)),
            "to" => to = Some(PathBuf::from(value)),
            "installer" => {
                if !INSTALLERS.contains(&value.as_str()) {
                    return Err(format!(
                        "-installer is install.sh or install.ps1, not {value}"
                    ));
                }
                installer = Some(value);
            }
            _ => {
                let seconds = value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| *seconds <= MAX_WAIT_SECS)
                    .ok_or_else(|| {
                        format!(
                            "-wait is a number of seconds, at most {MAX_WAIT_SECS}, not {value}"
                        )
                    })?;
                wait = Duration::from_secs(seconds);
            }
        }
    }
    Ok((
        Options {
            from: from.ok_or("-from is needed")?,
            to: to.ok_or("-to is needed")?,
            installer: installer.ok_or("-installer is needed")?,
            wait,
        },
        system,
    ))
}

#[cfg(test)]
fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    parse_with_scope(args).map(|(options, _)| options)
}

/// Runs `install-self` with `args`, the arguments after its name.
pub fn main<I>(program: &str, args: I) -> ExitCode
where
    I: IntoIterator<Item = String>,
{
    let (options, system) = match parse_with_scope(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{NAME}: {message}");
            eprint!("{}", usage(program));
            return ExitCode::from(Exit::Usage as u8);
        }
    };
    ExitCode::from(run_for_real(&options, system) as u8)
}

/// The run on this machine: the real binary, data directory, clock, files
/// and detection.
fn run_for_real(options: &Options, system: bool) -> Exit {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{NAME}: can't tell which file is running: {error}");
            return Exit::Failed;
        }
    };
    let destination = match options.to.to_str() {
        Some(destination) => destination,
        None => {
            eprintln!("{NAME}: -to isn't valid Unicode text. Nothing was changed.");
            return Exit::Usage;
        }
    };
    let source = match options.from.to_str() {
        Some(source) => source,
        None => {
            eprintln!("{NAME}: -from isn't valid Unicode text. Nothing was changed.");
            return Exit::Usage;
        }
    };
    let home = match install_home(&mut scope::Host, source, destination, system) {
        Ok(home) => home,
        Err(error) => {
            eprintln!("{NAME}: {error}");
            return Exit::Failed;
        }
    };
    if let Some(hint) = &home.machine_hint {
        println!("{hint}");
    }
    let identity = match Identity::current(home.kind) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("{NAME}: can't tell who is running: {error}");
            return Exit::Failed;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{NAME}: failed to start the async runtime: {error}");
            return Exit::Failed;
        }
    };
    let mut detect = |data_dir: &Path, installed: &Path, deadline: Instant| {
        runtime.block_on(client::detect(data_dir, &identity, installed, deadline))
    };
    let mut files = RealFiles;
    let mut clock = RealClock;
    let mut publish = |receipt: &Receipt, path: &Path| receipt.write_atomic(path);
    let mut env = Env {
        current_exe,
        data: home.data,
        windows: cfg!(windows),
        // The same variable `DataDir::for_this_user` reads on Unix.
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute()),
        unique: unique_suffix(),
        detect: &mut detect,
        files: &mut files,
        clock: &mut clock,
        publish: &mut publish,
    };
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut say = Say::new(&mut stdout, &mut stderr);
    say.kind = home.kind;
    install(options, &mut env, &mut say)
}

/// Selects the destination's records before a lock, stop or replacement.
fn install_home(
    probe: &mut dyn scope::Probe,
    source: &str,
    destination: &str,
    system: bool,
) -> Result<scope::Home, scope::Refusal> {
    if !system {
        let home = scope::lookup(probe, destination)?;
        home.require_privilege()?;
        return Ok(home);
    }
    // Check privilege before the destination or provisioning can do anything.
    if probe.platform() == crate::os_service::Platform::MacOs
        || !probe.privileged().is_ok_and(|privileged| privileged)
    {
        return scope::provision(probe);
    }
    let source_trust = probe.trusted_file(source).and_then(|()| {
        let parent = probe
            .platform()
            .parent(source)
            .ok_or_else(|| format!("{source} has no source directory"))?;
        probe.trusted_dir(&parent)
    });
    source_trust.map_err(|reason| scope::Refusal {
        reason: format!("Refusing -from {source}: {reason}"),
        remedy: Some("As root or an administrator, stage the installer binary in a directory writable only by root or administrators, then run it again with -system.".to_owned()),
    })?;
    let trust = match probe.state(destination) {
        scope::State::Missing => match probe.platform().parent(destination) {
            Some(parent) => probe.trusted_dir(&parent),
            None => Err(format!("{destination} has no destination directory")),
        },
        _ => probe.trusted_file(destination),
    };
    trust.map_err(|reason| scope::Refusal {
        reason: format!("Refusing -to {destination}: {reason}"),
        remedy: Some("As root or an administrator, fix the destination's ownership and permissions, or choose a trusted directory with --bin-dir / -InstallDir, then run the installer again with -system.".to_owned()),
    })?;
    let home = scope::provision(probe)?;
    home.require_privilege()?;
    Ok(home)
}

// --- What the run depends on, so tests can script it -------------------------

/// The detection of a supervisor: the data directory, the installed binary
/// and the deadline.
type Detect<'a> = dyn FnMut(&Path, &Path, Instant) -> Detection + 'a;
/// The publication of the receipt at a path.
type Publish<'a> = dyn FnMut(&Receipt, &Path) -> Result<(), WriteError> + 'a;

struct Env<'a> {
    /// The file that runs.
    current_exe: PathBuf,
    data: DataDir,
    /// Replace the way Windows has to (move the old file aside), not the
    /// way Unix does (a hard link to it); a field so each is tested anywhere.
    windows: bool,
    /// The user's home directory, where the default data directory lives.
    home: Option<PathBuf>,
    /// What makes the rollback material's name unique.
    unique: String,
    detect: &'a mut Detect<'a>,
    files: &'a mut dyn Files,
    clock: &'a mut dyn Clock,
    publish: &'a mut Publish<'a>,
}

/// The file operations the replacement and its rollback make.
trait Files {
    /// Moves `from` onto `to`, replacing any file there. On Windows it is
    /// written through, as the updater's moves are; on Unix the caller syncs
    /// the directory ([`Files::sync_dir`]).
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    fn hard_link(&mut self, original: &Path, link: &Path) -> io::Result<()>;
    fn copy(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&mut self, path: &Path) -> io::Result<()>;
    /// Makes the contents of the file `path` durable.
    fn sync_file(&mut self, path: &Path) -> io::Result<()>;
    /// Makes the entries of the directory `dir` durable, so a rename, link or
    /// new directory in it survives a crash. Nothing to do on Windows, whose
    /// write-through moves flush the log.
    fn sync_dir(&mut self, dir: &Path) -> io::Result<()>;
    /// Makes the one directory `dir`, private to the user on Unix.
    fn create_dir(&mut self, dir: &Path) -> io::Result<()>;
    /// `path` with symlinks and `..` followed, and no `\\?\` prefix.
    fn resolve(&mut self, path: &Path) -> io::Result<PathBuf>;
    /// What is at `path` (a symlink itself, not what it names): none only
    /// when it is not found; any other error means it can't be told.
    fn look(&mut self, path: &Path) -> io::Result<Option<fs::Metadata>>;
    /// Whether anything (a symlink included) is at `path`. Only "not found"
    /// is no; any other error means it can't be told.
    fn exists(&mut self, path: &Path) -> io::Result<bool>;
}

struct RealFiles;

impl Files for RealFiles {
    #[cfg(windows)]
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };

        let wide =
            |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
        let (from, to) = (wide(from), wide(to));
        // SAFETY: both are NUL-terminated UTF-16 strings that outlive the call.
        let moved = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_WRITE_THROUGH | MOVEFILE_REPLACE_EXISTING,
            )
        };
        if moved == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    #[cfg(unix)]
    fn sync_file(&mut self, path: &Path) -> io::Result<()> {
        // A read-only handle: the staged binary is the running image, and
        // Linux refuses to open that for writing (ETXTBSY). fsync flushes the
        // file through a read-only descriptor as well.
        fs::File::open(path)?.sync_all()
    }

    #[cfg(not(unix))]
    fn sync_file(&mut self, path: &Path) -> io::Result<()> {
        match fs::OpenOptions::new().write(true).open(path) {
            Ok(file) => file.sync_all(),
            // Windows won't open a running image for writing, and the staged
            // binary is this process's own image: it can't be flushed through
            // a handle of ours. The installer flushed the copy (FileStream
            // Flush(true)) and closed it before it ran this process, and the
            // write-through move that places it is the flush left. Any other
            // file in use is a real failure.
            Err(error) if error.raw_os_error() == Some(32) && is_own_image(path) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn sync_dir(&mut self, dir: &Path) -> io::Result<()> {
        // The updater's rule (EINVAL on a directory's fsync is done; nothing
        // on Windows), so the receipt and the binary agree on it.
        open_ferry_update::sync_directory(dir)
    }

    fn create_dir(&mut self, dir: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(dir)
        }
        #[cfg(not(unix))]
        {
            fs::DirBuilder::new().create(dir)
        }
    }

    fn resolve(&mut self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path).map(unverbatim)
    }

    fn look(&mut self, path: &Path) -> io::Result<Option<fs::Metadata>> {
        match fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(meta)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn exists(&mut self, path: &Path) -> io::Result<bool> {
        Ok(self.look(path)?.is_some())
    }

    fn hard_link(&mut self, original: &Path, link: &Path) -> io::Result<()> {
        fs::hard_link(original, link)
    }

    fn copy(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        fs::copy(from, to).map(|_| ())
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
}

/// Time, for the wait.
trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&mut self, duration: Duration);
}

struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Where the messages go: what is said, and what is wrong.
struct Say<'a> {
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    kind: ServiceKind,
}

impl<'a> Say<'a> {
    fn new(out: &'a mut dyn Write, err: &'a mut dyn Write) -> Self {
        Self {
            out,
            err,
            kind: ServiceKind::User,
        }
    }

    fn say(&mut self, message: &str) {
        let _ = writeln!(self.out, "{message}");
    }

    fn warn(&mut self, message: &str) {
        let _ = writeln!(self.err, "{NAME}: {message}");
    }
}

/// A suffix for rollback material's names, unique to this run.
fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

// --- The run ----------------------------------------------------------------

/// The installation `options` ask for, with `env` for everything it touches.
fn install(options: &Options, env: &mut Env<'_>, say: &mut Say<'_>) -> Exit {
    // The destination is resolved once, before any lock: the one path that
    // detection, the checks, the receipt and the replacement all use, so a
    // directory link retargeted meanwhile can't make the replacement act on a
    // binary the detection didn't cover.
    let dest = match canonical_dest(&options.to) {
        Ok(dest) => dest,
        Err(error) => {
            say.warn(&format!(
                "can't resolve -to {}: {error}. Nothing was changed.",
                options.to.display()
            ));
            return Exit::Usage;
        }
    };
    // Whether -to is there is asked of the files first: an error other than
    // "not found" is not a wrong argument (exit 2) but a failure to tell,
    // and nothing has changed.
    if let Err(error) = env.files.look(&dest) {
        say.warn(&format!(
            "can't tell whether {} exists: {error}. Nothing was changed.",
            dest.display()
        ));
        return Exit::Failed;
    }
    // The receipt records the destination as text, and the updater compares
    // that text with the binary it runs from: a lossy rendering would never
    // match, and auto-update would quietly never work. So refuse it, before
    // anything is touched. The checked text is what the receipt gets.
    let Some(binary) = dest.to_str().map(str::to_owned) else {
        say.warn(&format!(
            "-to {} isn't valid Unicode text, and the install receipt records it as text. Nothing was changed.",
            dest.display()
        ));
        return Exit::Usage;
    };
    if let Err(message) = check_paths(&options.from, &dest, &env.current_exe) {
        say.warn(&message);
        return Exit::Usage;
    }
    // Private, as the updater makes it.
    if let Err(error) = create_private_dir(env.data.root(), env.home.as_deref(), &mut *env.files) {
        say.warn(&format!(
            "can't make the data directory {}: {error}",
            env.data.root().display()
        ));
        return Exit::Failed;
    }

    // Both locks, kept until the receipt is published.
    let (_update_lock, _install_lock) = match wait_for_locks(options, &dest, env, say) {
        Ok(locks) => locks,
        Err(exit) => return exit,
    };

    let receipt = Receipt::new(
        &options.installer,
        open_ferry_update::CURRENT_VERSION,
        &binary,
        open_ferry_update::TARGET,
        chrono::Utc::now(),
    );
    replace(options, &dest, &receipt, env, say)
}

/// Waits, for at most `-wait`, for the update lock and for detection to
/// clear, and gives the locks. The exit when it can't.
fn wait_for_locks(
    options: &Options,
    dest: &Path,
    env: &mut Env<'_>,
    say: &mut Say<'_>,
) -> Result<
    (
        open_ferry_update::data_dir::UpdateLock,
        open_ferry_update::install_lock::InstallLock,
    ),
    Exit,
> {
    // One absolute deadline: every pause is capped by the time left, and
    // `-wait 0` makes one attempt.
    let deadline = env.clock.now() + options.wait;
    let mut told = false;
    loop {
        let waiting_for = match env.data.try_lock_existing_dir() {
            Err(error) => {
                say.warn(&format!(
                    "can't take the update lock in {}: {error}. Nothing was changed.",
                    env.data.root().display()
                ));
                return Err(Exit::Failed);
            }
            Ok(None) => {
                "another open-ferry update or install, which holds the update lock".to_owned()
            }
            Ok(Some(lock)) => {
                let detect_deadline = detect_deadline(env.clock.now(), deadline);
                match (env.detect)(env.data.root(), dest, detect_deadline) {
                    Detection::Clear(install_lock) => return Ok((lock, install_lock)),
                    Detection::Supervised(running) => {
                        say.warn(&supervised_message(&running, say.kind));
                        return Err(Exit::Supervised);
                    }
                    Detection::Refuse(reason) => {
                        say.warn(&format!(
                            "can't tell whether open-ferry is running from {}: {reason}. \
                             Nothing was changed.",
                            dest.display()
                        ));
                        return Err(Exit::Unknown);
                    }
                    // Not holding the update lock while it waits.
                    Detection::Busy(reason) => {
                        drop(lock);
                        format!(
                            "{reason} (a running open-ferry supervisor may be what holds the \
                             install lock)"
                        )
                    }
                }
            }
        };
        let now = env.clock.now();
        if now >= deadline {
            return Err(waited_out(options, &waiting_for, say));
        }
        if !told {
            say.say("Waiting for another open-ferry update or install to finish...");
            told = true;
        }
        env.clock.sleep(RETRY_EVERY.min(deadline - now));
        if env.clock.now() >= deadline {
            return Err(waited_out(options, &waiting_for, say));
        }
    }
}

fn waited_out(options: &Options, waiting_for: &str, say: &mut Say<'_>) -> Exit {
    say.warn(&format!(
        "gave up after waiting {} seconds for {waiting_for}. Nothing was changed. \
         Try again when it is done.",
        options.wait.as_secs()
    ));
    Exit::WaitedOut
}

/// The deadline of one detection: no later than the wait's, and no more
/// than [`DETECT_BUDGET`] from `now`. With `-wait 0` it has passed already:
/// the detection tries the install lock once, without waiting.
fn detect_deadline(now: Instant, wait_deadline: Instant) -> Instant {
    wait_deadline.min(now + DETECT_BUDGET)
}

fn supervised_message(running: &Running, kind: ServiceKind) -> String {
    let mut who = String::from("An open-ferry supervisor is running");
    if let Some(pid) = running.pid {
        who.push_str(&format!(" (process {pid}"));
        match &running.version {
            Some(version) => who.push_str(&format!(", version {version})")),
            None => who.push(')'),
        }
    }
    let update = match kind {
        ServiceKind::User => "Run `open-ferry update` to switch versions without downtime.",
        ServiceKind::System if cfg!(windows) => {
            "Run `open-ferry update` in a terminal opened as administrator to switch versions without downtime."
        }
        ServiceKind::System => {
            "Run `sudo open-ferry update` as root to switch versions without downtime."
        }
    };
    format!(
        "{who}, so the installer can't replace its binary. Nothing was changed.\n{update}\nTo rerun the installer instead:\n{}",
        hint::stop_and_retry(kind, "run the installer again")
    )
}

// --- Checks -----------------------------------------------------------------

/// Whether `meta` is a link or, on Windows, a reparse point.
fn is_link(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        has_reparse_attribute(meta.file_attributes())
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Whether Windows file `attributes` say a reparse point.
#[cfg(windows)]
fn has_reparse_attribute(attributes: u32) -> bool {
    attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// What is wrong with the files, if anything: `from` must be a regular file,
/// not a link, and the file that runs; `to` must not be a link or a
/// directory, and must be in a directory that exists.
fn check_paths(from: &Path, to: &Path, current_exe: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(from)
        .map_err(|error| format!("can't read -from {}: {error}", from.display()))?;
    if is_link(&meta) {
        return Err(format!(
            "-from {} is a link or reparse point: it must be the file itself",
            from.display()
        ));
    }
    if !meta.is_file() {
        return Err(format!("-from {} isn't a regular file", from.display()));
    }
    match same_file::is_same_file(from, current_exe) {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "-from {} isn't the binary that runs, {}: install-self moves itself",
                from.display(),
                current_exe.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "can't tell whether -from {} is the binary that runs: {error}",
                from.display()
            ));
        }
    }
    match fs::symlink_metadata(to) {
        Ok(meta) if is_link(&meta) => {
            return Err(format!(
                "-to {} is a link or reparse point: install to the file it names",
                to.display()
            ));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(format!("-to {} isn't a regular file", to.display()));
        }
        Ok(_) => {
            if same_file::is_same_file(from, to).unwrap_or(false) {
                return Err(format!("-from and -to are the same file, {}", to.display()));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        // Not reached from `install`, which looked at -to first and stopped
        // on this (exit 1); only a change in between.
        Err(error) => return Err(format!("can't read -to {}: {error}", to.display())),
    }
    let parent = parent_of(to);
    if !parent.is_dir() {
        return Err(format!(
            "-to {}: its directory {} doesn't exist",
            to.display(),
            parent.display()
        ));
    }
    Ok(())
}

/// Whether `path` is the file this process is running from.
#[cfg(not(unix))]
fn is_own_image(path: &Path) -> bool {
    std::env::current_exe().is_ok_and(|exe| same_file::is_same_file(exe, path).unwrap_or(false))
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// The directory that holds `dir`, whose sync makes `dir`'s entry durable:
/// none for a root, `.` for a relative `dir` with no directory part.
fn holder_of(dir: &Path) -> Option<&Path> {
    match dir.parent() {
        None => None,
        Some(parent) if parent.as_os_str().is_empty() => Some(Path::new(".")),
        Some(parent) => Some(parent),
    }
}

fn sync_holder_of(files: &mut dyn Files, dir: &Path) -> io::Result<()> {
    match holder_of(dir) {
        Some(holder) => files.sync_dir(holder),
        None => Ok(()),
    }
}

/// Makes `dir` and any parents it lacks, outermost first, private to the user
/// on Unix, and syncs the parent of each directory it makes, so a crash can't
/// lose the directory, and the receipt in it, after the install says done.
///
/// A run that finds directories already there may be the retry of one that
/// made them and failed to sync their parents, so their entries are not known
/// to be durable until this run syncs them. Whether `dir` is inside `home`
/// is decided on resolved paths (symlinks and `..` followed), not on the
/// text: a symlinked home is still home, and `$HOME/../shared` is not. When
/// the resolved `dir` is a real descendant of the resolved `home` (the default
/// layout, `~/.local/share/open-ferry`), every run syncs the parent of `dir`
/// and of each resolved ancestor of it below `home`, so the last one synced is
/// `home` itself, however many of them this run made. Otherwise (an
/// `XDG_DATA_HOME` elsewhere, a home that can't be resolved), there is no
/// known place to stop, and the rule is: the directories this run makes, and
/// `dir`'s own parent.
fn create_private_dir(dir: &Path, home: Option<&Path>, files: &mut dyn Files) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut current = Some(dir);
    while let Some(path) = current.filter(|path| !path.as_os_str().is_empty() && !path.exists()) {
        missing.push(path);
        current = path.parent();
    }
    // The directories this run made, resolved.
    let mut made: Vec<PathBuf> = Vec::new();
    for path in missing.into_iter().rev() {
        match files.create_dir(path) {
            Ok(()) => {}
            // Made meanwhile by someone else: its entry still needs the sync.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(error) => return Err(error),
        }
        let resolved = resolve_existing(files, path)?;
        sync_holder_of(files, &resolved)?;
        made.push(resolved);
    }
    // The directories whose entries this run has still to sync: `dir`, and
    // inside home each ancestor below it. A `dir` that exists and won't
    // resolve is an error, not a reason to sync through its lexical parent;
    // only a home that won't resolve selects the outside-home rule.
    let resolved_dir = resolve_existing(files, dir)?;
    let resolved_home = home.and_then(|home| files.resolve(home).ok());
    let mut unsynced = vec![resolved_dir.as_path()];
    if let Some(home) = &resolved_home
        && resolved_dir.starts_with(home)
        && resolved_dir != *home
    {
        unsynced.extend(
            resolved_dir
                .ancestors()
                .skip(1)
                .take_while(|ancestor| *ancestor != home),
        );
    }
    for path in unsynced {
        if !made.iter().any(|made| made == path) {
            sync_holder_of(files, path)?;
        }
    }
    Ok(())
}

/// `path`, which exists, resolved; an error that names it when it can't be.
fn resolve_existing(files: &mut dyn Files, path: &Path) -> io::Result<PathBuf> {
    files.resolve(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "can't resolve {} to sync its parent: {error}. The binary and the receipt were not changed",
                path.display()
            ),
        )
    })
}

/// `path` with the `\\?\` prefix Windows' canonical paths carry removed, when
/// it is valid Unicode (a path that is not keeps the prefix, which Windows
/// accepts); never a lossy conversion. Unchanged off Windows.
fn unverbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(text) = path.to_str() {
        return PathBuf::from(strip_verbatim(text));
    }
    path
}

/// `to` as the receipt names it: its directory resolved, and its own name.
fn canonical_dest(to: &Path) -> io::Result<PathBuf> {
    let name = to
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "it names no file"))?;
    let parent = unverbatim(fs::canonicalize(parent_of(to))?);
    Ok(parent.join(name))
}

/// `path` without the `\\?\` prefix Windows' canonical paths carry, which a
/// receipt would hold against the path the scripts wrote. Only Windows
/// has the prefix; the tests check it everywhere.
#[cfg(any(windows, test))]
fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        _ => path.to_owned(),
    }
}

// --- The replacement and its rollback ---------------------------------------

/// Puts `options.from` at `dest`, keeping rollback material until `receipt`
/// is published; restores the old binary if it can't be.
fn replace(
    options: &Options,
    dest: &Path,
    receipt: &Receipt,
    env: &mut Env<'_>,
    say: &mut Say<'_>,
) -> Exit {
    let from = options.from.as_path();
    // Only "not found" means a fresh install. Any other error (a permission
    // problem, a parent that isn't a directory) could hide a binary that is
    // there, and treating it as fresh would replace it with no rollback.
    let existed = match env.files.exists(dest) {
        Ok(existed) => existed,
        Err(error) => {
            say.warn(&format!(
                "can't tell whether {} exists: {error}. Nothing was changed.",
                dest.display()
            ));
            return Exit::Failed;
        }
    };
    let old = existed.then(|| old_path(dest, &env.unique));

    // The new binary's contents are on disk before it is placed.
    if let Err(error) = env.files.sync_file(from) {
        say.warn(&format!(
            "can't flush {} to disk: {error}. Nothing was changed.",
            from.display()
        ));
        return Exit::Failed;
    }

    // Rollback material, then the placement. Every move is durable before
    // the next step, and the receipt comes after the last of them.
    if let Some(old) = &old {
        if let Err(message) = make_material(old, dest, env) {
            say.warn(&format!(
                "can't keep {} to restore if the install fails: {message}. Nothing was changed.",
                dest.display()
            ));
            return Exit::Failed;
        }
        // Windows moved it aside; Unix linked it.
        if env.windows
            && let Err(failure) = move_durably(env, dest, old)
        {
            return match failure {
                MoveFailure::NotMoved(error) => {
                    say.warn(&format!(
                        "can't move {} aside: {error}. Nothing was changed.",
                        dest.display()
                    ));
                    Exit::Failed
                }
                MoveFailure::NotDurable(error) => {
                    say.warn(&format!(
                        "moved {} aside, but the disk didn't confirm it: {error}",
                        dest.display()
                    ));
                    undo_placement(Some(old), from, dest, env, say)
                }
            };
        }
    }
    // The bin directory's own entry (it may be new) is synced too, before the
    // receipt. Only one level up: if several ancestors are new, the ones above
    // the parent are not covered here. A failure is a placement failure.
    // Skipped when that directory is a root, which has no parent to sync.
    let placed = move_durably(env, from, dest).and_then(|()| {
        match holder_of(parent_of(dest)) {
            Some(holder) => env.files.sync_dir(holder),
            None => Ok(()),
        }
        .map_err(MoveFailure::NotDurable)
    });
    if let Err(failure) = placed {
        return match failure {
            MoveFailure::NotMoved(error) => {
                say.warn(&format!(
                    "can't move {} to {}: {error}",
                    from.display(),
                    dest.display()
                ));
                undo_placement(old.as_deref(), from, dest, env, say)
            }
            MoveFailure::NotDurable(error) => {
                say.warn(&format!(
                    "moved {} to {}, but the disk didn't confirm it: {error}",
                    from.display(),
                    dest.display()
                ));
                roll_back(old.as_deref(), from, dest, env, say)
            }
        };
    }

    let receipt_path = env.data.receipt_file();
    match (env.publish)(receipt, &receipt_path) {
        Ok(()) => {}
        Err(WriteError::PublishedNotDurable(error)) => {
            // The receipt and the binary agree: the install stands.
            say.warn(&format!(
                "the install receipt {} was written, but the disk didn't confirm it ({error}). \
                 A crash before the disk flushes may lose the receipt; if open-ferry then \
                 doesn't update itself, run the installer again.",
                receipt_path.display()
            ));
        }
        Err(WriteError::NotPublished(error)) => {
            say.warn(&format!(
                "can't write the install receipt {}: {error}",
                receipt_path.display()
            ));
            return roll_back(old.as_deref(), from, dest, env, say);
        }
    }

    // Committed: this install's own rollback material goes, and then the
    // material earlier installs left, which until now might have been the
    // only copy of the original.
    let mut still_running = finish(old.as_deref(), env, say);
    if existed {
        still_running |= sweep_old(dest, old.as_deref(), env);
    }
    if still_running {
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        say.say(&format!(
            "An older {name} is still running: restart it to run this one."
        ));
    }
    Exit::Installed
}

/// The rollback material's name: `dest`, `.old-` and a suffix.
fn old_path(dest: &Path, unique: &str) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".old-{unique}"));
    dest.with_file_name(name)
}

/// Makes the Unix rollback link (a copy when linking fails). On Windows
/// there is nothing to make here: the old file is moved aside after this.
/// A failure removes any partial file, and says if that fails too.
fn make_material(old: &Path, dest: &Path, env: &mut Env<'_>) -> Result<(), String> {
    if env.windows {
        return Ok(());
    }
    // A link shares the old file's data; a copy's data is synced as well. The
    // directory entry is synced either way.
    let made = match env.files.hard_link(dest, old) {
        Ok(()) => Ok(()),
        Err(_) => env
            .files
            .copy(dest, old)
            .and_then(|()| env.files.sync_file(old)),
    }
    .and_then(|()| env.files.sync_dir(parent_of(old)));
    let Err(error) = made else {
        return Ok(());
    };
    let mut message = error.to_string();
    match env.files.remove_file(old) {
        Ok(()) => {}
        Err(removal) if removal.kind() == io::ErrorKind::NotFound => {}
        Err(removal) => message.push_str(&format!(
            "; a partial copy, {}, is left and can be deleted ({removal})",
            old.display()
        )),
    }
    Err(message)
}

/// Undoes a failed placement of `from` at `dest`. Unix: `dest` is still the
/// old file, and only the link is removed. Windows: the old file was moved
/// aside, and is moved back.
fn undo_placement(
    old: Option<&Path>,
    from: &Path,
    dest: &Path,
    env: &mut Env<'_>,
    say: &mut Say<'_>,
) -> Exit {
    let Some(old) = old else {
        say.warn("Nothing was changed.");
        return Exit::Failed;
    };
    if env.windows {
        if let Err(failure) = move_durably(env, old, dest) {
            let moved = matches!(failure, MoveFailure::NotDurable(_));
            let error = failure.into_error();
            let (files, fix) = if moved {
                // Back at `dest`; whether it stays is unconfirmed.
                (
                    [
                        (dest, "the open-ferry that was installed"),
                        (old, "nothing"),
                        (from, "the new open-ferry"),
                    ],
                    format!(
                        "The old one is back at {}, but the disk didn't confirm it: check that \
                         it is still there after the next restart.",
                        dest.display()
                    ),
                )
            } else {
                (
                    [
                        (dest, "nothing"),
                        (old, "the open-ferry that was installed"),
                        (from, "the new open-ferry"),
                    ],
                    format!(
                        "To restore the old one, move {} to {}.",
                        old.display(),
                        dest.display()
                    ),
                )
            };
            say.warn(&stuck(
                &format!(
                    "moving {} back to {} failed: {error}",
                    old.display(),
                    dest.display()
                ),
                &files,
                &fix,
            ));
            return Exit::PartlyChanged;
        }
    } else if let Err(error) = env.files.remove_file(old) {
        say.warn(&format!(
            "can't remove {}: {error}. It can be deleted.",
            old.display()
        ));
    }
    say.warn("Nothing was changed.");
    Exit::Failed
}

/// Undoes a placed binary after the receipt was not published, so that the
/// binary and the receipt are as they were.
fn roll_back(
    old: Option<&Path>,
    from: &Path,
    dest: &Path,
    env: &mut Env<'_>,
    say: &mut Say<'_>,
) -> Exit {
    // Windows can rename a running image but not delete it: the new binary,
    // which runs, goes back to `from`.
    let to_from = env.windows || old.is_none();
    if to_from && let Err(failure) = move_durably(env, dest, from) {
        // A rename that failed left the new binary at `dest`; one that
        // moved but wasn't confirmed durable put it at `from`.
        let moved = matches!(failure, MoveFailure::NotDurable(_));
        let error = failure.into_error();
        let mut files = if moved {
            vec![(dest, "nothing"), (from, "the new open-ferry")]
        } else {
            vec![(dest, "the new open-ferry"), (from, "nothing")]
        };
        if let Some(old) = old {
            files.push((old, "the open-ferry that was installed"));
        }
        let fix = match (old, moved) {
            // `dest` is where the new one was; it still is.
            (Some(old), false) => format!(
                "To restore the old one, move {} to {} (replacing the new one).",
                old.display(),
                dest.display()
            ),
            // The new one went back to `from`; `dest` is empty.
            (Some(old), true) => format!(
                "To restore the old one, move {} to {}.",
                old.display(),
                dest.display()
            ),
            (None, false) => format!("To undo the install, delete {}.", dest.display()),
            (None, true) => format!(
                "The new one is back at {}, but the disk didn't confirm it: after the next \
                 restart, if {} is there again, delete it.",
                from.display(),
                dest.display()
            ),
        };
        say.warn(&stuck(
            &format!(
                "moving {} back to {} failed: {error}",
                dest.display(),
                from.display()
            ),
            &files,
            &fix,
        ));
        return Exit::PartlyChanged;
    }
    if let Some(old) = old
        && let Err(failure) = move_durably(env, old, dest)
    {
        let moved = matches!(failure, MoveFailure::NotDurable(_));
        let error = failure.into_error();
        let files = if moved {
            // The old binary is back at `dest`; whether it stays is unconfirmed.
            if to_from {
                [
                    (dest, "the open-ferry that was installed"),
                    (old, "nothing"),
                    (from, "the new open-ferry"),
                ]
            } else {
                [
                    (dest, "the open-ferry that was installed"),
                    (old, "nothing"),
                    (from, "nothing"),
                ]
            }
        } else if to_from {
            // Windows: the new binary went back to `from`.
            [
                (dest, "nothing"),
                (old, "the open-ferry that was installed"),
                (from, "the new open-ferry"),
            ]
        } else {
            [
                (dest, "the new open-ferry"),
                (old, "the open-ferry that was installed"),
                (from, "nothing"),
            ]
        };
        say.warn(&stuck(
            &format!(
                "moving {} back to {} failed: {error}",
                old.display(),
                dest.display()
            ),
            &files,
            &if moved {
                format!(
                    "The old one is back at {}, but the disk didn't confirm it: check that it \
                     is still there after the next restart.",
                    dest.display()
                )
            } else {
                format!(
                    "To restore the old one, move {} to {}.",
                    old.display(),
                    dest.display()
                )
            },
        ));
        return Exit::PartlyChanged;
    }
    say.warn("The old open-ferry and its receipt are as they were. Nothing was installed.");
    Exit::Failed
}

/// The message for a failed restore: what failed, what each file holds now,
/// and what to do.
fn stuck(what: &str, files: &[(&Path, &str)], fix: &str) -> String {
    let mut message = format!("the install failed, and then the restore did too: {what}.\n");
    for (path, holds) in files {
        message.push_str(&format!("  {} now holds {holds}.\n", path.display()));
    }
    message.push_str(fix);
    message
}

/// Removes the rollback material once the receipt is published. On Windows
/// a running image can't be deleted: whether that is why it stayed, for a
/// later install to remove.
fn finish(old: Option<&Path>, env: &mut Env<'_>, say: &mut Say<'_>) -> bool {
    let Some(old) = old else { return false };
    match env.files.remove_file(old) {
        Ok(()) => false,
        Err(_) if env.windows => true,
        Err(error) => {
            say.warn(&format!("can't remove {}: {error}", old.display()));
            false
        }
    }
}

/// How a durable move failed.
enum MoveFailure {
    /// The rename failed: nothing moved.
    NotMoved(io::Error),
    /// It moved, but the sync of the directory did not succeed: the move
    /// may not survive a crash.
    NotDurable(io::Error),
}

impl MoveFailure {
    fn into_error(self) -> io::Error {
        match self {
            Self::NotMoved(error) => error,
            Self::NotDurable(error) => {
                io::Error::other(format!("it moved, but the disk didn't confirm it: {error}"))
            }
        }
    }
}

/// Moves `from` onto `to`, and makes the move durable: written through on
/// Windows, the directories synced on Unix.
fn move_durably(env: &mut Env<'_>, from: &Path, to: &Path) -> Result<(), MoveFailure> {
    env.files.rename(from, to).map_err(MoveFailure::NotMoved)?;
    let (from_dir, to_dir) = (parent_of(from), parent_of(to));
    env.files
        .sync_dir(to_dir)
        .map_err(MoveFailure::NotDurable)?;
    if from_dir != to_dir {
        env.files
            .sync_dir(from_dir)
            .map_err(MoveFailure::NotDurable)?;
    }
    Ok(())
}

/// Removes the rollback material that earlier installs left, now their
/// processes may have stopped, and this install is committed. Not `own`, which
/// [`finish`] dealt with. Best effort, and only names this module made. On
/// Windows, whether one is still in use.
fn sweep_old(dest: &Path, own: Option<&Path>, env: &mut Env<'_>) -> bool {
    let Some(name) = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return false;
    };
    let prefix = format!("{name}.old-");
    let Ok(entries) = fs::read_dir(parent_of(dest)) else {
        return false;
    };
    let mut running = false;
    for entry in entries.filter_map(Result::ok) {
        let entry_name = entry.file_name().to_string_lossy().into_owned();
        let Some(suffix) = entry_name.strip_prefix(&prefix) else {
            continue;
        };
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            continue;
        }
        if own.is_some_and(|own| own.file_name() == Some(entry.file_name().as_os_str())) {
            continue;
        }
        if env.files.remove_file(&entry.path()).is_err() && env.windows {
            running = true;
        }
    }
    running
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod scope_tests {
    use super::*;
    use crate::os_service::Platform;
    use crate::scope::{Probe, Scope};
    use crate::update::scope_probe::{Fake, Node};

    const DEST: &str = "C:/Program Files/open-ferry/open-ferry.exe";

    #[test]
    fn the_scope_flag_is_optional_and_does_not_consume_a_value() {
        let args = |extra: &[&str]| {
            ["-from", "staged", "-to", DEST, "-installer", "install.ps1"]
                .into_iter()
                .chain(extra.iter().copied())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert!(!parse_with_scope(args(&[])).unwrap().1);
        for flag in ["-system", "--system"] {
            let (options, system) = parse_with_scope(args(&[flag, "-wait", "0"])).unwrap();
            assert!(system);
            assert_eq!(options.wait, Duration::ZERO);
            assert_eq!(options.to, Path::new(DEST));
        }
        assert!(parse_with_scope(args(&["---system"])).is_err());
    }

    #[test]
    fn the_destination_selects_machine_scope_and_replacement_requires_privilege() {
        let mut probe = Fake::new(Platform::Windows, false);
        let machine = scope::machine_root(&probe).unwrap().unwrap();
        probe.file(DEST).with_receipt(&machine, DEST);
        let refusal = install_home(&mut probe, "C:/secure/staged.exe", DEST, false)
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("terminal opened as administrator"));
        probe.privileged = Ok(true);
        let home = install_home(&mut probe, "C:/secure/staged.exe", DEST, false).unwrap();
        assert_eq!(home.scope, Scope::Machine);
        assert_eq!(home.kind, ServiceKind::System);
        assert_eq!(home.data.root(), Path::new(&machine));
        assert!(probe.created.is_empty());
    }

    #[test]
    fn account_installs_keep_unprivileged_behavior_but_privileged_records_need_trust() {
        for platform in [Platform::Windows, Platform::Linux, Platform::MacOs] {
            let mut probe = Fake::new(platform, false);
            let account = probe.account_dir().unwrap();
            probe.dir(&account).untrust(&account);
            assert_eq!(
                install_home(&mut probe, "C:/secure/staged.exe", DEST, false)
                    .unwrap()
                    .scope,
                Scope::Account
            );
            probe.privileged = Ok(true);
            let refusal = install_home(&mut probe, "C:/secure/staged.exe", DEST, false)
                .unwrap_err()
                .to_string();
            assert!(
                refusal.contains(match platform {
                    Platform::Windows => "not opened as administrator",
                    Platform::Linux => "without sudo",
                    Platform::MacOs => "macOS",
                }),
                "{refusal}"
            );
        }
    }

    #[test]
    fn system_install_needs_privilege_and_macos_refuses_before_provisioning() {
        for platform in [Platform::Windows, Platform::Linux, Platform::MacOs] {
            let mut probe = Fake::new(platform, false);
            assert!(install_home(&mut probe, "C:/secure/staged.exe", DEST, true).is_err());
            assert!(probe.created.is_empty());
            if platform == Platform::MacOs {
                probe.privileged = Ok(true);
                assert!(
                    install_home(&mut probe, "C:/secure/staged.exe", DEST, true)
                        .unwrap_err()
                        .to_string()
                        .contains("macOS")
                );
                assert!(probe.created.is_empty());
            }
        }
    }

    #[test]
    fn system_install_checks_an_existing_destination_or_the_new_destination_directory() {
        for exists in [false, true] {
            let mut probe = Fake::new(Platform::Windows, true);
            let parent = probe.platform.parent(DEST).unwrap();
            probe.dir(&parent);
            if exists {
                probe.file(DEST).untrust(DEST);
            } else {
                probe.untrust(&parent);
            }
            let refusal = install_home(&mut probe, "C:/secure/staged.exe", DEST, true)
                .unwrap_err()
                .to_string();
            assert!(refusal.contains("untrusted"), "{refusal}");
            assert!(probe.created.is_empty(), "trust before provisioning");
        }
    }

    #[test]
    fn system_install_checks_the_source_file_and_directory_before_provisioning() {
        for path in ["C:/secure/staged.exe", "C:/secure"] {
            let mut probe = Fake::new(Platform::Windows, true);
            probe.file(DEST).untrust(path);
            let refusal = install_home(&mut probe, "C:/secure/staged.exe", DEST, true)
                .unwrap_err()
                .to_string();
            assert!(refusal.contains("-from C:/secure/staged.exe"), "{refusal}");
            assert!(
                refusal.contains("writable only by root or administrators"),
                "{refusal}"
            );
            assert!(probe.created.is_empty());
        }
    }

    #[test]
    fn root_may_install_to_its_own_trusted_account_without_system() {
        let mut probe = Fake::new(Platform::Linux, true);
        let account = probe.account_dir().unwrap();
        probe.dir(&account);
        let home = install_home(
            &mut probe,
            "/root/staged",
            "/root/.local/bin/open-ferry",
            false,
        )
        .unwrap();
        assert_eq!(home.scope, Scope::Account);
        assert_eq!(home.data.root(), Path::new(&account));
        assert!(probe.created.is_empty());
    }

    #[test]
    fn system_provisioning_refusal_is_passed_through_and_success_uses_machine_receipts() {
        let mut probe = Fake::new(Platform::Windows, true);
        probe.file(DEST);
        probe.create = Some(io::ErrorKind::PermissionDenied);
        let refusal = install_home(&mut probe, "C:/secure/staged.exe", DEST, true).unwrap_err();
        let expected = scope::provision(&mut probe).unwrap_err();
        assert_eq!(refusal, expected);
        probe.create = None;
        let home = install_home(&mut probe, "C:/secure/staged.exe", DEST, true).unwrap();
        let root = scope::machine_root(&probe).unwrap().unwrap();
        assert_eq!(home.scope, Scope::Machine);
        assert_eq!(home.kind, ServiceKind::System);
        assert_eq!(
            home.data.receipt_file(),
            Path::new(&root).join(open_ferry_update::data_dir::RECEIPT_FILE)
        );
        assert_eq!(probe.created, [root]);
        let message = supervised_message(&Running::default(), home.kind);
        assert!(message.contains("open-ferry -supervise -system"));
    }

    #[test]
    fn an_unreadable_machine_sets_the_hint_for_install_self() {
        let mut probe = Fake::new(Platform::Windows, false);
        let machine = scope::machine_root(&probe).unwrap().unwrap();
        probe.node(&machine, Node::Hidden);
        let home = install_home(&mut probe, "C:/secure/staged.exe", DEST, false).unwrap();
        assert!(home.machine_hint.unwrap().contains("whole machine"));
    }

    #[cfg(windows)]
    #[test]
    fn system_install_writes_the_receipt_in_the_fake_machine_directory() {
        let scratch = std::env::var_os("OFP_TEST_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
            });
        fs::create_dir_all(&scratch).unwrap();
        let scratch = unverbatim(fs::canonicalize(scratch).unwrap());
        let dir = tempfile::tempdir_in(scratch).unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let from = bin.join("staged.exe");
        let to = bin.join("open-ferry.exe");
        fs::write(&from, b"new").unwrap();
        let options = Options {
            from: from.clone(),
            to: to.clone(),
            installer: "install.ps1".to_owned(),
            wait: Duration::ZERO,
        };
        let mut probe = Fake::new(Platform::Windows, true);
        probe.program_data = Ok(dir.path().join("ProgramData").to_str().unwrap().to_owned());
        probe.account = Ok(dir.path().join("account").to_str().unwrap().to_owned());
        probe.dir(bin.to_str().unwrap());
        let home = install_home(
            &mut probe,
            from.to_str().unwrap(),
            to.to_str().unwrap(),
            true,
        )
        .unwrap();
        assert_eq!(home.kind, ServiceKind::System);
        assert!(home.data.root().starts_with(dir.path()));
        fs::create_dir_all(home.data.root()).unwrap();
        let mut detect = |_: &Path, installed: &Path, _: Instant| {
            Detection::Clear(
                open_ferry_update::install_lock::InstallLock::try_acquire(installed).unwrap(),
            )
        };
        let mut files = RealFiles;
        let mut clock = RealClock;
        let mut publish = |receipt: &Receipt, path: &Path| receipt.write_atomic(path);
        let mut env = Env {
            current_exe: from,
            data: home.data.clone(),
            windows: true,
            home: None,
            unique: unique_suffix(),
            detect: &mut detect,
            files: &mut files,
            clock: &mut clock,
            publish: &mut publish,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut say = Say::new(&mut out, &mut err);
        say.kind = home.kind;
        assert_eq!(
            install(&options, &mut env, &mut say),
            Exit::Installed,
            "{}",
            String::from_utf8_lossy(&err)
        );
        let receipt = Receipt::load(&home.data.receipt_file()).unwrap().unwrap();
        assert_eq!(Path::new(&receipt.binary), canonical_dest(&to).unwrap());
        assert_eq!(fs::read(&to).unwrap(), b"new");
        assert!(
            !Path::new(&probe.account_dir().unwrap())
                .join(open_ferry_update::data_dir::RECEIPT_FILE)
                .exists()
        );
    }
}
