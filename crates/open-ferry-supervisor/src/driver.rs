//! Not upstream's: how the supervisor starts a worker and hears from it.
//!
//! The state machine never touches a process. It asks a [`Driver`] to start a
//! worker and gets back a [`WorkerPort`]: a way to send commands, and a stream
//! of [`Event`]s that arrives, tagged with the worker's id, on one channel.
//! [`ProcessDriver`] is the real one (a pump task per worker); tests script a
//! fake. A worker's last event is always `Exited`, and its messages come
//! before it, so nothing a worker said just before it died is lost.

use std::future::Future;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::control::{HandshakeError, WelcomeParams, handshake, worker_args};
use crate::endpoint::ServiceKind;
use crate::process::ProcessIdentity;
use crate::protocol::{Budgets, Installation, ListenerTopology, Message, Role, WindowsSocket};
use crate::worker::BoxedChannel;

/// The supervisor's name for one worker process, never reused.
pub type WorkerId = u64;

/// The sending half of the one channel every worker's events arrive on.
#[derive(Debug, Clone)]
pub struct EventSender {
    sender: UnboundedSender<(WorkerId, Event)>,
    survivors: Arc<std::sync::Mutex<std::collections::HashMap<WorkerId, WorkerPort>>>,
    /// Workers whose pump ended the process on a kill command and found it
    /// still running. The pump fills it before it emits `Exited`, so the
    /// registry reads it when that event arrives: a kill that was only
    /// requested, or that found the process already gone, is not in it. Beside
    /// `survivors` so `Event` and `ExitInfo` stay as they are.
    terminated: Arc<std::sync::Mutex<std::collections::HashSet<WorkerId>>>,
}

impl EventSender {
    /// For the scripted driver of the unit tests: the kill ended a running
    /// worker, as the pump's does.
    #[cfg(test)]
    pub(crate) fn mark_terminated(&self, id: WorkerId) {
        self.terminated.lock().unwrap().insert(id);
    }

    /// For the scripted driver of the unit tests: a child that exists but has
    /// no port yet, as the real driver's handshake leaves one.
    #[cfg(test)]
    pub(crate) fn track_survivor(&self, port: WorkerPort) {
        self.survivors.lock().unwrap().insert(port.id, port);
    }
}

impl std::ops::Deref for EventSender {
    type Target = UnboundedSender<(WorkerId, Event)>;
    fn deref(&self) -> &Self::Target {
        &self.sender
    }
}

/// Everything needed to start one worker.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub id: WorkerId,
    pub version: String,
    /// What the binary must hash to; checked just before it starts.
    pub sha256: String,
    pub binary: PathBuf,
    pub role: Role,
    pub switch_id: Option<String>,
    /// The absolute config path the worker is given.
    pub config: PathBuf,
    pub installation: Installation,
    pub topology: Vec<ListenerTopology>,
    /// The supervisor's own sockets, `main` first, with the names `topology` uses.
    pub listeners: Arc<Vec<(String, TcpListener)>>,
    pub budgets: Budgets,
    /// How long the worker has to connect and greet.
    pub handshake_timeout: Duration,
    /// Where its standard output and error go, appended; the null device if none.
    pub log: Option<PathBuf>,
}

/// A command for a running worker's pump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Send(Message),
    /// End the channel from our side: the worker's EOF means stop. Queued
    /// writes are discarded, rather than flushed before closing.
    Close,
    /// End the process by its handle and reap it.
    Kill,
}

/// How a worker process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub success: bool,
    pub text: String,
}

impl ExitInfo {
    pub fn from_status(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
            text: status.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Message(Message),
    /// The channel failed or closed while the process may still run (6.2). The
    /// worker is `lost`: stop it by its handle and acquire nothing it held.
    Lost(String),
    /// The process is gone. Always the last event of a worker.
    Exited(ExitInfo),
}

/// A started worker, greeted and sent its welcome.
#[derive(Debug, Clone)]
pub struct WorkerPort {
    pub id: WorkerId,
    pub identity: ProcessIdentity,
    /// The version string the worker reported.
    pub reported_version: String,
    /// The protocol version selected for it.
    pub protocol: u32,
    pub caps: Vec<String>,
    pub cmd: UnboundedSender<Cmd>,
}

#[derive(Debug)]
pub enum SpawnError {
    /// The binary is not there.
    Missing,
    /// The binary is not what it should be.
    HashMismatch { expected: String, found: String },
    /// The process could not be started.
    Start(std::io::Error),
    /// The worker did not connect and greet in time.
    Timeout,
    /// The worker needs a newer supervisor.
    Incompatible { needs_supervisor: u32 },
    /// The worker's greeting was wrong, or the channel failed.
    Handshake(String),
    /// The binary failed the trust check: the environment's fault, not the
    /// version's.
    Untrusted(String),
    /// A previous spawn's child has not had its exit observed yet. Nothing was
    /// started; wait for its `Exited` and try again.
    SurvivorPending,
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "the worker binary is missing"),
            Self::HashMismatch { expected, found } => {
                write!(f, "the worker binary hashes to {found}, not {expected}")
            }
            Self::Start(error) => write!(f, "the worker did not start: {error}"),
            Self::Timeout => write!(f, "the worker did not connect in time"),
            Self::Incompatible { needs_supervisor } => {
                write!(f, "the worker needs supervisor version {needs_supervisor}")
            }
            Self::Handshake(reason) => write!(f, "the worker's greeting failed: {reason}"),
            Self::Untrusted(reason) => write!(f, "the worker binary is not trusted: {reason}"),
            Self::SurvivorPending => {
                write!(f, "a previous spawn has not had its exit observed")
            }
        }
    }
}

impl std::error::Error for SpawnError {}

/// Starts workers.
pub trait Driver: Send + Sync + 'static {
    /// Checks the binary's hash, starts the worker, completes the handshake
    /// within `request.handshake_timeout` and starts delivering its events to
    /// `events`. Every created process stays tracked until its exit is observed,
    /// including a failed or cancelled handshake that survives termination.
    fn spawn(
        &self,
        request: SpawnRequest,
        events: EventSender,
    ) -> impl Future<Output = Result<WorkerPort, SpawnError>> + Send;
}

/// How long the pump waits for a worker to take one command.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the pump keeps reading what an exited worker left in the channel.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(100);

/// The real driver.
#[derive(Debug, Clone)]
pub struct ProcessDriver {
    pub kind: ServiceKind,
    pub exit_poll: Duration,
}

impl ProcessDriver {
    pub fn new(kind: ServiceKind, exit_poll: Duration) -> Self {
        Self { kind, exit_poll }
    }
}

enum OwnedProc {
    #[cfg(unix)]
    Unix(crate::process::unix::WorkerProcess),
    #[cfg(windows)]
    Windows(crate::process::windows::WorkerProcess),
}

struct Proc(Option<OwnedProc>);

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(process) = self.0.take() {
            // Emergency Unix reap may block: never perform it on the runtime.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(move || drop(process));
            } else {
                drop(process);
            }
        }
    }
}

impl Proc {
    fn identity(&self) -> ProcessIdentity {
        match self.0.as_ref().expect("process held") {
            #[cfg(unix)]
            OwnedProc::Unix(process) => process.identity(),
            #[cfg(windows)]
            OwnedProc::Windows(process) => process.identity(),
        }
    }

    fn pid(&self) -> u32 {
        self.identity().pid
    }

    fn try_exit(&mut self) -> std::io::Result<Option<ExitInfo>> {
        let status = match self.0.as_mut().expect("process held") {
            #[cfg(unix)]
            OwnedProc::Unix(process) => process.try_wait()?,
            #[cfg(windows)]
            OwnedProc::Windows(process) => process.try_wait()?,
        };
        Ok(status.map(ExitInfo::from_status))
    }

    /// Termination and its bounded wait run off the async runtime. The handle
    /// comes back even on a timeout, so polling still observes the real exit.
    ///
    /// The flag with the exit says the process was still running when the kill
    /// was made: a worker that had already exited (a crash the poll had not
    /// seen yet) is returned with its own status and `false`, so its exit is
    /// not taken for the kill's doing.
    async fn kill(&mut self) -> Option<(ExitInfo, bool)> {
        let pid = self.pid();
        // No check here: whether the worker is already gone is decided inside
        // the blocking call, right before the kill, and reported with the exit.
        let mut process = self.0.take().expect("process held");
        #[cfg(test)]
        let runtime_thread = std::thread::current().id();
        let (process, result) = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            assert_ne!(runtime_thread, std::thread::current().id());
            let result = match &mut process {
                #[cfg(unix)]
                OwnedProc::Unix(process) => process.terminate_and_wait(Duration::from_secs(2)),
                #[cfg(windows)]
                OwnedProc::Windows(process) => process.terminate_and_wait(Duration::from_secs(2)),
            };
            (process, result)
        })
        .await
        .expect("worker termination task panicked");
        self.0 = Some(process);
        match result {
            Ok((status, how)) => Some((
                ExitInfo::from_status(status),
                how == crate::process::Termination::Issued,
            )),
            Err(error) => {
                // A kill whose bounded wait timed out loses which case it was;
                // the try stays charged, which errs on the safe side.
                tracing::warn!("could not end worker {pid}: {error}");
                None
            }
        }
    }
}

/// Tracks a process before its first cancellable wait. An abandoned handshake
/// transfers its handle to cleanup, which observes its exit before releasing it.
struct Spawning {
    process: Option<Proc>,
    port: WorkerPort,
    commands: Option<UnboundedReceiver<Cmd>>,
    events: EventSender,
    exit_poll: Duration,
    #[cfg(feature = "blue-green-test")]
    termination_gate: PathBuf,
}

impl Spawning {
    fn new(
        process: Proc,
        request: &SpawnRequest,
        events: &EventSender,
        exit_poll: Duration,
    ) -> Self {
        let (cmd, commands) = unbounded_channel();
        let port = WorkerPort {
            id: request.id,
            identity: process.identity(),
            reported_version: request.version.clone(),
            protocol: 0,
            caps: Vec::new(),
            cmd,
        };
        events
            .survivors
            .lock()
            .unwrap()
            .insert(request.id, port.clone());
        Self {
            process: Some(process),
            port,
            commands: Some(commands),
            events: events.clone(),
            exit_poll,
            #[cfg(feature = "blue-green-test")]
            termination_gate: request
                .installation
                .data_dir
                .join(".blue-green-test")
                .join("termination-timeout.hold"),
        }
    }

    fn process(&mut self) -> &mut Proc {
        self.process.as_mut().expect("spawn process held")
    }

    // Cleanup owns the handle even if the caller cancels this bounded wait.
    async fn end(mut self) {
        let task = self.cleanup();
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
    }

    fn cleanup(&mut self) -> tokio::task::JoinHandle<()> {
        let mut process = self.process.take().expect("spawn process held");
        let events = self.events.clone();
        let id = self.port.id;
        let exit_poll = self.exit_poll;
        let mut commands = self.commands.take().expect("spawn commands held");
        #[cfg(feature = "blue-green-test")]
        let termination_gate = self.termination_gate.clone();
        tokio::spawn(async move {
            #[cfg(feature = "blue-green-test")]
            let held = termination_gate.exists();
            #[cfg(not(feature = "blue-green-test"))]
            let held = false;
            let mut exit = if held {
                None
            } else {
                process.kill().await.map(|(exit, _)| exit)
            };
            #[cfg(feature = "blue-green-test")]
            if held {
                let _ = std::fs::write(
                    termination_gate.with_file_name("termination-timeout.arrived"),
                    "yes",
                );
            }
            let mut poll = tokio::time::interval(exit_poll);
            while exit.is_none() {
                tokio::select! {
                    Some(command) = commands.recv() => {
                        #[cfg(feature = "blue-green-test")]
                        let held = termination_gate.exists();
                        #[cfg(not(feature = "blue-green-test"))]
                        let held = false;
                        if matches!(command, Cmd::Kill) && !held {
                            exit = process.kill().await.map(|(exit, _)| exit);
                        }
                    }
                    _ = poll.tick() => match process.try_exit() {
                        Ok(found) => exit = found,
                        Err(error) => tracing::warn!("could not look at failed worker {id}: {error}"),
                    }
                }
            }
            let _ = events.send((id, Event::Exited(exit.unwrap())));
        })
    }
}

impl Drop for Spawning {
    fn drop(&mut self) {
        if self.process.is_some() {
            self.cleanup();
        }
    }
}

async fn verify_binary(request: &SpawnRequest) -> Result<(), SpawnError> {
    let path = request.binary.clone();
    let found = tokio::task::spawn_blocking(move || open_ferry_update::staging::hash_file(&path))
        .await
        .map_err(|error| SpawnError::Start(std::io::Error::other(error.to_string())))?;
    match found {
        Ok(found) if found.eq_ignore_ascii_case(&request.sha256) => Ok(()),
        Ok(found) => Err(SpawnError::HashMismatch {
            expected: request.sha256.clone(),
            found,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(SpawnError::Missing),
        Err(error) => Err(SpawnError::Start(error)),
    }
}

fn open_log(request: &SpawnRequest) -> Result<Option<std::fs::File>, SpawnError> {
    let Some(path) = &request.log else {
        return Ok(None);
    };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(Some)
        .map_err(SpawnError::Start)
}

impl ProcessDriver {
    #[cfg(unix)]
    async fn start(
        &self,
        request: &SpawnRequest,
        deadline: tokio::time::Instant,
        events: &EventSender,
    ) -> Result<(Spawning, BoxedChannel, Vec<WindowsSocket>), SpawnError> {
        let _ = deadline;
        use std::process::{Command, Stdio};

        let mut command = Command::new(&request.binary);
        command.args(worker_args(&request.config, None));
        command.stdin(Stdio::null());
        if let Some(log) = open_log(request)? {
            let again = log.try_clone().map_err(SpawnError::Start)?;
            command.stdout(Stdio::from(log)).stderr(Stdio::from(again));
        }
        let listeners = request
            .listeners
            .iter()
            .map(|(_, listener)| listener.try_clone())
            .collect::<Result<Vec<_>, _>>()
            .map_err(SpawnError::Start)?;
        let (process, stream) = crate::process::unix::WorkerProcess::spawn(command, &listeners)
            .map_err(SpawnError::Start)?;
        let process = Spawning::new(
            Proc(Some(OwnedProc::Unix(process))),
            request,
            events,
            self.exit_poll,
        );
        match crate::control::unix_channel(stream) {
            Ok(channel) => Ok((process, channel, Vec::new())),
            Err(error) => {
                process.end().await;
                Err(SpawnError::Start(error))
            }
        }
    }

    #[cfg(windows)]
    async fn start(
        &self,
        request: &SpawnRequest,
        deadline: tokio::time::Instant,
        events: &EventSender,
    ) -> Result<(Spawning, BoxedChannel, Vec<WindowsSocket>), SpawnError> {
        use crate::control::ControlPipe;
        use crate::endpoint::Identity;
        use crate::process::windows::{SpawnSpec, WorkerProcess};
        use crate::sockets::windows::duplicate_for_process;

        let identity = Identity::current(self.kind).map_err(SpawnError::Start)?;
        let pipe = ControlPipe::create(&identity).map_err(SpawnError::Start)?;
        let args = worker_args(&request.config, Some(pipe.name()));
        let log = open_log(request)?;
        let spec = SpawnSpec {
            exe: &request.binary,
            args: &args,
            cwd: None,
            stdout: log.as_ref(),
            stderr: log.as_ref(),
        };
        let process = WorkerProcess::spawn(&spec).map_err(SpawnError::Start)?;
        let pid = process.pid();
        let mut process = Spawning::new(
            Proc(Some(OwnedProc::Windows(process))),
            request,
            events,
            self.exit_poll,
        );
        let mut poll = tokio::time::interval(self.exit_poll);
        let accepted = tokio::time::timeout_at(deadline, async {
            let accept = pipe.accept(pid);
            tokio::pin!(accept);
            loop {
                tokio::select! {
                    result = &mut accept => break result,
                    _ = poll.tick() => {
                        if let Some(exit) = process.process().try_exit()? {
                            break Err(std::io::Error::other(format!("worker exited before connecting: {}", exit.text)));
                        }
                    }
                }
            }
        }).await;
        let channel = match accepted {
            Ok(Ok(channel)) => {
                // The moment the connect phase of the shared load deadline is
                // over; a test measures the hello phase from it. Test builds
                // only: the seam `driver.accepted` writes `driver.accepted.arrived`
                // here (a test that also creates `.hold` and `.release` gets the
                // time without the driver waiting).
                #[cfg(feature = "blue-green-test")]
                let _ = open_ferry_update::test_seams::hit(
                    &request.installation.data_dir,
                    "driver.accepted",
                );
                channel
            }
            Ok(Err(error)) => {
                process.end().await;
                return Err(SpawnError::Handshake(error.to_string()));
            }
            Err(_) => {
                tracing::warn!("the worker did not connect before the load deadline");
                process.end().await;
                return Err(SpawnError::Timeout);
            }
        };
        let mut sockets = Vec::new();
        for (name, listener) in request.listeners.iter() {
            match duplicate_for_process(listener, pid) {
                Ok(info) => sockets.push(WindowsSocket {
                    name: name.clone(),
                    info,
                }),
                Err(error) => {
                    process.end().await;
                    return Err(SpawnError::Start(error));
                }
            }
        }
        Ok((process, channel, sockets))
    }
}

impl Driver for ProcessDriver {
    async fn spawn(
        &self,
        request: SpawnRequest,
        events: EventSender,
    ) -> Result<WorkerPort, SpawnError> {
        if !events.survivors.lock().unwrap().is_empty() {
            return Err(SpawnError::SurvivorPending);
        }
        if let Err(error) = std::fs::symlink_metadata(&request.binary) {
            return Err(if error.kind() == std::io::ErrorKind::NotFound {
                SpawnError::Missing
            } else {
                SpawnError::Start(error)
            });
        }
        if request.binary != request.installation.installed {
            let identity =
                crate::endpoint::Identity::current(self.kind).map_err(SpawnError::Start)?;
            let rules = crate::trust::TrustRules::for_identity(&identity);
            crate::trust::check_file(&request.binary, &rules)
                .map_err(|error| SpawnError::Untrusted(error.to_string()))?;
        }
        verify_binary(&request).await?;
        let deadline = tokio::time::Instant::now() + request.handshake_timeout;
        let (mut process, mut channel, sockets) = self.start(&request, deadline, &events).await?;
        let params = WelcomeParams {
            role: request.role,
            installation: request.installation.clone(),
            budgets: request.budgets.clone(),
            sockets,
            topology: request.topology.clone(),
            switch_id: request.switch_id.clone(),
        };
        let greeted = tokio::time::timeout_at(
            deadline,
            handshake(&mut channel, process.process().pid(), params),
        )
        .await;
        let negotiated = match greeted {
            Ok(Ok(negotiated)) => negotiated,
            Ok(Err(HandshakeError::Incompatible { needs_supervisor })) => {
                process.end().await;
                return Err(SpawnError::Incompatible { needs_supervisor });
            }
            Ok(Err(error)) => {
                process.end().await;
                return Err(SpawnError::Handshake(error.to_string()));
            }
            Err(_) => {
                tracing::warn!("the worker connected but did not greet before the load deadline");
                process.end().await;
                return Err(SpawnError::Timeout);
            }
        };
        let mut port = process.port.clone();
        port.reported_version = negotiated.hello.version.clone();
        port.protocol = negotiated.version;
        port.caps = negotiated.hello.caps.clone();
        let commands = process.commands.take().expect("spawn commands held");
        let process = process.process.take().expect("spawn process held");
        tokio::spawn(pump(
            request.id,
            process,
            channel,
            commands,
            events,
            self.exit_poll,
        ));
        Ok(port)
    }
}

/// Owns one worker's process and channel until it has exited.
async fn pump(
    id: WorkerId,
    mut process: Proc,
    channel: BoxedChannel,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    events: EventSender,
    exit_poll: Duration,
) {
    let emit = |event: Event| {
        let _ = events.send((id, event));
    };
    let mut poll = tokio::time::interval(exit_poll);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut reader, mut writer) = channel.into_read_write();
    let (write_tx, mut writes) = unbounded_channel::<Message>();
    let (failed_tx, mut failed) = tokio::sync::mpsc::unbounded_channel();
    let writer_task = tokio::spawn(async move {
        while let Some(message) = writes.recv().await {
            if !matches!(
                tokio::time::timeout(SEND_TIMEOUT, writer.send(&message)).await,
                Ok(Ok(()))
            ) {
                let _ = failed_tx.send(format!("could not send {message:?}"));
                break;
            }
        }
    });
    let mut readable = true;
    let mut writable = true;
    let mut commands_open = true;
    let exit = loop {
        tokio::select! {
            command = commands.recv(), if commands_open => match command {
                Some(Cmd::Send(message)) => {
                    if !writable {
                        continue;
                    }
                    let _ = write_tx.send(message);
                }
                Some(Cmd::Close) => {
                    // Abort the writer and drop both real halves: worker sees EOF.
                    writer_task.abort();
                    reader = crate::protocol::Channel::new(Box::new(tokio::io::empty()) as crate::worker::BoxedRead, tokio::io::sink());
                    writable = false;
                    readable = false;
                }
                Some(Cmd::Kill) => {
                    if let Some((exit, found_running)) = process.kill().await {
                        if found_running {
                            events.terminated.lock().unwrap().insert(id);
                        }
                        break exit;
                    }
                }
                None => {
                    // The owner let go of the port: nothing will ask again.
                    commands_open = false;
                }
            },
            frame = reader.receive::<Message>(), if readable => match frame {
                Ok(Some(frame)) => emit(Event::Message(frame.message)),
                Ok(None) => {
                    readable = false;
                    writable = false;
                    // EOF is already a lost channel. Abort queued writes,
                    // including Stop: only the retained process handle can
                    // guarantee termination now.
                    writer_task.abort();
                    emit(Event::Lost("the control channel closed".to_owned()));
                }
                Err(error) => {
                    readable = false;
                    writable = false;
                    writer_task.abort();
                    emit(Event::Lost(format!("the control channel failed: {error}")));
                }
            },
            reason = failed.recv(), if writable => {
                writable = false;
                readable = false;
                reader = crate::protocol::Channel::new(Box::new(tokio::io::empty()) as crate::worker::BoxedRead, tokio::io::sink());
                emit(Event::Lost(reason.unwrap_or_else(|| "writer ended".to_owned())));
            },
            _ = poll.tick() => {
                // Before Registry::add, the survivor entry itself owns one
                // sender. If it is the only one left, the returned port was
                // abandoned. Treat it like a cancelled handshake, retaining
                // the entry until Registry observes Exited.
                let abandoned = events.survivors.lock().unwrap().get(&id)
                    .is_some_and(|port| port.cmd.strong_count() == 1);
                if abandoned && let Some((exit, _)) = process.kill().await {
                    break exit;
                }
                match process.try_exit() {
                    Ok(Some(exit)) => break exit,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!("could not look at worker {id}: {error}");
                    }
                }
            },
        }
    };
    writer_task.abort();
    // What the worker wrote before it died is still in the channel.
    if readable {
        while let Ok(Ok(Some(frame))) =
            tokio::time::timeout(DRAIN_AFTER_EXIT, reader.receive::<Message>()).await
        {
            emit(Event::Message(frame.message));
        }
    }
    emit(Event::Exited(exit));
}

/// Every running worker the supervisor owns and the one stream of their
/// events. It lives above the panic boundary, so a panic in the state machine
/// loses no worker.
pub struct Registry {
    ports: std::collections::HashMap<WorkerId, WorkerPort>,
    events_tx: EventSender,
    events: UnboundedReceiver<(WorkerId, Event)>,
    next: WorkerId,
    shutdown: ShutdownLog,
}

/// How each worker ended during `stop_all` and the wait after it, for the
/// refund of a clean service stop (`Core::stop`). A worker is `clean` only on
/// evidence that the supervisor caused its end:
///
/// - an exit with success after `stop` was sent (a `Lost` between the two is
///   the worker's channel closing as it leaves, not a failure); or
/// - an exit after a kill the registry requested at the stop deadline AND the
///   pump reported that the kill found the process running and ended it
///   (`EventSender::terminated`). A queued kill is not evidence: it may reach a
///   process that had already died on its own. And if the channel failed after
///   `stop` while the process lived (`lost_after_stop`), the worker needed the
///   kill because of that failure, so that end is not clean.
///
/// It is `failed` when it showed a failure the supervisor did not cause: a
/// `load_failed`, an exit that is neither of the above, an exit or a lost
/// channel that was already queued before `stop` was sent, or a channel that
/// could not take `stop`.
///
/// Accepted residual: events carry no order against the `stop` send, so an
/// unasked exit with success that lands between the drain and that send is
/// counted clean too. A failure exit in that window is vetoed anyway, and an
/// exit with success is not a crash. The switch's own service-stop paths send
/// `stop` before this log watches; the answer is then read as an exit before
/// `stop`, which keeps the count (the safe side).
#[derive(Debug, Default)]
struct ShutdownLog {
    watching: bool,
    stop_sent: std::collections::HashSet<WorkerId>,
    /// A kill the registry queued (and the port took), not one that happened.
    kill_requested: std::collections::HashSet<WorkerId>,
    lost_after_stop: std::collections::HashSet<WorkerId>,
    failed: std::collections::HashSet<WorkerId>,
    clean: std::collections::HashSet<WorkerId>,
}

impl ShutdownLog {
    /// `terminated`: the pump's record that its kill ended this worker while it
    /// ran (read when `Exited` arrives).
    fn note(&mut self, id: WorkerId, event: &Event, terminated: bool) {
        if !self.watching {
            return;
        }
        match event {
            Event::Message(Message::LoadFailed { .. }) => {
                self.failed.insert(id);
            }
            Event::Message(_) => {}
            Event::Lost(_) => {
                if self.stop_sent.contains(&id) {
                    // Either the worker closing the channel as it exits, or a
                    // channel failure with the process alive: which one is
                    // settled by what follows (an exit with success, or a kill).
                    self.lost_after_stop.insert(id);
                } else {
                    self.failed.insert(id);
                }
            }
            Event::Exited(info) => {
                let asked = info.success && self.stop_sent.contains(&id);
                let killed = self.kill_requested.contains(&id)
                    && terminated
                    && !self.lost_after_stop.contains(&id);
                if (asked || killed) && !self.failed.contains(&id) {
                    self.clean.insert(id);
                } else {
                    self.failed.insert(id);
                }
            }
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        let (sender, events) = unbounded_channel();
        let events_tx = EventSender {
            sender,
            survivors: Arc::default(),
            terminated: Arc::default(),
        };
        Self {
            ports: std::collections::HashMap::new(),
            events_tx,
            events,
            next: 1,
            shutdown: ShutdownLog::default(),
        }
    }

    /// Where a driver sends events.
    pub fn events(&self) -> EventSender {
        self.events_tx.clone()
    }

    /// An id no worker has had.
    pub fn next_id(&mut self) -> WorkerId {
        let id = self.next;
        self.next += 1;
        id
    }

    pub fn add(&mut self, port: WorkerPort) {
        self.events_tx.survivors.lock().unwrap().remove(&port.id);
        self.ports.insert(port.id, port);
    }

    pub fn port(&self, id: WorkerId) -> Option<&WorkerPort> {
        self.ports.get(&id)
    }

    /// Children that were created but have no port: a handshake that failed or
    /// was cancelled, until its `Exited` is observed. (S4: the switch waits on
    /// these before it ends.)
    pub fn survivors(&self) -> Vec<WorkerId> {
        let mut ids: Vec<_> = self
            .events_tx
            .survivors
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        ids.sort_unstable();
        ids
    }

    /// A survivor's process identity and the version it was started as, for
    /// the status and the journal. Read-only; `None` once its exit is observed.
    pub fn survivor_info(&self, id: WorkerId) -> Option<(ProcessIdentity, String)> {
        self.events_tx
            .survivors
            .lock()
            .unwrap()
            .get(&id)
            .map(|port| (port.identity, port.reported_version.clone()))
    }

    pub fn running(&self) -> Vec<WorkerId> {
        let mut ids: Vec<_> = self.ports.keys().copied().collect();
        ids.extend(self.events_tx.survivors.lock().unwrap().keys().copied());
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Queues `message` for the worker. `false` if it has gone.
    pub fn send(&self, id: WorkerId, message: Message) -> bool {
        self.command(id, Cmd::Send(message))
    }

    pub fn close(&self, id: WorkerId) -> bool {
        self.command(id, Cmd::Close)
    }

    pub fn kill(&self, id: WorkerId) -> bool {
        self.command(id, Cmd::Kill)
    }

    fn command(&self, id: WorkerId, command: Cmd) -> bool {
        if let Some(port) = self.ports.get(&id) {
            return port.cmd.send(command).is_ok();
        }
        self.events_tx
            .survivors
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|port| port.cmd.send(command).is_ok())
    }

    fn note_shutdown(&mut self, id: WorkerId, event: &Event) {
        // Only an exit reads the flag, and it is the last event of its worker:
        // take the id out so the set does not grow with every killed worker.
        let terminated = match event {
            Event::Exited(_) => self.events_tx.terminated.lock().unwrap().remove(&id),
            _ => false,
        };
        self.shutdown.note(id, event, terminated);
    }

    /// The next event from any worker. A worker's `Exited` removes it.
    pub async fn next_event(&mut self) -> (WorkerId, Event) {
        // The registry holds a sender itself, so the stream never ends.
        loop {
            if let Some((id, event)) = self.events.recv().await {
                self.note_shutdown(id, &event);
                if matches!(event, Event::Exited(_)) {
                    self.ports.remove(&id);
                    self.events_tx.survivors.lock().unwrap().remove(&id);
                }
                return (id, event);
            }
        }
    }

    /// An event that is already waiting.
    pub fn try_next_event(&mut self) -> Option<(WorkerId, Event)> {
        let (id, event) = self.events.try_recv().ok()?;
        self.note_shutdown(id, &event);
        if matches!(event, Event::Exited(_)) {
            self.ports.remove(&id);
            self.events_tx.survivors.lock().unwrap().remove(&id);
        }
        Some((id, event))
    }

    /// Stops every worker as a service stop does (9.1): `stop` with the budget
    /// less five seconds, a wait of `budget` for the exits, then the handle's
    /// kill for any that remain, and a short wait to reap them. Returns the ids
    /// that were sent a kill, including any still registered after `kill_wait`.
    /// Only an `Exited` event removes a worker; a deadline is never an exit.
    pub async fn stop_all(&mut self, stop_budget: Duration, kill_wait: Duration) -> Vec<WorkerId> {
        let message_budget = stop_budget.saturating_sub(Duration::from_secs(5));
        let budget_ms = u64::try_from(message_budget.as_millis()).unwrap_or(u64::MAX);
        // What each worker did is kept from here (`stopped_cleanly`). Events
        // already queued happened before `stop` was sent: they are read first,
        // so a failure that was waiting is not mistaken for a result of the stop.
        // (A `stop` the switch sent earlier is not seen here: its answer reads as
        // an exit before `stop`, and the count stays.)
        self.shutdown = ShutdownLog {
            watching: true,
            ..ShutdownLog::default()
        };
        while self.try_next_event().is_some() {}
        // Kills made before this stop are not its doing. Consumed exits
        // already removed their ids; this clears any left unconsumed.
        self.events_tx.terminated.lock().unwrap().clear();
        for id in self.running() {
            if self.send(id, Message::Stop { budget_ms }) {
                self.shutdown.stop_sent.insert(id);
            } else {
                self.shutdown.failed.insert(id);
                self.kill(id);
            }
        }
        // A queued stop is not an exit. Keep every port until Exited arrives.
        self.wait_for_exits(stop_budget).await;
        let stragglers = self.running();
        for id in &stragglers {
            if self.kill(*id) {
                self.shutdown.kill_requested.insert(*id);
            }
        }
        self.wait_for_exits(kill_wait).await;
        stragglers
    }

    /// Whether `id` ended during the last `stop_all` for a reason the
    /// supervisor caused (see `ShutdownLog`), and showed no failure of its
    /// own. `false` for a worker not seen to exit, or that failed.
    pub fn stopped_cleanly(&self, id: WorkerId) -> bool {
        self.shutdown.clean.contains(&id) && !self.shutdown.failed.contains(&id)
    }

    /// Waits without a deadline for observed exits, warning about survivors.
    /// Used after a stop deadline before recovery may acquire their resources.
    pub async fn wait_until_empty(&mut self) {
        // The first warning comes after 10 s, not at once.
        let mut warning = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(10),
            Duration::from_secs(10),
        );
        warning.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !self.running().is_empty() {
            tokio::select! {
                _ = self.next_event() => {}
                _ = warning.tick() => {
                    for id in self.running() {
                        let pid = self.ports.get(&id).map(|port| port.identity.pid)
                            .or_else(|| self.events_tx.survivors.lock().unwrap().get(&id).map(|port| port.identity.pid));
                        tracing::warn!(worker = id, pid, "waiting for worker exit after stop");
                    }
                }
            }
        }
    }

    async fn wait_for_exits(&mut self, wait: Duration) {
        let _ = tokio::time::timeout(wait, async {
            while !self.running().is_empty() {
                self.next_event().await;
            }
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleeping_process() -> Proc {
        #[cfg(unix)]
        {
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", "exec sleep 120"]);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let (process, channel) =
                crate::process::unix::WorkerProcess::spawn(command, &[listener]).unwrap();
            drop(channel);
            Proc(Some(OwnedProc::Unix(process)))
        }
        #[cfg(windows)]
        {
            use crate::process::windows::{SpawnSpec, WorkerProcess};
            let exe = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32/WindowsPowerShell/v1.0/powershell.exe");
            let args = [
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Start-Sleep -Seconds 120".into(),
            ];
            let spec = SpawnSpec {
                exe: &exe,
                args: &args,
                cwd: None,
                stdout: None,
                stderr: None,
            };
            Proc(Some(OwnedProc::Windows(
                WorkerProcess::spawn(&spec).unwrap(),
            )))
        }
    }

    /// A worker that has exited and that nothing has observed: no wait or
    /// poll has been made on it. On Unix its exit is learned from the end of
    /// its stdout pipe (a zombie until reaped); on Windows by waiting on the
    /// handle, which observes without changing anything.
    fn exited_unobserved_process(code: u8) -> OwnedProc {
        #[cfg(unix)]
        {
            use std::io::Read;
            let (mut reader, writer) = std::io::pipe().unwrap();
            // The worker waits for its stdin to close, so it cannot exit
            // before the spawn has captured its identity.
            let (gate, hold) = std::io::pipe().unwrap();
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", &format!("read _; exit {code}")]);
            command.stdin(gate).stdout(writer);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let (process, channel) =
                crate::process::unix::WorkerProcess::spawn(command, &[listener]).unwrap();
            drop(channel);
            drop(hold);
            let (done, ended) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = done.send(reader.read_to_end(&mut sink));
            });
            ended
                .recv_timeout(Duration::from_secs(10))
                .expect("the worker's stdout did not end")
                .unwrap();
            // EOF can come before the child is waitable. WNOWAIT looks at the
            // exit without reaping it, so the code under test still finds it.
            // Linux only: other Unixes keep the EOF alone.
            #[cfg(target_os = "linux")]
            {
                let bound = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    // SAFETY: plain data that the call fills in.
                    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                    // SAFETY: `info` is valid for the call; the child is ours.
                    let rc = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            process.pid() as libc::id_t,
                            &mut info,
                            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                        )
                    };
                    assert_eq!(rc, 0, "waitid failed");
                    // SAFETY: a SIGCHLD siginfo, or zeroed when nothing is waitable.
                    if unsafe { info.si_pid() } != 0 {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < bound,
                        "the child never became waitable"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            OwnedProc::Unix(process)
        }
        #[cfg(windows)]
        {
            use crate::process::windows::{SpawnSpec, WorkerProcess};
            let exe = PathBuf::from(std::env::var_os("COMSPEC").unwrap());
            let args = ["/c".into(), format!("exit /b {code}").into()];
            let spec = SpawnSpec {
                exe: &exe,
                args: &args,
                cwd: None,
                stdout: None,
                stderr: None,
            };
            let process = WorkerProcess::spawn(&spec).unwrap();
            assert!(
                process
                    .process()
                    .wait_timeout(Duration::from_secs(10))
                    .unwrap()
                    .is_some(),
                "the worker did not exit"
            );
            OwnedProc::Windows(process)
        }
    }

    #[test]
    fn terminating_an_exited_worker_reports_it_was_already_gone() {
        let mut process = exited_unobserved_process(3);
        let (status, how) = match &mut process {
            #[cfg(unix)]
            OwnedProc::Unix(p) => p.terminate_and_wait(Duration::from_secs(2)).unwrap(),
            #[cfg(windows)]
            OwnedProc::Windows(p) => p.terminate_and_wait(Duration::from_secs(2)).unwrap(),
        };
        assert_eq!(how, crate::process::Termination::AlreadyExited);
        // Its own status: nothing was sent that could have changed it.
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn terminating_a_running_worker_reports_the_kill_as_issued() {
        let mut process = sleeping_process().0.take().unwrap();
        let (status, how) = match &mut process {
            #[cfg(unix)]
            OwnedProc::Unix(p) => p.terminate_and_wait(Duration::from_secs(5)).unwrap(),
            #[cfg(windows)]
            OwnedProc::Windows(p) => p.terminate_and_wait(Duration::from_secs(5)).unwrap(),
        };
        assert_eq!(how, crate::process::Termination::Issued);
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(libc::SIGKILL));
        }
        #[cfg(windows)]
        assert!(!status.success());
    }

    #[tokio::test]
    async fn kill_of_an_exited_unobserved_worker_returns_its_exit_and_false() {
        let mut process = Proc(Some(exited_unobserved_process(3)));
        let (exit, found_running) = tokio::time::timeout(Duration::from_secs(30), process.kill())
            .await
            .expect("the kill did not return")
            .expect("the exit is returned");
        assert!(!found_running);
        assert!(!exit.success);
    }

    #[tokio::test]
    async fn kill_of_a_running_worker_says_it_found_it_running() {
        let mut process = sleeping_process();
        let (_, found_running) = tokio::time::timeout(Duration::from_secs(30), process.kill())
            .await
            .expect("the kill did not return")
            .expect("the kill succeeds");
        assert!(found_running);
    }

    #[tokio::test]
    async fn a_killed_workers_id_leaves_the_terminated_set_with_its_exit() {
        let (read, _peer) = tokio::io::duplex(1024);
        let channel = crate::protocol::Channel::new(
            Box::new(read) as crate::worker::BoxedRead,
            Box::new(tokio::io::sink()) as crate::worker::BoxedWrite,
        );
        let mut registry = Registry::new();
        let id = registry.next_id();
        let (cmd, commands) = unbounded_channel();
        let task = tokio::spawn(pump(
            id,
            sleeping_process(),
            channel,
            commands,
            registry.events(),
            Duration::from_millis(10),
        ));
        cmd.send(Cmd::Kill).unwrap();
        loop {
            let (_, event) = tokio::time::timeout(Duration::from_secs(10), registry.next_event())
                .await
                .unwrap();
            if matches!(event, Event::Exited(_)) {
                break;
            }
        }
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("the pump did not end")
            .unwrap();
        assert!(registry.events_tx.terminated.lock().unwrap().is_empty());
    }

    struct FailedWrite;
    impl tokio::io::AsyncWrite for FailedWrite {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "write side closed",
            )))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_writer_failure_is_one_final_lost_even_when_the_peer_keeps_writing() {
        let (read, write) = tokio::io::duplex(1024);
        let channel = crate::protocol::Channel::new(
            Box::new(read) as crate::worker::BoxedRead,
            Box::new(FailedWrite) as crate::worker::BoxedWrite,
        );
        let mut peer = crate::protocol::Channel::new(tokio::io::empty(), write);
        let mut registry = Registry::new();
        let (cmd, commands) = unbounded_channel();
        let task = tokio::spawn(pump(
            1,
            sleeping_process(),
            channel,
            commands,
            registry.events(),
            Duration::from_millis(10),
        ));
        cmd.send(Cmd::Send(Message::Ping)).unwrap();
        let (_, lost) = tokio::time::timeout(Duration::from_secs(5), registry.next_event())
            .await
            .unwrap();
        assert!(matches!(lost, Event::Lost(_)));
        // The peer has no reader, but continues producing valid inbound frames.
        // Closing our reader may make this write fail; it must never become a Message.
        let _ = peer.send(&Message::Loaded { ms: 1 }).await;
        drop(peer);
        cmd.send(Cmd::Kill).unwrap();
        let (_, last) = tokio::time::timeout(Duration::from_secs(5), registry.next_event())
            .await
            .unwrap();
        assert!(
            matches!(last, Event::Exited(_)),
            "an event followed Lost: {last:?}"
        );
        task.await.unwrap();
        assert!(registry.try_next_event().is_none());
    }

    struct EndedWriter;
    impl tokio::io::AsyncWrite for EndedWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            panic!("test writer ended without a reason")
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_writer_ending_without_a_reason_reports_lost() {
        let (read, _peer) = tokio::io::duplex(1024);
        let channel = crate::protocol::Channel::new(
            Box::new(read) as crate::worker::BoxedRead,
            Box::new(EndedWriter) as crate::worker::BoxedWrite,
        );
        let mut registry = Registry::new();
        let (cmd, commands) = unbounded_channel();
        let task = tokio::spawn(pump(
            1,
            sleeping_process(),
            channel,
            commands,
            registry.events(),
            Duration::from_millis(10),
        ));
        cmd.send(Cmd::Send(Message::Ping)).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), registry.next_event()).await;
        cmd.send(Cmd::Kill).unwrap();
        let (_, last) = tokio::time::timeout(Duration::from_secs(5), registry.next_event())
            .await
            .unwrap();
        task.await.unwrap();
        assert_eq!(result.unwrap().1, Event::Lost("writer ended".to_owned()));
        assert!(matches!(last, Event::Exited(_)));
    }

    #[test]
    fn dropping_a_process_without_a_runtime_ends_its_child() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let process = sleeping_process();
        let identity = process.identity();
        drop(process);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while crate::process::process_alive(identity.pid, identity.started) {
            assert!(
                std::time::Instant::now() < deadline,
                "dropped child survived"
            );
            std::thread::yield_now();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn termination_uses_a_different_thread_and_returns_a_reaped_handle() {
        #[cfg(unix)]
        let process = {
            let mut command = std::process::Command::new("/bin/sh");
            // Intentional long-lived child, not a synchronization sleep. Kill
            // uses its retained handle and the control descriptor is unused.
            command.args(["-c", "exec sleep 120"]);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let (process, channel) =
                crate::process::unix::WorkerProcess::spawn(command, &[listener]).unwrap();
            drop(channel);
            OwnedProc::Unix(process)
        };
        #[cfg(windows)]
        let process = {
            use crate::process::windows::{SpawnSpec, WorkerProcess};
            let exe = std::path::PathBuf::from(std::env::var_os("COMSPEC").unwrap());
            let args = ["/c".into(), "exit /b 0".into()];
            let spec = SpawnSpec {
                exe: &exe,
                args: &args,
                cwd: None,
                stdout: None,
                stderr: None,
            };
            OwnedProc::Windows(WorkerProcess::spawn(&spec).unwrap())
        };
        let mut process = Proc(Some(process));
        crate::process::TERMINATION_MUST_NOT_BLOCK.with(|marked| marked.set(true));
        assert!(process.kill().await.is_some());
        crate::process::TERMINATION_MUST_NOT_BLOCK.with(|marked| marked.set(false));
        assert!(process.try_exit().unwrap().is_some());
    }
}
