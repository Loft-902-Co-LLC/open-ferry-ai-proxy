use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::net::IpAddr;
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use serde_json::Value;

use super::discover::file_name;
use super::machine::{Answer, Entry, EntryKind, Launch, Machine, Parent, Proc};
use super::switch::backup_dir;
use super::*;
use crate::check::{Finding, Level};
use crate::os_service::{Cmd, Context, Output, Owner, Platform, System};

/// What the fake's binaries hold: open-ferry's, and CLIProxyAPI's.
const OPEN_FERRY: &[u8] = b"open-ferry binary";
const CLIPROXYAPI: &[u8] = b"CLIProxyAPI binary";

/// What the probe says of CLIProxyAPI.
const THEIRS: &str = "CLIProxyAPI";

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
            broken: false,
            next_pid: 9001,
            events: Vec::new(),
            findings: Vec::new(),
            checked: Vec::new(),
            terminal: false,
            replies: VecDeque::new(),
            questions: Vec::new(),
            launches: Vec::new(),
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
        self.serving
            .push((process.pid, Answer::Other(THEIRS.to_owned())));
        self.procs.insert(process.pid, process);
    }

    fn data(&self, path: &str) -> &[u8] {
        self.files.get(path).map_or(&[], Vec::as_slice)
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
                        self.start(&exe, Vec::new(), None, None).unwrap();
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
        let data = self
            .files
            .get(exe)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        let pid = self.next_pid;
        self.next_pid += 1;
        self.procs.insert(
            pid,
            Proc {
                pid,
                name: file_name(self.platform, exe),
                exe: Some(exe.to_owned()),
                args,
                cwd,
                env,
                parent: None,
            },
        );
        if data == OPEN_FERRY {
            if !self.broken {
                self.serving.push((pid, Answer::OpenFerry));
            }
        } else {
            self.serving.push((pid, Answer::Other(THEIRS.to_owned())));
        }
        Ok(pid)
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
        self.files.contains_key(path) || self.dirs.contains(path)
    }

    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        if self.unreadable.contains(path) {
            return Err(denied());
        }
        self.files.get(path).cloned().ok_or_else(not_found)
    }

    fn real_path(&self, path: &str) -> io::Result<String> {
        Ok(path.to_owned())
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
        self.events.push(format!("remove {path}"));
        self.files.remove(path).map(|_| ()).ok_or_else(not_found)
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

    fn stop(&mut self, pid: u32, name: &str) -> io::Result<()> {
        self.events.push(format!("stop {pid}"));
        let Some(process) = self.procs.get(&pid) else {
            return Ok(());
        };
        if self.unstoppable.contains(&pid) {
            return Err(denied());
        }
        if !process.name.eq_ignore_ascii_case(name) {
            return Err(io::Error::other(format!("process {pid} isn't {name}")));
        }
        self.end(pid);
        self.fire(&format!("stop {pid}"));
        Ok(())
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
        Ok(entries
            .into_iter()
            .map(|(name, kind)| Entry { name, kind })
            .collect())
    }

    fn copy(&mut self, from: &str, to: &str) -> io::Result<()> {
        self.events.push(format!("copy {from} -> {to}"));
        let data = self.read(from)?;
        self.need_parent(to)?;
        self.files.insert(to.to_owned(), data);
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
        self.events.push(format!("rename {from} -> {to}"));
        self.need_parent(to)?;
        let data = self.files.remove(from).ok_or_else(not_found)?;
        self.files.insert(to.to_owned(), data);
        Ok(())
    }

    fn create_private_dir(&mut self, path: &str) -> io::Result<()> {
        self.events.push(format!("mkdir {path}"));
        if self.exists(path) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "it exists"));
        }
        self.need_parent(path)?;
        self.dirs.insert(path.to_owned());
        self.private.insert(path.to_owned());
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
    const READS: [&str; 11] = [
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
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n<Tasks>\r\n  <!-- \\Microsoft\\Windows\\Defrag\\ScheduledDefrag -->\r\n  <Task version=\"1.6\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n    <Settings><Enabled>true</Enabled></Settings>\r\n    <Actions Context=\"LocalSystem\"><Exec><Command>%windir%\\system32\\defrag.exe</Command><Arguments>-c -h -o</Arguments></Exec></Actions>\r\n  </Task>\r\n  <!-- {name} -->\r\n  <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n    <RegistrationInfo><URI>{name}</URI></RegistrationInfo>\r\n    <Settings><Enabled>{enabled}</Enabled></Settings>\r\n    <Actions Context=\"Author\">\r\n      <Exec>\r\n        <Command>{command}</Command>\r\n        <Arguments>{arguments}</Arguments>\r\n        <WorkingDirectory>{dir}</WorkingDirectory>\r\n      </Exec>\r\n    </Actions>\r\n  </Task>\r\n</Tasks>\r\n"
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
            &format!("copy {SYSTEMD_CONFIG} -> {}/config.yaml", systemd_backup()),
            &format!("write {LINUX_RECORD}"),
            &format!("write {}/migration.json", systemd_backup()),
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
        fake.data(&format!("{backup}/config.yaml")),
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
        "Turn the systemd user service cliproxyapi.service back on as it was: `systemctl --user enable cliproxyapi.service`, `systemctl --user start cliproxyapi.service`.",
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
    assert!(fake.exists(&format!("{}/config.yaml", systemd_backup())));

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
        &format!("Restored /home/me/cpa/config.yaml from {backup}/config.yaml"),
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
    fake.file(OPT_CPA, CLIPROXYAPI);
    fake.file("/opt/cpa/config.yaml", config_text("/opt/cpa/auths"));
    credentials(&mut fake, "/opt/cpa/auths");
    fake.process(Proc {
        pid: 5151,
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
        "Rename CLIProxyAPI's binary, /opt/cpa/cli-proxy-api, to /opt/cpa/cli-proxy-api.cliproxyapi, and copy open-ferry (/home/me/.local/bin/open-ferry) to /opt/cpa/cli-proxy-api, so that the program bash starts open-ferry.",
    );
    has(
        out,
        "`cli-proxy-api` in its place logs `open-ferry Version: ...` as it starts",
    );
    has(
        out,
        "Moved CLIProxyAPI's binary to /opt/cpa/cli-proxy-api.cliproxyapi",
    );
    has(out, "Copied open-ferry to /opt/cpa/cli-proxy-api");
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
    assert_eq!(
        record["cliproxyapi"]["started_by"],
        "the program bash (process 777)"
    );

    let undo = migrate(&mut fake, &context, &["-undo", "-yes"]);
    assert_eq!(undo.code, 0, "{}", undo.all());
    has(
        &undo.out,
        "Move CLIProxyAPI's binary back from /opt/cpa/cli-proxy-api.cliproxyapi to /opt/cpa/cli-proxy-api, and remove open-ferry's copy.",
    );
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
        "migrate: CLIProxyAPI's binary isn't at /opt/cpa/cli-proxy-api.cliproxyapi, so nothing was changed. Put it back at /opt/cpa/cli-proxy-api by hand.",
    );
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
