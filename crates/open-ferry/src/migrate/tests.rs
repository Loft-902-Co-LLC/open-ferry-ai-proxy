use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::net::IpAddr;
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use serde_json::Value;

use super::discover::file_name;
use super::machine::{Answer, Entry, EntryKind, Held, Launch, Machine, Parent, Proc};
use super::switch::backup_dir;
use super::*;
use crate::check::{Finding, Level};
use crate::os_service::{Cmd, Context, Output, Owner, Platform, System};

/// What the fake's binaries hold: open-ferry's, and CLIProxyAPI's.
const OPEN_FERRY: &[u8] = b"open-ferry binary";
const CLIPROXYAPI: &[u8] = b"CLIProxyAPI binary";

/// The fake's clock: every switch is made at this time.
const NOW: &str = "2026-10-08T12:00:00Z";
const STAMP: &str = "20261008T120000Z";

/// What a command, or the end of a process, does to the fake's processes.
#[derive(Clone)]
enum Effect {
    /// The processes running this binary end.
    Exit(String),
    /// This binary starts, unless a process runs it already.
    Launch(String),
    /// Something else answers as open-ferry on the port, from this process.
    Serve(u32),
    /// The process with this ID is gone, and another has its ID: its start
    /// time is not the one it had.
    Reuse(u32),
    /// A symbolic link is made at this path, to this target.
    Link(String, String),
    /// A file with this content is put at this path (a file that another
    /// process makes while a step runs).
    Place(String, Vec<u8>),
    /// This process ends, and no other.
    End(u32),
    /// A process starts.
    Spawn(Box<Proc>),
}

/// A machine in memory: files, processes, a service manager that answers
/// as told, the proxy's port and a person at the terminal.
struct Fake {
    platform: Platform,
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    /// Paths that can't be read.
    unreadable: BTreeSet<String>,
    /// The directories made private.
    private: BTreeSet<String>,
    /// What commands print, by their command line or its start.
    answers: BTreeMap<String, Output>,
    procs: BTreeMap<u32, Proc>,
    /// What answers on the port: the first holds it.
    serving: Vec<(u32, Answer)>,
    /// What a command, or `stop <pid>`, does.
    effects: BTreeMap<String, Vec<Effect>>,
    /// Processes that can't be stopped.
    unstoppable: BTreeSet<u32>,
    /// Symbolic links: the link's path, and where it points.
    links: BTreeMap<String, String>,
    /// Steps that fail, as the events name them (`rename a -> b`,
    /// `copy a -> b`, `remove a`, `symlink a -> b`).
    failing: BTreeSet<String>,
    /// The binary the install receipt names.
    installed: Option<String>,
    /// Whether open-ferry starts but never answers.
    broken: bool,
    next_pid: u32,
    events: Vec<String>,
    findings: Vec<Finding>,
    /// What `check` was asked of.
    checked: Vec<String>,
    terminal: bool,
    replies: VecDeque<bool>,
    questions: Vec<String>,
    launches: Vec<Launch>,
    /// The first word of each started process's command line.
    argv0s: BTreeMap<u32, String>,
    /// What `launchctl print` said of the jobs `bootout` unloaded.
    unloaded: BTreeMap<String, Output>,
    /// Lock files another run holds.
    held: BTreeSet<String>,
    /// Paths whose real path can't be found.
    no_real: BTreeSet<String>,
    /// Commands that succeed and change nothing in a manager's answers: a
    /// service that stays as it is when asked to stop.
    stuck: BTreeSet<String>,
    /// open-ferry's own binary.
    own: String,
}

impl Fake {
    /// A machine running `context`'s open-ferry, whose service managers
    /// know nothing of CLIProxyAPI.
    fn new(context: &Context) -> Fake {
        let mut fake = Fake {
            platform: context.platform,
            files: BTreeMap::new(),
            dirs: BTreeSet::new(),
            unreadable: BTreeSet::new(),
            private: BTreeSet::new(),
            answers: BTreeMap::new(),
            procs: BTreeMap::new(),
            serving: Vec::new(),
            effects: BTreeMap::new(),
            unstoppable: BTreeSet::new(),
            links: BTreeMap::new(),
            failing: BTreeSet::new(),
            installed: None,
            broken: false,
            next_pid: 9001,
            events: Vec::new(),
            findings: Vec::new(),
            checked: Vec::new(),
            terminal: false,
            replies: VecDeque::new(),
            questions: Vec::new(),
            launches: Vec::new(),
            argv0s: BTreeMap::new(),
            unloaded: BTreeMap::new(),
            held: BTreeSet::new(),
            no_real: BTreeSet::new(),
            stuck: BTreeSet::new(),
            own: context.exe.clone(),
        };
        fake.file(&context.exe, OPEN_FERRY);
        fake.dir(&context.cwd);
        match context.platform {
            Platform::Linux => {
                fake.answer("id -u", 0, "1000\n");
                fake.answer("systemctl --user show", 0, "LoadState=not-found\n");
                fake.answer("systemctl show", 0, "LoadState=not-found\n");
            }
            Platform::MacOs => {
                fake.answer("id -u", 0, "501\n");
                fake.fail("launchctl print", 113, "Could not find service");
            }
            Platform::Windows => {
                if let Some(temp) = context.var("TEMP") {
                    fake.dir(temp);
                }
                fake.fail(
                    "schtasks.exe /query /tn open-ferry",
                    1,
                    "ERROR: The system cannot find the file specified.",
                );
                fake.fail(
                    "sc.exe query open-ferry",
                    1060,
                    "The specified service does not exist as an installed service.",
                );
                fake.fail(
                    "sc.exe qc",
                    1060,
                    "The specified service does not exist as an installed service.",
                );
                fake.answer(
                    "whoami.exe /user /fo csv /nh",
                    0,
                    "\"pc\\me\",\"S-1-5-21-1-2-3-1001\"\r\n",
                );
                fake.answer("whoami.exe /groups /fo csv /nh", 0, USER_GROUPS);
                fake.answer("schtasks.exe /query /xml ONE", 0, "<Tasks></Tasks>\r\n");
            }
        }
        fake
    }

    fn file(&mut self, path: &str, data: impl AsRef<[u8]>) {
        if let Some(parent) = self.platform.parent(path) {
            self.dir(&parent);
        }
        self.files.insert(path.to_owned(), data.as_ref().to_vec());
    }

    /// Adds `path` and the directories above it.
    fn dir(&mut self, path: &str) {
        let mut path = path.to_owned();
        loop {
            self.dirs.insert(path.clone());
            match self.platform.parent(&path) {
                Some(parent) if parent != path => path = parent,
                _ => break,
            }
        }
    }

    fn answer(&mut self, cmd: &str, code: i32, stdout: &str) {
        self.answers.insert(
            cmd.to_owned(),
            Output {
                code: Some(code),
                stdout: stdout.to_owned(),
                stderr: String::new(),
            },
        );
    }

    fn fail(&mut self, cmd: &str, code: i32, stderr: &str) {
        self.answers.insert(
            cmd.to_owned(),
            Output {
                code: Some(code),
                stdout: String::new(),
                stderr: stderr.to_owned(),
            },
        );
    }

    fn on(&mut self, key: &str, effect: Effect) {
        self.effects.entry(key.to_owned()).or_default().push(effect);
    }

    /// A running CLIProxyAPI, which holds the port.
    fn process(&mut self, process: Proc) {
        self.serving.push((process.pid, Answer::CliProxyApi));
        self.procs.insert(process.pid, process);
    }

    fn data(&self, path: &str) -> &[u8] {
        self.files
            .get(&self.resolve(path))
            .map_or(&[], Vec::as_slice)
    }

    /// The answer for `line`: its own, or the longest that starts it.
    fn lookup(&self, line: &str) -> Option<Output> {
        if let Some(output) = self.answers.get(line) {
            return Some(output.clone());
        }
        self.answers
            .iter()
            .filter(|(key, _)| line.starts_with(&format!("{key} ")))
            .max_by_key(|(key, _)| key.len())
            .map(|(_, output)| output.clone())
    }

    /// What a command changes in the service managers' answers.
    fn changed(&mut self, line: &str) {
        self.manager_changed(line);
        if line.starts_with("schtasks.exe /create /tn open-ferry ") {
            self.answer("schtasks.exe /query /tn open-ferry", 0, "");
        }
        if line == "schtasks.exe /delete /tn open-ferry /f" {
            self.fail("schtasks.exe /query /tn open-ferry", 1, "ERROR");
        }
        if line.starts_with("sc.exe create open-ferry ") {
            self.answer("sc.exe query open-ferry", 0, "");
        }
        if line == "sc.exe delete open-ferry" {
            self.fail("sc.exe query open-ferry", 1060, "");
        }
        // A job `bootout` unloads is not loaded until `bootstrap` loads it.
        if let Some(job) = line.strip_prefix("launchctl bootout ") {
            let key = format!("launchctl print {job}");
            if let Some(output) = self.answers.remove(&key) {
                self.unloaded.insert(key.clone(), output);
                self.fail(&key, 113, "Could not find service");
            }
        }
        if let Some((domain, plist)) = line
            .strip_prefix("launchctl bootstrap ")
            .and_then(|rest| rest.split_once(' '))
        {
            let label = file_name(self.platform, plist);
            let label = label.strip_suffix(".plist").unwrap_or(&label);
            let key = format!("launchctl print {domain}/{label}");
            if let Some(output) = self.unloaded.remove(&key) {
                self.answers.insert(key, output);
            }
        }
    }

    /// What `stop` and `start` do to the answers about a service of
    /// CLIProxyAPI's (open-ferry's own are set by `create` and `delete`).
    fn manager_changed(&mut self, line: &str) {
        if self.stuck.contains(line) {
            return;
        }
        let words: Vec<&str> = line.split(' ').collect();
        let (action, unit, query) = match words.as_slice() {
            ["systemctl", "--user", action, unit] => {
                (*action, *unit, format!("systemctl --user show {unit}"))
            }
            ["systemctl", action, unit] => (*action, *unit, format!("systemctl show {unit}")),
            ["sc.exe", action, name] => (*action, *name, format!("sc.exe query {name}")),
            _ => return,
        };
        if unit == "open-ferry.service" || unit == "open-ferry" {
            return;
        }
        let up = match action {
            "start" => true,
            "stop" => false,
            _ => return,
        };
        if line.starts_with("sc.exe") {
            let state = if up { "4  RUNNING" } else { "1  STOPPED" };
            self.answer(
                &query,
                0,
                &format!("SERVICE_NAME: {unit}\r\n        STATE              : {state}\r\n"),
            );
        } else if let Some(output) = self.answers.get_mut(&query) {
            output.stdout = if up {
                output
                    .stdout
                    .replace("ActiveState=inactive", "ActiveState=active")
            } else {
                output
                    .stdout
                    .replace("ActiveState=active", "ActiveState=inactive")
            };
        }
    }

    fn fire(&mut self, key: &str) {
        let Some(effects) = self.effects.get(key).cloned() else {
            return;
        };
        for effect in effects {
            match effect {
                Effect::Exit(exe) => {
                    let pids: Vec<u32> = self
                        .procs
                        .values()
                        .filter(|process| process.exe.as_deref() == Some(exe.as_str()))
                        .map(|process| process.pid)
                        .collect();
                    for pid in pids {
                        self.end(pid);
                    }
                }
                Effect::Launch(exe) => {
                    if !self
                        .procs
                        .values()
                        .any(|process| process.exe.as_deref() == Some(exe.as_str()))
                    {
                        // open-ferry's server reads its arguments.
                        let args = if exe == self.own {
                            vec!["-config".to_owned(), "/fake/config.yaml".to_owned()]
                        } else {
                            Vec::new()
                        };
                        self.start(&exe, args, None, None).unwrap();
                    }
                }
                Effect::Serve(pid) => self.serving.insert(0, (pid, Answer::OpenFerry)),
                Effect::Link(path, target) => {
                    self.links.insert(path, target);
                }
                Effect::Place(path, data) => {
                    self.files.insert(path, data);
                }
                Effect::End(pid) => self.end(pid),
                Effect::Spawn(process) => {
                    self.procs.insert(process.pid, (*process).clone());
                }
                Effect::Reuse(pid) => {
                    if let Some(process) = self.procs.get_mut(&pid) {
                        process.started = Some(9_999_999);
                        process.name = "other".to_owned();
                        process.exe = Some("/usr/bin/other".to_owned());
                    }
                }
            }
        }
    }

    fn end(&mut self, pid: u32) {
        self.procs.remove(&pid);
        self.serving.retain(|(serving, _)| *serving != pid);
    }

    /// Starts the binary at `exe`: open-ferry's answers as open-ferry,
    /// unless it is broken, and any other as CLIProxyAPI.
    fn start(
        &mut self,
        exe: &str,
        args: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
    ) -> io::Result<u32> {
        // Like a kernel, it names the process after the path it was given,
        // and reports the file that path leads to as its binary.
        let real = self.resolve(exe);
        let data = self
            .files
            .get(&real)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        let pid = self.next_pid;
        self.next_pid += 1;
        self.argv0s.insert(pid, exe.to_owned());
        self.procs.insert(
            pid,
            Proc {
                pid,
                name: file_name(self.platform, exe),
                exe: Some(real),
                args,
                cwd,
                env,
                parent: None,
                started: Some(2000 + u64::from(pid)),
            },
        );
        if data == OPEN_FERRY {
            if !self.broken {
                self.serving.push((pid, Answer::OpenFerry));
            }
        } else {
            self.serving.push((pid, Answer::CliProxyApi));
        }
        Ok(pid)
    }

    /// Where `path` leads, through symbolic links.
    fn resolve(&self, path: &str) -> String {
        let mut path = path.to_owned();
        for _ in 0..8 {
            // A link at the path, or at a directory above it.
            let found = self.links.iter().find_map(|(link, target)| {
                if path == *link {
                    return Some(target.clone());
                }
                let rest = path.strip_prefix(link.as_str())?;
                let rest = rest.strip_prefix('/').or_else(|| rest.strip_prefix('\\'))?;
                Some(self.platform.join(target, rest))
            });
            match found {
                Some(next) => path = next,
                None => break,
            }
        }
        path
    }

    /// Fails the step when it was told to.
    fn step(&mut self, event: String) -> io::Result<()> {
        let failing = self.failing.contains(&event);
        self.events.push(event.clone());
        if failing {
            return Err(io::Error::other(format!("{event} failed")));
        }
        Ok(())
    }

    fn need_parent(&self, path: &str) -> io::Result<()> {
        match self.platform.parent(path) {
            Some(parent) if self.dirs.contains(&parent) => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path}'s directory doesn't exist"),
            )),
        }
    }
}

/// A record's temporary file named without what makes it unique, as the
/// events name it: `<file>.<pid>.<time>.<count>.tmp` is `<file>.tmp`.
fn plain(path: &str) -> String {
    let parts: Vec<&str> = path.rsplitn(5, '.').collect();
    let unique = parts.len() == 5
        && parts[0] == "tmp"
        && !parts[1].is_empty()
        && parts[1].bytes().all(|b| b.is_ascii_digit())
        && !parts[2].is_empty()
        && parts[2].bytes().all(|b| b.is_ascii_hexdigit())
        && !parts[3].is_empty()
        && parts[3].bytes().all(|b| b.is_ascii_digit());
    if unique {
        format!("{}.tmp", parts[4])
    } else {
        path.to_owned()
    }
}

const USER_GROUPS: &str =
    "\"Mandatory Label\\Medium Mandatory Level\",\"Label\",\"S-1-16-8192\",\"\"\r\n";
const ADMIN_GROUPS: &str =
    "\"Mandatory Label\\High Mandatory Level\",\"Label\",\"S-1-16-12288\",\"\"\r\n";

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "access denied")
}

fn not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "not found")
}

impl System for Fake {
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
        let line = cmd.to_string();
        self.events.push(format!("run {line}"));
        let output = self.lookup(&line).unwrap_or(Output {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        });
        if output.success() {
            self.changed(&line);
            self.fire(&line);
        }
        Ok(output)
    }

    fn show(&mut self, cmd: &Cmd) -> io::Result<Option<i32>> {
        let line = cmd.to_string();
        self.events.push(format!("show {line}"));
        let code = self.lookup(&line).map_or(Some(0), |output| output.code);
        if code == Some(0) {
            self.changed(&line);
            self.fire(&line);
        }
        Ok(code)
    }

    fn exists(&self, path: &str) -> bool {
        let path = self.resolve(path);
        self.files.contains_key(&path) || self.dirs.contains(&path)
    }

    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        if self.unreadable.contains(path) {
            return Err(denied());
        }
        self.files
            .get(&self.resolve(path))
            .cloned()
            .ok_or_else(not_found)
    }

    fn real_path(&self, path: &str) -> io::Result<String> {
        if self.no_real.contains(path) {
            return Err(not_found());
        }
        Ok(self.resolve(path))
    }

    fn owner(&self, path: &str) -> io::Result<Owner> {
        Ok(Owner {
            uid: 0,
            gid: 0,
            mode: 0o755,
            dir: !self.files.contains_key(path),
        })
    }

    fn create_dir_all(&mut self, path: &str) -> io::Result<()> {
        self.events.push(format!("mkdir {path}"));
        self.dir(path);
        Ok(())
    }

    fn write(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        self.events.push(format!("write {path}"));
        self.need_parent(path)?;
        self.files.insert(path.to_owned(), data.to_vec());
        Ok(())
    }

    fn remove(&mut self, path: &str) -> io::Result<()> {
        self.step(format!("remove {path}"))?;
        if self.links.remove(path).is_some() {
            return Ok(());
        }
        self.files.remove(path).ok_or_else(not_found)?;
        // Linux names what a process runs from a removed file so.
        for process in self.procs.values_mut() {
            if process.exe.as_deref() == Some(path) {
                process.exe = Some(format!("{path} (deleted)"));
            }
        }
        Ok(())
    }
}

impl Machine for Fake {
    fn processes(&mut self, names: &[&str]) -> io::Result<Vec<Proc>> {
        Ok(self
            .procs
            .values()
            .filter(|process| {
                names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&process.name))
            })
            .cloned()
            .collect())
    }

    fn running(&mut self, pid: u32) -> bool {
        self.procs.contains_key(&pid)
    }

    fn started(&mut self, pid: u32) -> Option<u64> {
        self.procs.get(&pid).and_then(|process| process.started)
    }

    fn stop(&mut self, pid: u32, name: &str, started: Option<u64>) -> io::Result<()> {
        self.events.push(format!("stop {pid}"));
        let Some(process) = self.procs.get(&pid) else {
            return Ok(());
        };
        if self.unstoppable.contains(&pid) {
            return Err(denied());
        }
        if started.is_some() && process.started != started {
            return Err(io::Error::other(format!(
                "process {pid} is another process now"
            )));
        }
        if !process.name.eq_ignore_ascii_case(name) {
            return Err(io::Error::other(format!("process {pid} isn't {name}")));
        }
        self.end(pid);
        self.fire(&format!("stop {pid}"));
        Ok(())
    }

    fn stop_exact(&mut self, pid: u32, started: u64) -> io::Result<()> {
        self.events.push(format!("stop {pid}"));
        let Some(process) = self.procs.get(&pid) else {
            return Ok(());
        };
        if self.unstoppable.contains(&pid) {
            return Err(denied());
        }
        if process.started != Some(started) {
            return Ok(());
        }
        self.end(pid);
        self.fire(&format!("stop {pid}"));
        Ok(())
    }

    fn lock(&mut self, path: &str) -> io::Result<Option<Held>> {
        if self.held.contains(path) {
            return Ok(None);
        }
        self.events.push(format!("lock {path}"));
        Ok(Some(Box::new(())))
    }

    fn spawn(&mut self, launch: &Launch) -> io::Result<u32> {
        self.events.push(format!("spawn {}", launch.exe));
        self.launches.push(launch.clone());
        self.start(
            &launch.exe,
            launch.args.clone(),
            Some(launch.cwd.clone()),
            launch.env.clone(),
        )
    }

    fn list_dir(&self, path: &str) -> io::Result<Vec<Entry>> {
        if self.unreadable.contains(path) {
            return Err(denied());
        }
        let resolved = self.resolve(path);
        let path = resolved.as_str();
        if !self.dirs.contains(path) {
            return Err(not_found());
        }
        let platform = self.platform;
        let mut entries = BTreeMap::new();
        for file in self.files.keys() {
            if platform.parent(file).as_deref() == Some(path) {
                entries.insert(file_name(platform, file), EntryKind::File);
            }
        }
        for dir in &self.dirs {
            if dir != path && platform.parent(dir).as_deref() == Some(path) {
                entries.insert(file_name(platform, dir), EntryKind::Dir);
            }
        }
        for link in self.links.keys() {
            if platform.parent(link).as_deref() == Some(path) {
                entries.insert(file_name(platform, link), EntryKind::Link);
            }
        }
        Ok(entries
            .into_iter()
            .map(|(name, kind)| Entry { name, kind })
            .collect())
    }

    fn copy(&mut self, from: &str, to: &str) -> io::Result<()> {
        let event = format!("copy {from} -> {to}");
        self.step(event.clone())?;
        let data = self.read(from)?;
        self.need_parent(to)?;
        // Like the real copy, it writes through a link at `to`.
        let to = self.resolve(to);
        self.files.insert(to, data);
        self.fire(&event);
        Ok(())
    }

    fn copy_new(&mut self, from: &str, to: &str) -> io::Result<()> {
        let event = format!("copy {from} -> {to}");
        self.events.push(event.clone());
        // Like the real copy, it opens `from` before it makes `to`.
        let data = self.read(from)?;
        if self.files.contains_key(to) || self.links.contains_key(to) || self.dirs.contains(to) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "it exists"));
        }
        self.need_parent(to)?;
        // A copy that fails part way removes the file it made.
        if self.failing.contains(&event) {
            return Err(io::Error::other(format!("{event} failed")));
        }
        self.files.insert(to.to_owned(), data);
        self.fire(&event);
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
        let event = format!("rename {} -> {}", plain(from), plain(to));
        self.step(event.clone())?;
        self.need_parent(to)?;
        if let Some(target) = self.links.remove(from) {
            self.links.insert(to.to_owned(), target);
            return Ok(());
        }
        let data = self.files.remove(from).ok_or_else(not_found)?;
        self.files.insert(to.to_owned(), data);
        // A process keeps running the file it was started from. Linux
        // follows the file to `to`; Windows goes on naming the old path.
        if self.platform != Platform::Windows {
            for process in self.procs.values_mut() {
                if process.exe.as_deref() == Some(from) {
                    process.exe = Some(to.to_owned());
                }
            }
        }
        self.fire(&event);
        Ok(())
    }

    fn rename_new(&mut self, from: &str, to: &str) -> io::Result<()> {
        // Nothing is replaced, a link or a directory included.
        if self.files.contains_key(to) || self.links.contains_key(to) || self.dirs.contains(to) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "it exists"));
        }
        self.rename(from, to)
    }

    fn link_target(&self, path: &str) -> Option<String> {
        self.links.get(path).cloned()
    }

    fn symlink(&mut self, target: &str, link: &str) -> io::Result<()> {
        self.step(format!("symlink {target} -> {link}"))?;
        self.need_parent(link)?;
        if self.links.contains_key(link) || self.exists(link) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "it exists"));
        }
        self.links.insert(link.to_owned(), target.to_owned());
        Ok(())
    }

    fn installed_binary(&mut self) -> Option<String> {
        self.installed.clone()
    }

    fn create_private_dir(&mut self, path: &str) -> io::Result<()> {
        self.events.push(format!("mkdir {path}"));
        if self.exists(path) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "it exists"));
        }
        self.need_parent(path)?;
        self.dirs.insert(path.to_owned());
        self.private.insert(path.to_owned());
        self.fire(&format!("mkdir {path}"));
        Ok(())
    }

    fn argv0(&mut self, pid: u32) -> Option<String> {
        self.procs.get(&pid)?;
        self.argv0s.get(&pid).cloned()
    }

    fn write_new(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        self.events.push(format!("write {}", plain(path)));
        self.need_parent(path)?;
        if self.files.contains_key(path) || self.links.contains_key(path) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "it exists"));
        }
        self.files.insert(path.to_owned(), data.to_vec());
        Ok(())
    }

    fn probe(&mut self, _ip: IpAddr, _port: u16, _tls: bool) -> Answer {
        self.serving.first().map_or_else(
            || Answer::Nothing("connection refused".to_owned()),
            |(_, answer)| answer.clone(),
        )
    }

    fn check(
        &mut self,
        config: &str,
        working_dir: Option<&str>,
        management_password: bool,
    ) -> Vec<Finding> {
        self.checked.push(format!(
            "{config} in {}{}",
            working_dir.unwrap_or("-"),
            if management_password {
                ", with MANAGEMENT_PASSWORD"
            } else {
                ""
            }
        ));
        self.findings.clone()
    }

    fn sleep(&mut self, _duration: Duration) {}

    fn now(&self) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
    }

    fn terminal(&self) -> bool {
        self.terminal
    }

    fn ask(&mut self, question: &str) -> bool {
        self.questions.push(question.to_owned());
        self.replies.pop_front().unwrap_or(false)
    }
}

// --- Contexts ---

fn context(
    platform: Platform,
    exe: &str,
    cwd: &str,
    env: &[(&str, &str)],
    installed: &str,
) -> Context {
    Context {
        platform,
        exe: exe.to_owned(),
        cwd: cwd.to_owned(),
        env: env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        installed_config: Ok(installed.to_owned()),
    }
}

fn linux() -> Context {
    context(
        Platform::Linux,
        "/home/me/.local/bin/open-ferry",
        "/home/me",
        &[("HOME", "/home/me")],
        "/home/me/.config/open-ferry/config.yaml",
    )
}

fn linux_root() -> Context {
    context(
        Platform::Linux,
        "/usr/local/bin/open-ferry",
        "/root",
        &[("HOME", "/root")],
        "/root/.config/open-ferry/config.yaml",
    )
}

fn macos() -> Context {
    context(
        Platform::MacOs,
        "/Users/me/.local/bin/open-ferry",
        "/Users/me",
        &[("HOME", "/Users/me")],
        "/Users/me/.config/open-ferry/config.yaml",
    )
}

fn macos_root() -> Context {
    context(
        Platform::MacOs,
        "/usr/local/bin/open-ferry",
        "/var/root",
        &[("HOME", "/var/root")],
        "/var/root/.config/open-ferry/config.yaml",
    )
}

fn windows_at(exe: &str) -> Context {
    context(
        Platform::Windows,
        exe,
        r"C:\Users\me",
        &[
            ("USERPROFILE", r"C:\Users\me"),
            ("TEMP", r"C:\Users\me\AppData\Local\Temp"),
            ("ProgramFiles", r"C:\Program Files"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ("SystemRoot", r"C:\Windows"),
        ],
        r"C:\Users\me\AppData\Roaming\open-ferry\config.yaml",
    )
}

fn windows() -> Context {
    windows_at(r"C:\Users\me\AppData\Local\Programs\open-ferry\open-ferry.exe")
}

fn windows_program_files() -> Context {
    windows_at(r"C:\Program Files\open-ferry\open-ferry.exe")
}

// --- Helpers ---

/// What `migrate` did: its exit code and what it printed.
struct Ran {
    code: u8,
    out: String,
    err: String,
}

impl Ran {
    fn all(&self) -> String {
        format!("{}\n--- stderr ---\n{}", self.out, self.err)
    }
}

/// Runs `migrate` with `args` on `fake`, and checks that no secret was
/// shown.
fn migrate(fake: &mut Fake, context: &Context, args: &[&str]) -> Ran {
    let request = parse(args.iter().map(|arg| (*arg).to_owned())).unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(fake, context, &request, &mut out, &mut err);
    let ran = Ran {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    };
    assert!(
        !ran.all().contains("s3cret"),
        "a secret was shown:\n{}",
        ran.all()
    );
    ran
}

fn has(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} isn't in:\n{text}");
}

fn lacks(text: &str, needle: &str) {
    assert!(!text.contains(needle), "{needle:?} is in:\n{text}");
}

/// Checks that `expected` happened in this order.
fn in_order(events: &[String], expected: &[&str]) {
    let mut from = 0;
    for item in expected {
        match events.iter().skip(from).position(|event| event == item) {
            Some(at) => from += at + 1,
            None => panic!("{item:?} didn't happen, or not in order:\n{events:#?}"),
        }
    }
}

fn happened(fake: &Fake, event: &str) -> bool {
    fake.events.iter().any(|known| known == event)
}

/// Checks that only reading commands were run, and nothing was written,
/// stopped or started.
fn read_only(fake: &Fake) {
    const READS: [&str; 12] = [
        "reg.exe query ",
        "id -u",
        "systemctl --user show ",
        "systemctl show ",
        "docker ps ",
        "launchctl print ",
        "plutil ",
        "tasklist.exe ",
        "sc.exe qc ",
        "sc.exe query ",
        "schtasks.exe /query ",
        "whoami.exe ",
    ];
    for event in &fake.events {
        let reads = event
            .strip_prefix("run ")
            .is_some_and(|line| READS.iter().any(|read| line.starts_with(read)));
        assert!(reads, "{event:?} changes something:\n{:#?}", fake.events);
    }
}

/// The switch's record, as saved.
fn saved(fake: &Fake, path: &str) -> Value {
    let text = String::from_utf8(fake.data(path).to_vec()).unwrap();
    assert!(
        !text.contains("s3cret"),
        "the record holds a secret:\n{text}"
    );
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}\n{text}"))
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// A config on port 8317 with `auth` as its auth directory.
fn config_text(auth: &str) -> String {
    format!("port: 8317\nauth-dir: '{auth}'\napi-keys:\n  - 's3cret-client-key'\n")
}

/// An auth directory with a Claude, a Codex and a Gemini credential, a
/// file with none, and a logs directory.
fn credentials(fake: &mut Fake, dir: &str) {
    let platform = fake.platform;
    fake.file(&platform.join(dir, "claude.json"), br#"{"type":"claude"}"#);
    fake.file(&platform.join(dir, "codex.json"), br#"{"type":"codex"}"#);
    fake.file(&platform.join(dir, "gemini.json"), br#"{"type":"gemini"}"#);
    fake.file(&platform.join(dir, "notes.json"), b"{}");
    let logs = platform.join(dir, "logs");
    fake.file(&platform.join(&logs, "main.log"), b"a log line\n");
}

/// A launchd plist.
fn plist(label: &str, args: &[&str], working_dir: Option<&str>, env: &[(&str, &str)]) -> String {
    let mut text = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>KeepAlive</key>\n\t<true/>\n",
    );
    text.push_str(&format!("\t<key>Label</key>\n\t<string>{label}</string>\n"));
    text.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for arg in args {
        text.push_str(&format!("\t\t<string>{arg}</string>\n"));
    }
    text.push_str("\t</array>\n");
    if let Some(dir) = working_dir {
        text.push_str(&format!(
            "\t<key>WorkingDirectory</key>\n\t<string>{dir}</string>\n"
        ));
    }
    if !env.is_empty() {
        text.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        for (name, value) in env {
            text.push_str(&format!(
                "\t\t<key>{name}</key>\n\t\t<string>{value}</string>\n"
            ));
        }
        text.push_str("\t</dict>\n");
    }
    text.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n</dict>\n</plist>\n");
    text
}

/// `schtasks /query /xml ONE`'s output for one task, after one that has
/// nothing to do with CLIProxyAPI.
fn tasks_xml(name: &str, enabled: bool, command: &str, arguments: &str, dir: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n<Tasks>\r\n  <!-- \\Microsoft\\Windows\\Defrag\\ScheduledDefrag -->\r\n  <Task version=\"1.6\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n    <Settings><Enabled>true</Enabled></Settings>\r\n    <Actions Context=\"LocalSystem\"><Exec><Command>%windir%\\system32\\defrag.exe</Command><Arguments>-c -h -o</Arguments></Exec></Actions>\r\n  </Task>\r\n  <!-- {name} -->\r\n  <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n    <RegistrationInfo><URI>{name}</URI></RegistrationInfo>\r\n    <Principals><Principal id=\"Author\"><UserId>S-1-5-21-1-2-3-1001</UserId><LogonType>InteractiveToken</LogonType></Principal></Principals>\r\n    <Settings><Enabled>{enabled}</Enabled></Settings>\r\n    <Actions Context=\"Author\">\r\n      <Exec>\r\n        <Command>{command}</Command>\r\n        <Arguments>{arguments}</Arguments>\r\n        <WorkingDirectory>{dir}</WorkingDirectory>\r\n      </Exec>\r\n    </Actions>\r\n  </Task>\r\n</Tasks>\r\n"
    )
}

// --- A systemd user service ---

const SYSTEMD_CPA: &str = "/home/me/cpa/cli-proxy-api";
const SYSTEMD_CONFIG: &str = "/home/me/cpa/config.yaml";
const LINUX_RECORD: &str = "/home/me/.config/open-ferry/migration.json";
const USER_UNIT: &str = "/home/me/.config/systemd/user/open-ferry.service";

fn systemd_backup() -> String {
    format!("/home/me/cpa/open-ferry-migrate-{STAMP}")
}

fn shown_unit(
    pid: u32,
    exec: &str,
    dir: &str,
    active: bool,
    environment: &str,
    user: &str,
) -> String {
    format!(
        "LoadState=loaded\nMainPID={pid}\nFragmentPath=/etc/systemd/system/cliproxyapi.service\nUnitFileState=enabled\nActiveState={}\nExecStart={{ path={} ; argv[]={exec} ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid={pid} ; code=(null) ; status=0/0 }}\nWorkingDirectory={dir}\nEnvironment={environment}\nEnvironmentFiles=\nUser={user}\n",
        if active { "active" } else { "inactive" },
        exec.split(' ').next().unwrap_or_default()
    )
}

/// CLIProxyAPI run by the user's systemd unit `cliproxyapi.service`, as
/// community installers set it up.
fn systemd_user() -> (Fake, Context) {
    let context = linux();
    let mut fake = Fake::new(&context);
    fake.file(SYSTEMD_CPA, CLIPROXYAPI);
    fake.file(SYSTEMD_CONFIG, config_text("~/.cli-proxy-api"));
    credentials(&mut fake, "/home/me/.cli-proxy-api");
    fake.process(Proc {
        pid: 4242,
        started: Some(1000),
        name: "cli-proxy-api".to_owned(),
        exe: Some(SYSTEMD_CPA.to_owned()),
        args: strings(&["-config", SYSTEMD_CONFIG]),
        cwd: Some("/home/me/cpa".to_owned()),
        env: Some(vars(&[
            ("HOME", "/home/me"),
            ("MANAGEMENT_PASSWORD", "s3cret-password"),
        ])),
        parent: Some(Parent {
            pid: 1,
            name: Some("systemd".to_owned()),
        }),
    });
    fake.file(
        "/proc/4242/cgroup",
        "0::/user.slice/user-1000.slice/user@1000.service/app.slice/cliproxyapi.service\n",
    );
    fake.answer(
        "systemctl --user show cliproxyapi.service",
        0,
        &shown_unit(
            4242,
            "/home/me/cpa/cli-proxy-api -config /home/me/cpa/config.yaml",
            "/home/me/cpa",
            true,
            "",
            "",
        ),
    );
    fake.on(
        "systemctl --user stop cliproxyapi.service",
        Effect::Exit(SYSTEMD_CPA.to_owned()),
    );
    fake.on(
        "systemctl --user start cliproxyapi.service",
        Effect::Launch(SYSTEMD_CPA.to_owned()),
    );
    fake.on(
        "systemctl --user enable --now open-ferry.service",
        Effect::Launch(context.exe.clone()),
    );
    fake.on(
        "systemctl --user disable --now open-ferry.service",
        Effect::Exit(context.exe.clone()),
    );
    (fake, context)
}

// Not upstream's: a systemd user service is switched to open-ferry's own
// user service, with the backup and the record made first, and switched
// back with -undo; a second -undo has nothing to do.
#[test]
fn switches_a_systemd_user_service_and_back() {
    let (mut fake, context) = systemd_user();
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let out = &ran.out;
    has(out, "Found CLIProxyAPI:");
    has(out, "4242 (/home/me/cpa/cli-proxy-api)");
    has(out, "the systemd user service cliproxyapi.service");
    has(out, "/home/me/cpa/config.yaml (its -config)");
    has(out, "port 8317 on every address (http)");
    has(
        out,
        "The switch (a service switch, to a systemd user service):",
    );
    has(out, "The management key (MANAGEMENT_PASSWORD).");
    has(
        out,
        "Stop and disable the systemd user service cliproxyapi.service, keeping its definition: `systemctl --user stop cliproxyapi.service`, `systemctl --user disable cliproxyapi.service`. Wait up to 20 seconds for CLIProxyAPI (process 4242) to exit.",
    );
    has(
        out,
        &format!(
            "Back up the config, its .env files and the auth directory into {}, which only you can open.",
            systemd_backup()
        ),
    );
    has(
        out,
        "Switched: open-ferry answers on http://127.0.0.1:8317.",
    );
    has(out, "Its logs: journalctl --user -u open-ferry");
    has(
        out,
        "The switch's record is /home/me/.config/open-ferry/migration.json. To switch back: open-ferry migrate -undo",
    );
    in_order(
        &fake.events,
        &[
            &format!("mkdir {}", systemd_backup()),
            &format!(
                "copy {SYSTEMD_CONFIG} -> {}/config/config.yaml",
                systemd_backup()
            ),
            &format!("write {LINUX_RECORD}.tmp"),
            &format!("rename {LINUX_RECORD}.tmp -> {LINUX_RECORD}"),
            &format!("write {}/migration.json.tmp", systemd_backup()),
            "run systemctl --user stop cliproxyapi.service",
            "run systemctl --user disable cliproxyapi.service",
            &format!("write {USER_UNIT}"),
            "run systemctl --user daemon-reload",
            "run systemctl --user enable --now open-ferry.service",
        ],
    );
    // The backup: private, with the config and the credentials, but not
    // the logs.
    let backup = systemd_backup();
    assert!(fake.private.contains(&backup));
    assert_eq!(
        fake.data(&format!("{backup}/config/config.yaml")),
        config_text("~/.cli-proxy-api").as_bytes()
    );
    for name in ["claude.json", "codex.json", "gemini.json", "notes.json"] {
        assert!(fake.exists(&format!("{backup}/auth/{name}")), "{name}");
    }
    assert!(!fake.exists(&format!("{backup}/auth/logs/main.log")));
    assert!(!fake.exists(&format!("{backup}/auth/logs")));
    // The record, and its copy in the backup.
    let record = saved(&fake, LINUX_RECORD);
    assert_eq!(record["version"].as_u64(), Some(1));
    assert_eq!(record["status"], "switched");
    assert_eq!(record["created"], NOW);
    assert_eq!(record["platform"], "linux");
    assert_eq!(record["cliproxyapi"]["config"], SYSTEMD_CONFIG);
    assert_eq!(record["cliproxyapi"]["auth_dir"], "/home/me/.cli-proxy-api");
    assert_eq!(record["cliproxyapi"]["listen"]["port"].as_u64(), Some(8317));
    assert_eq!(record["switch"]["kind"], "service");
    assert_eq!(record["switch"]["ours"], "systemd-user");
    assert_eq!(record["switch"]["theirs"]["manager"], "systemd");
    assert_eq!(record["switch"]["theirs"]["unit"], "cliproxyapi.service");
    assert_eq!(record["switch"]["theirs"]["was_enabled"], true);
    assert_eq!(record["switch"]["theirs"]["was_active"], true);
    assert_eq!(record["backup"]["dir"], backup.as_str());
    assert_eq!(record, saved(&fake, &format!("{backup}/migration.json")));
    assert!(fake.exists(USER_UNIT));
    assert!(!fake.procs.contains_key(&4242));

    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(
        &undo.out,
        &format!(
            "Switching back to CLIProxyAPI, as switched at {NOW} (the record: {LINUX_RECORD}):"
        ),
    );
    has(
        &undo.out,
        "1. Stop and remove the open-ferry service installed as a systemd user service",
    );
    has(
        &undo.out,
        "Turn the systemd user service cliproxyapi.service back on as it was, once open-ferry has stopped: `systemctl --user enable cliproxyapi.service`, `systemctl --user start cliproxyapi.service`.",
    );
    has(
        &undo.out,
        "CLIProxyAPI answers on http://127.0.0.1:8317 again.",
    );
    has(
        &undo.out,
        &format!(
            "Switched back to CLIProxyAPI. The config and the credentials are left as open-ferry had them; the backup is kept in {backup}."
        ),
    );
    in_order(
        &fake.events,
        &[
            "run systemctl --user disable --now open-ferry.service",
            &format!("remove {USER_UNIT}"),
            "run systemctl --user enable cliproxyapi.service",
            "run systemctl --user start cliproxyapi.service",
        ],
    );
    assert!(!fake.exists(USER_UNIT));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");
    assert!(
        fake.procs
            .values()
            .any(|process| process.exe.as_deref() == Some(SYSTEMD_CPA))
    );

    let again = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(again.code, 1);
    has(
        &again.err,
        &format!("migrate: the switch made at {NOW} was already undone, at {NOW}"),
    );
}

// Not upstream's: without -yes, migrate asks; with no one at a terminal it
// changes nothing and says what to run.
#[test]
fn asks_before_switching() {
    let (mut fake, context) = systemd_user();
    let ran = migrate(&mut fake, &context, &[]);
    assert_eq!(ran.code, 1);
    has(
        &ran.out,
        "Nothing was changed: there is no one at a terminal to ask. To switch, run: /home/me/.local/bin/open-ferry migrate -yes",
    );
    assert!(fake.questions.is_empty());
    read_only(&fake);

    let (mut fake, context) = systemd_user();
    fake.terminal = true;
    fake.replies.push_back(false);
    let ran = migrate(&mut fake, &context, &[]);
    assert_eq!(ran.code, 0);
    has(&ran.out, "Nothing was changed.");
    assert_eq!(fake.questions, ["Switch to open-ferry now? [y/N]"]);
    read_only(&fake);

    let (mut fake, context) = systemd_user();
    fake.terminal = true;
    fake.replies.push_back(true);
    let ran = migrate(&mut fake, &context, &[]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "Switched: open-ferry answers on http://127.0.0.1:8317.",
    );
}

// Not upstream's: -dry-run shows the switch and changes nothing.
#[test]
fn a_dry_run_changes_nothing() {
    let (mut fake, context) = systemd_user();
    let files = fake.files.clone();
    let dirs = fake.dirs.clone();
    let ran = migrate(&mut fake, &context, &["-dry-run", "-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(&ran.out, "Dry run: nothing was changed.");
    has(
        &ran.out,
        "Install open-ferry as a systemd user service (/home/me/.config/systemd/user/open-ferry.service), running `/home/me/.local/bin/open-ferry -config /home/me/cpa/config.yaml` in /home/me/cpa, and start it: create /home/me/.config/systemd/user; write /home/me/.config/systemd/user/open-ferry.service; run `systemctl --user daemon-reload`; run `systemctl --user enable --now open-ferry.service`.",
    );
    has(
        &ran.out,
        "Wait up to 30 seconds for open-ferry to answer on http://127.0.0.1:8317, and undo the switch if it doesn't.",
    );
    has(&ran.out, "To switch back: `open-ferry migrate -undo`.");
    has(
        &ran.out,
        "it comments out the sections only open-ferry reads, `routing.quota` and `management.separate-address`.",
    );
    assert_eq!(fake.files, files);
    assert_eq!(fake.dirs, dirs);
    read_only(&fake);
}

// Not upstream's: -json is one line for the install scripts, and changes
// nothing.
#[test]
fn json_says_what_was_found() {
    let (mut fake, context) = systemd_user();
    let ran = migrate(&mut fake, &context, &["-json"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(ran.out.lines().count(), 1, "{}", ran.out);
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_eq!(json["found"], true);
    assert_eq!(json["can_switch"], true);
    assert_eq!(
        json["summary"],
        "CLIProxyAPI (process 4242), started by the systemd user service cliproxyapi.service"
    );
    assert_eq!(json["switch"], "service");
    assert_eq!(json["target"], "systemd-user");
    assert_eq!(json["process"]["pid"].as_u64(), Some(4242));
    assert_eq!(json["process"]["exe"], SYSTEMD_CPA);
    assert_eq!(json["starter"]["kind"], "systemd");
    assert_eq!(json["config"], SYSTEMD_CONFIG);
    assert_eq!(json["config_from"], "its -config");
    assert_eq!(json["working_dir"], "/home/me/cpa");
    assert_eq!(json["auth_dir"], "/home/me/.cli-proxy-api");
    assert_eq!(json["listen"]["port"].as_u64(), Some(8317));
    assert_eq!(json["listen"]["host"], "");
    assert_eq!(json["credentials"]["served"]["Claude"].as_u64(), Some(1));
    assert_eq!(json["credentials"]["served"]["Codex"].as_u64(), Some(1));
    assert_eq!(
        json["credentials"]["not_served"]["gemini-cli"].as_u64(),
        Some(1)
    );
    assert_eq!(json["credentials"]["other_files"].as_u64(), Some(1));
    assert_eq!(json["credentials"]["unreadable"].as_u64(), Some(0));
    assert_eq!(json["claude_sign_ins"]["count"].as_u64(), Some(1));
    has(
        json["claude_sign_ins"]["warning"].as_str().unwrap(),
        "Claude sign-in",
    );
    assert_eq!(json["blockers"], Value::Array(Vec::new()));
    assert_eq!(json["backup_dir"], systemd_backup().as_str());
    assert_eq!(json["record"], LINUX_RECORD);
    assert_eq!(json["command"], "/home/me/.local/bin/open-ferry migrate");
    assert!(json["plan"].as_array().is_some_and(|plan| plan.len() == 5));
    // install.sh reads the line as text: it starts with "found", and each
    // key it reads is there once.
    assert!(
        ran.out.starts_with(r#"{"found":true,"summary":""#),
        "{}",
        ran.out
    );
    for key in [
        r#""summary":"#,
        r#""can_switch":"#,
        r#""config":"#,
        r#""host":"#,
        r#""port":"#,
        r#""tls":"#,
    ] {
        assert_eq!(ran.out.matches(key).count(), 1, "{key}");
    }
    read_only(&fake);
}

// Not upstream's: what carries over and what doesn't, and the warning on
// Claude sign-ins.
#[test]
fn says_what_carries_over_and_warns_of_claude_sign_ins() {
    let (mut fake, context) = systemd_user();
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let out = &ran.out;
    has(
        out,
        "Carries over:\n  - The address and port: port 8317 on every address (http).",
    );
    has(out, "The client keys: 1 key in api-keys.");
    has(
        out,
        "The credential files open-ferry serves: 1 Claude, 1 Codex.",
    );
    has(
        out,
        "The logs directory: /home/me/cpa/logs, or /home/me/.cli-proxy-api/logs if that can't be written to.",
    );
    has(
        out,
        "Doesn't carry over:\n  - Credential files of providers open-ferry doesn't serve: 1 gemini-cli. They stay in the auth directory, unused.",
    );
    has(
        out,
        "1 other .json file in the auth directory that holds no credential open-ferry reads.",
    );
    has(
        out,
        "Claude sign-ins:\n  - 1 credential file from CLIProxyAPI's Claude sign-in.",
    );
    has(out, "docs/claude-subscription.md");
}

// Not upstream's: open-ferry's check runs on the config in CLIProxyAPI's
// working directory, and its warnings and errors are shown, but for the
// port CLIProxyAPI holds.
#[test]
fn shows_what_check_finds() {
    let (mut fake, context) = systemd_user();
    let finding = |level, check: &str, message: &str, fix: &str| Finding {
        level,
        check: check.to_owned(),
        message: message.to_owned(),
        fix: fix.to_owned(),
    };
    fake.findings = vec![
        finding(Level::Ok, "config", "the config loads", ""),
        finding(
            Level::Error,
            "address",
            "port 8317 is in use",
            "stop what uses it",
        ),
        finding(
            Level::Error,
            "client keys",
            "a client key is too short",
            "make it longer",
        ),
        finding(Level::Warning, "clock", "the clock can't be checked", ""),
    ];
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "Check:\n  - error: A client key is too short.\n    Fix: Make it longer.",
    );
    lacks(&ran.out, "port 8317 is in use");
    lacks(&ran.out, "the config loads");
    lacks(&ran.out, "clock");
    assert_eq!(
        fake.checked,
        ["/home/me/cpa/config.yaml in /home/me/cpa, with MANAGEMENT_PASSWORD"]
    );
}

// Not upstream's: when open-ferry doesn't answer, the switch is undone and
// CLIProxyAPI's service is turned back on.
#[test]
fn rolls_back_when_open_ferry_does_not_answer() {
    let (mut fake, context) = systemd_user();
    fake.broken = true;
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        "open-ferry didn't answer: nothing answered on http://127.0.0.1:8317 within 30 seconds (connection refused). Undoing the switch.",
    );
    has(
        &ran.out,
        "CLIProxyAPI answers on http://127.0.0.1:8317 again.",
    );
    has(
        &ran.err,
        "migrate: The switch is undone: open-ferry didn't answer: nothing answered on http://127.0.0.1:8317 within 30 seconds (connection refused). Its logs: journalctl --user -u open-ferry. CLIProxyAPI is back as it was; the backup is kept.",
    );
    in_order(
        &fake.events,
        &[
            "run systemctl --user enable --now open-ferry.service",
            "run systemctl --user disable --now open-ferry.service",
            &format!("remove {USER_UNIT}"),
            "run systemctl --user enable cliproxyapi.service",
            "run systemctl --user start cliproxyapi.service",
        ],
    );
    assert!(!fake.exists(USER_UNIT));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "rolled-back");
    assert!(fake.exists(&format!("{}/config/config.yaml", systemd_backup())));

    // A rolled-back switch has nothing to undo.
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1);
    has(
        &undo.err,
        &format!("the switch made at {NOW} failed and was undone then: there is nothing to undo"),
    );
}

// Not upstream's: when CLIProxyAPI's service won't stop, or its process
// doesn't exit, it is turned back on and open-ferry isn't installed.
#[test]
fn rolls_back_when_cliproxyapi_does_not_stop() {
    let (mut fake, context) = systemd_user();
    fake.fail(
        "systemctl --user stop cliproxyapi.service",
        1,
        "Access denied",
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(&ran.out, "Failed to stop CLIProxyAPI: ");
    has(&ran.out, "Turning its service back on.");
    has(
        &ran.err,
        "migrate: The switch is undone: CLIProxyAPI didn't stop: ",
    );
    assert!(happened(
        &fake,
        "run systemctl --user start cliproxyapi.service"
    ));
    assert!(!happened(&fake, &format!("write {USER_UNIT}")));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "rolled-back");

    let (mut fake, context) = systemd_user();
    fake.effects
        .remove("systemctl --user stop cliproxyapi.service");
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.err,
        "CLIProxyAPI didn't stop: process 4242 still runs 20 seconds after its service was stopped",
    );
    assert!(!happened(&fake, &format!("write {USER_UNIT}")));
}

// Not upstream's: when open-ferry's service doesn't install, what was
// installed is removed and CLIProxyAPI's service is turned back on.
#[test]
fn rolls_back_when_the_service_does_not_install() {
    let (mut fake, context) = systemd_user();
    fake.fail(
        "systemctl --user enable --now open-ferry.service",
        1,
        "Failed to enable unit",
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(&ran.out, "Failed to install open-ferry's service: ");
    has(
        &ran.err,
        "migrate: The switch is undone: open-ferry's service didn't install: ",
    );
    in_order(
        &fake.events,
        &[
            &format!("remove {USER_UNIT}"),
            "run systemctl --user enable cliproxyapi.service",
            "run systemctl --user start cliproxyapi.service",
        ],
    );
    has(
        &ran.out,
        "CLIProxyAPI answers on http://127.0.0.1:8317 again.",
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "rolled-back");
}

// Not upstream's: -undo -restore copies the backed-up config and
// credentials back, and warns of refreshed tokens first.
#[test]
fn undo_restores_the_backup() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.file(SYSTEMD_CONFIG, "port: 8317\nchanged: by open-ferry\n");
    fake.file(
        "/home/me/.cli-proxy-api/claude.json",
        br#"{"type":"claude","refreshed":true}"#,
    );
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    let backup = systemd_backup();
    has(
        &undo.out,
        &format!("Copy back from {backup}: /home/me/cpa/config.yaml, /home/me/.cli-proxy-api."),
    );
    has(
        &undo.out,
        &format!("-restore puts back the config, .env and credential files as they were at {NOW}."),
    );
    has(
        &undo.out,
        &format!("Restored /home/me/cpa/config.yaml from {backup}/config/config.yaml"),
    );
    has(
        &undo.out,
        &format!("Restored /home/me/.cli-proxy-api from {backup}/auth"),
    );
    has(
        &undo.out,
        "The config and the credentials are restored from the backup",
    );
    assert_eq!(
        fake.data(SYSTEMD_CONFIG),
        config_text("~/.cli-proxy-api").as_bytes()
    );
    assert_eq!(
        fake.data("/home/me/.cli-proxy-api/claude.json"),
        br#"{"type":"claude"}"#
    );
}

// Not upstream's: -undo shows its steps with -dry-run, and asks first.
#[test]
fn undo_asks_first() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let events = fake.events.len();
    let files = fake.files.clone();

    let dry = migrate(&mut fake, &context, &["-undo", "-dry-run"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    has(
        &dry.out,
        "Leave the config and the auth directory as they are: with -restore, the backed-up ones are copied back.",
    );
    has(&dry.out, "Dry run: nothing was changed.");

    let ran = migrate(&mut fake, &context, &["-undo"]);
    assert_eq!(ran.code, 1);
    has(
        &ran.out,
        "Nothing was changed: there is no one at a terminal to ask. To switch back, run: /home/me/.local/bin/open-ferry migrate -undo -yes",
    );
    let ran = migrate(&mut fake, &context, &["-undo", "-restore"]);
    assert_eq!(ran.code, 1);
    has(
        &ran.out,
        "To switch back, run: /home/me/.local/bin/open-ferry migrate -undo -restore -yes",
    );

    fake.terminal = true;
    fake.replies.push_back(false);
    let ran = migrate(&mut fake, &context, &["-undo"]);
    assert_eq!(ran.code, 0);
    has(&ran.out, "Nothing was changed.");
    assert_eq!(fake.questions, ["Switch back to CLIProxyAPI now? [y/N]"]);
    assert_eq!(fake.files, files);
    let reads: Vec<&String> = fake.events.iter().skip(events).collect();
    assert!(reads.is_empty(), "{reads:#?}");
}

// Not upstream's: -undo without a record, or with another platform's.
#[test]
fn undo_needs_the_record() {
    let (mut fake, context) = systemd_user();
    let ran = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(ran.code, 1);
    has(
        &ran.err,
        "migrate: there is no switch to undo: /home/me/.config/open-ferry/migration.json doesn't exist. A switch made with sudo is undone with sudo, as its record is root's.",
    );

    let context = windows();
    let mut fake = Fake::new(&context);
    let ran = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(ran.code, 1);
    has(
        &ran.err,
        r"there is no switch to undo: C:\Users\me\AppData\Roaming\open-ferry\migration.json doesn't exist.",
    );
    lacks(&ran.err, "sudo");

    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let mut record = saved(&fake, LINUX_RECORD);
    record["platform"] = Value::String("macos".to_owned());
    fake.file(LINUX_RECORD, record.to_string());
    let ran = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(ran.code, 1);
    has(
        &ran.err,
        "the switch's record, /home/me/.config/open-ferry/migration.json, is of a switch on macos",
    );

    fake.file(LINUX_RECORD, "{");
    let ran = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(ran.code, 1);
    has(
        &ran.err,
        "the switch's record, /home/me/.config/open-ferry/migration.json, doesn't read",
    );
}

// Not upstream's: a switch that isn't undone blocks another, and
// open-ferry already on the port stops the switch before any change.
#[test]
fn does_not_switch_twice() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        "not running (its binary: /home/me/cpa/cli-proxy-api)",
    );
    has(
        &ran.out,
        &format!(
            "A switch made at {NOW} isn't undone ({LINUX_RECORD}). Run `open-ferry migrate -undo` before switching again."
        ),
    );
    has(
        &ran.out,
        "The switch is blocked: nothing was changed. Fix what is listed under Blockers, then run `/home/me/.local/bin/open-ferry migrate` again.",
    );

    let (mut fake, context) = systemd_user();
    fake.serving.insert(0, (1, Answer::OpenFerry));
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1);
    has(
        &ran.err,
        "migrate: open-ferry already answers on http://127.0.0.1:8317, so nothing was changed. Is CLIProxyAPI already switched?",
    );
    read_only(&fake);
}

// Not upstream's: what about the config, its environment and its auth
// directory blocks the switch.
#[test]
fn blocks_on_the_config_the_environment_and_the_auth_directory() {
    let blocked = |fake: &mut Fake, context: &Context, blocker: &str| {
        let ran = migrate(fake, context, &["-yes"]);
        assert_eq!(ran.code, 1, "{}", ran.all());
        has(&ran.out, "Blockers:");
        has(&ran.out, blocker);
        has(&ran.out, "The switch is blocked: nothing was changed.");
        read_only(fake);
    };

    let (mut fake, context) = systemd_user();
    fake.files.remove(SYSTEMD_CONFIG);
    blocked(
        &mut fake,
        &context,
        "Can't read its config, /home/me/cpa/config.yaml: not found.",
    );

    let (mut fake, context) = systemd_user();
    fake.file(SYSTEMD_CONFIG, "port: [8317\n");
    blocked(
        &mut fake,
        &context,
        "Its config, /home/me/cpa/config.yaml, doesn't load in open-ferry:",
    );

    let (mut fake, context) = systemd_user();
    if let Some(process) = fake.procs.get_mut(&4242) {
        process.env = Some(vars(&[
            ("HOME", "/home/me"),
            ("PGSTORE_DSN", "postgres://s3cret@db/cpa"),
        ]));
    }
    fake.file("/home/me/cpa/.env", "DEPLOY=cloud\n");
    blocked(
        &mut fake,
        &context,
        "It uses remote storage, cloud mode or Home mode, which open-ferry doesn't have: PGSTORE_DSN (in its environment), DEPLOY=cloud (in /home/me/cpa/.env). open-ferry needs the config and credentials as local files.",
    );

    let (mut fake, context) = systemd_user();
    if let Some(process) = fake.procs.get_mut(&4242) {
        process.env = Some(Vec::new());
    }
    blocked(
        &mut fake,
        &context,
        "Can't find its auth directory: its auth-dir, ~/.cli-proxy-api, starts with ~, and its home directory can't be told.",
    );

    let (mut fake, context) = systemd_user();
    fake.unreadable.insert("/home/me/.cli-proxy-api".to_owned());
    blocked(
        &mut fake,
        &context,
        "Its auth directory, /home/me/.cli-proxy-api, can't be read: access denied",
    );

    let (mut fake, context) = systemd_user();
    fake.unreadable
        .insert("/home/me/.cli-proxy-api/claude.json".to_owned());
    blocked(
        &mut fake,
        &context,
        "1 file in its auth directory can't be read, so it can't be backed up.",
    );

    // A missing auth directory is only a warning.
    let (mut fake, context) = systemd_user();
    let auth: Vec<String> = fake
        .files
        .keys()
        .filter(|path| path.starts_with("/home/me/.cli-proxy-api/"))
        .cloned()
        .collect();
    for path in auth {
        fake.files.remove(&path);
    }
    fake.dirs
        .retain(|dir| !dir.starts_with("/home/me/.cli-proxy-api"));
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "Its auth directory, /home/me/.cli-proxy-api, doesn't exist, so it has no credential files.",
    );
}

// Not upstream's: another user's systemd service is theirs to switch.
#[test]
fn blocks_on_another_users_service() {
    let (mut fake, context) = systemd_user();
    fake.file(
        "/proc/4242/cgroup",
        "0::/user.slice/user-1001.slice/user@1001.service/app.slice/cliproxyapi.service\n",
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        "Its systemd user service, cliproxyapi.service, is that of user ID 1001, not yours: run `open-ferry migrate` as that user.",
    );
    read_only(&fake);
}

// Not upstream's: two CLIProxyAPIs need -config to tell them apart.
#[test]
fn chooses_between_two_by_their_config() {
    let (mut fake, context) = systemd_user();
    fake.file(
        "/home/me/other/config.yaml",
        config_text("/home/me/other/auths"),
    );
    fake.process(Proc {
        pid: 4243,
        started: Some(1000),
        name: "cli-proxy-api".to_owned(),
        exe: Some(SYSTEMD_CPA.to_owned()),
        args: strings(&["-config", "/home/me/other/config.yaml"]),
        cwd: Some("/home/me/other".to_owned()),
        env: Some(vars(&[("HOME", "/home/me")])),
        parent: None,
    });
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    assert_eq!(ran.code, 1);
    has(
        &ran.err,
        "migrate: Found more than one CLIProxyAPI running (processes 4242, 4243). Name the config of the one to switch with -config.",
    );
    let ran = migrate(&mut fake, &context, &["-json"]);
    assert_eq!(ran.code, 1);
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_eq!(json["found"], false);
    has(
        json["error"].as_str().unwrap(),
        "Found more than one CLIProxyAPI running",
    );

    let ran = migrate(
        &mut fake,
        &context,
        &["-json", "-config", "cpa/config.yaml"],
    );
    assert_eq!(ran.code, 0, "{}", ran.all());
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_eq!(json["process"]["pid"].as_u64(), Some(4242));
    assert_eq!(json["config"], SYSTEMD_CONFIG);
    assert_eq!(json["config_from"], "migrate's -config");
    assert_eq!(
        json["command"],
        "/home/me/.local/bin/open-ferry migrate -config /home/me/cpa/config.yaml"
    );
}

// --- A systemd system service ---

const SYSTEM_CPA: &str = "/usr/local/bin/cli-proxy-api";
const SYSTEM_UNIT: &str = "/etc/systemd/system/open-ferry.service";

fn system_unit(context: Context, running: bool) -> (Fake, Context) {
    let mut fake = Fake::new(&context);
    fake.file(SYSTEM_CPA, CLIPROXYAPI);
    fake.file(
        "/etc/cliproxyapi/config.yaml",
        config_text("/etc/cliproxyapi/auths"),
    );
    credentials(&mut fake, "/etc/cliproxyapi/auths");
    fake.file(
        "/etc/systemd/system/cliproxyapi.service",
        "[Service]\nExecStart=/usr/local/bin/cli-proxy-api -config /etc/cliproxyapi/config.yaml\n",
    );
    let pid = if running { 4343 } else { 0 };
    fake.answer(
        "systemctl show cliproxyapi.service",
        0,
        &shown_unit(
            pid,
            "/usr/local/bin/cli-proxy-api -config /etc/cliproxyapi/config.yaml",
            "/etc/cliproxyapi",
            running,
            "DEPLOY=local",
            if running { "cliproxy" } else { "" },
        ),
    );
    if running {
        fake.process(Proc {
            pid: 4343,
            started: Some(1000),
            name: "cli-proxy-api".to_owned(),
            exe: Some(SYSTEM_CPA.to_owned()),
            args: strings(&["-config", "/etc/cliproxyapi/config.yaml"]),
            cwd: None,
            env: None,
            parent: Some(Parent {
                pid: 1,
                name: Some("systemd".to_owned()),
            }),
        });
        fake.file(
            "/proc/4343/cgroup",
            "0::/system.slice/cliproxyapi.service\n",
        );
    }
    fake.on(
        "systemctl enable --now open-ferry.service",
        Effect::Launch(context.exe.clone()),
    );
    fake.on(
        "systemctl disable --now open-ferry.service",
        Effect::Exit(context.exe.clone()),
    );
    (fake, context)
}

// Not upstream's: a system service is given open-ferry's path after links,
// and the record names that path, the one its processes report.
#[test]
fn a_system_service_records_open_ferrys_path_after_links() {
    const REAL: &str = "/opt/open-ferry/bin/open-ferry";
    let (mut fake, context) = system_unit(linux_root(), false);
    fake.answer("id -u", 0, "0\n");
    fake.files.remove(&context.exe);
    fake.file(REAL, OPEN_FERRY);
    fake.links.insert(context.exe.clone(), REAL.to_owned());
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(
        saved(&fake, "/root/.config/open-ferry/migration.json")["switch"]["ours_exe"],
        REAL
    );
}

// Not upstream's: a system service that isn't running is switched from its
// unit's command line, and only enabled again by -undo.
#[test]
fn switches_a_stopped_system_service_and_back() {
    let context = linux_root();
    let (mut fake, context) = {
        let (mut fake, context) = system_unit(context, false);
        fake.answer("id -u", 0, "0\n");
        (fake, context)
    };
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "not running (its binary: /usr/local/bin/cli-proxy-api)",
    );
    has(
        &ran.out,
        "CLIProxyAPI isn't running: its command line is read from cliproxyapi.service.",
    );
    has(
        &ran.out,
        "The variables its service sets (DEPLOY): open-ferry's service doesn't set them. Put those it needs in .env in /etc/cliproxyapi.",
    );
    has(&ran.out, "Its logs: journalctl -u open-ferry");
    in_order(
        &fake.events,
        &[
            "run systemctl disable cliproxyapi.service",
            &format!("write {SYSTEM_UNIT}"),
            "run systemctl enable --now open-ferry.service",
        ],
    );
    assert!(!happened(&fake, "run systemctl stop cliproxyapi.service"));
    let record = saved(&fake, "/root/.config/open-ferry/migration.json");
    assert_eq!(record["switch"]["ours"], "systemd-system");
    assert_eq!(record["switch"]["theirs"]["was_active"], false);

    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(happened(&fake, "run systemctl enable cliproxyapi.service"));
    assert!(!happened(&fake, "run systemctl start cliproxyapi.service"));
    assert!(!fake.exists(SYSTEM_UNIT));
    lacks(&undo.out, "answers on");
}

// Not upstream's: a system service needs root, and one that runs as
// another user is switched by hand.
#[test]
fn a_system_service_needs_root_and_its_own_user() {
    let (mut fake, context) = system_unit(linux(), true);
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        "Its system service runs as the user cliproxy, and open-ferry's runs as root: switch by hand, as docs/migrating-from-cliproxyapi.md says.",
    );
    has(
        &ran.out,
        "Switching a system service needs root. Run: sudo /home/me/.local/bin/open-ferry migrate.",
    );
    has(
        &ran.out,
        "then run `sudo /home/me/.local/bin/open-ferry migrate` again.",
    );
    // Its environment can't be read, so the unit's is.
    has(&ran.out, "The variables its service sets (DEPLOY)");
    read_only(&fake);
}

// --- A drop-in: CLIProxyAPI started by hand ---

const OPT_CPA: &str = "/opt/cpa/cli-proxy-api";
const OPT_MOVED: &str = "/opt/cpa/cli-proxy-api.cliproxyapi";

fn opt_backup() -> String {
    format!("/opt/cpa/open-ferry-migrate-{STAMP}")
}

/// CLIProxyAPI started from a shell, with upstream's -password.
fn bare_process() -> (Fake, Context) {
    let context = linux();
    let mut fake = Fake::new(&context);
    // An install receipt names the installed open-ferry, which the link follows.
    fake.installed = Some(context.exe.clone());
    fake.file(OPT_CPA, CLIPROXYAPI);
    fake.file("/opt/cpa/config.yaml", config_text("/opt/cpa/auths"));
    credentials(&mut fake, "/opt/cpa/auths");
    fake.process(Proc {
        pid: 5151,
        started: Some(1000),
        name: "cli-proxy-api".to_owned(),
        exe: Some(OPT_CPA.to_owned()),
        args: strings(&["-password", "s3cret-password"]),
        cwd: Some("/opt/cpa".to_owned()),
        env: Some(vars(&[("HOME", "/home/me")])),
        parent: Some(Parent {
            pid: 777,
            name: Some("bash".to_owned()),
        }),
    });
    fake.file(
        "/proc/5151/cgroup",
        "0::/user.slice/user-1000.slice/session-2.scope\n",
    );
    (fake, context)
}

// Not upstream's: a CLIProxyAPI that no service runs has its binary
// replaced by open-ferry's, which is started with its command line; -undo
// puts it back and starts CLIProxyAPI the same way.
#[test]
fn drops_in_for_a_bare_process_and_back() {
    let (mut fake, context) = bare_process();
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let out = &ran.out;
    has(out, "the program bash (process 777)");
    has(
        out,
        "/opt/cpa/config.yaml (upstream's default, config.yaml in its working directory)",
    );
    has(
        out,
        "The switch (a drop-in: open-ferry in place of CLIProxyAPI's binary):",
    );
    has(
        out,
        "The start command and its flags (-password), which open-ferry takes as CLIProxyAPI does.",
    );
    has(
        out,
        "Rename CLIProxyAPI's binary, /opt/cpa/cli-proxy-api, to /opt/cpa/cli-proxy-api.cliproxyapi, and make /opt/cpa/cli-proxy-api a symbolic link to the installed open-ferry, /home/me/.local/bin/open-ferry, so that the program bash starts open-ferry.",
    );
    has(
        out,
        "The link follows the installed open-ferry: `open-ferry update` replaces /home/me/.local/bin/open-ferry, and /opt/cpa/cli-proxy-api runs the new version from the next start.",
    );
    has(
        out,
        "`cli-proxy-api` in its place logs `open-ferry Version: ...` as it starts",
    );
    has(
        out,
        "Moved CLIProxyAPI's binary to /opt/cpa/cli-proxy-api.cliproxyapi",
    );
    has(
        out,
        "Linked /opt/cpa/cli-proxy-api to /home/me/.local/bin/open-ferry, the installed open-ferry",
    );
    has(out, "Stopped CLIProxyAPI (process 5151)");
    has(
        out,
        &format!(
            "Started open-ferry (process 9001) with CLIProxyAPI's command line; its output goes to {}/open-ferry.log",
            opt_backup()
        ),
    );
    has(
        out,
        "Switched: open-ferry answers on http://127.0.0.1:8317.",
    );
    assert_eq!(fake.data(OPT_CPA), OPEN_FERRY);
    assert_eq!(
        fake.link_target(OPT_CPA).as_deref(),
        Some("/home/me/.local/bin/open-ferry")
    );
    assert_eq!(fake.data(OPT_MOVED), CLIPROXYAPI);
    assert!(fake.private.contains(&opt_backup()));
    let launch = fake.launches.first().unwrap();
    assert_eq!(launch.exe, OPT_CPA);
    assert_eq!(launch.args, ["-password", "s3cret-password"]);
    assert_eq!(launch.cwd, "/opt/cpa");
    assert_eq!(launch.env, Some(vars(&[("HOME", "/home/me")])));
    assert_eq!(launch.log, format!("{}/open-ferry.log", opt_backup()));
    assert!(fake.questions.is_empty());
    let record = saved(&fake, LINUX_RECORD);
    assert_eq!(record["status"], "switched");
    assert_eq!(record["switch"]["kind"], "drop-in");
    assert_eq!(record["switch"]["binary"], OPT_CPA);
    assert_eq!(record["switch"]["moved_to"], OPT_MOVED);
    assert_eq!(record["switch"]["restarted"], true);
    assert_eq!(record["switch"]["pid"].as_u64(), Some(5151));
    assert_eq!(record["switch"]["started"].as_u64(), Some(1000));
    assert_eq!(record["switch"]["link"], "/home/me/.local/bin/open-ferry");
    assert_eq!(
        record["cliproxyapi"]["started_by"],
        "the program bash (process 777)"
    );

    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(
        &undo.out,
        "Remove the symbolic link /opt/cpa/cli-proxy-api, and move CLIProxyAPI's binary back from /opt/cpa/cli-proxy-api.cliproxyapi to /opt/cpa/cli-proxy-api.",
    );
    assert_eq!(fake.link_target(OPT_CPA), None);
    has(
        &undo.out,
        "Moved CLIProxyAPI's binary back to /opt/cpa/cli-proxy-api",
    );
    has(&undo.out, "Stopped open-ferry (process 9001)");
    has(
        &undo.out,
        &format!(
            "Started CLIProxyAPI (process 9002) with the same command line; its output goes to {}/cliproxyapi.log",
            opt_backup()
        ),
    );
    has(
        &undo.out,
        "CLIProxyAPI answers on http://127.0.0.1:8317 again.",
    );
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
    assert!(!fake.exists(OPT_MOVED));
    assert!(!fake.exists("/opt/cpa/cli-proxy-api.open-ferry"));
    assert_eq!(
        fake.launches.get(1).map(|launch| launch.args.clone()),
        Some(strings(&["-password", "s3cret-password"]))
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");
}

// Not upstream's: a launcher that starts CLIProxyAPI's binary again starts
// open-ferry, and migrate doesn't start a second one.
#[test]
fn a_launcher_that_restarts_it_starts_open_ferry() {
    let (mut fake, context) = bare_process();
    fake.on("stop 5151", Effect::Launch(OPT_CPA.to_owned()));
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(&ran.out, "The program bash started open-ferry.");
    assert!(fake.launches.is_empty());
    assert_eq!(saved(&fake, LINUX_RECORD)["switch"]["restarted"], true);
}

// Not upstream's: a CLIProxyAPI that won't stop, or an open-ferry that
// doesn't answer, puts CLIProxyAPI's binary back.
#[test]
fn a_drop_in_rolls_back() {
    let (mut fake, context) = bare_process();
    fake.unstoppable.insert(5151);
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.err,
        "migrate: The switch is undone: CLIProxyAPI (process 5151) didn't stop: access denied. CLIProxyAPI is back as it was; the backup is kept.",
    );
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
    assert!(!fake.exists(OPT_MOVED));
    assert!(!fake.exists("/opt/cpa/cli-proxy-api.open-ferry"));
    assert!(fake.procs.contains_key(&5151));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "rolled-back");

    let (mut fake, context) = bare_process();
    fake.broken = true;
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        "open-ferry didn't answer: nothing answered on http://127.0.0.1:8317 within 30 seconds (connection refused). Undoing the switch.",
    );
    has(&ran.out, "Stopped open-ferry (process 9001)");
    has(
        &ran.out,
        &format!(
            "Started CLIProxyAPI again (process 9002); its output goes to {}/cliproxyapi.log",
            opt_backup()
        ),
    );
    has(
        &ran.out,
        "CLIProxyAPI answers on http://127.0.0.1:8317 again.",
    );
    has(
        &ran.err,
        "migrate: The switch is undone: open-ferry didn't answer:",
    );
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "rolled-back");
}

// Not upstream's: -undo keeps a binary that an update put in open-ferry's
// place, and needs CLIProxyAPI's where the switch moved it.
#[test]
fn undo_of_a_drop_in_keeps_what_it_did_not_put_there() {
    let (mut fake, context) = bare_process();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    // An installer put a file where the link was.
    fake.links.remove(OPT_CPA);
    fake.file(OPT_CPA, b"CLIProxyAPI 9, updated in place");
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(
        &undo.out,
        "What was at /opt/cpa/cli-proxy-api isn't this open-ferry's binary (an update may have replaced it): it is kept at /opt/cpa/cli-proxy-api.open-ferry.",
    );
    assert_eq!(
        fake.data("/opt/cpa/cli-proxy-api.open-ferry"),
        b"CLIProxyAPI 9, updated in place"
    );
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);

    let (mut fake, context) = bare_process();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.files.remove(OPT_MOVED);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1);
    has(
        &undo.err,
        "migrate: CLIProxyAPI's binary isn't at /opt/cpa/cli-proxy-api.cliproxyapi, and what is at /opt/cpa/cli-proxy-api isn't it.",
    );
    has(
        &undo.err,
        "wasn't put back, and the record stays open: put it at /opt/cpa/cli-proxy-api by hand",
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
}

// Not upstream's: what blocks a drop-in: flags open-ferry doesn't take, a
// subcommand's name, a binary already moved, this open-ferry in its place,
// a path that can't be read and a working directory that can't.
#[test]
fn blocks_a_drop_in() {
    let blocked = |change: &dyn Fn(&mut Fake), blocker: &str| {
        let (mut fake, context) = bare_process();
        change(&mut fake);
        let ran = migrate(&mut fake, &context, &["-yes"]);
        assert_eq!(ran.code, 1, "{}", ran.all());
        has(&ran.out, blocker);
        read_only(&fake);
    };
    let args = |args: &'static [&'static str]| {
        move |fake: &mut Fake| {
            if let Some(process) = fake.procs.get_mut(&5151) {
                process.args = strings(args);
            }
        }
    };
    blocked(
        &args(&["-home-disable-cluster-discovery", "-tui"]),
        "Its start command has -home-disable-cluster-discovery, which open-ferry doesn't take: it would stop with its usage. Take it out of what starts CLIProxyAPI first.",
    );
    blocked(
        &args(&["-discover", "-xai-login"]),
        "Its start command has -discover, -xai-login, which open-ferry doesn't take: it would stop with its usage. Take them out of what starts CLIProxyAPI first.",
    );
    blocked(
        &args(&["-bogus"]),
        "Its start command has -bogus, which open-ferry doesn't take: it would stop with its usage.",
    );
    blocked(
        &args(&["check"]),
        "Its start command's first argument is check, which open-ferry would run as its subcommand.",
    );
    blocked(
        &|fake: &mut Fake| fake.file(OPT_MOVED, CLIPROXYAPI),
        "/opt/cpa/cli-proxy-api.cliproxyapi already exists, so CLIProxyAPI's binary can't be moved there.",
    );
    blocked(
        &|fake: &mut Fake| {
            if let Some(process) = fake.procs.get_mut(&5151) {
                process.exe = None;
            }
        },
        "The path of CLIProxyAPI's binary (process 5151) can't be read.",
    );
    blocked(
        &|fake: &mut Fake| {
            if let Some(process) = fake.procs.get_mut(&5151) {
                process.cwd = None;
            }
        },
        "Can't tell which config it uses: its working directory can't be read. Name the config with -config.",
    );

    // open-ferry's own binary, started as cli-proxy-api.
    let context = context(
        Platform::Linux,
        OPT_CPA,
        "/home/me",
        &[("HOME", "/home/me")],
        "/home/me/.config/open-ferry/config.yaml",
    );
    let (mut fake, _) = bare_process();
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        "/opt/cpa/cli-proxy-api is this open-ferry binary: run the open-ferry you installed, not one in CLIProxyAPI's place.",
    );
}

// Not upstream's: arguments after the flags, a launcher that has exited
// and a systemd unit that runs CLIProxyAPI through another program.
#[test]
fn names_each_launcher() {
    let (mut fake, context) = bare_process();
    if let Some(process) = fake.procs.get_mut(&5151) {
        process.args = strings(&["-password", "s3cret-password", "extra"]);
        process.parent = None;
    }
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "a program that has since exited (such as a launcher script or a closed terminal)",
    );
    has(&ran.out, "so that what started it starts open-ferry.");
    has(
        &ran.out,
        "If not, stop it and start it again the way you started it, and open-ferry starts in its place.",
    );
    has(
        &ran.out,
        "Its command line has arguments after the flags, which open-ferry, as CLIProxyAPI, ignores.",
    );

    let (mut fake, context) = bare_process();
    fake.file("/proc/5151/cgroup", "0::/system.slice/cron.service\n");
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "the systemd unit cron.service (which runs it through another program)",
    );
    has(
        &ran.out,
        "If not, restart cron.service, and open-ferry starts in its place.",
    );
}

// --- Windows: scheduled tasks ---

const TASK_CPA: &str = r"C:\Users\me\cpa\cli-proxy-api.exe";
const TASK_MOVED: &str = r"C:\Users\me\cpa\cli-proxy-api.exe.cliproxyapi";
const WINDOWS_RECORD: &str = r"C:\Users\me\AppData\Roaming\open-ferry\migration.json";
const TASK_XML: &str = r"C:\Users\me\AppData\Local\Temp\open-ferry-task.xml";

fn task_backup() -> String {
    format!(r"C:\Users\me\cpa\open-ferry-migrate-{STAMP}")
}

/// CLIProxyAPI started at logon by a task that runs a PowerShell script.
fn launcher_task() -> (Fake, Context) {
    let context = windows();
    let mut fake = Fake::new(&context);
    fake.file(TASK_CPA, CLIPROXYAPI);
    fake.file(
        r"C:\Users\me\cpa\config.yaml",
        config_text(r"C:\Users\me\cpa\auths"),
    );
    credentials(&mut fake, r"C:\Users\me\cpa\auths");
    fake.file(
        r"C:\Users\me\cpa\start.ps1",
        "Set-Location $PSScriptRoot\r\n& .\\cli-proxy-api.exe\r\n",
    );
    fake.answer(
        "schtasks.exe /query /xml ONE",
        0,
        &tasks_xml(
            r"\CLIProxyAPI",
            true,
            "powershell.exe",
            r#"-NoProfile -WindowStyle Hidden -File "%USERPROFILE%\cpa\start.ps1""#,
            r"%USERPROFILE%\cpa",
        ),
    );
    fake.process(Proc {
        pid: 6060,
        started: Some(1000),
        name: "cli-proxy-api.exe".to_owned(),
        exe: Some(TASK_CPA.to_owned()),
        args: Vec::new(),
        cwd: Some(r"C:\Users\me\cpa".to_owned()),
        env: None,
        parent: Some(Parent {
            pid: 6000,
            name: None,
        }),
    });
    (fake, context)
}

// Not upstream's: the plan for a task that runs a launcher script, as
// `open-ferry migrate -dry-run` shows it.
#[test]
fn shows_the_plan_for_a_launcher_task() {
    let (mut fake, context) = launcher_task();
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    println!("{}", ran.out);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let out = &ran.out;
    has(
        out,
        r"the scheduled task \CLIProxyAPI (through its launcher start.ps1)",
    );
    has(
        out,
        &format!(
            r"1. Back up the config, its .env files and the auth directory into {}.",
            task_backup()
        ),
    );
    has(
        out,
        r"3. Rename CLIProxyAPI's binary, C:\Users\me\cpa\cli-proxy-api.exe, to C:\Users\me\cpa\cli-proxy-api.exe.cliproxyapi, and copy open-ferry (C:\Users\me\AppData\Local\Programs\open-ferry\open-ferry.exe) to C:\Users\me\cpa\cli-proxy-api.exe, so that the scheduled task \CLIProxyAPI starts open-ferry.",
    );
    has(
        out,
        &format!(
            r"4. Ask you, unless -yes is given, whether to stop CLIProxyAPI (process 6060) now. With your yes, it is ended (Windows ends it at once); if the scheduled task \CLIProxyAPI doesn't start it again within 3 seconds, open-ferry is started in its place with the same command line, in C:\Users\me\cpa, its output going to {}\open-ferry.log. If not, end CLIProxyAPI and run the scheduled task \CLIProxyAPI again, or sign out and in, and open-ferry starts in its place.",
            task_backup()
        ),
    );
    has(out, "Can't read CLIProxyAPI's environment");
    lacks(out, "which only you can open");
    read_only(&fake);
}

// Not upstream's: with a yes to the switch and a no to stopping
// CLIProxyAPI, open-ferry waits in its place; -undo puts CLIProxyAPI's
// binary back and leaves its process be.
#[test]
fn drops_in_for_a_launcher_task_and_back() {
    let (mut fake, context) = launcher_task();
    fake.terminal = true;
    fake.replies.extend([true, false]);
    let ran = migrate(&mut fake, &context, &[]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(
        fake.questions,
        [
            "Switch to open-ferry now? [y/N]",
            r"Stop CLIProxyAPI (process 6060) now and start open-ferry in its place, with the same command line? If not, end CLIProxyAPI and run the scheduled task \CLIProxyAPI again, or sign out and in yourself. [y/N]",
        ]
    );
    has(
        &ran.out,
        r"open-ferry is in place of CLIProxyAPI's binary. CLIProxyAPI (process 6060) runs until you end CLIProxyAPI and run the scheduled task \CLIProxyAPI again, or sign out and in; open-ferry starts in its place then. Check it then with `open-ferry check -config C:\Users\me\cpa\config.yaml`.",
    );
    assert!(fake.procs.contains_key(&6060));
    assert!(!happened(&fake, "stop 6060"));
    assert_eq!(fake.data(TASK_CPA), OPEN_FERRY);
    assert_eq!(fake.data(TASK_MOVED), CLIPROXYAPI);
    let record = saved(&fake, WINDOWS_RECORD);
    assert_eq!(record["platform"], "windows");
    assert_eq!(record["switch"]["restarted"], false);
    assert!(fake.exists(&format!(r"{}\auth\claude.json", task_backup())));

    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(
        &undo.out,
        "CLIProxyAPI (process 6060) still runs, as it did before the switch: open-ferry never ran in its place.",
    );
    has(
        &undo.out,
        "CLIProxyAPI answers on http://127.0.0.1:8317 again.",
    );
    assert!(!happened(&fake, "stop 6060"));
    assert_eq!(fake.data(TASK_CPA), CLIPROXYAPI);
    assert!(!fake.exists(TASK_MOVED));
}

// Not upstream's: with a yes to both, CLIProxyAPI is ended and open-ferry
// started, by migrate or by the task when it starts the binary again.
#[test]
fn drops_in_for_a_launcher_task_now() {
    let (mut fake, context) = launcher_task();
    fake.terminal = true;
    fake.replies.extend([true, true]);
    let ran = migrate(&mut fake, &context, &[]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(&ran.out, "Stopped CLIProxyAPI (process 6060)");
    has(
        &ran.out,
        &format!(
            r"Started open-ferry (process 9001) with CLIProxyAPI's command line; its output goes to {}\open-ferry.log",
            task_backup()
        ),
    );
    assert_eq!(
        fake.launches.first().map(|launch| launch.cwd.as_str()),
        Some(r"C:\Users\me\cpa")
    );

    let (mut fake, context) = launcher_task();
    fake.on("stop 6060", Effect::Launch(TASK_CPA.to_owned()));
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        r"The scheduled task \CLIProxyAPI started open-ferry.",
    );
    assert!(fake.launches.is_empty());
}

// Not upstream's: a launcher task whose CLIProxyAPI isn't running can't be
// switched until it runs.
#[test]
fn a_launcher_task_needs_cliproxyapi_running() {
    let (mut fake, context) = launcher_task();
    fake.procs.clear();
    fake.serving.clear();
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        r"The scheduled task \CLIProxyAPI starts CLIProxyAPI through a launcher, and CLIProxyAPI isn't running: start it, then run `open-ferry migrate` again.",
    );
    lacks(&ran.out, "isn't a service open-ferry can switch");
    read_only(&fake);
}

const BINARY_TASK_CPA: &str = r"C:\Users\me\cpa\cli-proxy-api.exe";

/// CLIProxyAPI run at logon by a task that runs its binary.
fn binary_task(running: bool) -> (Fake, Context) {
    let context = windows();
    let mut fake = Fake::new(&context);
    fake.file(BINARY_TASK_CPA, CLIPROXYAPI);
    fake.file(
        r"C:\Users\me\cpa\config.yaml",
        config_text(r"C:\Users\me\cpa\auths"),
    );
    credentials(&mut fake, r"C:\Users\me\cpa\auths");
    fake.answer(
        "schtasks.exe /query /xml ONE",
        0,
        &tasks_xml(
            r"\CLIProxyAPI",
            true,
            r"%USERPROFILE%\cpa\cli-proxy-api.exe",
            r#"-config "%USERPROFILE%\cpa\config.yaml""#,
            r"%USERPROFILE%\cpa",
        ),
    );
    if running {
        fake.process(Proc {
            pid: 8080,
            started: Some(1000),
            name: "cli-proxy-api.exe".to_owned(),
            exe: Some(BINARY_TASK_CPA.to_owned()),
            args: strings(&["-config", r"C:\Users\me\cpa\config.yaml"]),
            cwd: Some(r"C:\Users\me\cpa".to_owned()),
            env: None,
            parent: Some(Parent {
                pid: 1200,
                name: Some("svchost.exe".to_owned()),
            }),
        });
    }
    fake.on(
        "schtasks.exe /run /tn open-ferry",
        Effect::Launch(context.exe.clone()),
    );
    fake.on(
        "schtasks.exe /end /tn open-ferry",
        Effect::Exit(context.exe.clone()),
    );
    fake.on(
        r"schtasks.exe /run /tn \CLIProxyAPI",
        Effect::Launch(BINARY_TASK_CPA.to_owned()),
    );
    (fake, context)
}

// Not upstream's: a task that runs the binary is disabled and open-ferry's
// own task installed; -undo enables and runs it again.
#[test]
fn switches_a_task_that_runs_the_binary_and_back() {
    let (mut fake, context) = binary_task(true);
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(&ran.out, r"the scheduled task \CLIProxyAPI");
    has(
        &ran.out,
        "The switch (a service switch, to a scheduled task):",
    );
    has(
        &ran.out,
        r"Disable the scheduled task \CLIProxyAPI, keeping it: `schtasks.exe /change /tn \CLIProxyAPI /disable`. Then stop CLIProxyAPI (process 8080), which Windows ends at once.",
    );
    has(&ran.out, "CLIProxyAPI (process 8080) ended");
    has(
        &ran.out,
        "Switched: open-ferry answers on http://127.0.0.1:8317.",
    );
    in_order(
        &fake.events,
        &[
            r"run schtasks.exe /change /tn \CLIProxyAPI /disable",
            "stop 8080",
            &format!("write {TASK_XML}"),
            &format!("run schtasks.exe /create /tn open-ferry /xml {TASK_XML}"),
            "run schtasks.exe /run /tn open-ferry",
            &format!("remove {TASK_XML}"),
        ],
    );
    assert!(!fake.exists(TASK_XML));
    let record = saved(&fake, WINDOWS_RECORD);
    assert_eq!(record["switch"]["ours"], "scheduled-task");
    assert_eq!(record["switch"]["theirs"]["manager"], "task");
    assert_eq!(record["switch"]["theirs"]["name"], r"\CLIProxyAPI");
    assert_eq!(record["switch"]["theirs"]["was_running"], true);

    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    in_order(
        &fake.events,
        &[
            "run schtasks.exe /end /tn open-ferry",
            "run schtasks.exe /delete /tn open-ferry /f",
            r"run schtasks.exe /change /tn \CLIProxyAPI /enable",
            r"run schtasks.exe /run /tn \CLIProxyAPI",
        ],
    );
    has(
        &undo.out,
        "CLIProxyAPI answers on http://127.0.0.1:8317 again.",
    );
}

// Not upstream's: a task that isn't running is read for its command line,
// with its variables expanded.
#[test]
fn reads_a_task_that_is_not_running() {
    let (mut fake, context) = binary_task(false);
    let ran = migrate(&mut fake, &context, &["-json"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_eq!(
        json["summary"],
        r"CLIProxyAPI, not running, set up as the scheduled task \CLIProxyAPI"
    );
    assert_eq!(json["process"], Value::Null);
    assert_eq!(json["exe"], BINARY_TASK_CPA);
    assert_eq!(json["starter"]["kind"], "scheduled-task");
    assert_eq!(json["config"], r"C:\Users\me\cpa\config.yaml");
    assert_eq!(json["config_from"], "its -config");
    assert_eq!(json["working_dir"], r"C:\Users\me\cpa");
    assert_eq!(json["can_switch"], true);
    has(
        &json["warnings"].to_string(),
        r"CLIProxyAPI isn't running: its command line is read from the task \\CLIProxyAPI",
    );
    read_only(&fake);

    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(happened(
        &fake,
        r"run schtasks.exe /change /tn \CLIProxyAPI /enable"
    ));
    assert!(!happened(&fake, r"run schtasks.exe /run /tn \CLIProxyAPI"));
}

/// One scheduled task, as `schtasks /query /xml` prints it.
fn task_block(name: &str, arguments: &str, user: &str) -> String {
    format!(
        "  <!-- {name} -->\r\n  <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n    <RegistrationInfo><URI>{name}</URI></RegistrationInfo>\r\n    <Principals><Principal id=\"Author\"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType></Principal></Principals>\r\n    <Settings><Enabled>true</Enabled></Settings>\r\n    <Actions Context=\"Author\"><Exec><Command>%USERPROFILE%\\cpa\\cli-proxy-api.exe</Command><Arguments>{arguments}</Arguments><WorkingDirectory>%USERPROFILE%\\cpa</WorkingDirectory></Exec></Actions>\r\n  </Task>\r\n"
    )
}

fn tasks_of(blocks: &[String]) -> String {
    format!("<Tasks>\r\n{}</Tasks>\r\n", blocks.concat())
}

const CPA_ARGUMENTS: &str = r#"-config "%USERPROFILE%\cpa\config.yaml""#;

// Not upstream's: two scheduled tasks that run the running CLIProxyAPI the
// same way block the switch, by name, and a task with other arguments is
// not one of them.
#[test]
fn blocks_when_more_than_one_task_starts_cliproxyapi() {
    let (mut fake, context) = binary_task(true);
    fake.answer(
        "schtasks.exe /query /xml ONE",
        0,
        &tasks_of(&[
            task_block(r"\CLIProxyAPI", CPA_ARGUMENTS, "pc\\me"),
            task_block(r"\CLIProxyAPI copy", CPA_ARGUMENTS, "pc\\me"),
            task_block(r"\Other config", r#"-config "D:\other.yaml""#, "pc\\me"),
        ]),
    );
    let ran = migrate(&mut fake, &context, &["-json"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_eq!(json["can_switch"], false);
    let blockers = json["blockers"].to_string();
    has(
        json["blockers"][0].as_str().unwrap(),
        r"More than one scheduled task runs CLIProxyAPI this way (\CLIProxyAPI, \CLIProxyAPI copy)",
    );
    lacks(&blockers, "Other config");
    read_only(&fake);
}

// Not upstream's: a task of another user can't be switched, because
// open-ferry's task would run as the user running migrate; the same user
// by name or by SID can.
#[test]
fn blocks_when_the_task_runs_as_another_user() {
    let (mut fake, context) = binary_task(true);
    fake.answer(
        "schtasks.exe /query /xml ONE",
        0,
        &tasks_of(&[task_block(r"\CLIProxyAPI", CPA_ARGUMENTS, "pc\\svc")]),
    );
    let ran = migrate(&mut fake, &context, &["-json"]);
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_eq!(json["can_switch"], false);
    has(
        &json["blockers"].to_string(),
        r"runs as pc\\svc, and open-ferry's task would run as you (pc\\me)",
    );

    for user in ["pc\\me", "S-1-5-21-1-2-3-1001"] {
        let (mut fake, context) = binary_task(true);
        fake.answer(
            "schtasks.exe /query /xml ONE",
            0,
            &tasks_of(&[task_block(r"\CLIProxyAPI", CPA_ARGUMENTS, user)]),
        );
        let ran = migrate(&mut fake, &context, &["-json"]);
        let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
        assert_eq!(json["can_switch"], true, "{user}: {}", ran.all());
    }
}

// Not upstream's: a parent that isn't NSSM doesn't make the process a
// service's wrapper.
#[test]
fn a_parent_that_is_not_nssm_is_not_a_wrapper() {
    let (mut fake, context) = nssm_service(windows_program_files(), true);
    fake.procs.get_mut(&7070).unwrap().parent = Some(Parent {
        pid: 7000,
        name: Some("runner.exe".to_owned()),
    });
    fake.answer(
        "sc.exe qc CLIProxyAPI",
        0,
        "SERVICE_NAME: CLIProxyAPI\r\n        BINARY_PATH_NAME   : \"C:\\tools\\runner.exe\"\r\n        START_TYPE         : 2   AUTO_START\r\n",
    );
    fake.fail("schtasks.exe /query /xml ONE", 1, "no tasks");
    let ran = migrate(&mut fake, &context, &["-json"]);
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_ne!(json["starter"]["kind"], "windows-service", "{}", ran.all());
}

// Not upstream's: a stopped NSSM service's command line is read from the
// service's NSSM parameters in the registry.
#[test]
fn reads_a_stopped_nssm_service_from_its_parameters() {
    let (mut fake, context) = nssm_service(windows_program_files(), true);
    fake.procs.clear();
    fake.serving.clear();
    fake.answer(
        r"reg.exe query HKLM\SYSTEM\CurrentControlSet\Services\CLIProxyAPI\Parameters",
        0,
        "\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\CLIProxyAPI\\Parameters\r\n    Application    REG_SZ    C:\\Program Files\\CLIProxyAPI\\cli-proxy-api.exe\r\n    AppParameters    REG_SZ    -config \"C:\\Program Files\\CLIProxyAPI\\config.yaml\"\r\n    AppDirectory    REG_SZ    C:\\Program Files\\CLIProxyAPI\r\n",
    );
    let ran = migrate(&mut fake, &context, &["-json"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_eq!(json["starter"]["kind"], "windows-service");
    assert_eq!(json["exe"], NSSM_CPA);
    assert_eq!(json["config"], r"C:\Program Files\CLIProxyAPI\config.yaml");
    assert_eq!(json["working_dir"], r"C:\Program Files\CLIProxyAPI");
    has(
        &json["warnings"].to_string(),
        "the NSSM parameters of the service CLIProxyAPI",
    );
    read_only(&fake);
}

// Not upstream's: a stopped systemd unit whose ExecStart quotes a path with
// a space is read from the unit file, as systemd reads it.
#[test]
fn reads_a_quoted_exec_start_of_a_stopped_unit() {
    let (mut fake, context) = system_unit(linux_root(), false);
    fake.file("/opt/my dir/cli-proxy-api", CLIPROXYAPI);
    fake.file(
        "/etc/systemd/system/cliproxyapi.service",
        "[Service]\nExecStart=\"/opt/my dir/cli-proxy-api\" -config /etc/cliproxyapi/config.yaml\n",
    );
    fake.answer(
        "systemctl show cliproxyapi.service",
        0,
        &shown_unit(
            0,
            "/opt/my dir/cli-proxy-api -config /etc/cliproxyapi/config.yaml",
            "/etc/cliproxyapi",
            false,
            "",
            "",
        )
        .replace("path=/opt/my ", "path=/opt/my dir/cli-proxy-api "),
    );
    let ran = migrate(&mut fake, &context, &["-json"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_eq!(json["exe"], "/opt/my dir/cli-proxy-api");
    assert_eq!(json["config"], "/etc/cliproxyapi/config.yaml");
}

// Not upstream's: a later EnvironmentFiles entry overrides an earlier one,
// and a file that is missing is skipped.
#[test]
fn a_later_environment_file_overrides_an_earlier_one() {
    for key in ["EnvironmentFiles", "EnvironmentFile"] {
        let (mut fake, context) = system_unit(linux_root(), false);
        fake.file("/etc/cliproxyapi/config.yaml", config_text("~/auths"));
        fake.file("/etc/a.env", "HOME=/srv/a\n");
        fake.file("/etc/b.env", "HOME=/srv/b\n");
        let shown = shown_unit(
            0,
            "/usr/local/bin/cli-proxy-api -config /etc/cliproxyapi/config.yaml",
            "/etc/cliproxyapi",
            false,
            "",
            "",
        )
        .replace(
            "EnvironmentFiles=\n",
            &format!(
                "{key}=/etc/a.env (ignore_errors=no)\n{key}=/etc/b.env (ignore_errors=no)\n{key}=/etc/missing.env (ignore_errors=yes)\n"
            ),
        );
        fake.answer("systemctl show cliproxyapi.service", 0, &shown);
        let ran = migrate(&mut fake, &context, &["-json"]);
        assert_eq!(ran.code, 0, "{key}: {}", ran.all());
        let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
        assert_eq!(json["auth_dir"], "/srv/b/auths", "{key}: {}", ran.all());
    }
}

/// A stopped system unit whose files are `fragment` and, when given,
/// `drop_in`, and for which `systemctl show` prints `path` and `argv`.
fn stopped_unit(fragment: &str, drop_in: Option<&str>, path: &str, argv: &str) -> (Fake, Context) {
    let (mut fake, context) = system_unit(linux_root(), false);
    fake.file("/etc/systemd/system/cliproxyapi.service", fragment);
    fake.file("/etc/old.yaml", config_text("/etc/cliproxyapi/auths"));
    fake.file("/etc/cpa new.yaml", config_text("/etc/cliproxyapi/auths"));
    let mut shown = shown_unit(0, "a", "/etc/cliproxyapi", false, "", "").replace(
        "path=a ; argv[]=a ;",
        &format!("path={path} ; argv[]={argv} ;"),
    );
    if let Some(text) = drop_in {
        let dir = "/etc/systemd/system/cliproxyapi.service.d";
        fake.file(&format!("{dir}/override.conf"), text);
        shown.push_str(&format!("DropInPaths={dir}/override.conf\n"));
    }
    fake.answer("systemctl show cliproxyapi.service", 0, &shown);
    (fake, context)
}

fn blockers_of(fake: &mut Fake, context: &Context) -> (Value, String) {
    let ran = migrate(fake, context, &["-json"]);
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap_or_else(|e| {
        panic!("{e}: {}", ran.all());
    });
    // This fake isn't root; that isn't what these tests look at.
    let blockers: Vec<&Value> = json["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|blocker| !blocker.as_str().unwrap_or_default().contains("needs root"))
        .collect();
    let blockers = serde_json::to_string(&blockers).unwrap();
    (json, blockers)
}

// Not upstream's: a drop-in that replaces ExecStart is the command, and a
// quoted path with a space in it is one word.
#[test]
fn a_drop_in_replaces_the_exec_start_of_a_stopped_unit() {
    let (mut fake, context) = stopped_unit(
        "[Service]\nExecStart=/usr/local/bin/cli-proxy-api -config /etc/old.yaml\n",
        Some(
            "[Service]\nExecStart=\nExecStart=/usr/local/bin/cli-proxy-api -config \"/etc/cpa new.yaml\"\n",
        ),
        "/usr/local/bin/cli-proxy-api",
        "/usr/local/bin/cli-proxy-api -config /etc/cpa new.yaml",
    );
    let (json, blockers) = blockers_of(&mut fake, &context);
    assert_eq!(json["config"], "/etc/cpa new.yaml", "{blockers}");
    assert_eq!(blockers, "[]");
}

// Not upstream's: when the files and `systemctl show` give another command
// (here a drop-in `systemctl show` doesn't list), the unit is blocked, and
// its command line is not split on whitespace.
#[test]
fn a_command_line_that_differs_from_systemctl_blocks_a_stopped_unit() {
    let (mut fake, context) = stopped_unit(
        "[Service]\nExecStart=/usr/local/bin/cli-proxy-api -config /etc/old.yaml\n",
        None,
        "/usr/local/bin/cli-proxy-api",
        "/usr/local/bin/cli-proxy-api -config /etc/cpa new.yaml",
    );
    let (json, blockers) = blockers_of(&mut fake, &context);
    has(&blockers, "can't be told reliably");
    assert_ne!(json["config"], "/etc/cpa");
    assert_ne!(json["config"], "/etc/cpa new.yaml");
}

// Not upstream's: after `@`, the program is the first word and argv[0] the
// second.
#[test]
fn an_at_sign_exec_start_keeps_the_program_apart_from_argv0() {
    let (mut fake, context) = stopped_unit(
        "[Service]\nExecStart=@/usr/local/bin/cli-proxy-api custom-argv0 -config /etc/old.yaml\n",
        None,
        "/usr/local/bin/cli-proxy-api",
        "custom-argv0 -config /etc/old.yaml",
    );
    let (json, blockers) = blockers_of(&mut fake, &context);
    assert_eq!(json["exe"], "/usr/local/bin/cli-proxy-api", "{blockers}");
    assert_eq!(json["config"], "/etc/old.yaml");
    assert_eq!(blockers, "[]");
}

// Not upstream's: more than one ExecStart blocks a unit that isn't
// oneshot.
#[test]
fn several_exec_starts_block_a_unit_that_is_not_oneshot() {
    let fragment = "[Service]\nExecStart=/usr/local/bin/cli-proxy-api -config /etc/old.yaml\nExecStart=/bin/true\n";
    let (mut fake, context) = stopped_unit(
        fragment,
        None,
        "/usr/local/bin/cli-proxy-api",
        "/usr/local/bin/cli-proxy-api -config /etc/old.yaml",
    );
    let (_, blockers) = blockers_of(&mut fake, &context);
    has(&blockers, "ExecStart= commands");

    let (mut fake, context) = stopped_unit(
        &format!("{fragment}Type=oneshot\n"),
        None,
        "/usr/local/bin/cli-proxy-api",
        "/usr/local/bin/cli-proxy-api -config /etc/old.yaml",
    );
    let shown = fake.answers["systemctl show cliproxyapi.service"]
        .stdout
        .clone();
    fake.answer(
        "systemctl show cliproxyapi.service",
        0,
        &format!("{shown}Type=oneshot\n"),
    );
    let (_, blockers) = blockers_of(&mut fake, &context);
    assert_eq!(blockers, "[]");
}

// Not upstream's: a value an earlier EnvironmentFile sets and a later one
// overrides doesn't make a remote store; an effective one does.
#[test]
fn only_the_effective_environment_values_make_a_remote_store() {
    let run = |first: &str, second: &str| {
        let (mut fake, context) = system_unit(linux_root(), false);
        fake.file("/etc/a.env", first);
        fake.file("/etc/b.env", second);
        let shown = shown_unit(
            0,
            "/usr/local/bin/cli-proxy-api -config /etc/cliproxyapi/config.yaml",
            "/etc/cliproxyapi",
            false,
            "",
            "",
        )
        .replace(
            "EnvironmentFiles=\n",
            "EnvironmentFiles=/etc/a.env (ignore_errors=no)\nEnvironmentFiles=/etc/b.env (ignore_errors=no)\n",
        );
        fake.answer("systemctl show cliproxyapi.service", 0, &shown);
        blockers_of(&mut fake, &context).1
    };
    // The later file empties what the earlier one set.
    assert_eq!(run("PGSTORE_DSN=postgres://x\n", "PGSTORE_DSN=\n"), "[]");
    // The later file's value is the one in effect.
    has(
        &run("PGSTORE_DSN=\n", "PGSTORE_DSN=postgres://x\n"),
        "PGSTORE_DSN (in /etc/b.env)",
    );
}

/// `binary_task` with its task's XML changed by `change`, and its process
/// as `cwd` says.
fn changed_task(change: impl Fn(String) -> String, cwd: &str) -> (Fake, Context) {
    let (mut fake, context) = binary_task(true);
    let xml = fake.answers["schtasks.exe /query /xml ONE"].stdout.clone();
    fake.answer("schtasks.exe /query /xml ONE", 0, &change(xml));
    if let Some(process) = fake.procs.get_mut(&8080) {
        process.cwd = Some(cwd.to_owned());
    }
    (fake, context)
}

const PRINCIPAL: &str = "<Principals><Principal id=\"Author\"><UserId>S-1-5-21-1-2-3-1001</UserId><LogonType>InteractiveToken</LogonType></Principal></Principals>";

// Not upstream's: a task whose principal is a group, is missing, or has a
// run level or logon type open-ferry's task doesn't have, can't be
// replaced by it.
#[test]
fn a_task_that_runs_otherwise_than_open_ferrys_blocks() {
    let principals = |xml: String, now: &str| xml.replace(PRINCIPAL, now);
    for (now, reason) in [
        (
            "<Principals><Principal id=\"Author\"><GroupId>S-1-5-32-544</GroupId><LogonType>Group</LogonType></Principal></Principals>",
            "runs as the group S-1-5-32-544",
        ),
        ("", "doesn't have the one principal"),
        (
            "<Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType></Principal></Principals>",
            "doesn't say which user",
        ),
        (
            "<Principals><Principal id=\"Author\"><UserId>S-1-5-21-1-2-3-1001</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>",
            "run level HighestAvailable",
        ),
        (
            "<Principals><Principal id=\"Author\"><UserId>S-1-5-21-1-2-3-1001</UserId><LogonType>Password</LogonType></Principal></Principals>",
            "logon type Password",
        ),
        (
            "<Principals><Principal id=\"Author\"><UserId>S-1-5-21-1-2-3-1001</UserId><LogonType>S4U</LogonType></Principal></Principals>",
            "logon type S4U",
        ),
        (
            "<Principals><Principal id=\"Author\"><UserId>S-1-5-21-1-2-3-1001</UserId></Principal></Principals>",
            "no logon type",
        ),
    ] {
        let (mut fake, context) = changed_task(|xml| principals(xml, now), r"C:\Users\me\cpa");
        let (json, blockers) = blockers_of(&mut fake, &context);
        assert_eq!(json["starter"]["kind"], "scheduled-task", "{reason}");
        has(&blockers, reason);
    }
    // The same user, `LeastPrivilege` said outright, passes.
    let (mut fake, context) = changed_task(
        |xml| {
            xml.replace(
                "</LogonType>",
                "</LogonType><RunLevel>LeastPrivilege</RunLevel>",
            )
        },
        r"C:\Users\me\cpa",
    );
    let (json, blockers) = blockers_of(&mut fake, &context);
    assert_eq!(json["starter"]["kind"], "scheduled-task");
    assert_eq!(blockers, "[]");
}

// Not upstream's: a task with no working directory runs in System32, so it
// isn't the task of a process started by hand in another directory.
#[test]
fn a_task_with_no_working_directory_runs_in_system32() {
    let without = |xml: String| {
        xml.replace(
            "<WorkingDirectory>%USERPROFILE%\\cpa</WorkingDirectory>",
            "",
        )
    };
    let (mut fake, context) = changed_task(without, r"C:\Users\me\cpa");
    let (json, _) = blockers_of(&mut fake, &context);
    assert_ne!(json["starter"]["kind"], "scheduled-task");

    let (mut fake, context) = changed_task(without, r"C:\Windows\System32");
    let (json, _) = blockers_of(&mut fake, &context);
    assert_eq!(json["starter"]["kind"], "scheduled-task");
}

// --- Fix round 4: discovery ---

/// `stopped_unit`'s plan, as printed.
fn plan_of(fake: &mut Fake, context: &Context) -> String {
    migrate(fake, context, &["-dry-run"]).all()
}

// Not upstream's: a systemd blocker names the unit's file and the kind of
// trouble, but shows no argument and no raw command, in the plan or in
// -json.
#[test]
fn a_systemd_blocker_shows_no_argument_values() {
    let secret = "hunter2-secret";
    let program = "/usr/local/bin/cli-proxy-api";
    for (fragment, argv, what) in [
        // The files and `systemctl show` disagree.
        (
            format!("[Service]\nExecStart={program} -config /etc/old.yaml -password {secret}\n"),
            format!("{program} -config /etc/old.yaml"),
            "don't agree",
        ),
        // The line can't be split.
        (
            format!("[Service]\nExecStart={program} -config /etc/old.yaml -password \"{secret}\n"),
            format!("{program} -config /etc/old.yaml"),
            "can't be split",
        ),
        // A variable.
        (
            format!("[Service]\nExecStart={program} -config /etc/old.yaml -password {secret}$X\n"),
            format!("{program} -config /etc/old.yaml -password {secret}"),
            "uses a $ variable",
        ),
    ] {
        let (mut fake, context) = stopped_unit(&fragment, None, program, &argv);
        let (json, blockers) = blockers_of(&mut fake, &context);
        has(&blockers, what);
        has(&blockers, "/etc/systemd/system/cliproxyapi.service");
        assert!(!json.to_string().contains(secret), "{what}: {json}");
        let plan = plan_of(&mut fake, &context);
        has(&plan, what);
        lacks(&plan, secret);
    }
}

// Not upstream's: a unit whose files changed since systemd loaded them is
// not what systemd runs.
#[test]
fn a_unit_that_needs_a_daemon_reload_blocks() {
    let program = "/usr/local/bin/cli-proxy-api";
    let argv = format!("{program} -config /etc/old.yaml");
    let (mut fake, context) = stopped_unit(
        &format!("[Service]\nExecStart={argv}\n"),
        None,
        program,
        &argv,
    );
    let shown = fake.answers["systemctl show cliproxyapi.service"]
        .stdout
        .clone();
    let (_, blockers) = blockers_of(&mut fake, &context);
    assert_eq!(blockers, "[]");
    for (state, blocks) in [("no", false), ("yes", true)] {
        fake.answer(
            "systemctl show cliproxyapi.service",
            0,
            &format!("{shown}NeedDaemonReload={state}\n"),
        );
        let (_, blockers) = blockers_of(&mut fake, &context);
        if blocks {
            has(&blockers, "changed since systemd loaded them");
        } else {
            assert_eq!(blockers, "[]");
        }
    }
}

// Not upstream's: a `$` variable or a `%` specifier in ExecStart is
// expanded by systemd and not by migrate, so the unit is blocked; `$$` and
// `%%` are literal.
#[test]
fn a_variable_or_specifier_in_exec_start_blocks() {
    let program = "/usr/local/bin/cli-proxy-api";
    for word in ["$CONFIG", "${CONFIG}", "%h/cpa.yaml", "%n", "a$", "x%"] {
        let argv = format!("{program} -config {word}");
        let (mut fake, context) = stopped_unit(
            &format!("[Service]\nExecStart={argv}\n"),
            None,
            program,
            &argv,
        );
        let (_, blockers) = blockers_of(&mut fake, &context);
        has(&blockers, "uses a $ variable or a % specifier");
    }
    for word in ["a$$b", "100%%"] {
        let argv = format!("{program} -config {word}");
        let (mut fake, context) = stopped_unit(
            &format!("[Service]\nExecStart={argv}\n"),
            None,
            program,
            &argv,
        );
        let (_, blockers) = blockers_of(&mut fake, &context);
        lacks(&blockers, "uses a $ variable or a % specifier");
    }
}

// Not upstream's: a process whose directory can't be read doesn't make a
// task with the same command its starter.
#[test]
fn an_unreadable_process_directory_blocks_a_matching_task() {
    let (mut fake, context) = binary_task(true);
    if let Some(process) = fake.procs.get_mut(&8080) {
        process.cwd = None;
    }
    let (json, blockers) = blockers_of(&mut fake, &context);
    assert_eq!(json["starter"]["kind"], "scheduled-task");
    has(&blockers, "working directory can't be read");
    has(&blockers, "switch by hand");
}

/// A Windows context that knows `windir`, which `windows()` doesn't.
fn windows_with_windir() -> Context {
    let mut context = windows();
    context
        .env
        .insert("windir".to_owned(), r"C:\Windows".to_owned());
    context
}

// Not upstream's: `%windir%` and the other variables a task commonly uses
// are known, so a stopped task's directory is found, not dropped.
#[test]
fn a_stopped_task_with_a_windir_directory_keeps_it() {
    let (mut fake, _) = changed_task(
        |xml| {
            xml.replace(
                r"%USERPROFILE%\cpa</WorkingDirectory>",
                r"%windir%\System32</WorkingDirectory>",
            )
        },
        r"C:\Users\me\cpa",
    );
    fake.procs.remove(&8080);
    let context = windows_with_windir();
    let (json, blockers) = blockers_of(&mut fake, &context);
    assert_eq!(json["working_dir"], r"C:\Windows\System32", "{blockers}");
    assert_eq!(blockers, "[]");
}

// Not upstream's: a variable that is still unknown in a task's directory,
// command or arguments blocks, whether CLIProxyAPI runs or not.
#[test]
fn an_unknown_variable_in_a_task_blocks() {
    let cases: [(&str, &str, &str); 3] = [
        (
            r"%USERPROFILE%\cpa</WorkingDirectory>",
            r"%NOSUCH%\cpa</WorkingDirectory>",
            "working directory",
        ),
        (
            r"<Command>%USERPROFILE%\cpa\cli-proxy-api.exe",
            r"<Command>%USERPROFILE%\%NOSUCH%\cli-proxy-api.exe",
            "command",
        ),
        (
            r#"-config "%USERPROFILE%\cpa\config.yaml""#,
            r#"-config "%NOSUCH%\config.yaml""#,
            "arguments",
        ),
    ];
    for running in [false, true] {
        for (from, to, part) in cases {
            let (mut fake, context) = binary_task(running);
            let xml = fake.answers["schtasks.exe /query /xml ONE"].stdout.clone();
            assert!(xml.contains(from), "{part}");
            fake.answer("schtasks.exe /query /xml ONE", 0, &xml.replace(from, to));
            let (_, blockers) = blockers_of(&mut fake, &context);
            // A blocker starts a sentence, so its first letter is a capital.
            has(
                &blockers.to_lowercase(),
                &format!("{part} of the scheduled task"),
            );
            has(&blockers, "uses a variable that migrate doesn't know");
        }
    }
}

// --- Windows: a service through NSSM ---

const NSSM_CPA: &str = r"C:\Program Files\CLIProxyAPI\cli-proxy-api.exe";

fn nssm_service(context: Context, admin: bool) -> (Fake, Context) {
    let mut fake = Fake::new(&context);
    if admin {
        fake.answer("whoami.exe /groups /fo csv /nh", 0, ADMIN_GROUPS);
    }
    fake.file(NSSM_CPA, CLIPROXYAPI);
    fake.file(
        r"C:\Program Files\CLIProxyAPI\config.yaml",
        config_text(r"C:\Program Files\CLIProxyAPI\auths"),
    );
    credentials(&mut fake, r"C:\Program Files\CLIProxyAPI\auths");
    fake.process(Proc {
        pid: 7070,
        started: Some(1000),
        name: "cli-proxy-api.exe".to_owned(),
        exe: Some(NSSM_CPA.to_owned()),
        args: strings(&["-config", r"C:\Program Files\CLIProxyAPI\config.yaml"]),
        cwd: Some(r"C:\Program Files\CLIProxyAPI".to_owned()),
        env: None,
        parent: Some(Parent {
            pid: 7000,
            name: Some("nssm.exe".to_owned()),
        }),
    });
    fake.answer(
        r#"tasklist.exe /svc /fo csv /nh /fi "PID eq 7000""#,
        0,
        "\"nssm.exe\",\"7000\",\"CLIProxyAPI\"\r\n",
    );
    fake.answer(
        "sc.exe qc CLIProxyAPI",
        0,
        "[SC] QueryServiceConfig SUCCESS\r\n\r\nSERVICE_NAME: CLIProxyAPI\r\n        TYPE               : 10  WIN32_OWN_PROCESS\r\n        START_TYPE         : 2   AUTO_START\r\n        ERROR_CONTROL      : 1   NORMAL\r\n        BINARY_PATH_NAME   : \"C:\\Program Files\\nssm\\nssm.exe\"\r\n        DISPLAY_NAME       : CLIProxyAPI\r\n",
    );
    fake.on("sc.exe stop CLIProxyAPI", Effect::Exit(NSSM_CPA.to_owned()));
    fake.on(
        "sc.exe start CLIProxyAPI",
        Effect::Launch(NSSM_CPA.to_owned()),
    );
    fake.on(
        "sc.exe start open-ferry",
        Effect::Launch(context.exe.clone()),
    );
    fake.on("sc.exe stop open-ferry", Effect::Exit(context.exe.clone()));
    (fake, context)
}

// Not upstream's: a Windows service run through NSSM is switched to
// open-ferry's own service, by an administrator.
#[test]
fn switches_a_windows_service_and_back() {
    let (mut fake, context) = nssm_service(windows_program_files(), true);
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "the Windows service CLIProxyAPI (through nssm.exe)",
    );
    has(
        &ran.out,
        "The switch (a service switch, to a Windows service):",
    );
    has(
        &ran.out,
        "Stop and disable the Windows service CLIProxyAPI (through nssm.exe), keeping its definition: `sc.exe stop CLIProxyAPI`, `sc.exe config CLIProxyAPI start= disabled`.",
    );
    in_order(
        &fake.events,
        &[
            "run sc.exe stop CLIProxyAPI",
            "run sc.exe config CLIProxyAPI start= disabled",
            "run sc.exe start open-ferry",
        ],
    );
    assert!(
        fake.events
            .iter()
            .any(|event| event.starts_with("run sc.exe create open-ferry "))
    );
    let record = saved(&fake, WINDOWS_RECORD);
    assert_eq!(record["switch"]["ours"], "windows-service");
    assert_eq!(record["switch"]["theirs"]["manager"], "windows-service");
    assert_eq!(record["switch"]["theirs"]["start_type"], "auto");
    assert!(fake.private.contains(&format!(
        r"C:\Program Files\CLIProxyAPI\open-ferry-migrate-{STAMP}"
    )));

    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    in_order(
        &fake.events,
        &[
            "run sc.exe stop open-ferry",
            "run sc.exe delete open-ferry",
            "run sc.exe config CLIProxyAPI start= auto",
            "run sc.exe start CLIProxyAPI",
        ],
    );
    has(
        &undo.out,
        "CLIProxyAPI answers on http://127.0.0.1:8317 again.",
    );
}

// Not upstream's: a Windows service needs an administrator, and an
// open-ferry where only administrators can change it.
#[test]
fn a_windows_service_needs_an_administrator() {
    let (mut fake, context) = nssm_service(windows_program_files(), false);
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        r#"Switching a Windows service needs an administrator. Run this in a terminal opened with "Run as administrator": "C:\Program Files\open-ferry\open-ferry.exe" migrate."#,
    );
    read_only(&fake);

    let (mut fake, context) = nssm_service(windows(), true);
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        r"A Windows service runs as LocalSystem, so open-ferry's binary and its config must be where only administrators can change them, such as C:\Program Files; C:\Users\me\AppData\Local\Programs\open-ferry\open-ferry.exe isn't.",
    );
    read_only(&fake);
}

// --- macOS ---

const BREW_CPA: &str = "/opt/homebrew/Cellar/cliproxyapi/8.0.20/bin/cliproxyapi";
const BREW_PLIST: &str = "/Users/me/Library/LaunchAgents/homebrew.mxcl.cliproxyapi.plist";
const AGENT_PLIST: &str =
    "/Users/me/Library/LaunchAgents/io.github.loft-902-co-llc.open-ferry.plist";

/// CLIProxyAPI run by `brew services`, from the formula's plist.
fn brew_services() -> (Fake, Context) {
    let context = macos();
    let mut fake = Fake::new(&context);
    fake.file(BREW_CPA, CLIPROXYAPI);
    fake.file(
        "/opt/homebrew/etc/cliproxyapi.conf",
        config_text("~/.cli-proxy-api"),
    );
    credentials(&mut fake, "/Users/me/.cli-proxy-api");
    fake.file(
        BREW_PLIST,
        plist(
            "homebrew.mxcl.cliproxyapi",
            &["/opt/homebrew/opt/cliproxyapi/bin/cliproxyapi"],
            None,
            &[],
        ),
    );
    fake.process(Proc {
        pid: 3131,
        started: Some(1000),
        name: "cliproxyapi".to_owned(),
        exe: Some(BREW_CPA.to_owned()),
        args: Vec::new(),
        cwd: Some("/".to_owned()),
        env: Some(vars(&[("HOME", "/Users/me")])),
        parent: Some(Parent {
            pid: 1,
            name: Some("launchd".to_owned()),
        }),
    });
    fake.answer(
        "launchctl print gui/501/homebrew.mxcl.cliproxyapi",
        0,
        "gui/501/homebrew.mxcl.cliproxyapi = {\n\tactive count = 1\n\tstate = running\n\tpid = 3131\n}\n",
    );
    fake.on(
        "launchctl bootout gui/501/homebrew.mxcl.cliproxyapi",
        Effect::Exit(BREW_CPA.to_owned()),
    );
    fake.on(
        &format!("launchctl bootstrap gui/501 {BREW_PLIST}"),
        Effect::Launch(BREW_CPA.to_owned()),
    );
    fake.on(
        &format!("launchctl bootstrap gui/501 {AGENT_PLIST}"),
        Effect::Launch(context.exe.clone()),
    );
    fake.on(
        "launchctl bootout gui/501/io.github.loft-902-co-llc.open-ferry",
        Effect::Exit(context.exe.clone()),
    );
    (fake, context)
}

// Not upstream's: `brew services`' job is booted out and disabled, and
// open-ferry's launch agent loaded; -undo loads Homebrew's again.
#[test]
fn switches_brew_services_and_back() {
    let (mut fake, context) = brew_services();
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "Homebrew's `brew services` (the launchd job homebrew.mxcl.cliproxyapi, /Users/me/Library/LaunchAgents/homebrew.mxcl.cliproxyapi.plist)",
    );
    has(
        &ran.out,
        "/opt/homebrew/etc/cliproxyapi.conf (the Homebrew build's default)",
    );
    has(
        &ran.out,
        "The switch (a service switch, to a launchd agent):",
    );
    has(
        &ran.out,
        "`brew services start cliproxyapi` and `brew services restart cliproxyapi` load CLIProxyAPI's job again",
    );
    in_order(
        &fake.events,
        &[
            &format!("mkdir /opt/homebrew/etc/open-ferry-migrate-{STAMP}"),
            "run launchctl bootout gui/501/homebrew.mxcl.cliproxyapi",
            "run launchctl disable gui/501/homebrew.mxcl.cliproxyapi",
            &format!("write {AGENT_PLIST}"),
            "run launchctl enable gui/501/io.github.loft-902-co-llc.open-ferry",
            &format!("run launchctl bootstrap gui/501 {AGENT_PLIST}"),
        ],
    );
    has(
        &ran.out,
        "Switched: open-ferry answers on http://127.0.0.1:8317.",
    );
    let record = saved(&fake, "/Users/me/.config/open-ferry/migration.json");
    assert_eq!(record["platform"], "macos");
    assert_eq!(record["switch"]["ours"], "launch-agent");
    assert_eq!(record["switch"]["theirs"]["manager"], "launchd");
    assert_eq!(
        record["switch"]["theirs"]["label"],
        "homebrew.mxcl.cliproxyapi"
    );
    assert_eq!(record["switch"]["theirs"]["domain"], "gui/501");

    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    in_order(
        &fake.events,
        &[
            "run launchctl bootout gui/501/io.github.loft-902-co-llc.open-ferry",
            &format!("remove {AGENT_PLIST}"),
            "run launchctl enable gui/501/homebrew.mxcl.cliproxyapi",
            &format!("run launchctl bootstrap gui/501 {BREW_PLIST}"),
        ],
    );
    has(
        &undo.out,
        "CLIProxyAPI answers on http://127.0.0.1:8317 again.",
    );
    assert!(!fake.exists(AGENT_PLIST));
}

// Not upstream's: a launch daemon that isn't loaded is read from its plist,
// disabled, and only enabled again by -undo.
#[test]
fn switches_a_stopped_launch_daemon_and_back() {
    let context = macos_root();
    let mut fake = Fake::new(&context);
    fake.answer("id -u", 0, "0\n");
    fake.file("/usr/local/bin/cliproxyapi", CLIPROXYAPI);
    fake.file(
        "/usr/local/etc/cliproxyapi/config.yaml",
        config_text("/usr/local/etc/cliproxyapi/auths"),
    );
    credentials(&mut fake, "/usr/local/etc/cliproxyapi/auths");
    fake.file(
        "/Library/LaunchDaemons/com.example.other.plist",
        plist("com.example.other", &["/usr/local/bin/other"], None, &[]),
    );
    fake.file(
        "/Library/LaunchDaemons/com.example.cliproxyapi.plist",
        plist(
            "com.example.cliproxyapi",
            &[
                "/usr/local/bin/cliproxyapi",
                "-config",
                "/usr/local/etc/cliproxyapi/config.yaml",
            ],
            Some("/usr/local/etc/cliproxyapi"),
            &[("DEPLOY", "local")],
        ),
    );
    let daemon = "/Library/LaunchDaemons/io.github.loft-902-co-llc.open-ferry.plist";
    fake.on(
        &format!("launchctl bootstrap system {daemon}"),
        Effect::Launch(context.exe.clone()),
    );
    fake.on(
        "launchctl bootout system/io.github.loft-902-co-llc.open-ferry",
        Effect::Exit(context.exe.clone()),
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "the launchd job com.example.cliproxyapi (/Library/LaunchDaemons/com.example.cliproxyapi.plist)",
    );
    has(
        &ran.out,
        "not running (its binary: /usr/local/bin/cliproxyapi)",
    );
    has(
        &ran.out,
        "The switch (a service switch, to a launchd daemon):",
    );
    has(&ran.out, "The variables its service sets (DEPLOY)");
    in_order(
        &fake.events,
        &[
            "run launchctl disable system/com.example.cliproxyapi",
            &format!("write {daemon}"),
            &format!("run launchctl bootstrap system {daemon}"),
        ],
    );
    assert!(!happened(
        &fake,
        "run launchctl bootout system/com.example.cliproxyapi"
    ));

    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(happened(
        &fake,
        "run launchctl enable system/com.example.cliproxyapi"
    ));
    assert!(!happened(
        &fake,
        "run launchctl bootstrap system /Library/LaunchDaemons/com.example.cliproxyapi.plist"
    ));
    assert!(!fake.exists(daemon));
}

// --- Containers ---

const DOCKER_PS: &str = r#"{"Command":"\"./CLIProxyAPI\"","Image":"eceasy/cli-proxy-api:latest","Labels":"com.docker.compose.project=cpa,com.docker.compose.service=cli-proxy-api,com.docker.compose.project.working_dir=/home/me/cpa,com.docker.compose.project.config_files=/home/me/cpa/docker-compose.yml","Names":"cli-proxy-api","Ports":"0.0.0.0:8317->8317/tcp"}"#;

// Not upstream's: a container is never changed: its steps are shown.
#[test]
fn shows_the_steps_for_a_container() {
    let context = linux();
    let mut fake = Fake::new(&context);
    fake.process(Proc {
        pid: 2020,
        started: Some(1000),
        name: "CLIProxyAPI".to_owned(),
        exe: Some("/CLIProxyAPI/CLIProxyAPI".to_owned()),
        args: Vec::new(),
        cwd: Some("/CLIProxyAPI".to_owned()),
        env: None,
        parent: Some(Parent {
            pid: 2000,
            name: Some("containerd-shim-runc-v2".to_owned()),
        }),
    });
    fake.file(
        "/proc/2020/cgroup",
        "0::/system.slice/docker-0123abcd.scope\n",
    );
    fake.answer(
        "docker ps --no-trunc --format \"{{json .}}\"",
        0,
        &format!("{DOCKER_PS}\n"),
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "a container (cli-proxy-api, from eceasy/cli-proxy-api:latest)",
    );
    has(
        &ran.out,
        "The switch (a container: the steps to take yourself):",
    );
    has(
        &ran.out,
        "1. In /home/me/cpa/docker-compose.yml, for the service cli-proxy-api, change the image to ghcr.io/loft-902-co-llc/open-ferry:latest, or to a version.",
    );
    has(&ran.out, "Run `docker compose up -d` in /home/me/cpa.");
    has(
        &ran.out,
        "open-ferry doesn't change containers or Compose files: make the changes above yourself.",
    );
    read_only(&fake);

    let ran = migrate(&mut fake, &context, &["-json"]);
    let json: Value = serde_json::from_str(ran.out.trim()).unwrap();
    assert_eq!(json["switch"], "container");
    assert_eq!(json["can_switch"], false);
    assert_eq!(json["starter"]["kind"], "container");

    // Not running on the machine, only in Docker's.
    let context = macos();
    let mut fake = Fake::new(&context);
    fake.answer(
        "docker ps --no-trunc --format \"{{json .}}\"",
        0,
        &format!("{DOCKER_PS}\n"),
    );
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(&ran.out, "not running");
    has(
        &ran.out,
        "The switch (a container: the steps to take yourself):",
    );
    read_only(&fake);
}

// --- Nothing found ---

// Not upstream's: with nothing running or set up, migrate says so on each
// platform, and changes nothing.
#[test]
fn finds_nothing() {
    for context in [linux(), macos(), windows()] {
        let mut fake = Fake::new(&context);
        let ran = migrate(&mut fake, &context, &["-yes"]);
        assert_eq!(ran.code, 1, "{}", ran.all());
        has(
            &ran.out,
            "CLIProxyAPI wasn't found: it isn't running, and no service, scheduled task or container of it is set up. Nothing was changed.",
        );
        read_only(&fake);
        let ran = migrate(&mut fake, &context, &["-json"]);
        assert_eq!(ran.code, 1);
        assert_eq!(ran.out, "{\"found\":false}\n");
    }
}

// --- Usage ---

// Not upstream's: the command line, read as the server's flags are.
#[test]
fn reads_its_command_line() {
    let parsed = |args: &[&str]| parse(args.iter().map(|arg| (*arg).to_owned()));
    assert_eq!(
        parsed(&["-config", "c.yaml", "--yes", "-json"]),
        Ok(Request {
            config: Some("c.yaml".to_owned()),
            yes: true,
            dry_run: true,
            json: true,
            undo: false,
            restore: false,
        })
    );
    assert_eq!(
        parsed(&["-undo", "-restore", "-dry-run"]),
        Ok(Request {
            dry_run: true,
            undo: true,
            restore: true,
            ..Request::default()
        })
    );
    let invalid = |args: &[&str]| match parsed(args) {
        Err(FlagError::Invalid(message)) => message,
        other => panic!("{args:?}: {other:?}"),
    };
    assert_eq!(invalid(&["now"]), "unexpected argument: now");
    assert_eq!(
        invalid(&["-bogus"]),
        "flag provided but not defined: -bogus"
    );
    assert_eq!(invalid(&["-restore"]), "-restore goes with -undo");
    assert_eq!(invalid(&["-undo", "-json"]), "-json doesn't go with -undo");
    assert_eq!(
        invalid(&["-undo", "-config", "c.yaml"]),
        "-config doesn't go with -undo: the switch's record says which config"
    );
    assert_eq!(parsed(&["-h"]), Err(FlagError::Help));
    let usage = usage("open-ferry");
    has(
        &usage,
        "Usage: open-ferry migrate [-config PATH] [-yes] [-dry-run] [-json]",
    );
    has(
        &usage,
        "open-ferry migrate -undo [-restore] [-yes] [-dry-run]",
    );
    has(&usage, "See docs/migrating-from-cliproxyapi.md.");
}

// Not upstream's: steps and blockers are shown as sentences.
#[test]
fn makes_sentences() {
    assert_eq!(sentence(""), "");
    assert_eq!(sentence(" its config "), "Its config.");
    assert_eq!(sentence("done!"), "Done!");
    assert_eq!(sentence("Already one."), "Already one.");
    assert_eq!(sentence("open-ferry runs"), "open-ferry runs.");
}

// Not upstream's: the backup goes beside the config, or beside the auth
// directory when the config is in it.
#[test]
fn places_the_backup() {
    let now = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
    assert_eq!(
        backup_dir(
            Platform::Linux,
            "/home/me/cpa/config.yaml",
            Some("/home/me/.cli-proxy-api"),
            now
        ),
        Some(format!("/home/me/cpa/open-ferry-migrate-{STAMP}"))
    );
    assert_eq!(
        backup_dir(
            Platform::Linux,
            "/srv/cpa/auths/conf/config.yaml",
            Some("/srv/cpa/auths"),
            now
        ),
        Some(format!("/srv/cpa/open-ferry-migrate-{STAMP}"))
    );
    assert_eq!(
        backup_dir(Platform::Windows, r"C:\cpa\config.yaml", None, now),
        Some(format!(r"C:\cpa\open-ferry-migrate-{STAMP}"))
    );
}

// --- The fix round: backups, links, records, processes and retries ---

/// `systemd_user()` with the config at `config`.
fn systemd_user_config(config: &str) -> (Fake, Context) {
    let (mut fake, context) = systemd_user();
    let text = fake.files.remove(SYSTEMD_CONFIG).unwrap();
    fake.file(config, text);
    if let Some(process) = fake.procs.get_mut(&4242) {
        process.args = strings(&["-config", config]);
    }
    fake.answer(
        "systemctl --user show cliproxyapi.service",
        0,
        &shown_unit(
            4242,
            &format!("/home/me/cpa/cli-proxy-api -config {config}"),
            "/home/me/cpa",
            true,
            "",
            "",
        ),
    );
    (fake, context)
}

// Not upstream's: a config named like a file the backup keeps is backed up
// under config/, so it takes nothing's place, and -undo -restore puts it
// back.
#[test]
fn backs_up_a_config_named_like_the_record() {
    for name in ["migration.json", "auth", "working-dir.env"] {
        let config = format!("/home/me/cpa/{name}");
        let (mut fake, context) = systemd_user_config(&config);
        let ran = migrate(&mut fake, &context, &["-yes"]);
        assert_eq!(ran.code, 0, "{name}: {}", ran.all());
        let backup = systemd_backup();
        let text = config_text("~/.cli-proxy-api");
        assert_eq!(
            fake.data(&format!("{backup}/config/{name}")),
            text.as_bytes()
        );
        // The record's copy and the credentials are where they belong.
        assert_eq!(
            saved(&fake, &format!("{backup}/migration.json"))["status"],
            "switched"
        );
        assert!(fake.exists(&format!("{backup}/auth/claude.json")));

        fake.file(&config, "changed: by open-ferry\n");
        let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
        assert_eq!(undo.code, 0, "{name}: {}", undo.all());
        has(
            &undo.out,
            &format!("Restored {config} from {backup}/config/{name}"),
        );
        assert_eq!(fake.data(&config), text.as_bytes());
    }
}

// Not upstream's: -restore replaces a symbolic link where a config was,
// and says so, and leaves what the link led to as it was.
#[test]
fn restore_replaces_a_symbolic_link_at_a_file() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.files.remove(SYSTEMD_CONFIG);
    fake.links.insert(
        SYSTEMD_CONFIG.to_owned(),
        "/home/me/elsewhere.yaml".to_owned(),
    );
    fake.file("/home/me/elsewhere.yaml", "somebody else's file\n");
    // A link where the config was a file at the switch leads somewhere else
    // now: nothing is written there, and it is said.
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.out,
        &format!(
            "{SYSTEMD_CONFIG} wasn't restored: it led to {SYSTEMD_CONFIG} at the switch and now leads to /home/me/elsewhere.yaml, so the write would change another place."
        ),
    );
    assert_eq!(
        fake.link_target(SYSTEMD_CONFIG).as_deref(),
        Some("/home/me/elsewhere.yaml")
    );
    assert_eq!(
        fake.data("/home/me/elsewhere.yaml"),
        b"somebody else's file\n"
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");

    // A record without the places noted is not written through: the file
    // is skipped and said, with where to copy it from.
    let mut record = saved(&fake, LINUX_RECORD);
    for file in record["backup"]["files"].as_array_mut().unwrap() {
        let file = file.as_object_mut().unwrap();
        file.remove("real");
        file.remove("parent");
    }
    fake.file(LINUX_RECORD, record.to_string());
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.out,
        &format!(
            "{SYSTEMD_CONFIG} wasn't restored: the record doesn't say where it and its directory really were at the switch, so the write could change another place. Copy the file by hand from "
        ),
    );
    assert_eq!(
        fake.link_target(SYSTEMD_CONFIG).as_deref(),
        Some("/home/me/elsewhere.yaml")
    );
    assert_eq!(
        fake.data("/home/me/elsewhere.yaml"),
        b"somebody else's file\n"
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
}

// Not upstream's: a symbolic link where the auth directory was is not
// written through. -undo says so, still switches back, and keeps the record
// open; with the link gone, running it again finishes.
#[test]
fn restore_refuses_a_linked_auth_directory_and_can_be_run_again() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.links.insert(
        "/home/me/.cli-proxy-api".to_owned(),
        "/home/me/other-auth".to_owned(),
    );
    fake.dir("/home/me/other-auth");
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.out,
        "/home/me/.cli-proxy-api wasn't restored: it led to /home/me/.cli-proxy-api at the switch and now leads to /home/me/other-auth, so the write would change another place.",
    );
    has(&undo.err, "Switching back failed at: ");
    has(
        &undo.err,
        "The record stays open: run `open-ferry migrate -undo` again",
    );
    // CLIProxyAPI's service is not turned on over files that weren't put back.
    assert!(!happened(
        &fake,
        "run systemctl --user start cliproxyapi.service"
    ));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
    assert!(!fake.exists("/home/me/other-auth/claude.json"));

    fake.links.remove("/home/me/.cli-proxy-api");
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    // On the retry: restore, then start.
    assert!(happened(
        &fake,
        "run systemctl --user start cliproxyapi.service"
    ));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");
}

// Not upstream's: a rollback step that fails leaves the record open, as
// switching, and says what is left; -undo finishes it once the step works.
#[test]
fn a_rollback_that_fails_keeps_the_record_open() {
    let (mut fake, context) = systemd_user();
    fake.broken = true;
    fake.fail("systemctl --user enable cliproxyapi.service", 1, "boom");
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.err,
        "migrate: The switch is undone as far as it could be: open-ferry didn't answer: ",
    );
    has(&ran.err, "Undoing it failed at: ");
    has(
        &ran.err,
        "The record stays open: run `open-ferry migrate -undo`",
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switching");
    assert_eq!(
        saved(&fake, &format!("{}/migration.json", systemd_backup()))["status"],
        "switching"
    );

    fake.answers
        .remove("systemctl --user enable cliproxyapi.service");
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(&undo.out, "open-ferry isn't installed as ");
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");

    // And the same for a step of a service that doesn't stop.
    let (mut fake, context) = systemd_user();
    fake.fail(
        "systemctl --user stop cliproxyapi.service",
        1,
        "Access denied",
    );
    fake.fail("systemctl --user start cliproxyapi.service", 1, "no");
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switching");
}

// Not upstream's: the backup is placed by where the config and the auth
// directory really are: with the config in the real directory a link leads
// to, it goes beside that directory, not into it, and the copy of the auth
// directory is made from its real path, once.
#[test]
fn a_backup_is_placed_by_where_a_linked_auth_directory_really_is() {
    let (mut fake, context) = systemd_user_config("/data/auths/config.yaml");
    fake.files
        .retain(|path, _| !path.starts_with("/home/me/.cli-proxy-api/"));
    fake.dirs.remove("/home/me/.cli-proxy-api");
    credentials(&mut fake, "/data/auths");
    fake.links.insert(
        "/home/me/.cli-proxy-api".to_owned(),
        "/data/auths".to_owned(),
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    let backup = format!("/data/open-ferry-migrate-{STAMP}");
    has(&ran.out, &format!("into {backup}"));
    assert!(fake.exists(&format!("{backup}/auth/claude.json")));
    assert!(fake.exists(&format!("{backup}/config/config.yaml")));
    assert!(
        !fake
            .files
            .keys()
            .chain(fake.dirs.iter())
            .any(|path| path.starts_with("/data/auths/open-ferry-migrate")),
        "the backup is inside the auth directory:\n{:#?}",
        fake.files.keys().collect::<Vec<_>>()
    );
    let record = saved(&fake, LINUX_RECORD);
    assert_eq!(record["backup"]["files"][1]["from"], "/data/auths");
}

// Not upstream's: a record directory that is a link into the auth directory
// would put the record among the credentials, so migrate refuses before it
// changes anything.
#[test]
fn a_record_directory_linked_into_the_auth_directory_blocks() {
    let (mut fake, context) = systemd_user();
    fake.links.insert(
        "/home/me/.config/open-ferry".to_owned(),
        "/home/me/.cli-proxy-api".to_owned(),
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(&ran.all(), "The auth directory (/home/me/.cli-proxy-api)");
    has(&ran.all(), "Nothing was changed.");
    assert!(fake.procs.contains_key(&4242));
    assert!(
        !fake
            .files
            .keys()
            .any(|path| path.ends_with("migration.json")),
        "{:#?}",
        fake.files.keys().collect::<Vec<_>>()
    );
    assert!(!happened(
        &fake,
        "run systemctl --user stop cliproxyapi.service"
    ));
}

// Not upstream's: the record is written to a temporary file that is moved
// into place, so a failed move leaves no half-written record, and migrate
// stops before it changes anything.
#[test]
fn a_failed_record_write_leaves_no_record_and_changes_nothing() {
    let (mut fake, context) = systemd_user();
    fake.failing
        .insert(format!("rename {LINUX_RECORD}.tmp -> {LINUX_RECORD}"));
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(&ran.err, "Nothing but the backup was made.");
    assert!(!fake.exists(LINUX_RECORD));
    assert!(!fake.exists(&format!("{LINUX_RECORD}.tmp")));
    assert!(!happened(
        &fake,
        "run systemctl --user stop cliproxyapi.service"
    ));
}

// Not upstream's: a record that doesn't read is replaced by the copy in the
// backup, whose directory is read from what is left of it; with no copy
// to use, -undo says why.
#[test]
fn undo_reads_the_backups_copy_when_the_record_does_not_read() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.file(
        LINUX_RECORD,
        format!(r#"{{"backup":{{"dir":"{}"}}}}"#, systemd_backup()),
    );
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(&undo.out, "doesn't read");
    has(
        &undo.out,
        &format!(
            "The copy in the backup, {}/migration.json, is read instead.",
            systemd_backup()
        ),
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");

    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.file(LINUX_RECORD, "{ not json");
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.err, "The backup's copy can't be used either: ");
    has(
        &undo.err,
        "isn't JSON, so the backup's directory isn't known",
    );
}

// Not upstream's: a process ID that another process has taken since is not
// stopped, as the start time that was recorded with it is not the new one's.
#[test]
fn a_reused_process_id_is_not_stopped() {
    let (mut fake, context) = bare_process();
    fake.on(
        &format!("rename {OPT_CPA} -> {OPT_MOVED}"),
        Effect::Reuse(5151),
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.err,
        "CLIProxyAPI (process 5151) didn't stop: process 5151 is another process now",
    );
    assert_eq!(
        fake.procs.get(&5151).map(|p| p.name.as_str()),
        Some("other")
    );
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "rolled-back");

    // -undo of a drop-in that never restarted CLIProxyAPI: its process ID
    // is another's now, so it isn't said to run, or touched.
    let (mut fake, context) = launcher_task();
    fake.terminal = true;
    fake.replies.extend([true, false]);
    assert_eq!(migrate(&mut fake, &context, &[]).code, 0);
    assert_eq!(
        saved(&fake, WINDOWS_RECORD)["switch"]["started"].as_u64(),
        Some(1000)
    );
    fake.procs.insert(
        6060,
        Proc {
            pid: 6060,
            started: Some(5555),
            name: "notepad.exe".to_owned(),
            exe: Some(r"C:\Windows\notepad.exe".to_owned()),
            args: Vec::new(),
            cwd: None,
            env: None,
            parent: None,
        },
    );
    // Nothing answers now, so CLIProxyAPI is not found running.
    fake.serving.clear();
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.out,
        &format!(
            "open-ferry wasn't running from {TASK_CPA}, and CLIProxyAPI isn't running. Start CLIProxyAPI as you do: "
        ),
    );
    has(&undo.err, "then run `open-ferry migrate -undo` again");
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone-not-started");
    lacks(&undo.out, "still runs, as it did before the switch");
    assert!(fake.procs.contains_key(&6060));
    assert!(!fake.events[events..].iter().any(|e| e == "stop 6060"));
}

// Not upstream's: undoing a drop-in looks at what is there at each step, so
// that after a step failed, running it again finishes.
#[test]
fn undo_of_a_drop_in_can_be_run_again() {
    let (mut fake, context) = bare_process();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.failing
        .insert(format!("rename {OPT_MOVED} -> {OPT_CPA}"));
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.err,
        &format!("failed to move {OPT_MOVED} back to {OPT_CPA}"),
    );
    // The link is gone and CLIProxyAPI's binary is still where it was moved.
    assert_eq!(fake.link_target(OPT_CPA), None);
    assert!(fake.exists(OPT_MOVED));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");

    fake.failing.clear();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
    assert!(!fake.exists(OPT_MOVED));
    has(
        &undo.out,
        "Started CLIProxyAPI (process 9002) with the same command line",
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");

    // A step done by an earlier run is taken as done: with the binary back,
    // only what is left is done.
    let (mut fake, context) = bare_process();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.unstoppable.insert(9001);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.err, "failed to stop open-ferry (process 9001)");
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
    fake.unstoppable.clear();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(
        &undo.out,
        &format!(
            "{OPT_MOVED} isn't there: taking {OPT_CPA} to be CLIProxyAPI's binary, as put back before; going on."
        ),
    );
    has(&undo.out, "Stopped open-ferry (process 9001)");
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");
}

// Not upstream's: CLIProxyAPI isn't started while open-ferry still answers
// on the port; the record stays open, and -undo finishes once it is gone.
#[test]
fn does_not_start_cliproxyapi_beside_an_open_ferry_that_stays() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.serving.insert(0, (7777, Answer::OpenFerry));
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.err,
        "open-ferry hasn't ended 20 seconds after it was stopped (open-ferry still answers on http://127.0.0.1:8317), so CLIProxyAPI is not started beside it",
    );
    assert!(
        !fake.events[events..]
            .iter()
            .any(|event| event == "run systemctl --user start cliproxyapi.service")
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");

    fake.serving.retain(|(pid, _)| *pid != 7777);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(happened(
        &fake,
        "run systemctl --user start cliproxyapi.service"
    ));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");

    // A switch that is rolled back waits the same way.
    let (mut fake, context) = systemd_user();
    fake.fail(
        "systemctl --user enable --now open-ferry.service",
        1,
        "Failed to enable unit",
    );
    fake.on(
        "systemctl --user stop cliproxyapi.service",
        Effect::Serve(7777),
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    assert!(!happened(
        &fake,
        "run systemctl --user start cliproxyapi.service"
    ));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switching");
}

// Not upstream's: the same for a drop-in: the open-ferry that was put in
// CLIProxyAPI's place is stopped, and CLIProxyAPI isn't started while
// something answers as open-ferry.
#[test]
fn a_drop_in_does_not_start_cliproxyapi_beside_an_open_ferry_that_stays() {
    let (mut fake, context) = bare_process();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.serving.insert(0, (7777, Answer::OpenFerry));
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.err, "so CLIProxyAPI is not started beside it");
    assert_eq!(fake.launches.len(), 1);
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");

    // With it gone, the retry puts nothing back twice. It has no command line
    // to start CLIProxyAPI with, so it says what to do and keeps the record
    // open, in a status of its own.
    fake.serving.retain(|(pid, _)| *pid != 7777);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.out,
        "open-ferry wasn't running from /opt/cpa/cli-proxy-api, and CLIProxyAPI isn't running. Start CLIProxyAPI as you do: ",
    );
    has(&undo.err, "then run `open-ferry migrate -undo` again");
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone-not-started");

    // Once CLIProxyAPI is started as the person does it, -undo closes.
    fake.process(Proc {
        pid: 8123,
        started: Some(2000),
        name: "cli-proxy-api".to_owned(),
        exe: Some(OPT_CPA.to_owned()),
        args: strings(&["-password", "s3cret-password"]),
        cwd: Some("/opt/cpa".to_owned()),
        env: None,
        parent: None,
    });
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(&undo.out, "CLIProxyAPI is running.");
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");
}

// Not upstream's: the switch is a symbolic link to the installed
// open-ferry, which the install receipt names when there is one, else to
// this open-ferry's real path.
#[test]
fn the_drop_in_links_to_the_installed_open_ferry() {
    let installed = "/home/me/.local/share/open-ferry/bin/open-ferry";
    let (mut fake, context) = bare_process();
    fake.installed = Some(installed.to_owned());
    fake.file(installed, OPEN_FERRY);
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(fake.link_target(OPT_CPA).as_deref(), Some(installed));
    assert_eq!(saved(&fake, LINUX_RECORD)["switch"]["link"], installed);

    // A receipt that names a binary that isn't there is not followed: the
    // drop-in is a copy.
    let (mut fake, context) = bare_process();
    fake.installed = Some("/gone/open-ferry".to_owned());
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(fake.link_target(OPT_CPA), None);
    assert_eq!(fake.data(OPT_CPA), OPEN_FERRY);
}

// Not upstream's: with no install receipt there is nothing for a link to
// follow, so the drop-in is a copy; the plan and -json say so.
#[test]
fn a_drop_in_without_a_receipt_is_a_copy() {
    let (mut fake, context) = bare_process();
    fake.installed = None;
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        "copy open-ferry (/home/me/.local/bin/open-ferry) to /opt/cpa/cli-proxy-api",
    );
    has(
        &ran.out,
        "There is no install receipt for this open-ferry (the install script writes one), so there is nothing for a link to follow: the drop-in is a copy.",
    );
    has(&ran.out, "reports a new release");
    lacks(&ran.out, "The link follows the installed open-ferry");

    let json = migrate(&mut fake, &context, &["-json"]);
    let json = serde_json::from_str::<Value>(json.out.trim()).unwrap();
    assert_eq!(json["drop_in"], "copy");

    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(fake.link_target(OPT_CPA), None);
    assert_eq!(fake.data(OPT_CPA), OPEN_FERRY);
    assert!(saved(&fake, LINUX_RECORD)["switch"]["link"].is_null());
    assert_eq!(
        saved(&fake, LINUX_RECORD)["switch"]["sha256"]
            .as_str()
            .map(str::len),
        Some(64)
    );

    // And -undo puts CLIProxyAPI's binary back, by its digest.
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
}

// Not upstream's: -json says how a drop-in is made: a symbolic link on
// Linux and macOS, a copy on Windows, and nothing for a service.
#[test]
fn json_says_how_a_drop_in_is_made() {
    let drop_in = |fake: &mut Fake, context: &Context| {
        let ran = migrate(fake, context, &["-json"]);
        assert_eq!(ran.code, 0, "{}", ran.all());
        serde_json::from_str::<Value>(ran.out.trim()).unwrap()
    };
    let (mut fake, context) = bare_process();
    let json = drop_in(&mut fake, &context);
    assert_eq!(json["switch"], "drop-in");
    assert_eq!(json["drop_in"], "symlink");

    // Without an install receipt there is nothing to link to.
    let (mut fake, context) = bare_process();
    fake.installed = None;
    let json = drop_in(&mut fake, &context);
    assert_eq!(json["drop_in"], "copy");

    let (mut fake, context) = launcher_task();
    let json = drop_in(&mut fake, &context);
    assert_eq!(json["switch"], "drop-in");
    assert_eq!(json["drop_in"], "copy");

    let (mut fake, context) = systemd_user();
    let json = drop_in(&mut fake, &context);
    assert_eq!(json["switch"], "service");
    assert!(json.get("drop_in").is_some_and(Value::is_null));
}

// Not upstream's: a copy of open-ferry on Windows reports a release but
// doesn't install it; the plan says so and how to update it.
#[test]
fn a_windows_copy_says_it_does_not_update() {
    let (mut fake, context) = launcher_task();
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    has(
        &ran.out,
        &format!(
            "The copy at {TASK_CPA} reports a new release (`open-ferry update`) but doesn't install it: the updater replaces the installed open-ferry, not this copy. To update it, run `open-ferry migrate -undo`, then `open-ferry migrate` again."
        ),
    );
    lacks(&ran.out, "The link follows the installed open-ferry");

    let (mut fake, context) = bare_process();
    let ran = migrate(&mut fake, &context, &["-dry-run"]);
    lacks(&ran.out, "reports a new release");
}

// --- Fix round 3 ---

/// A running CLIProxyAPI, as `bare_process` runs it, started like a service's.
fn proxy_at(pid: u32, exe: &str) -> Proc {
    Proc {
        pid,
        started: Some(3000),
        name: "cli-proxy-api".to_owned(),
        exe: Some(exe.to_owned()),
        args: Vec::new(),
        cwd: Some("/opt/cpa".to_owned()),
        env: None,
        parent: None,
    }
}

/// The manager of `systemd_user()`'s unit says it runs.
fn running_unit(fake: &mut Fake) {
    fake.answer(
        "systemctl --user show cliproxyapi.service",
        0,
        &shown_unit(
            4242,
            "/home/me/cpa/cli-proxy-api -config /home/me/cpa/config.yaml",
            "/home/me/cpa",
            true,
            "",
            "",
        ),
    );
}

// Not upstream's: -restore doesn't write under a running proxy. It stops it
// with the plan showing it and a yes (-yes is the yes), restores, and only
// then starts CLIProxyAPI.
#[test]
fn restore_stops_a_running_proxy_with_a_yes_and_restores_before_it_starts() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.file(SYSTEMD_CONFIG, "changed: by open-ferry\n");
    fake.process(proxy_at(4242, SYSTEMD_CPA));
    running_unit(&mut fake);

    // Without the yes, nothing is stopped or written, and CLIProxyAPI's
    // service is not started over it.
    fake.terminal = true;
    fake.replies.extend([true, false]);
    let undo = migrate(&mut fake, &context, &["-undo", "-restore"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.out,
        "CLIProxyAPI runs (process 4242), and -restore doesn't write under a running proxy: nothing was restored.",
    );
    assert_eq!(fake.data(SYSTEMD_CONFIG), b"changed: by open-ferry\n");
    assert!(fake.procs.contains_key(&4242));
    assert!(!happened(
        &fake,
        "run systemctl --user start cliproxyapi.service"
    ));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");

    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    // CLIProxyAPI is under its service manager: it is stopped through the
    // manager, not killed by its process ID.
    has(&undo.out, "Ran: systemctl --user stop cliproxyapi.service");
    assert!(!fake.procs.contains_key(&4242));
    assert_eq!(
        fake.data(SYSTEMD_CONFIG),
        config_text("~/.cli-proxy-api").as_bytes()
    );
    let events = &fake.events[before..];
    assert!(
        !events.iter().any(|event| event == "stop 4242"),
        "{events:#?}"
    );
    in_order(
        events,
        &[
            "run systemctl --user stop cliproxyapi.service",
            &format!(
                "copy {}/config/config.yaml -> {SYSTEMD_CONFIG}",
                systemd_backup()
            ),
            "run systemctl --user start cliproxyapi.service",
        ],
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");
}

/// Whether a file was copied after event `before`.
fn copied_since(fake: &Fake, before: usize) -> bool {
    fake.events[before..]
        .iter()
        .any(|event| event.starts_with("copy "))
}

/// A systemd unit that runs again after the switch (someone started it, or
/// its manager did), with CLIProxyAPI's process in it.
fn running_again(fake: &mut Fake) {
    fake.process(proxy_at(4242, SYSTEMD_CPA));
    running_unit(fake);
}

// Not upstream's: when the manager can't stop the service, no process is
// killed under it, nothing is restored, and the record stays open.
#[test]
fn restore_blocks_when_the_manager_cannot_stop_the_service() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.file(SYSTEMD_CONFIG, "changed: by open-ferry\n");
    running_again(&mut fake);
    fake.fail(
        "systemctl --user stop cliproxyapi.service",
        1,
        "Access denied",
    );

    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.out, "so nothing was restored");
    has(&undo.out, "no process is killed under its manager");
    assert_eq!(fake.data(SYSTEMD_CONFIG), b"changed: by open-ferry\n");
    assert!(fake.procs.contains_key(&4242));
    assert!(!fake.events[before..].iter().any(|e| e.starts_with("stop ")));
    assert!(!copied_since(&fake, before));
    assert!(!happened(
        &fake,
        "run systemctl --user start cliproxyapi.service"
    ));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
}

// Not upstream's: a stop that succeeds but leaves the service running is
// waited for, then blocks, and no process is killed.
#[test]
fn restore_blocks_when_the_service_stays_running_after_the_stop() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.file(SYSTEMD_CONFIG, "changed: by open-ferry\n");
    running_again(&mut fake);
    fake.stuck
        .insert("systemctl --user stop cliproxyapi.service".to_owned());

    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.out, "hasn't stopped");
    has(&undo.out, "so nothing was restored");
    assert_eq!(fake.data(SYSTEMD_CONFIG), b"changed: by open-ferry\n");
    assert!(!fake.events[before..].iter().any(|e| e.starts_with("stop ")));
    assert!(!copied_since(&fake, before));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
}

// Not upstream's: a service its manager says runs is stopped through the
// manager first, even when no process of it is seen.
#[test]
fn restore_stops_through_the_manager_when_no_process_is_seen() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    running_unit(&mut fake);
    assert!(!fake.procs.contains_key(&4242));

    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    in_order(
        &fake.events[before..],
        &[
            "run systemctl --user stop cliproxyapi.service",
            &format!(
                "copy {}/config/config.yaml -> {SYSTEMD_CONFIG}",
                systemd_backup()
            ),
            "run systemctl --user start cliproxyapi.service",
        ],
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");
}

// Not upstream's: when it isn't known whether the service has stopped,
// nothing is restored.
#[test]
fn restore_blocks_when_the_state_of_the_service_is_unknown() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.file(SYSTEMD_CONFIG, "changed: by open-ferry\n");
    fake.fail("systemctl --user show cliproxyapi.service", 1, "Failed");

    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.out,
        "it isn't known whether CLIProxyAPI's service has stopped",
    );
    assert_eq!(fake.data(SYSTEMD_CONFIG), b"changed: by open-ferry\n");
    assert!(!copied_since(&fake, before));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
}

// Not upstream's: a task is disabled before it is ended, so that nothing
// starts it during the copy, and enabled and run again after the copy.
#[test]
fn restore_disables_a_task_before_it_ends_it_and_enables_it_after_the_copy() {
    let (mut fake, context) = binary_task(true);
    fake.on(
        r"schtasks.exe /end /tn \CLIProxyAPI",
        Effect::Exit(BINARY_TASK_CPA.to_owned()),
    );
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    // The task ran CLIProxyAPI again after the switch.
    fake.process(Proc {
        pid: 8181,
        started: Some(2000),
        name: "cli-proxy-api.exe".to_owned(),
        exe: Some(BINARY_TASK_CPA.to_owned()),
        args: strings(&["-config", r"C:\Users\me\cpa\config.yaml"]),
        cwd: Some(r"C:\Users\me\cpa".to_owned()),
        env: None,
        parent: None,
    });

    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    let events = &fake.events[before..];
    let copy = events
        .iter()
        .find(|event| event.starts_with("copy "))
        .cloned()
        .unwrap();
    in_order(
        events,
        &[
            r"run schtasks.exe /change /tn \CLIProxyAPI /disable",
            r"run schtasks.exe /end /tn \CLIProxyAPI",
            &copy,
            r"run schtasks.exe /change /tn \CLIProxyAPI /enable",
            r"run schtasks.exe /run /tn \CLIProxyAPI",
        ],
    );
    assert!(!events.iter().any(|event| event == "stop 8181"));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone");
}

// Not upstream's: a task that can't be disabled could start CLIProxyAPI
// during the copy, so nothing is restored.
#[test]
fn restore_blocks_when_the_task_cannot_be_disabled() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.fail(
        r"schtasks.exe /change /tn \CLIProxyAPI /disable",
        1,
        "ERROR: Access is denied.",
    );

    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.out, "so nothing was restored");
    assert!(!copied_since(&fake, before));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");
}

// Not upstream's: what runs is read after the task is disabled, not before.
// A task that starts between the two is ended, and nothing is copied until
// it has ended.
#[test]
fn restore_reads_the_task_after_disabling_it() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    assert!(fake.procs.keys().all(|pid| *pid != 8080));
    // The task starts CLIProxyAPI as it is disabled, and /end ends it.
    fake.on(
        r"schtasks.exe /change /tn \CLIProxyAPI /disable",
        Effect::Launch(BINARY_TASK_CPA.to_owned()),
    );
    fake.on(
        r"schtasks.exe /end /tn \CLIProxyAPI",
        Effect::Exit(BINARY_TASK_CPA.to_owned()),
    );

    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    let events = &fake.events[before..];
    let copy = events
        .iter()
        .find(|event| event.starts_with("copy "))
        .cloned()
        .unwrap();
    in_order(
        events,
        &[
            r"run schtasks.exe /change /tn \CLIProxyAPI /disable",
            r"run schtasks.exe /end /tn \CLIProxyAPI",
            &copy,
        ],
    );
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone");
}

// Not upstream's: CLIProxyAPI that runs again just before the copy blocks it.
#[test]
fn restore_blocks_when_the_proxy_runs_again_just_before_the_copy() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.on(
        r"schtasks.exe /change /tn \CLIProxyAPI /disable",
        Effect::Launch(BINARY_TASK_CPA.to_owned()),
    );
    // Ending it starts it again, as a restart policy would.
    fake.on(
        r"schtasks.exe /end /tn \CLIProxyAPI",
        Effect::Exit(BINARY_TASK_CPA.to_owned()),
    );
    fake.on(
        r"schtasks.exe /end /tn \CLIProxyAPI",
        Effect::Launch(BINARY_TASK_CPA.to_owned()),
    );

    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.out, "runs again");
    assert!(!copied_since(&fake, before));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");
}

/// The first `-undo -restore` after a switch, with the service still off as
/// `migrate` left it: the copy is made and the record is closed.
fn first_restore(mut fake: Fake, context: Context, record: &str) {
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let before = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(copied_since(&fake, before), "{:#?}", &fake.events[before..]);
    assert_eq!(saved(&fake, record)["status"], "undone");
}

// Not upstream's: the first -undo -restore works for each manager.
#[test]
fn the_first_restore_works_after_a_systemd_switch() {
    let (fake, context) = systemd_user();
    first_restore(fake, context, LINUX_RECORD);
}

#[test]
fn the_first_restore_works_after_a_launchd_switch() {
    let (fake, context) = brew_services();
    first_restore(fake, context, "/Users/me/.config/open-ferry/migration.json");
}

#[test]
fn the_first_restore_works_after_a_windows_service_switch() {
    let (fake, context) = nssm_service(windows_program_files(), true);
    first_restore(fake, context, WINDOWS_RECORD);
}

#[test]
fn the_first_restore_works_after_a_task_switch() {
    let (fake, context) = binary_task(true);
    first_restore(fake, context, WINDOWS_RECORD);
}

// Not upstream's: the plan of -undo -restore says that a proxy is stopped,
// that the credentials come back as they were, and that a failed copy keeps
// the record open.
#[test]
fn the_restore_plan_says_what_it_does() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-dry-run"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(
        &undo.out,
        "ask whether to stop it (-yes is the yes); nothing is stopped without it, and nothing is copied while it runs.",
    );
    has(
        &undo.out,
        "If the copy fails, CLIProxyAPI isn't started and the record stays open: run -undo -restore again.",
    );
    has(
        &undo.out,
        "The credentials come back as they were at the switch, so a token refreshed since then is replaced by the older one.",
    );
}

// Not upstream's: a file whose directory is a link to somewhere else now
// than at the switch is not written, and the copy is reported.
#[test]
fn restore_skips_a_directory_that_leads_elsewhere_now() {
    let (mut fake, context) = systemd_user_config("/data/cfg/config.yaml");
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    assert_eq!(
        saved(&fake, LINUX_RECORD)["backup"]["files"][0]["parent"],
        "/data/cfg"
    );
    fake.links
        .insert("/data/cfg".to_owned(), "/data/other".to_owned());
    fake.dir("/data/other");
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.out,
        "/data/cfg/config.yaml wasn't restored: its directory led to /data/cfg at the switch and now leads to /data/other",
    );
    assert!(!fake.exists("/data/other/config.yaml"));
    assert!(!happened(
        &fake,
        "run systemctl --user start cliproxyapi.service"
    ));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
}

// Not upstream's: -undo waits for open-ferry's service itself to have ended,
// not for the port to be quiet, and says what it saw when it doesn't.
#[test]
fn undo_waits_for_the_service_and_times_out_clearly() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    // Nothing answers on the port, but the manager says it is stopping.
    fake.answer(
        "systemctl --user show open-ferry.service",
        0,
        "LoadState=loaded\nActiveState=deactivating\n",
    );
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.err,
        "open-ferry hasn't ended 20 seconds after it was stopped (its service is still deactivating), so CLIProxyAPI is not started beside it.",
    );
    has(&undo.err, "The record stays open");
    assert!(
        !fake.events[events..]
            .iter()
            .any(|event| event == "run systemctl --user start cliproxyapi.service")
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");

    // Once the manager says it has ended, the retry finishes.
    fake.answer(
        "systemctl --user show open-ferry.service",
        0,
        "LoadState=loaded\nActiveState=inactive\n",
    );
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");
}

// Not upstream's: a launchd job that is loaded already is not loaded again.
#[test]
fn undo_goes_on_when_the_launchd_job_is_loaded_already() {
    let (mut fake, context) = brew_services();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    // The job is loaded already (an earlier -undo got that far).
    fake.answer(
        "launchctl print gui/501/homebrew.mxcl.cliproxyapi",
        0,
        "gui/501/homebrew.mxcl.cliproxyapi = {\n\tstate = running\n}\n",
    );
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(&undo.out, "CLIProxyAPI's service is on already; going on.");
    assert!(happened(
        &fake,
        "run launchctl enable gui/501/homebrew.mxcl.cliproxyapi"
    ));
    assert!(!happened(
        &fake,
        &format!("run launchctl bootstrap gui/501 {BREW_PLIST}")
    ));
    assert_eq!(
        saved(&fake, "/Users/me/.config/open-ferry/migration.json")["status"],
        "undone"
    );
}

// Not upstream's: a Windows service that runs already is not started again.
#[test]
fn undo_goes_on_when_the_windows_service_runs_already() {
    let (mut fake, context) = nssm_service(windows_program_files(), true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.answer(
        "sc.exe query CLIProxyAPI",
        0,
        "SERVICE_NAME: CLIProxyAPI\r\n        TYPE               : 10  WIN32_OWN_PROCESS\r\n        STATE              : 4  RUNNING\r\n",
    );
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(&undo.out, "CLIProxyAPI's service is on already; going on.");
    assert!(happened(&fake, "run sc.exe config CLIProxyAPI start= auto"));
    assert!(!happened(&fake, "run sc.exe start CLIProxyAPI"));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone");
}

// Not upstream's: the switch refuses when its own files are the config, a
// .env file or the auth directory, and a leftover temporary file of an older
// name is neither used nor written through.
#[test]
fn refuses_when_the_record_is_the_config_and_never_reuses_a_temp_name() {
    let (mut fake, context) = systemd_user_config(LINUX_RECORD);
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.out,
        &format!(
            "The config ({LINUX_RECORD}) is {LINUX_RECORD} or inside it or holds it, and the switch writes there: move one of them. Nothing was changed."
        ),
    );
    read_only(&fake);

    let (mut fake, context) = systemd_user();
    let stale = format!("{LINUX_RECORD}.tmp");
    fake.file(&stale, "somebody else's file\n");
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(fake.data(&stale), b"somebody else's file\n");
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
    // The temporary files were moved into place, none left.
    assert!(
        !fake
            .files
            .keys()
            .any(|path| path.starts_with(LINUX_RECORD) && path != LINUX_RECORD && path != &stale),
        "{:#?}",
        fake.files.keys().collect::<Vec<_>>()
    );
}

// Not upstream's: the record's directory is made private where it is new.
#[test]
fn the_records_directory_is_made_private() {
    let (mut fake, context) = systemd_user();
    fake.dirs.remove("/home/me/.config/open-ferry");
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert!(fake.private.contains("/home/me/.config/open-ferry"));
}

// Not upstream's: only a file with the digest recorded at the switch is put
// back as CLIProxyAPI's; anything else is recovered by hand, with the record
// kept open, and nothing is moved.
#[test]
fn undo_puts_back_only_a_binary_with_the_recorded_digest() {
    let (mut fake, context) = bare_process();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let sha = saved(&fake, LINUX_RECORD)["switch"]["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(sha.len(), 64);
    // Something else was put where CLIProxyAPI's binary was moved.
    fake.file(OPT_MOVED, "not CLIProxyAPI");
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.err,
        &format!(
            "{OPT_MOVED} isn't CLIProxyAPI's binary: its SHA-256 isn't the one recorded at the switch. CLIProxyAPI's binary (SHA-256 {sha}) wasn't put back, and the record stays open"
        ),
    );
    assert_eq!(fake.data(OPT_MOVED), b"not CLIProxyAPI");
    assert_eq!(
        fake.link_target(OPT_CPA).as_deref(),
        Some("/home/me/.local/bin/open-ferry")
    );
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");

    // With the right file back where it was moved, it finishes.
    fake.file(OPT_MOVED, CLIPROXYAPI);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
}

// Not upstream's: the drop-in's process is the one started through the
// drop-in's path. An open-ferry that was not, or only has the name, is never
// stopped for it. (macOS: UNVERIFIED how the command line is read.)
#[test]
fn undo_stops_only_the_process_started_through_the_drop_in() {
    let (mut fake, context) = bare_process();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let open_ferry = |pid: u32, name: &str, argv0: &str, cwd: &str| {
        let mut process = proxy_at(pid, "/home/me/.local/bin/open-ferry");
        process.name = name.to_owned();
        process.cwd = Some(cwd.to_owned());
        (process, argv0.to_owned())
    };
    // The installed open-ferry, run by its own path; the same by a bare name;
    // and one started in /opt/cpa as ./cli-proxy-api.
    let cases = [
        open_ferry(6001, "open-ferry", "/home/me/.local/bin/open-ferry", "/"),
        open_ferry(6002, "cli-proxy-api", "cli-proxy-api", "/opt/cpa"),
        open_ferry(6003, "cli-proxy-api", "./cli-proxy-api", "/opt/cpa"),
    ];
    for (process, argv0) in cases {
        fake.argv0s.insert(process.pid, argv0);
        fake.procs.insert(process.pid, process);
    }
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(fake.procs.contains_key(&6001));
    assert!(fake.procs.contains_key(&6002));
    assert!(!fake.procs.contains_key(&6003));
    assert!(!fake.procs.contains_key(&9001));
}

// --- Round 4: one run at a time, who is stopped, and where things go ---

/// The lock file beside the Linux record.
const LINUX_LOCK: &str = "/home/me/.config/open-ferry/migration.json.lock";

/// Whether any event from `from` on is `event`.
fn happened_since(fake: &Fake, from: usize, event: &str) -> bool {
    fake.events[from..].iter().any(|known| known == event)
}

/// The renames of real files from `from` on (the record's temporary files
/// don't count).
fn renames_since(fake: &Fake, from: usize) -> Vec<String> {
    fake.events[from..]
        .iter()
        .filter(|event| event.starts_with("rename ") && !event.contains(".tmp"))
        .cloned()
        .collect()
}

// Not upstream's: two `migrate` runs don't work at once. The second finds the
// lock held, says so, exits 1, and changes nothing: no rename, no stopped
// process. Once the first lets go, it can run.
#[test]
fn only_one_migrate_runs_at_a_time() {
    let (mut fake, context) = bare_process();
    fake.held.insert(LINUX_LOCK.to_owned());
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.all(),
        &format!("another `open-ferry migrate` is running (it holds {LINUX_LOCK})"),
    );
    assert!(renames_since(&fake, 0).is_empty(), "{:#?}", fake.events);
    assert!(!fake.events.iter().any(|event| event.starts_with("stop ")));
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);

    fake.held.clear();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    assert_eq!(fake.data(OPT_MOVED), CLIPROXYAPI);

    // -undo takes the same lock.
    fake.held.insert(LINUX_LOCK.to_owned());
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "another `open-ferry migrate` is running");
    assert!(renames_since(&fake, events).is_empty());
    assert_eq!(fake.data(OPT_MOVED), CLIPROXYAPI);
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
    fake.held.clear();
    assert_eq!(migrate(&mut fake, &context, &["-undo", "-yes"]).code, 0);
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
}

// Not upstream's: only an answer that says it is CLIProxyAPI closes
// `undone-not-started`. Another server's HTTP answer on the port does not.
#[test]
fn a_stray_http_answer_does_not_close_undone_not_started() {
    let (mut fake, context) = launcher_task();
    fake.terminal = true;
    fake.replies.extend([true, false]);
    assert_eq!(migrate(&mut fake, &context, &[]).code, 0);
    // CLIProxyAPI's process is gone, and another server answers (a 404).
    fake.procs.remove(&6060);
    fake.serving.clear();
    fake.serving.push((
        1,
        Answer::Other("a server that isn't open-ferry (HTTP status 404)".to_owned()),
    ));
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone-not-started");

    // An answer that says it is CLIProxyAPI does close it.
    fake.serving.clear();
    fake.serving.push((1, Answer::CliProxyApi));
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone");
}

// Not upstream's: a task that is deleted reads as gone, but the process it
// ran may go on. -undo waits for that process, and CLIProxyAPI's task is not
// run beside it.
#[test]
fn a_deleted_task_is_not_an_ended_one() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    // Ending the task does not end its process.
    fake.effects.remove("schtasks.exe /end /tn open-ferry");
    fake.procs.get_mut(&9001).unwrap().args =
        strings(&["service", "run", "-config", r"C:\Users\me\cpa\config.yaml"]);
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "process 9001 still runs");
    assert!(!happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");

    // Once the process has ended, the retry finishes.
    fake.end(9001);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone");
}

/// Makes the task's process 9001 a supervisor that ends at `/end`, and gives
/// it a server (process 9100) that lives on.
fn supervisor_with_a_server(fake: &mut Fake) {
    let config = r"C:\Users\me\cpa\config.yaml";
    let supervisor = fake.procs.get_mut(&9001).unwrap();
    supervisor.args = strings(&["service", "run", "-config", config]);
    let child = Proc {
        pid: 9100,
        started: Some(12000),
        args: strings(&["-config", config]),
        parent: Some(Parent {
            pid: 9001,
            name: Some(supervisor.name.clone()),
        }),
        ..supervisor.clone()
    };
    fake.procs.insert(9100, child);
    fake.effects.remove("schtasks.exe /end /tn open-ferry");
    fake.on("schtasks.exe /end /tn open-ferry", Effect::End(9001));
}

// Not upstream's: a task's supervisor ends at /end, but its server may live
// on, with nothing to probe on the address. -undo takes the server too,
// before the task is deleted, so CLIProxyAPI is not started beside it, and
// a retry finds it by the installed config when the supervisor is gone.
#[test]
fn undo_waits_for_the_server_of_a_task_supervisor() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    supervisor_with_a_server(&mut fake);
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "process 9100 still runs");
    assert!(!fake.procs.contains_key(&9001));
    assert!(fake.procs.contains_key(&9100));
    assert!(!happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");

    // The supervisor is gone now: the server is found by its config.
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "process 9100 still runs");
    assert!(!happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");

    fake.end(9100);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone");
}

/// Makes the task's process 9001 a supervisor that ends at `/end`, and gives
/// it back.
fn task_supervisor(fake: &mut Fake) -> Proc {
    let config = r"C:\Users\me\cpa\config.yaml";
    let supervisor = fake.procs.get_mut(&9001).unwrap();
    supervisor.args = strings(&["service", "run", "-config", config]);
    let supervisor = supervisor.clone();
    fake.effects.remove("schtasks.exe /end /tn open-ferry");
    fake.on("schtasks.exe /end /tn open-ferry", Effect::End(9001));
    supervisor
}

// Not upstream's: a supervisor in its restart pause starts a server after the
// processes were taken and before /end. The server outlives the supervisor
// and nothing can be probed on the address: it is found again once the
// captured processes have ended, and waited for.
#[test]
fn undo_waits_for_a_server_started_after_the_capture() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let supervisor = task_supervisor(&mut fake);
    let server = Proc {
        pid: 9100,
        started: Some(12000),
        args: strings(&["-config", r"C:\Users\me\cpa\config.yaml"]),
        parent: Some(Parent {
            pid: 9001,
            name: Some(supervisor.name.clone()),
        }),
        ..supervisor
    };
    fake.on(
        "schtasks.exe /end /tn open-ferry",
        Effect::Spawn(Box::new(server)),
    );
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "process 9100 still runs");
    assert!(!fake.procs.contains_key(&9001));
    assert!(!happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");
}

// Not upstream's: a server found only through its supervisor (another
// executable path, and no -config) is taken with it and waited for.
#[test]
fn undo_takes_a_server_found_only_through_its_supervisor() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let supervisor = task_supervisor(&mut fake);
    let server = Proc {
        pid: 9100,
        started: Some(12000),
        exe: Some(r"C:\Downloads\open-ferry.exe".to_owned()),
        args: strings(&["-serve"]),
        parent: Some(Parent {
            pid: 9001,
            name: Some(supervisor.name.clone()),
        }),
        ..supervisor
    };
    fake.procs.insert(9100, server);
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "process 9100 still runs");
    assert!(!happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");

    fake.end(9100);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone");
}

// Not upstream's: -undo may be run from another copy of open-ferry (a
// download, under another name) than the one the service runs. It looks for
// the service's processes by the path the record names, so the orphan server
// of the installed binary is still found and waited for.
#[test]
fn undo_from_another_binary_finds_the_orphan_server_of_the_installed_one() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    assert_eq!(
        saved(&fake, WINDOWS_RECORD)["switch"]["ours_exe"],
        context.exe
    );
    // The server outlives the task's end.
    fake.effects.remove("schtasks.exe /end /tn open-ferry");
    fake.procs.get_mut(&9001).unwrap().args = strings(&["-config", r"C:\Users\me\cpa\config.yaml"]);
    let mut other = context.clone();
    other.exe = r"C:\Users\me\Downloads\open-ferry-new.exe".to_owned();
    fake.file(&other.exe, OPEN_FERRY);

    let events = fake.events.len();
    let undo = migrate(&mut fake, &other, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "process 9001 still runs");
    assert!(!happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");

    fake.end(9001);
    let undo = migrate(&mut fake, &other, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone");
}

// Not upstream's: a process names the file it runs, after links, while the
// service is given open-ferry's path, which may be a link (Homebrew's, say).
// -undo still finds the orphan server of the linked binary and waits for it.
#[test]
fn undo_finds_the_orphan_server_of_a_linked_open_ferry() {
    const REAL: &str = "/opt/homebrew/Cellar/open-ferry/1.0.0/bin/open-ferry";
    const RECORD: &str = "/Users/me/.config/open-ferry/migration.json";
    let (mut fake, context) = brew_services();
    fake.files.remove(&context.exe);
    fake.file(REAL, OPEN_FERRY);
    fake.links.insert(context.exe.clone(), REAL.to_owned());
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 0, "{}", ran.all());
    assert_eq!(saved(&fake, RECORD)["switch"]["ours_exe"], context.exe);
    // The agent's server names the file the link leads to; its bootout ends it.
    let agent = fake
        .procs
        .values()
        .find(|process| process.exe.as_deref() == Some(REAL))
        .map(|process| process.pid)
        .unwrap();
    fake.on(
        "launchctl bootout gui/501/io.github.loft-902-co-llc.open-ferry",
        Effect::End(agent),
    );
    // A server of CLIProxyAPI's config that the agent's bootout leaves.
    fake.procs.insert(
        9300,
        Proc {
            pid: 9300,
            started: Some(13000),
            name: "open-ferry".to_owned(),
            exe: Some(REAL.to_owned()),
            args: strings(&["-config", "/opt/homebrew/etc/cliproxyapi.conf"]),
            cwd: Some("/".to_owned()),
            env: None,
            parent: None,
        },
    );
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "process 9300 still runs");
    assert!(!happened_since(
        &fake,
        events,
        &format!("run launchctl bootstrap gui/501 {BREW_PLIST}")
    ));
    assert_eq!(saved(&fake, RECORD)["status"], "switched");

    fake.end(9300);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(happened_since(
        &fake,
        events,
        &format!("run launchctl bootstrap gui/501 {BREW_PLIST}")
    ));
    assert_eq!(saved(&fake, RECORD)["status"], "undone");
}

// Not upstream's: an open-ferry process whose arguments can't be read could
// be the service's, so nothing is known and -undo changes nothing.
#[test]
fn undo_blocks_on_an_open_ferry_process_it_cannot_read() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    let mut unread = fake.procs.get(&9001).unwrap().clone();
    unread.pid = 9200;
    unread.args = Vec::new();
    unread.parent = None;
    fake.procs.insert(9200, unread);
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "(process 9200) can't be read");
    has(&undo.all(), "Nothing was changed");
    assert!(!happened_since(
        &fake,
        events,
        "run schtasks.exe /delete /tn open-ferry /f"
    ));
    assert!(!happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");
}

// Not upstream's: a query of the task that fails for some reason other than
// "no such task" is not taken for a task that is gone.
#[test]
fn a_failed_task_query_is_not_a_gone_task() {
    let (mut fake, context) = binary_task(true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.fail(
        "schtasks.exe /query /tn open-ferry",
        1,
        "ERROR: Access is denied.",
    );
    fake.fail(
        "schtasks.exe /query /fo csv /nh",
        1,
        "ERROR: Access is denied.",
    );
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "failed with");
    assert!(!happened_since(
        &fake,
        events,
        r"run schtasks.exe /run /tn \CLIProxyAPI"
    ));
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "switched");
}

// Not upstream's: on Windows a running file that was renamed keeps its old
// path, which then names CLIProxyAPI's binary. -undo takes the open-ferry
// processes by identity before it renames anything, writes them to the
// record, and stops exactly those. Where the copy was set aside is recorded
// too, so a retry finds it.
#[test]
fn undo_stops_the_open_ferry_whose_path_now_reads_as_cliproxyapis() {
    let (mut fake, context) = launcher_task();
    fake.terminal = true;
    fake.replies.extend([true, true]);
    assert_eq!(migrate(&mut fake, &context, &[]).code, 0);
    let aside = format!("{TASK_CPA}.open-ferry");

    fake.unstoppable.insert(9001);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.all(), "failed to stop open-ferry (process 9001)");
    let record = saved(&fake, WINDOWS_RECORD);
    assert_eq!(record["status"], "switched");
    assert_eq!(
        record["switch"]["identities"],
        serde_json::json!([{ "pid": 9001, "started": 11001 }])
    );
    assert_eq!(record["switch"]["aside"], aside);
    assert_eq!(fake.data(TASK_CPA), CLIPROXYAPI);

    // The retry stops that process, whatever its path now says, and removes
    // the copy it set aside. The task then starts CLIProxyAPI.
    fake.unstoppable.clear();
    fake.on("stop 9001", Effect::Launch(TASK_CPA.to_owned()));
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(&undo.out, "Stopped open-ferry (process 9001)");
    assert!(!fake.procs.contains_key(&9001));
    assert!(!fake.exists(&aside));
    assert_eq!(fake.data(TASK_CPA), CLIPROXYAPI);
    assert_eq!(saved(&fake, WINDOWS_RECORD)["status"], "undone");
}

// Not upstream's: open-ferry's copy is set aside where nothing is; a file at
// the plain name is never renamed onto and is left alone. Where it went is
// written to the record before it is moved, so a retry cleans it up.
#[test]
fn the_aside_is_where_nothing_is_and_a_retry_finds_it() {
    let (mut fake, context) = bare_process();
    fake.installed = None;
    let plain = format!("{OPT_CPA}.open-ferry");
    let stamped = format!("{plain}-{STAMP}");
    fake.file(&plain, "somebody else's file\n");
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    assert_eq!(fake.data(OPT_CPA), OPEN_FERRY);

    fake.unstoppable.insert(9001);
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    assert_eq!(saved(&fake, LINUX_RECORD)["switch"]["aside"], stamped);
    assert_eq!(fake.data(&plain), b"somebody else's file\n");
    assert!(
        !fake
            .events
            .iter()
            .any(|event| event.ends_with(&format!("-> {plain}")))
    );

    fake.unstoppable.clear();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    assert!(!fake.exists(&stamped));
    assert_eq!(fake.data(&plain), b"somebody else's file\n");
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "undone");
}

// Not upstream's: a file whose real path can't be found stops the switch
// before anything is changed, since -restore couldn't check where it goes.
#[test]
fn a_file_with_no_real_path_blocks_the_switch() {
    let (mut fake, context) = systemd_user();
    fake.no_real.insert(SYSTEMD_CONFIG.to_owned());
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.all(),
        &format!("ailed to find where {SYSTEMD_CONFIG} really is"),
    );
    has(&ran.all(), "Nothing was changed");
    assert!(renames_since(&fake, 0).is_empty());
    assert!(!fake.events.iter().any(|event| event.starts_with("stop ")));
    assert!(!fake.events.iter().any(|event| event.starts_with("copy ")));
}

// Not upstream's: -restore looks again at where a file goes just before it
// writes it: a directory made a link by the copy before is not written
// through. (A swap between that look and the write is not caught; see
// `restore`.)
#[test]
fn restore_looks_again_before_each_write() {
    let (mut fake, context) = systemd_user();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.dir("/home/me/other-auth");
    fake.on(
        &format!(
            "copy {}/config/config.yaml -> {SYSTEMD_CONFIG}",
            systemd_backup()
        ),
        Effect::Link(
            "/home/me/.cli-proxy-api".to_owned(),
            "/home/me/other-auth".to_owned(),
        ),
    );
    let undo = migrate(&mut fake, &context, &["-undo", "-restore", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(&undo.out, "wasn't restored");
    assert!(
        !fake
            .events
            .iter()
            .any(|event| event.contains("-> /home/me/.cli-proxy-api/")),
        "{:#?}",
        fake.events
    );
}

// Not upstream's: a stopped Windows service whose name has RUNNING in it is
// stopped: only the number on the STATE line says how it stands.
#[test]
fn a_stopped_service_named_running_is_stopped() {
    let text = "SERVICE_NAME: CLIProxyAPI_RUNNING\r\n        TYPE               : 10  WIN32_OWN_PROCESS\r\n        STATE              : 1  STOPPED\r\n";
    assert_eq!(crate::os_service::service_state_number(text), Some(1));
    assert_eq!(crate::os_service::service_state_number("RUNNING\r\n"), None);

    let (mut fake, context) = nssm_service(windows_program_files(), true);
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.answer("sc.exe query CLIProxyAPI", 0, text);
    let events = fake.events.len();
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    lacks(&undo.out, "is on already");
    assert!(happened_since(
        &fake,
        events,
        "run sc.exe start CLIProxyAPI"
    ));
}

// Not upstream's: the switch's own files are compared by where they really
// are: a config that is a link to the record is the record.
#[test]
fn the_overlap_check_follows_links() {
    let (mut fake, context) = systemd_user();
    let text = fake.files.remove(SYSTEMD_CONFIG).unwrap();
    fake.file(LINUX_RECORD, text);
    fake.links
        .insert(SYSTEMD_CONFIG.to_owned(), LINUX_RECORD.to_owned());
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(&ran.all(), "or inside it or holds it");
    has(&ran.all(), "Nothing was changed");
    assert!(renames_since(&fake, 0).is_empty());
}

// Not upstream's: with the `:` prefix systemd leaves `$$` as written, so
// migrate can't read the config path from it and the unit is blocked.
#[test]
fn a_dollar_after_the_colon_prefix_blocks() {
    let program = "/opt/cpa/cli-proxy-api";
    let argv = format!("{program} -config /etc/cpa$$x.yaml");
    let (mut fake, context) = stopped_unit(
        &format!("[Service]\nExecStart=:{argv}\n"),
        None,
        program,
        &argv,
    );
    let (_, blockers) = blockers_of(&mut fake, &context);
    has(&blockers, "starts with : and has a $");
    has(&blockers, "/etc/systemd/system/cliproxyapi.service");
    // The same line without the prefix is a literal `$`, as before.
    let (mut fake, context) = stopped_unit(
        &format!("[Service]\nExecStart={argv}\n"),
        None,
        program,
        &argv,
    );
    let (_, blockers) = blockers_of(&mut fake, &context);
    lacks(&blockers, "starts with : and has a $");
}

// Not upstream's: two drop-ins on one binary with record places of their own
// (another XDG_CONFIG_HOME, or sudo) both pass the first check. The one that
// moves second finds CLIProxyAPI's binary already saved, fails at the move,
// rolls back, and leaves the saved binary and the first run's open-ferry as
// they are.
#[test]
fn a_second_drop_in_never_replaces_the_first_ones_saved_binary() {
    let (mut fake, context) = bare_process();
    // The first run gets as far as its copy of open-ferry while this one
    // makes its backup directory, after this one looked for `moved`.
    let backup = format!("mkdir {}", opt_backup());
    fake.on(
        &backup,
        Effect::Place(OPT_MOVED.to_owned(), b"saved by the first run".to_vec()),
    );
    fake.on(
        &backup,
        Effect::Place(OPT_CPA.to_owned(), OPEN_FERRY.to_vec()),
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.err,
        "Failed to move /opt/cpa/cli-proxy-api to /opt/cpa/cli-proxy-api.cliproxyapi",
    );
    assert_eq!(fake.data(OPT_MOVED), b"saved by the first run");
    assert_eq!(fake.data(OPT_CPA), OPEN_FERRY);
    assert!(renames_since(&fake, 0).is_empty(), "{:#?}", fake.events);
    assert!(fake.procs.contains_key(&5151));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "rolled-back");
}

/// Where the drop-in's copy of open-ferry is made before it is moved.
fn staged() -> String {
    format!("{OPT_CPA}.new-{}", std::process::id())
}

// Not upstream's: open-ferry is copied to a file of this run's own and moved
// to the binary's path, so a copy that fails part way removes only that file:
// the binary's path is never touched, and CLIProxyAPI's binary is moved back.
#[test]
fn a_drop_in_that_fails_part_way_through_the_copy_puts_the_binary_back() {
    let (mut fake, context) = bare_process();
    fake.installed = None;
    fake.failing
        .insert(format!("copy {} -> {}", context.exe, staged()));
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.err,
        "failed to put open-ferry at /opt/cpa/cli-proxy-api",
    );
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
    assert!(!fake.exists(OPT_MOVED));
    assert!(!fake.exists(&staged()));
    assert!(!happened(&fake, &format!("remove {OPT_CPA}")));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "rolled-back");
}

// Not upstream's: a copy that fails before it makes its file (open-ferry can't
// be read) leaves a file a crashed run left at the same name alone.
#[test]
fn a_copy_that_fails_before_its_file_leaves_a_crashed_runs_file_alone() {
    let (mut fake, context) = bare_process();
    fake.installed = None;
    fake.file(&staged(), b"a crashed run's copy");
    fake.unreadable.insert(context.exe.clone());
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.err,
        "failed to put open-ferry at /opt/cpa/cli-proxy-api",
    );
    assert_eq!(fake.data(&staged()), b"a crashed run's copy");
    assert!(!happened(&fake, &format!("remove {}", staged())));
    assert_eq!(fake.data(OPT_CPA), CLIPROXYAPI);
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "rolled-back");
}

// Not upstream's: another run puts CLIProxyAPI's binary back at the binary's
// path between this run's move and its copy. Nothing removes or replaces it:
// the copy's file is removed, the move back fails, and the record says so.
#[test]
fn a_binary_put_back_by_another_run_is_never_removed_or_replaced() {
    let (mut fake, context) = bare_process();
    fake.installed = None;
    fake.on(
        &format!("rename {OPT_CPA} -> {OPT_MOVED}"),
        Effect::Place(OPT_CPA.to_owned(), b"put back by another run".to_vec()),
    );
    let ran = migrate(&mut fake, &context, &["-yes"]);
    assert_eq!(ran.code, 1, "{}", ran.all());
    has(
        &ran.err,
        "failed to move /opt/cpa/cli-proxy-api.cliproxyapi back to /opt/cpa/cli-proxy-api",
    );
    assert_eq!(fake.data(OPT_CPA), b"put back by another run");
    assert_eq!(fake.data(OPT_MOVED), CLIPROXYAPI);
    assert!(!fake.exists(&staged()));
    assert!(!happened(&fake, &format!("remove {OPT_CPA}")));
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switching");
}

// Not upstream's: -undo never replaces a file at the binary's path. One that
// appears after open-ferry's was set aside stays, CLIProxyAPI's saved binary
// stays where it is, and the record stays open.
#[test]
fn undo_never_replaces_a_file_that_appears_at_the_binary() {
    let (mut fake, context) = bare_process();
    assert_eq!(migrate(&mut fake, &context, &["-yes"]).code, 0);
    fake.links.remove(OPT_CPA);
    fake.file(OPT_CPA, OPEN_FERRY);
    fake.on(
        &format!("rename {OPT_CPA} -> /opt/cpa/cli-proxy-api.open-ferry"),
        Effect::Place(OPT_CPA.to_owned(), b"put there by someone".to_vec()),
    );
    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 1, "{}", undo.all());
    has(
        &undo.err,
        "failed to move /opt/cpa/cli-proxy-api.cliproxyapi back to /opt/cpa/cli-proxy-api",
    );
    assert_eq!(fake.data(OPT_CPA), b"put there by someone");
    assert_eq!(fake.data(OPT_MOVED), CLIPROXYAPI);
    assert_eq!(saved(&fake, LINUX_RECORD)["status"], "switched");
}
