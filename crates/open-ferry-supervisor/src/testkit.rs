//! Not upstream's: what the core's unit tests are made of (v3 13, "Unit
//! tests"). A scripted [`Driver`] whose workers answer the supervisor's
//! messages as told, a [`Disk`] in memory that records every write and fails
//! the ones it is told to, and a host whose mode a test changes. Time is
//! tokio's paused clock; the wall clock is a [`ManualClock`].
//!
//! Compiled for the crate's own tests only.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use open_ferry_update::data_dir::DataDir;
use open_ferry_update::fetch::BoxFuture;
use open_ferry_update::journal::{Artifact, Journal, JournalError, Outcome};
use open_ferry_update::runner::Runner;

use crate::clock::ManualClock;
use crate::core::{Command, Runtime, SwitchRequest, Tracked};
use crate::disk::{ArtifactError, Disk};
use crate::driver::{
    Cmd, Driver, Event, EventSender, ExitInfo, Registry, SpawnError, SpawnRequest, WorkerId,
    WorkerPort,
};
use crate::endpoint::ServiceKind;
use crate::host::{Host, ModeSnapshot};
use crate::install::InstallResult;
use crate::process::ProcessIdentity;
use crate::protocol::{Admission, EndpointReply, ListenerTopology, Message, Role};
use crate::snapshot::Shared;
use crate::startup::Config;
use crate::statefile::StateWriteError;
use crate::timings::{Backoff, Timings};
use open_ferry_update::journal::{How, WorkerRole};

pub const NAME: &str = "open-ferry";

/// What a scripted worker does when something happens to it.
#[derive(Debug, Clone)]
pub enum Step {
    Msg(Message),
    /// The process ends. In a `kill` script this is a kill that found it
    /// running, as the pump reports it.
    Exit(bool, &'static str),
    /// The process ends, and in a `kill` script the kill found it already
    /// gone (it died on its own first): the exit is not the kill's doing.
    Died(bool, &'static str),
    /// The channel closes; the process lives.
    Lost,
    Sleep(Duration),
}

pub fn handed_over() -> Step {
    Step::Msg(Message::HandedOver {
        state_id: "h".to_owned(),
        live: 0,
    })
}

pub fn loaded() -> Step {
    Step::Msg(Message::Loaded { ms: 5 })
}

#[derive(Default)]
pub struct Script {
    rules: HashMap<(String, String), Vec<Step>>,
    spawn_fail: HashMap<String, SpawnFail>,
    events: HashMap<String, (WorkerId, EventSender)>,
    pub log: Vec<String>,
    /// `send <version> <trigger>` for each message the worker's channel took,
    /// at the moment it was sent: unlike `log`, which a worker busy with an
    /// earlier script writes late, in its turn.
    pub received: Vec<String>,
    /// `<version> installed` or `<version> kept` for each spawn: the location
    /// the worker was started from, which `log` does not say.
    pub places: Vec<String>,
    /// While set, a child created by `Survive` ignores every kill.
    termination_held: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone)]
pub enum SpawnFail {
    Missing,
    Start,
    Incompatible,
    /// The real handshake's refusal of a worker that lacks a required
    /// capability: a descriptive `Handshake` error, as `control` returns it.
    MissingCap,
    /// The spawn never returns, and creates no child.
    Hang,
    /// The handshake takes this long, then the worker is healthy.
    Delay(Duration),
    /// A child is created (the registry tracks it as a survivor) and the
    /// spawn never returns; dropping the spawn tells the child to end. It
    /// exits only once `FakeDriver::hold_termination(false)`.
    Survive,
    /// As `Survive`, but the spawn fails with a timeout after this long.
    SurviveThenFail(Duration),
    /// The driver's trust refusal (2.5): the environment's fault.
    Untrusted,
    /// The driver's refusal while an earlier spawn's child is unreaped: this
    /// many spawns are refused, then the version is healthy.
    SurvivorPending(u32),
    /// Refused as untrusted only when the binary is not the installed file
    /// (a kept `versions/<v>` copy): the same bytes run from the installed
    /// path. Logs `refused kept <v>` and `ran installed <v>`.
    UntrustedKept,
    /// The start fails (an attempt) only from the installed file; the same
    /// bytes in a kept copy start. Logs `ran kept <v>` for those.
    StartInstalled,
    /// The real driver's `verify_binary` for a kept copy (driver.rs:387):
    /// the file at the request's path is hashed and compared to the request's
    /// hash; a mismatch (or an unreadable file) is `SpawnError::HashMismatch`
    /// (`Missing` when there is no file), a match starts healthy. The installed
    /// path is not checked here, as the real driver's is by its own hash.
    VerifyKept,
    /// One outcome per spawn, in order; healthy once they are used up.
    Script(Vec<SpawnFail>),
    /// Runs the hook as the spawn returns, then fails it to start: a cancel
    /// accepted at that very moment is ready along with the failure.
    HookThenStart(SpawnHook),
}

/// A closure a test runs at a spawn's return.
#[derive(Clone)]
pub struct SpawnHook(pub Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for SpawnHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpawnHook")
    }
}

/// A driver whose workers follow a [`Script`].
#[derive(Clone, Default)]
pub struct FakeDriver {
    pub script: Arc<Mutex<Script>>,
    pid: Arc<AtomicU32>,
}

impl FakeDriver {
    /// What version `version` does on `trigger` (`spawn`, `hand_over`,
    /// `resume`, `take_over`, `go`, `release`, `stop`, `ping`,
    /// `peers_gone`), replacing the healthy default.
    pub fn on(&self, version: &str, trigger: &str, steps: Vec<Step>) {
        self.script
            .lock()
            .unwrap()
            .rules
            .insert((version.to_owned(), trigger.to_owned()), steps);
    }

    pub fn fail_spawn(&self, version: &str, how: SpawnFail) {
        self.script
            .lock()
            .unwrap()
            .spawn_fail
            .insert(version.to_owned(), how);
    }

    /// `version` starts again.
    pub fn heal_spawn(&self, version: &str) {
        self.script.lock().unwrap().spawn_fail.remove(version);
    }

    /// Whether a child of a failed or cancelled handshake ignores kills.
    pub fn hold_termination(&self, held: bool) {
        self.script
            .lock()
            .unwrap()
            .termination_held
            .store(held, Ordering::SeqCst);
    }

    pub fn emit(&self, version: &str, event: Event) {
        let script = self.script.lock().unwrap();
        let (id, events) = script.events.get(version).expect("a spawned worker");
        events.send((*id, event)).unwrap();
    }

    pub fn log(&self) -> Vec<String> {
        self.script.lock().unwrap().log.clone()
    }

    /// The messages sent to workers, as `send <version> <trigger>`, noted when
    /// they were sent, not when the worker got to them.
    pub fn received(&self) -> Vec<String> {
        self.script.lock().unwrap().received.clone()
    }

    /// Where each spawn started from, as `<version> installed|kept`.
    pub fn places(&self) -> Vec<String> {
        self.script.lock().unwrap().places.clone()
    }

    /// The log lines that start with `prefix`.
    pub fn log_of(&self, prefix: &str) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|line| line.starts_with(prefix))
            .collect()
    }
}

fn trigger(message: &Message) -> &'static str {
    match message {
        Message::HandOver { .. } => "hand_over",
        Message::Resume => "resume",
        Message::TakeOver => "take_over",
        Message::Go => "go",
        Message::PeersGone => "peers_gone",
        Message::Release => "release",
        Message::Stop { .. } => "stop",
        Message::Ping => "ping",
        _ => "other",
    }
}

fn default_steps(role: &Role, on: &str) -> Vec<Step> {
    match on {
        "spawn" if *role == Role::Standby => vec![loaded()],
        "hand_over" => vec![handed_over()],
        "resume" => vec![Step::Msg(Message::Resumed)],
        "take_over" => vec![Step::Msg(Message::Ready)],
        "release" => vec![Step::Msg(Message::Drained), Step::Exit(true, "drained")],
        "stop" => vec![Step::Exit(true, "stopped")],
        "kill" => vec![Step::Exit(false, "killed")],
        "ping" => vec![Step::Msg(Message::Status {
            mode: "active".to_owned(),
            live: 0,
            signins_pending: 0,
            phase: "serving".to_owned(),
        })],
        _ => Vec::new(),
    }
}

impl Driver for FakeDriver {
    fn spawn(
        &self,
        request: SpawnRequest,
        events: EventSender,
    ) -> impl std::future::Future<Output = Result<WorkerPort, SpawnError>> + Send {
        let script = self.script.clone();
        let pid = self.pid.fetch_add(1, Ordering::SeqCst) + 100;
        async move {
            let fail = {
                let mut script = script.lock().unwrap();
                script.places.push(format!(
                    "{} {}",
                    request.version,
                    if request.binary == request.installation.installed {
                        "installed"
                    } else {
                        "kept"
                    }
                ));
                script.log.push(format!(
                    "spawn {} {}",
                    request.version,
                    if request.role == Role::Active {
                        "active"
                    } else {
                        "standby"
                    }
                ));
                match script.spawn_fail.get(&request.version).cloned() {
                    Some(SpawnFail::Script(mut steps)) => {
                        let first = (!steps.is_empty()).then(|| steps.remove(0));
                        if steps.is_empty() {
                            script.spawn_fail.remove(&request.version);
                        } else {
                            script
                                .spawn_fail
                                .insert(request.version.clone(), SpawnFail::Script(steps));
                        }
                        first
                    }
                    other => other,
                }
            };
            match fail {
                Some(SpawnFail::Untrusted) => {
                    return Err(SpawnError::Untrusted("not owned by the user".to_owned()));
                }
                Some(SpawnFail::UntrustedKept) => {
                    let mut script = script.lock().unwrap();
                    if request.binary == request.installation.installed {
                        script
                            .log
                            .push(format!("ran installed {}", request.version));
                    } else {
                        script.log.push(format!("refused kept {}", request.version));
                        return Err(SpawnError::Untrusted("not owned by the user".to_owned()));
                    }
                }
                Some(SpawnFail::StartInstalled) => {
                    if request.binary == request.installation.installed {
                        return Err(SpawnError::Start(io::Error::other("no exec")));
                    }
                    script
                        .lock()
                        .unwrap()
                        .log
                        .push(format!("ran kept {}", request.version));
                }
                Some(SpawnFail::VerifyKept) => {
                    if request.binary != request.installation.installed {
                        match open_ferry_update::staging::hash_file(&request.binary) {
                            Ok(found) if found.eq_ignore_ascii_case(&request.sha256) => {}
                            Ok(found) => {
                                return Err(SpawnError::HashMismatch {
                                    expected: request.sha256.clone(),
                                    found,
                                });
                            }
                            Err(_) => return Err(SpawnError::Missing),
                        }
                    }
                }
                Some(SpawnFail::SurvivorPending(times)) => {
                    let mut script = script.lock().unwrap();
                    if times <= 1 {
                        script.spawn_fail.remove(&request.version);
                    } else {
                        script.spawn_fail.insert(
                            request.version.clone(),
                            SpawnFail::SurvivorPending(times - 1),
                        );
                    }
                    return Err(SpawnError::SurvivorPending);
                }
                Some(SpawnFail::Missing) => return Err(SpawnError::Missing),
                Some(SpawnFail::Start) => {
                    return Err(SpawnError::Start(io::Error::other("no exec")));
                }
                Some(SpawnFail::HookThenStart(hook)) => {
                    (hook.0)();
                    return Err(SpawnError::Start(io::Error::other("no exec")));
                }
                Some(SpawnFail::Incompatible) => {
                    return Err(SpawnError::Incompatible {
                        needs_supervisor: 9,
                    });
                }
                Some(SpawnFail::MissingCap) => {
                    return Err(SpawnError::Handshake(
                        "the worker is missing required capabilities: required-test-cap".to_owned(),
                    ));
                }
                Some(SpawnFail::Hang) => std::future::pending::<()>().await,
                Some(SpawnFail::Delay(wait)) => tokio::time::sleep(wait).await,
                Some(survive @ (SpawnFail::Survive | SpawnFail::SurviveThenFail(_))) => {
                    let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
                    let port = WorkerPort {
                        id: request.id,
                        identity: ProcessIdentity {
                            pid,
                            started: u64::from(pid),
                        },
                        reported_version: request.version.clone(),
                        protocol: 0,
                        caps: Vec::new(),
                        cmd: cmd.clone(),
                    };
                    events.track_survivor(port);
                    let held = script.lock().unwrap().termination_held.clone();
                    let (id, version, events_for_child) =
                        (request.id, request.version.clone(), events.clone());
                    let child_script = script.clone();
                    tokio::spawn(async move {
                        // The child: ends on a kill the test has not held.
                        let mut asked = false;
                        loop {
                            tokio::select! {
                                command = rx.recv() => match command {
                                    Some(Cmd::Kill) => {
                                        asked = true;
                                        child_script.lock().unwrap().log.push(format!("kill {version}"));
                                    }
                                    Some(_) => {}
                                    None => break,
                                },
                                () = tokio::time::sleep(Duration::from_millis(50)) => {}
                            }
                            if asked && !held.load(Ordering::SeqCst) {
                                break;
                            }
                        }
                        child_script
                            .lock()
                            .unwrap()
                            .log
                            .push(format!("exited {version}"));
                        let _ = events_for_child.send((
                            id,
                            Event::Exited(ExitInfo {
                                code: Some(1),
                                success: false,
                                text: "killed".to_owned(),
                            }),
                        ));
                    });
                    // Dropping the spawn, or failing it, tells the child to end,
                    // as the real driver's cleanup does.
                    struct EndsChild(tokio::sync::mpsc::UnboundedSender<Cmd>);
                    impl Drop for EndsChild {
                        fn drop(&mut self) {
                            let _ = self.0.send(Cmd::Kill);
                        }
                    }
                    let _ends = EndsChild(cmd);
                    match survive {
                        SpawnFail::SurviveThenFail(wait) => {
                            tokio::time::sleep(wait).await;
                            return Err(SpawnError::Timeout);
                        }
                        _ => std::future::pending::<()>().await,
                    }
                }
                Some(SpawnFail::Script(_)) | None => {}
            }
            let (cmd, mut rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
            let port = WorkerPort {
                id: request.id,
                identity: ProcessIdentity {
                    pid,
                    started: u64::from(pid),
                },
                reported_version: request.version.clone(),
                protocol: 1,
                caps: Vec::new(),
                cmd,
            };
            let id = request.id;
            script
                .lock()
                .unwrap()
                .events
                .insert(request.version.clone(), (id, events.clone()));
            let version = request.version.clone();
            let role = request.role.clone();
            // The channel is read at once, to note each message when it is
            // sent; the worker plays them in order, one script at a time.
            let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
            {
                let (script, version) = (script.clone(), version.clone());
                tokio::spawn(async move {
                    while let Some(command) = rx.recv().await {
                        if let Cmd::Send(message) = &command {
                            script
                                .lock()
                                .unwrap()
                                .received
                                .push(format!("send {version} {}", trigger(message)));
                        }
                        if turn_tx.send(command).is_err() {
                            break;
                        }
                    }
                });
            }
            tokio::spawn(async move {
                // `spawn` steps run once the supervisor has the port.
                tokio::task::yield_now().await;
                let mut exited = run(&script, &events, id, &version, &role, "spawn").await;
                while !exited {
                    match turn_rx.recv().await {
                        Some(Cmd::Send(message)) => {
                            let on = trigger(&message);
                            script
                                .lock()
                                .unwrap()
                                .log
                                .push(format!("send {version} {on}"));
                            exited = run(&script, &events, id, &version, &role, on).await;
                        }
                        Some(Cmd::Close) => {
                            script.lock().unwrap().log.push(format!("close {version}"));
                        }
                        Some(Cmd::Kill) => {
                            script.lock().unwrap().log.push(format!("kill {version}"));
                            exited = run(&script, &events, id, &version, &role, "kill").await;
                        }
                        None => {
                            let _ = events.send((
                                id,
                                Event::Exited(ExitInfo {
                                    code: None,
                                    success: false,
                                    text: "closed".to_owned(),
                                }),
                            ));
                            exited = true;
                        }
                    }
                }
            });
            Ok(port)
        }
    }
}

/// Plays one trigger's steps. `true` when the process ended.
async fn run(
    script: &Arc<Mutex<Script>>,
    events: &EventSender,
    id: u64,
    version: &str,
    role: &Role,
    on: &str,
) -> bool {
    let steps = {
        let script = script.lock().unwrap();
        script
            .rules
            .get(&(version.to_owned(), on.to_owned()))
            .cloned()
            .unwrap_or_else(|| default_steps(role, on))
    };
    for step in steps {
        match step {
            Step::Msg(message) => {
                let _ = events.send((id, Event::Message(message)));
            }
            Step::Sleep(duration) => tokio::time::sleep(duration).await,
            Step::Lost => {
                let _ = events.send((id, Event::Lost("the channel closed".to_owned())));
            }
            Step::Exit(success, text) | Step::Died(success, text) => {
                if on == "kill" && matches!(step, Step::Exit(..)) {
                    events.mark_terminated(id);
                }
                let _ = events.send((
                    id,
                    Event::Exited(ExitInfo {
                        code: Some(i32::from(!success)),
                        success,
                        text: text.to_owned(),
                    }),
                ));
                return true;
            }
        }
    }
    false
}

// ---- host -------------------------------------------------------------------

pub struct FakeHost {
    pub mode: Mutex<ModeSnapshot>,
    /// The hash this supervisor binary reports as its own (`None`: unknown).
    pub own_sha256: Mutex<Option<String>>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            mode: Mutex::new(ModeSnapshot {
                off: false,
                mode_gen: 0,
            }),
            own_sha256: Mutex::new(None),
        }
    }
}

impl FakeHost {
    pub fn set_mode(&self, off: bool, mode_gen: u64) {
        *self.mode.lock().unwrap() = ModeSnapshot { off, mode_gen };
    }
}

impl Host for FakeHost {
    fn topology(&self) -> Vec<ListenerTopology> {
        Vec::new()
    }
    fn bind(&self) -> io::Result<Vec<(String, TcpListener)>> {
        Ok(Vec::new())
    }
    fn effective_mode(&self) -> ModeSnapshot {
        *self.mode.lock().unwrap()
    }
    fn own_version(&self) -> String {
        "1.0.0".to_owned()
    }
    fn own_sha256(&self) -> Option<String> {
        self.own_sha256.lock().unwrap().clone()
    }
    fn waiting_for_update_lock(&self, _waited: Duration) {}
}

/// `--version` as the binary at `versions/<version>/...` would print it.
pub struct FakeRunner;

impl Runner for FakeRunner {
    fn version<'a>(
        &'a self,
        binary: &'a Path,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        let version = binary.parent().and_then(Path::file_name).map_or_else(
            || "1.0.0".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        Box::pin(async move { Ok(format!("open-ferry {version}")) })
    }
}

/// A binary whose `--version` fails or prints something fixed.
pub struct ScriptedRunner(pub Result<String, String>);

impl Runner for ScriptedRunner {
    fn version<'a>(
        &'a self,
        _binary: &'a Path,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        let answer = self.0.clone();
        Box::pin(async move { answer })
    }
}

// ---- disk -------------------------------------------------------------------

#[derive(Default)]
pub struct DiskState {
    /// The journal as the last successful write left it.
    pub on_disk: Option<Journal>,
    /// Every journal write tried: name and whether it landed.
    pub journal_writes: Vec<(String, bool)>,
    /// Names whose writes fail, and how many more times.
    pub fail_journal: HashMap<String, u32>,
    /// Writes called `name` that land before `fail_journal` starts to bite.
    pub fail_journal_after: HashMap<String, u32>,
    /// Every journal that reached the disk, by write name, as it was written.
    pub journal_log: Vec<(String, Journal)>,
    pub fail_state: HashMap<String, u32>,
    /// Names whose writes land but report `NotDurable`, and how many more times.
    pub not_durable_journal: HashMap<String, u32>,
    /// Names whose state writes land but report an error (the directory sync
    /// failed), and how many more times: the same as `save` does on a write
    /// that landed unsynced.
    pub not_durable_state: HashMap<String, u32>,
    pub state_writes: Vec<(String, Outcome, bool)>,
    /// The records the state file holds, by id, merged.
    pub state: Vec<Outcome>,
    pub artifacts: HashMap<String, String>,
    pub pins: HashSet<String>,
    pub consumed: Vec<String>,
    pub installed: Option<String>,
    pub installed_size: Option<u64>,
    /// The installed path is a link: the switched-off measurement, which does
    /// not follow one, sees nothing, though the following measurements do.
    pub installed_linked: bool,
    pub install_calls: Vec<String>,
    pub install_fail: u32,
    /// Directory syncs of the installed file that fail: the install that
    /// lands the file, then each retry that finds it in place.
    pub dir_sync_fail: u32,
    /// Why the kept copy is not trusted at the install (checked each time).
    pub untrusted_source: Option<String>,
    pub handover_removed: u32,
    pub cancel_during_j3: Option<Arc<Shared>>,
    pub j3_cancel_answer: Option<crate::snapshot::CancelAnswer>,
    /// Everything done to the disk, in order, as `J1`, `S1`, `install`, ...
    pub order: Vec<String>,
    /// The prunes asked for, in order (not in `order`: a prune is not part of a
    /// switch's sequence).
    pub prunes: Vec<crate::disk::PruneRequest>,
    /// What the real disk answered each prune with (`real` only), as `Debug`.
    pub real_outcomes: Vec<String>,
    /// The outcomes the next prunes answer with; `Done` with nothing after.
    pub prune_script: std::collections::VecDeque<crate::disk::PruneOutcome>,
    /// The version directories the fake disk lists, for `all_versions`.
    pub versions: std::collections::BTreeSet<String>,
    /// Versions the fake keep set adds to what it computes from the journal.
    pub extra_keep: std::collections::BTreeSet<String>,
    /// Makes the fake keep set fail with this reason.
    pub keep_fails: Option<String>,
    /// Makes the fake version listing fail with this reason.
    pub versions_fail: Option<String>,
    /// How many times a keep set was computed.
    pub keep_calls: u32,
    /// Runs as a pin is consumed, before it is recorded: to see what was
    /// published while the pin still stood.
    pub on_consume: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
pub struct FakeDisk {
    pub state: Mutex<DiskState>,
    /// A real disk over a temp data dir. When set, the keep set, the version
    /// list and the prune are the real ones (the rest stays scripted).
    pub real: Option<crate::disk::RealDisk>,
    /// With `real`, the installed file and the install transaction are the
    /// real ones too (the update lock is then really taken).
    pub real_install: bool,
    /// With `real`, the state writes are the real ones too (the update lock is
    /// then really taken, and the state file is really read and replaced).
    pub real_state: bool,
    /// With `real`, a journal write that lands (also one reported as not
    /// durable) lands in the real store too, so the real keep set reads it.
    pub real_journal: bool,
}

impl FakeDisk {
    pub fn add_artifact(&self, version: &str, sha: &str) {
        self.state
            .lock()
            .unwrap()
            .artifacts
            .insert(version.to_owned(), sha.to_owned());
    }

    pub fn add_pin(&self, id: &str) {
        self.state.lock().unwrap().pins.insert(id.to_owned());
    }

    pub fn fail_journal(&self, name: &str, times: u32) {
        self.state
            .lock()
            .unwrap()
            .fail_journal
            .insert(name.to_owned(), times);
    }

    pub fn not_durable_journal(&self, name: &str, times: u32) {
        self.state
            .lock()
            .unwrap()
            .not_durable_journal
            .insert(name.to_owned(), times);
    }

    /// The next `times` state writes called `name` land, then report an error.
    pub fn not_durable_state(&self, name: &str, times: u32) {
        self.state
            .lock()
            .unwrap()
            .not_durable_state
            .insert(name.to_owned(), times);
    }

    /// The journals that reached the disk, with the write that made each.
    pub fn journal_log(&self) -> Vec<(String, Journal)> {
        self.state.lock().unwrap().journal_log.clone()
    }

    /// Makes the installed path a link (see `installed_linked`).
    pub fn link_installed(&self) {
        self.state.lock().unwrap().installed_linked = true;
    }

    /// The next `times` journal writes called `name` fail, after `after` that land.
    pub fn fail_journal_after(&self, name: &str, after: u32, times: u32) {
        let mut state = self.state.lock().unwrap();
        state.fail_journal_after.insert(name.to_owned(), after);
        state.fail_journal.insert(name.to_owned(), times);
    }

    pub fn fail_state(&self, name: &str, times: u32) {
        self.state
            .lock()
            .unwrap()
            .fail_state
            .insert(name.to_owned(), times);
    }

    pub fn order(&self) -> Vec<String> {
        self.state.lock().unwrap().order.clone()
    }

    pub fn on_disk(&self) -> Option<Journal> {
        self.state.lock().unwrap().on_disk.clone()
    }

    /// The state file's record `id`.
    pub fn record(&self, id: &str) -> Option<open_ferry_update::state::SwitchRecord> {
        self.state
            .lock()
            .unwrap()
            .state
            .iter()
            .find(|outcome| outcome.record.id == id)
            .map(|outcome| outcome.record.clone())
    }
}

impl FakeDisk {
    /// With `real_journal`, puts the journal the scripted write landed into the
    /// real store as well. The real write takes the sequence number itself, so
    /// the copy starts one below the scripted journal's (already bumped) one and
    /// ends on the same; a real write that fails is a broken test, not ignored.
    fn mirror_journal(&self, journal: &Journal, name: &str) {
        if self.real_journal
            && let Some(real) = &self.real
        {
            let mut copy = journal.clone();
            copy.seq = journal.seq - 1;
            real.write_journal(&mut copy, name)
                .unwrap_or_else(|error| panic!("the real journal write {name} failed: {error:?}"));
            assert_eq!(copy.seq, journal.seq, "the real journal's sequence drifted");
        }
    }
}

impl Disk for FakeDisk {
    fn write_journal(&self, journal: &mut Journal, name: &str) -> Result<(), JournalError> {
        let mut state = self.state.lock().unwrap();
        if name == "J3"
            && let Some(shared) = &state.cancel_during_j3
        {
            state.j3_cancel_answer = Some(shared.request_cancel());
        }
        let skipped = match state.fail_journal_after.get_mut(name) {
            Some(skip) if *skip > 0 => {
                *skip -= 1;
                true
            }
            _ => false,
        };
        if !skipped
            && let Some(left) = state.fail_journal.get_mut(name)
            && *left > 0
        {
            *left -= 1;
            state.journal_writes.push((name.to_owned(), false));
            state.order.push(format!("{name}!"));
            return Err(JournalError::Io(io::Error::other("scripted failure")));
        }
        if let Some(left) = state.not_durable_journal.get_mut(name)
            && *left > 0
        {
            // Landed, but the directory sync failed.
            *left -= 1;
            journal.seq += 1;
            state.on_disk = Some(journal.clone());
            state.journal_log.push((name.to_owned(), journal.clone()));
            self.mirror_journal(journal, name);
            state.journal_writes.push((name.to_owned(), true));
            state.order.push(format!("{name}~"));
            return Err(JournalError::NotDurable(io::Error::other("scripted sync")));
        }
        journal.seq += 1;
        state.on_disk = Some(journal.clone());
        state.journal_log.push((name.to_owned(), journal.clone()));
        self.mirror_journal(journal, name);
        state.journal_writes.push((name.to_owned(), true));
        state.order.push(name.to_owned());
        Ok(())
    }

    fn write_state(&self, name: &str, outcome: &Outcome) -> Result<(), StateWriteError> {
        let unsynced = |state: &mut DiskState| match state.not_durable_state.get_mut(name) {
            Some(left) if *left > 0 => {
                *left -= 1;
                true
            }
            _ => false,
        };
        if self.real_state
            && let Some(real) = &self.real
        {
            let mut written = real.write_state(name, outcome);
            let mut state = self.state.lock().unwrap();
            if written.is_ok() && unsynced(&mut state) {
                // Landed in the file; only its sync is reported failed.
                written = Err(StateWriteError::Io(io::Error::other("scripted sync")));
                state
                    .state_writes
                    .push((name.to_owned(), outcome.clone(), true));
                state.order.push(format!("{name}~"));
                return written;
            }
            let landed = written.is_ok();
            state
                .state_writes
                .push((name.to_owned(), outcome.clone(), landed));
            state.order.push(if landed {
                name.to_owned()
            } else {
                format!("{name}!")
            });
            return written;
        }
        let mut state = self.state.lock().unwrap();
        if let Some(left) = state.fail_state.get_mut(name)
            && *left > 0
        {
            *left -= 1;
            state
                .state_writes
                .push((name.to_owned(), outcome.clone(), false));
            state.order.push(format!("{name}!"));
            return Err(StateWriteError::Busy);
        }
        let unsynced = unsynced(&mut state);
        match state
            .state
            .iter_mut()
            .find(|held| held.record.id == outcome.record.id)
        {
            Some(held) => {
                held.record.merge(&outcome.record);
                if outcome.mark_failed.is_some() {
                    held.mark_failed.clone_from(&outcome.mark_failed);
                }
            }
            None => state.state.push(outcome.clone()),
        }
        state
            .state_writes
            .push((name.to_owned(), outcome.clone(), true));
        if unsynced {
            state.order.push(format!("{name}~"));
            return Err(StateWriteError::Io(io::Error::other("scripted sync")));
        }
        state.order.push(name.to_owned());
        Ok(())
    }

    fn artifact(&self, version: &str, sha256: &str) -> Result<Artifact, ArtifactError> {
        let state = self.state.lock().unwrap();
        match state.artifacts.get(version) {
            None => Err(ArtifactError::Missing),
            Some(found) if !found.eq_ignore_ascii_case(sha256) => Err(ArtifactError::Mismatch {
                found: found.clone(),
            }),
            Some(found) => Ok(Artifact {
                version: version.to_owned(),
                sha256: found.clone(),
                size: 10,
            }),
        }
    }

    fn check_pin(
        &self,
        id: &str,
        _version: &str,
        _sha256: &str,
        _now: DateTime<Utc>,
    ) -> Result<(), String> {
        if self.state.lock().unwrap().pins.contains(id) {
            Ok(())
        } else {
            Err("there is no live pin with that id".to_owned())
        }
    }

    fn consume_pin(&self, id: &str) -> Result<(), String> {
        let hook = self.state.lock().unwrap().on_consume.clone();
        if let Some(hook) = hook {
            hook();
        }
        let mut state = self.state.lock().unwrap();
        state.consumed.push(id.to_owned());
        state.order.push(format!("pin:{id}"));
        state.pins.remove(id);
        Ok(())
    }

    fn installed_sha256(&self) -> Option<String> {
        if self.real_install
            && let Some(real) = &self.real
        {
            return real.installed_sha256();
        }
        self.state.lock().unwrap().installed.clone()
    }

    fn installed_switched_off(&self) -> (Option<String>, u64) {
        if self.state.lock().unwrap().installed_linked {
            return (None, 0);
        }
        if self.real_install
            && let Some(real) = &self.real
        {
            return real.installed_switched_off();
        }
        (self.installed_sha256(), self.installed_size().unwrap_or(0))
    }

    fn installed_size(&self) -> Option<u64> {
        if self.real_install
            && let Some(real) = &self.real
        {
            return real.installed_size();
        }
        let state = self.state.lock().unwrap();
        state
            .installed_size
            .or_else(|| state.installed.as_ref().map(|_| 1))
    }

    fn choose_aside(&self) -> Result<String, String> {
        if self.real_install
            && let Some(real) = &self.real
        {
            return real.choose_aside();
        }
        Ok("open-ferry.old".to_owned())
    }

    async fn install_transaction(&self, journal: Journal) -> (Journal, InstallResult) {
        match &self.real {
            Some(real) if self.real_install => real.install_transaction(journal).await,
            _ => crate::disk::install_transaction(self, journal),
        }
    }

    fn install(
        &self,
        target: &Artifact,
        _expected: Option<&str>,
        _from: &str,
        _aside: Option<&str>,
    ) -> InstallResult {
        let mut state = self.state.lock().unwrap();
        state.install_calls.push(target.version.clone());
        state.order.push("install".to_owned());
        if state.install_fail > 0 {
            state.install_fail -= 1;
            return InstallResult::Failed("scripted install failure".to_owned());
        }
        state.installed = Some(target.sha256.clone());
        if state.dir_sync_fail > 0 {
            state.dir_sync_fail -= 1;
            return InstallResult::Failed("the directory sync failed".to_owned());
        }
        InstallResult::Replaced
    }

    fn sync_installed_directory(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        state.order.push("dir-sync".to_owned());
        if state.dir_sync_fail > 0 {
            state.dir_sync_fail -= 1;
            return Err("scripted sync failure".to_owned());
        }
        Ok(())
    }

    fn check_install_source(&self, _version: &str) -> Result<(), String> {
        match &self.state.lock().unwrap().untrusted_source {
            Some(why) => Err(why.clone()),
            None => Ok(()),
        }
    }

    fn remove_handover(&self) {
        let mut state = self.state.lock().unwrap();
        state.handover_removed += 1;
        state.order.push("handover-removed".to_owned());
    }

    fn keep_set(
        &self,
        memory: Option<&Journal>,
        tracked: &[String],
        now: DateTime<Utc>,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        self.state.lock().unwrap().keep_calls += 1;
        if let Some(real) = &self.real {
            return real.keep_set(memory, tracked, now);
        }
        let state = self.state.lock().unwrap();
        if let Some(why) = &state.keep_fails {
            return Err(why.clone());
        }
        let mut keep: std::collections::BTreeSet<String> = tracked.iter().cloned().collect();
        keep.extend(state.extra_keep.iter().cloned());
        if let Some(journal) = memory {
            keep.extend(journal.keep_set(&[], now, |_, _| true));
        }
        Ok(keep)
    }

    fn all_versions(&self) -> Result<std::collections::BTreeSet<String>, String> {
        match &self.real {
            Some(real) => real.all_versions(),
            None => {
                let state = self.state.lock().unwrap();
                match &state.versions_fail {
                    Some(why) => Err(why.clone()),
                    None => Ok(state.versions.clone()),
                }
            }
        }
    }

    async fn prune(&self, request: crate::disk::PruneRequest) -> crate::disk::PruneOutcome {
        if let Some(real) = &self.real {
            self.state.lock().unwrap().prunes.push(request.clone());
            let outcome = real.prune(request).await;
            self.state
                .lock()
                .unwrap()
                .real_outcomes
                .push(format!("{outcome:?}"));
            return outcome;
        }
        let mut state = self.state.lock().unwrap();
        state.prunes.push(request);
        state
            .prune_script
            .pop_front()
            .unwrap_or(crate::disk::PruneOutcome::Done {
                removed: Vec::new(),
                pins_removed: 0,
            })
    }
}

// ---- the rig ------------------------------------------------------------------

pub const SHA_1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
pub const SHA_2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
pub const SHA_3: &str = "3333333333333333333333333333333333333333333333333333333333333333";

pub fn artifact(version: &str) -> Artifact {
    let sha = match version {
        "1.0.0" => SHA_1,
        "2.0.0" => SHA_2,
        _ => SHA_3,
    };
    Artifact {
        version: version.to_owned(),
        sha256: sha.to_owned(),
        size: 10,
    }
}

pub struct Rig {
    pub cfg: Config,
    pub driver: FakeDriver,
    pub host: FakeHost,
    pub disk: FakeDisk,
    pub clock: ManualClock,
    pub registry: Registry,
    pub shared: Arc<Shared>,
    pub commands: tokio::sync::mpsc::UnboundedReceiver<Command>,
    pub command_tx: tokio::sync::mpsc::UnboundedSender<Command>,
    pub rt: Option<Runtime>,
}

impl Rig {
    /// A supervisor with `1.0.0` running as the active worker, `2.0.0` and
    /// `0.9.0` kept, a pin `pin` for `2.0.0`.
    pub async fn new() -> Self {
        let timings = Timings::default();
        let cfg = Config {
            data: DataDir::at(PathBuf::from("/unit-test-data")),
            installed: PathBuf::from("/unit-test-bin").join(NAME),
            config: PathBuf::from("/unit-test-config.yaml"),
            kind: ServiceKind::User,
            timings,
            receipt: serde_json::json!({}),
            binary_name: NAME.to_owned(),
            runner: Arc::new(FakeRunner),
            detect_timeout: Duration::from_secs(5),
            #[cfg(any(test, feature = "blue-green-test"))]
            test_endpoint_location: None,
        };
        let driver = FakeDriver::default();
        let disk = FakeDisk::default();
        disk.add_artifact("1.0.0", SHA_1);
        disk.add_artifact("2.0.0", SHA_2);
        disk.add_artifact("0.9.0", SHA_3);
        disk.add_pin("pin");
        disk.state.lock().unwrap().installed = Some(SHA_1.to_owned());
        let mut registry = Registry::new();
        let (command_tx, commands) = tokio::sync::mpsc::unbounded_channel();
        let mut journal = Journal::first_start(artifact("1.0.0"), None);
        journal.installed = Some(open_ferry_update::journal::Installed::from(&artifact(
            "1.0.0",
        )));
        journal.seq = 1;
        disk.state.lock().unwrap().on_disk = Some(journal.clone());
        let now = Instant::now();
        let mut rt = Runtime {
            journal: Some(journal),
            journal_stale: false,
            journal_retry: None,
            pending: Vec::new(),
            run: None,
            disabled: None,
            degraded: None,
            keep_all: false,
            listeners: Arc::new(Vec::new()),
            workers: std::collections::BTreeMap::new(),
            active: None,
            watch: None,
            returning: None,
            restart: None,
            backoff: Backoff::new(&timings),
            install_due: None,
            prune_due: None,
            prune_refusals: 0,
            needs_restart: false,
            switching: false,
            stop_requested: false,
            view: None,
            untrusted: Vec::new(),
            ladder_off: None,
            switched_off: false,
            settle_snapshot: None,
        };
        // The active worker, started the way the core would.
        let request = SpawnRequest {
            id: registry.next_id(),
            version: "1.0.0".to_owned(),
            sha256: SHA_1.to_owned(),
            binary: cfg.data.binary("1.0.0", NAME),
            role: Role::Active,
            switch_id: None,
            config: cfg.config.clone(),
            installation: crate::protocol::Installation {
                installed: cfg.installed.clone(),
                data_dir: cfg.data.root().to_path_buf(),
                receipt: cfg.receipt.clone(),
            },
            topology: Vec::new(),
            listeners: Arc::new(Vec::new()),
            budgets: timings.budgets(),
            handshake_timeout: timings.load,
            log: None,
        };
        let id = request.id;
        let port = driver
            .spawn(request, registry.events())
            .await
            .expect("spawn A");
        let tracked = Tracked::new(artifact("1.0.0"), WorkerRole::Active, &port, now, true);
        registry.add(port);
        rt.workers.insert(id, tracked);
        rt.active = Some(id);
        rt.run = Some(crate::startup::Run {
            artifact: artifact("1.0.0"),
            binary: cfg.data.binary("1.0.0", NAME),
        });
        Self {
            cfg,
            driver,
            host: FakeHost::default(),
            disk,
            clock: ManualClock::new(Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()),
            registry,
            shared: Shared::new(),
            commands,
            command_tx,
            rt: Some(rt),
        }
    }
}

/// A core over the rig's parts, borrowing them one by one so a test can still
/// look at the disk and the driver while it runs.
macro_rules! core_of {
    ($rig:expr) => {
        $crate::core::Core::new(
            &$rig.cfg,
            &$rig.driver,
            &$rig.host,
            &$rig.disk,
            &$rig.clock,
            &mut $rig.registry,
            $rig.shared.clone(),
            &mut $rig.commands,
            $rig.rt.take().expect("the runtime"),
        )
    };
}
pub(crate) use core_of;

use tokio::time::Instant;

/// A manual or automatic update to `2.0.0`.
pub fn update(auto: bool) -> SwitchRequest {
    SwitchRequest {
        to: "2.0.0".to_owned(),
        sha256: SHA_2.to_owned(),
        pin: "pin".to_owned(),
        how: How::Update,
        auto,
        admitted: Admission {
            while_off: false,
            mode_gen: 0,
        },
    }
}

/// Drains the replies a switch sent.
pub fn drain(
    replies: &mut tokio::sync::mpsc::UnboundedReceiver<EndpointReply>,
) -> Vec<EndpointReply> {
    let mut all = Vec::new();
    while let Ok(reply) = replies.try_recv() {
        all.push(reply);
    }
    all
}

pub fn done_result(replies: &[EndpointReply]) -> serde_json::Value {
    replies
        .iter()
        .find_map(|reply| match reply {
            EndpointReply::Done { result, .. } => Some(result.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no done reply in {replies:?}"))
}

/// Not upstream's: running one test in a child process of the test binary,
/// under a parent deadline, as `open-ferry-update`'s isolated tests do. A
/// test that can hang on a regression (a FIFO open) or needs a process-wide
/// setting (the umask) uses it: the parent kills the child by its handle when
/// the deadline passes and requires the child's completion marker.
pub(crate) mod isolated {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const MARKER_ENV: &str = "OFP_S8_ISOLATED_DIR";

    /// In the child: the directory the completion marker goes in. `None` in
    /// the parent, which then calls [`run_in_child`].
    pub(crate) fn child_dir() -> Option<PathBuf> {
        std::env::var_os(MARKER_ENV).map(PathBuf::from)
    }

    /// In the child, at the end of the test body.
    pub(crate) fn mark_completed(dir: &std::path::Path) {
        std::fs::write(dir.join("completed"), b"completed").unwrap();
    }

    /// Runs the test `module_path!()::name` again, alone, in a child of this
    /// binary. It must exit successfully and write its marker within `limit`;
    /// otherwise the child is killed and waited for by its handle and this
    /// panics.
    pub(crate) fn run_in_child(module: &str, name: &str, limit: Duration) {
        let dir = tempfile::tempdir().unwrap();
        // The test's path inside the crate: `module_path!()` starts with the crate's name.
        let inside = module.split_once("::").map_or(module, |(_, rest)| rest);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{inside}::{name}"), "--test-threads=1"])
            .env(MARKER_ENV, dir.path())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated child of {name}: {status}");
                assert!(
                    dir.path().join("completed").is_file(),
                    "isolated child of {name} did not run its body"
                );
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("isolated child of {name} exceeded its {limit:?} deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
