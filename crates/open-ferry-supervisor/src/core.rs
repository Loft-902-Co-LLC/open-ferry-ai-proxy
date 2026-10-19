//! Not upstream's: the supervisor's core (v3 5, 6, 9.2). One task owns every
//! worker handle and the in-memory journal; it is the only thing that writes
//! the journal and the state file. Its pieces:
//!
//! - this file: the runtime, the event loop, workers that exit or are lost
//!   outside a switch, the drain of a released worker, the watch window and
//!   its confirmation, the install step (3.9), and the retries of journal and
//!   state writes (3.6);
//! - `switch.rs`: admission, the sequence of 5.1 and the failure rows of 6.1;
//! - `serve.rs`: the endpoint's handlers, which only read a snapshot and send
//!   commands.
//!
//! It is written over the [`Driver`], the [`Clock`], the [`Host`] and the
//! [`Disk`], so the unit tests run every branch with a scripted driver and
//! paused time.
//!
//! Time. Deadlines are `tokio::time::Instant`s, so a paused clock drives them.
//! The wall [`Clock`] is used only for the stamps written into the journal and
//! the state.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use open_ferry_update::journal::{
    Artifact, Blame, How, InstallPending, Installed, Journal, LadderRecord, MarkFailed, Outcome,
    Phase, Return, Worker, WorkerRole,
};
use open_ferry_update::state::{SwitchRecord, installed_binary};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

use crate::clock::Clock;
use crate::disk::Disk;
use crate::driver::{Driver, Event, Registry, SpawnError, SpawnRequest, WorkerId, WorkerPort};
use crate::host::Host;
use crate::ladder::{
    Progress, all_known, list_of, progress_of, record_loaded, record_start,
    restart_record_after_return, return_list, run_of, same_rung, usable_record,
};
use crate::protocol::{Admission, EndpointReply, Installation, Message, Role};
use crate::snapshot::{DrainView, InstallView, Shared, Snapshot, SwitchView, WorkerView};
use crate::startup::{Config, Run, Started};
use crate::timings::Backoff;

/// How often the core looks at its timers. A switch re-evaluates the mode
/// every second (5.3); everything else is as coarse.
pub const TICK: Duration = Duration::from_secs(1);

pub(crate) const AUTOMATIC_COOLDOWN: chrono::Duration = chrono::Duration::hours(6);

pub(crate) fn unfinished_at(record: &SwitchRecord) -> Option<chrono::DateTime<chrono::Utc>> {
    if record.to.is_empty()
        || !record
            .result
            .as_deref()
            .is_some_and(|result| result != "switched")
    {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(record.ended_at.as_deref().unwrap_or(&record.at))
        .ok()
        .map(|ended| ended.with_timezone(&chrono::Utc))
}

/// What the endpoint's handlers ask of the core.
#[derive(Debug)]
pub enum Command {
    /// A `switch` request. The replies (`accepted`, `progress`, `done` or
    /// `error`) go back on `replies`.
    Switch {
        request: SwitchRequest,
        replies: UnboundedSender<EndpointReply>,
    },
    /// A service stop: stop every worker as 9.1 says and end.
    Stop,
}

/// The `switch` request (v3 5.1 step 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchRequest {
    pub to: String,
    pub sha256: String,
    /// The id of a pin file (T3); consumed once J1 has landed.
    pub pin: String,
    pub how: How,
    pub auto: bool,
    pub admitted: Admission,
}

/// How the core ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// A stop was asked for and the workers were stopped.
    Stopped,
    /// Too many panics inside the window: the supervisor exits non-zero.
    TooManyPanics,
}

/// A worker the core started.
#[derive(Debug, Clone)]
pub struct Tracked {
    pub artifact: Artifact,
    pub role: WorkerRole,
    pub pid: u32,
    pub proc_started: u64,
    pub since: Instant,
    /// Its channel closed while the process lives (6.2).
    pub lost: bool,
    /// When a lost worker that hasn't exited is terminated by its handle.
    pub kill_at: Option<Instant>,
    pub live: u64,
    pub signins: u64,
    /// Grows with every `status` message.
    pub status_seq: u64,
    /// It is told `peers_gone` once no draining worker remains (T7).
    pub wants_peers_gone: bool,
    pub peers_gone_sent: bool,
    /// Started at startup or by a restart as the active worker: it is told
    /// nothing about peers until its `loaded` was seen, because L9.15 makes
    /// `peers_gone` invalid, and ignored, in any phase but active and watching.
    pub awaiting_loaded: bool,
    /// When an active start that has greeted but not sent `loaded` is ended:
    /// `timings.active_load` (3 minutes) after its handshake. Cleared by
    /// `loaded`. It stays set once passed: a service stop after it takes
    /// nothing back, even if the timer has not fired yet.
    pub load_deadline: Option<Instant>,
    /// The deadline passed and the worker was killed for it. Final: a `loaded`
    /// that comes after it is ignored, and the start is charged as failed.
    pub load_timed_out: bool,
    /// The worker sent `load_failed`. Kept for its whole life, so a stop that
    /// comes after the Core read it does not take the try back.
    pub load_failed: bool,
    /// The ladder rung this worker was started from (`None` for a worker a
    /// ladder did not start, such as a switch's B), the failed starts that
    /// rung had had before this one, and the rungs the ladder had used up. A
    /// worker that exits before it reports `loaded` has failed to start, so
    /// its rung is charged one more; once it has loaded the rung's count is 0.
    pub rung: Option<Run>,
    pub attempts: u32,
    pub spent: Vec<Run>,
    pub drain: Option<Drain>,
}

/// A released worker finishing its connections.
#[derive(Debug, Clone)]
pub struct Drain {
    pub since: Instant,
    pub deadline: Instant,
    /// The switch whose record gets `drained_ms` and `cut_connections`.
    pub record: Option<OpenRecord>,
    pub pinged: bool,
    /// When `stop` is sent, after the ping's answer.
    pub stop_at: Option<Instant>,
    pub stopped: bool,
    pub cut: u64,
}

/// What a drain's record patch needs.
#[derive(Debug, Clone)]
pub struct OpenRecord {
    pub id: String,
    pub how: How,
    pub from: Artifact,
    pub to: Artifact,
    pub started: String,
}

/// The switch an operation belongs to, once it is past `loading`.
#[derive(Debug, Clone)]
pub struct Open {
    pub id: String,
    pub how: How,
    pub from: Artifact,
    pub to: Artifact,
    pub started: String,
    pub phase: Phase,
    /// It reached `ready` (S1 was made).
    pub reached_ready: bool,
}

impl Open {
    pub fn record(&self) -> OpenRecord {
        OpenRecord {
            id: self.id.clone(),
            how: self.how,
            from: self.from.clone(),
            to: self.to.clone(),
            started: self.started.clone(),
        }
    }
}

/// The open watch window.
#[derive(Debug, Clone)]
pub struct Watch {
    pub open: Open,
    /// The worker that serves the new version.
    pub b: WorkerId,
    pub deadline: Instant,
    /// J5 failed: when it is tried again.
    pub confirm_due: Option<Instant>,
    pub confirm_attempts: u32,
}

/// An automatic return that has been decided (J-R1) but whose fresh worker
/// isn't running yet.
#[derive(Debug, Clone)]
pub struct Returning {
    pub open: Open,
    pub ret: Return,
    /// The first rung at entry; later J-R1 choices must not reorder the ladder.
    pub initial_target: Artifact,
    /// The forward switch's own record was already given the state because
    /// nothing could start (6.1 g); it is not written again for that.
    pub unstarted_recorded: bool,
}

/// The restart of an active worker that isn't running.
#[derive(Debug, Clone)]
pub struct Restart {
    pub due: Instant,
    /// Where the ladder stands: the rung and its failed tries, three per rung
    /// (6.1 g).
    pub progress: Progress,
    /// `progress` was read from the journal's ladder record at start: it is
    /// checked against the ladder's rungs once, before it is used, and
    /// ignored if it names one that is not there.
    pub recorded: bool,
}

impl Tracked {
    /// Where the ladder stands once this worker is gone: a worker that never
    /// reported `loaded` failed to start, and its rung is charged for it; one
    /// that loaded starts again from its own rung with nothing against it.
    pub fn progress_after_exit(&self) -> Progress {
        let Some(rung) = &self.rung else {
            return Progress::default();
        };
        let mut progress = Progress {
            current: None,
            spent: self.spent.clone(),
        };
        if self.awaiting_loaded {
            progress.failed(rung, self.attempts);
        } else {
            progress.loaded(rung);
        }
        progress
    }
}

/// A state write that failed, to be tried again (3.6).
#[derive(Debug, Clone)]
pub struct PendingState {
    pub name: &'static str,
    pub outcome: Outcome,
    pub attempts: u32,
    pub due: Instant,
    /// The same outcome is held by the journal; remove it there once the
    /// state has it.
    pub in_journal: bool,
}

/// Everything the core keeps in memory. A panic drops it; the journal on disk
/// is what the re-entry recovers from.
pub struct Runtime {
    pub journal: Option<Journal>,
    pub journal_stale: bool,
    pub journal_retry: Option<Instant>,
    pub pending: Vec<PendingState>,
    pub run: Option<Run>,
    pub disabled: Option<String>,
    pub degraded: Option<String>,
    pub keep_all: bool,
    pub listeners: Arc<Vec<(String, TcpListener)>>,
    pub workers: BTreeMap<WorkerId, Tracked>,
    pub active: Option<WorkerId>,
    pub watch: Option<Watch>,
    pub returning: Option<Returning>,
    pub restart: Option<Restart>,
    pub backoff: Backoff,
    pub install_due: Option<Instant>,
    /// When the next prune of `versions/` is due: set at start, and again
    /// after a confirm, a busy update lock or a refusal. `None` when none is
    /// pending.
    pub prune_due: Option<Instant>,
    /// Consecutive prunes that were refused (not merely busy): the backoff.
    pub prune_refusals: u32,
    /// A worker needed a newer supervisor (an incompatible spawn): the service
    /// must be restarted. Kept for this supervisor's life; status shows it.
    pub needs_restart: bool,
    /// The transition lock (5.1 step 0).
    pub switching: bool,
    pub stop_requested: bool,
    /// What `status` shows of the open switch while one is running.
    pub view: Option<SwitchView>,
    /// The execution path of every binary the driver refused as untrusted
    /// (2.5) in this run: a kept `versions/<v>` copy or the installed file.
    /// The refusal is about that location, not the bytes (the installed file
    /// may hold the same bytes and be healthy). It is the environment's
    /// fault: no ladder retries one.
    pub untrusted: Vec<std::path::PathBuf>,
    /// A ladder write failed while a switch was open: the reason switching is
    /// to be turned off, applied when the switch's handler ends (round 8b).
    /// Until then no ladder rung is recorded, and none is spawned on the
    /// strength of a write.
    pub ladder_off: Option<String>,
    /// The switch-off has applied (the Core's `apply_ladder_off`, or start-up's
    /// `switch_off`): no journal and no state write reaches the disk from any
    /// path until a restart. One flag for both routes, and not "no journal":
    /// refusal mode and a start-up with switching disabled keep their own
    /// behaviour.
    pub switched_off: bool,
    /// Test seam: the journal as it was when the switch's handler settled,
    /// before the deferred switch-off dropped it.
    #[cfg(test)]
    pub settle_snapshot: Option<Journal>,
}

/// Why `spawn_run` did not start a worker.
pub(crate) enum RunError {
    /// The start failed; it counts as an attempt.
    Failed(String),
    /// The driver refused the binary as untrusted: the environment's, and not
    /// an attempt (the ladder moves on without that version).
    Untrusted(String),
    /// A service stop arrived while waiting.
    Stopped,
}

impl Runtime {
    /// The runtime of a supervisor that has just started (or re-entered).
    pub fn from_started(
        started: &Started,
        registry: &Registry,
        cfg: &Config,
        clock: &dyn Clock,
        now: Instant,
    ) -> Self {
        let mut runtime = Self {
            journal: started.journal.clone(),
            journal_stale: started.journal_stale,
            journal_retry: started
                .journal_stale
                .then_some(now + cfg.timings.status_every),
            pending: Vec::new(),
            run: started.run.clone(),
            disabled: started.switching_disabled.clone(),
            degraded: started.degraded.clone(),
            keep_all: started.keep_all_versions,
            listeners: started.listeners.clone(),
            workers: BTreeMap::new(),
            active: None,
            watch: None,
            returning: None,
            restart: None,
            backoff: Backoff::new(&cfg.timings),
            install_due: None,
            prune_due: None,
            prune_refusals: 0,
            needs_restart: false,
            switching: false,
            stop_requested: false,
            view: None,
            untrusted: Vec::new(),
            ladder_off: None,
            switched_off: started.switched_off,
            #[cfg(test)]
            settle_snapshot: None,
        };
        // Where the ladder stood, as the journal has it. Switching off persists
        // nothing, so nothing old is trusted either. A record of another ladder,
        // or one naming a rung that is not on this ladder, is ignored.
        let record = if runtime.disabled.is_none() {
            runtime.journal.as_ref().and_then(|journal| {
                let known = journal
                    .ladder
                    .as_ref()
                    .map(|record| progress_of(cfg, record))
                    .unwrap_or_default();
                let full = list_of(
                    cfg,
                    journal,
                    started.installed_rung.clone(),
                    started.installed_sha256.as_deref(),
                    &known,
                );
                usable_record(cfg, journal, &full).cloned()
            })
        } else {
            None
        };
        if let (Some(id), Some(run)) = (started.worker, started.run.as_ref())
            && let Some(port) = registry.port(id)
        {
            runtime.workers.insert(
                id,
                Tracked::new(run.artifact.clone(), WorkerRole::Active, port, now, false),
            );
            if let Some(tracked) = runtime.workers.get_mut(&id) {
                tracked.awaiting_loaded = true;
                // From the handshake, not from now: start-up wrote the journal
                // after it, and that time is the worker's.
                tracked.load_deadline =
                    Some(started.worker_greeted.unwrap_or(now) + cfg.timings.active_load);
                // The rung start-up chose: if it exits before `loaded`, that
                // is one failed start on it, and the restart goes on from it.
                tracked.rung = Some(run.clone());
                // Start-up counted this try before it spawned: the record has
                // it, and it is not counted twice.
                if let Some(record) = &record {
                    tracked.spent = record.spent.iter().map(|rung| run_of(cfg, rung)).collect();
                    if record
                        .current
                        .as_ref()
                        .is_some_and(|rung| same_rung(&run_of(cfg, rung), run))
                    {
                        tracked.attempts = record.attempts.saturating_sub(1);
                    }
                }
            }
            runtime.active = Some(id);
        } else if started.run.is_some() || started.degraded.is_some() {
            // A worker that didn't start is retried with backoff. A failed
            // start-up spawn already counts as one try on its rung: in the
            // record when there is one, else here.
            let recorded = record.is_some();
            let progress = match &record {
                Some(record) => progress_of(cfg, record),
                None => {
                    let mut progress = Progress::default();
                    if started.run_failed
                        && let Some(run) = &started.run
                    {
                        progress.failed(run, 0);
                    }
                    progress
                }
            };
            runtime.restart = Some(Restart {
                due: now + cfg.timings.restart_first,
                progress,
                recorded,
            });
        }
        if let Some(switch) = runtime
            .journal
            .as_ref()
            .and_then(|journal| journal.switch.as_ref())
            && let Some(ret) = &switch.return_
        {
            runtime.returning = Some(Returning {
                open: Open {
                    id: switch.id.clone(),
                    how: switch.how,
                    from: switch.from.clone(),
                    to: switch.to.clone(),
                    started: switch.started.clone(),
                    phase: switch.phase,
                    reached_ready: switch.phase == Phase::Watching,
                },
                ret: ret.clone(),
                // The ladder is always walked from the switch's `from`; the
                // journalled target says where on it recovery stands.
                initial_target: switch.from.clone(),
                // Recovery records the outcome (state file or journal) without
                // `returned_to`; J-R2 adds that by id. That is taken from the
                // evidence, not assumed: the journal holds the outcome, or
                // the state file has the forward switch's failed record.
                unstarted_recorded: journal_has_outcome(&runtime.journal, &switch.id)
                    || state_has_failed_record(cfg, &switch.id),
            });
            // The journalled target is the rung recovery chose; a start-up
            // spawn of it that failed counts as one try.
            let rung = if ret.installed {
                Run {
                    binary: cfg.installed.clone(),
                    artifact: ret.target.clone(),
                }
            } else {
                Run {
                    binary: cfg.data.binary(&ret.target.version, &cfg.binary_name),
                    artifact: ret.target.clone(),
                }
            };
            // Start-up runs the rung where the ladder has it (the installed
            // rung from `cfg.installed`), so a failed try is charged to the
            // same location and bytes, never to the bytes alone.
            let tried = u32::from(
                started.run_failed
                    && started
                        .run
                        .as_ref()
                        .is_some_and(|run| same_rung(run, &rung)),
            );
            let (progress, recorded) = match &record {
                Some(record) => (progress_of(cfg, record), true),
                None => (Progress::on(rung, tried), false),
            };
            runtime.restart = Some(Restart {
                due: now + cfg.timings.restart_first,
                progress,
                recorded,
            });
        }
        // No install retry while switching is off (the installed file isn't
        // touched in an environment that was found unsafe); the restarts above
        // are not gated.
        if runtime.disabled.is_none()
            && let Some(pending) = runtime
                .journal
                .as_ref()
                .and_then(|journal| journal.install_pending.as_ref())
        {
            // The retry time a failed attempt recorded survives a restart
            // (B-L1); a missing or unreadable one falls back to the first wait.
            let recorded = pending
                .next_try
                .as_deref()
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|at| at.with_timezone(&chrono::Utc) - clock.now());
            runtime.install_due = Some(match recorded {
                Some(left) => now + left.to_std().unwrap_or(Duration::ZERO),
                None => now + cfg.timings.install_retry,
            });
        }
        if started.state_pending
            && let Some(journal) = &runtime.journal
        {
            for outcome in journal.outcomes.clone() {
                runtime.pending.push(PendingState {
                    name: "S3",
                    outcome,
                    attempts: 0,
                    due: now + cfg.timings.install_retry,
                    in_journal: true,
                });
            }
        }
        runtime
    }
}

/// The journal holds an outcome for switch `id`.
fn journal_has_outcome(journal: &Option<Journal>, id: &str) -> bool {
    journal
        .as_ref()
        .is_some_and(|journal| journal.outcomes.iter().any(|held| held.record.id == id))
}

/// The state file has a failed record for switch `id`: the forward switch's
/// outcome was already given to it.
fn state_has_failed_record(cfg: &Config, id: &str) -> bool {
    let Ok(state) = open_ferry_update::state::State::load_strict(&cfg.data.state_file()) else {
        return false;
    };
    state
        .last_switch
        .iter()
        .chain(state.history.iter())
        .any(|record| {
            record.id == id
                && matches!(
                    record.result.as_deref(),
                    Some("failed-to-load" | "failed-after-switch")
                )
        })
}

impl Tracked {
    pub fn new(
        artifact: Artifact,
        role: WorkerRole,
        port: &WorkerPort,
        now: Instant,
        wants_peers_gone: bool,
    ) -> Self {
        Self {
            artifact,
            role,
            pid: port.identity.pid,
            proc_started: port.identity.started,
            since: now,
            lost: false,
            kill_at: None,
            live: 0,
            signins: 0,
            status_seq: 0,
            wants_peers_gone,
            peers_gone_sent: false,
            awaiting_loaded: false,
            load_deadline: None,
            load_timed_out: false,
            load_failed: false,
            rung: None,
            attempts: 0,
            spent: Vec::new(),
            drain: None,
        }
    }
}

/// The core.
pub struct Core<'a, D: Driver, H: Host, S: Disk> {
    pub cfg: &'a Config,
    pub driver: &'a D,
    pub host: &'a H,
    pub disk: &'a S,
    pub clock: &'a dyn Clock,
    pub registry: &'a mut Registry,
    pub shared: Arc<Shared>,
    pub commands: &'a mut UnboundedReceiver<Command>,
    pub rt: Runtime,
    pub(crate) next_tick: Instant,
    /// A source of switch ids.
    pub(crate) counter: u64,
    /// The run part of this Core's ids, drawn once in `new`.
    pub(crate) run_part: String,
    /// What the switch in progress was admitted with (5.3).
    pub(crate) admitted: Option<Admission>,
    /// The forward window displaced by a rollback until that rollback commits.
    pub(crate) displaced_watch: Option<(Watch, open_ferry_update::journal::Switch)>,
    /// The last journal write landed but was not synced (item 10).
    pub(crate) not_durable: bool,
    /// The supervisor's own reading of the effective mode (5.3): the last
    /// evaluation, and a generation that grows when one differs from it.
    pub(crate) mode_last: Option<crate::host::ModeSnapshot>,
    pub(crate) mode_gen: u64,
    /// Non-switched targets stay cooled down even when history evicts them.
    pub(crate) automatic_cooldowns: BTreeMap<String, chrono::DateTime<chrono::Utc>>,
    /// The active worker, watched through the waits before its hand over.
    pub(crate) watch_a: Option<WorkerId>,
    /// Where A's ladder stood when A was removed during a switch, read before
    /// anything could forget it. Every restart a switch route makes for A goes
    /// on from it; `None` outside a switch or while A is still tracked.
    pub(crate) a_progress: Option<Progress>,
    /// Irreversible follow-ups held back until a durable write.
    pub(crate) deferred: Deferred,
    /// The worker whose spawn (and handshake) the switch is waiting for. A
    /// child that was created but has no port yet is tracked by the registry
    /// only; the switch keeps its identity so every way out waits for its
    /// real exit (reviewer H2, oracle H3).
    pub(crate) spawning: Option<WorkerId>,
    /// The handshake survivors the status and the journal's workers last
    /// listed (see `survivors_changed`).
    pub(crate) listed_survivors: Vec<WorkerId>,
    /// The last `keep_set` reply and what it was computed from (see
    /// `keep_reply`).
    keep_cache: std::sync::Mutex<Option<KeepCache>>,
}

/// The run part of a Core's switch ids: its start in UNIX milliseconds as 12
/// fixed-width lowercase hex digits, then 4 random hex digits. Ids order by
/// (start ms, counter), which the state file uses to break same-second ties
/// between records (a wall clock that steps backwards across a restart can
/// misorder two runs).
fn run_part(start_ms: i64) -> String {
    let mut bytes = [0u8; 2];
    let _ = getrandom::fill(&mut bytes);
    format!(
        "{:012x}{:04x}",
        start_ms.max(0) as u64 & 0xffff_ffff_ffff,
        u16::from_be_bytes(bytes)
    )
}

/// A computed `keep_set` reply, and the inputs it is valid for.
struct KeepCache {
    journal: Option<Journal>,
    tracked: Vec<String>,
    keep_all: bool,
    at: Instant,
    reply: Option<Vec<String>>,
}

/// What waits for a durable journal.
#[derive(Debug, Default)]
pub(crate) struct Deferred {
    pub pin: Option<String>,
    /// The switch whose `handover.json` is to go.
    pub handover: Option<String>,
}

impl<'a, D: Driver, H: Host, S: Disk> Core<'a, D, H, S> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: &'a Config,
        driver: &'a D,
        host: &'a H,
        disk: &'a S,
        clock: &'a dyn Clock,
        registry: &'a mut Registry,
        shared: Arc<Shared>,
        commands: &'a mut UnboundedReceiver<Command>,
        rt: Runtime,
    ) -> Self {
        let mut core = Self {
            cfg,
            driver,
            host,
            disk,
            clock,
            registry,
            shared,
            commands,
            rt,
            next_tick: Instant::now() + TICK,
            counter: 0,
            run_part: run_part(clock.now().timestamp_millis()),
            admitted: None,
            displaced_watch: None,
            not_durable: false,
            mode_last: None,
            mode_gen: 0,
            automatic_cooldowns: BTreeMap::new(),
            watch_a: None,
            a_progress: None,
            deferred: Deferred::default(),
            spawning: None,
            listed_survivors: Vec::new(),
            keep_cache: std::sync::Mutex::new(None),
        };
        let records: Vec<_> = core
            .rt
            .journal
            .iter()
            .flat_map(|journal| &journal.outcomes)
            .chain(core.rt.pending.iter().map(|pending| &pending.outcome))
            .map(|outcome| outcome.record.clone())
            .collect();
        for record in records {
            core.remember_outcome(&record);
        }
        core
    }

    // ---- small helpers ----------------------------------------------------

    pub(crate) fn stamp(&self) -> String {
        self.clock
            .now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    pub(crate) fn prune_automatic_cooldowns(&mut self) {
        let now = self.clock.now();
        self.automatic_cooldowns
            .retain(|_, ended| now - *ended < AUTOMATIC_COOLDOWN);
    }

    fn remember_outcome(&mut self, record: &SwitchRecord) {
        self.prune_automatic_cooldowns();
        if let Some(ended) = unfinished_at(record) {
            self.automatic_cooldowns
                .entry(record.to.clone())
                .and_modify(|last| *last = (*last).max(ended))
                .or_insert(ended);
        }
    }

    pub(crate) fn stamp_before(&self, ago: Duration) -> String {
        (self.clock.now() - chrono::Duration::from_std(ago).unwrap_or_default())
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    pub(crate) fn stamp_after(&self, wait: Duration) -> String {
        (self.clock.now() + chrono::Duration::from_std(wait).unwrap_or_default())
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// 1 minute doubling to 1 hour (3.6, 3.9), as configured.
    pub(crate) fn retry_wait(&self, attempts: u32) -> Duration {
        let factor = 1u32.checked_shl(attempts).unwrap_or(u32::MAX);
        self.cfg
            .timings
            .install_retry
            .saturating_mul(factor)
            .min(self.cfg.timings.install_retry_max)
    }

    /// Reads the effective mode and grows `mode_gen` when it differs from the
    /// last reading (5.3).
    pub(crate) fn eval_mode(&mut self) -> crate::host::ModeSnapshot {
        let now = self.host.effective_mode();
        if self
            .mode_last
            .is_some_and(|last| last.off != now.off || last.mode_gen != now.mode_gen)
        {
            self.mode_gen += 1;
        }
        self.mode_last = Some(now);
        now
    }

    /// The id of the next operation: `sw-<run>-<counter>`. See [`run_part`].
    pub(crate) fn new_id(&mut self) -> String {
        self.counter += 1;
        format!("sw-{}-{}", self.run_part, self.counter)
    }

    /// Lists the handshake survivors in the status and the journal's workers
    /// whenever the registry's set differs from what was last listed: when a
    /// failed spawn leaves a child, and again once its `Exited` is consumed
    /// (a switch or not). Every wait and tick calls it, so no path needs a
    /// journal write of its own to show a survivor.
    pub(crate) fn survivors_changed(&mut self) {
        let now = self.registry.survivors();
        if now == self.listed_survivors {
            return;
        }
        self.listed_survivors = now;
        self.publish();
        let _ = self.write_journal("J-workers");
    }

    pub(crate) fn sync_workers(&mut self) {
        let mut workers: Vec<Worker> = self
            .rt
            .workers
            .values()
            .map(|tracked| Worker {
                pid: tracked.pid,
                started: tracked.proc_started,
                version: tracked.artifact.version.clone(),
                role: tracked.role,
                lost: tracked.lost,
            })
            .collect();
        // A child of a failed handshake that has not exited is still a worker
        // of the data dir (recovery waits on the journal's workers). The
        // record has no survivor role: it is listed as the standby it was
        // meant to be.
        for id in self.registry.survivors() {
            if let Some((identity, version)) = self.registry.survivor_info(id) {
                workers.push(Worker {
                    pid: identity.pid,
                    started: identity.started,
                    version,
                    role: WorkerRole::Standby,
                    lost: false,
                });
            }
        }
        if let Some(journal) = self.rt.journal.as_mut() {
            journal.workers = workers;
        }
    }

    /// Writes the in-memory journal as `name`. A failure sets `journal_stale`
    /// and schedules a retry; the in-memory journal stays as it is. A write
    /// that landed but could not be synced counts as written.
    pub(crate) fn write_journal(&mut self, name: &str) -> Result<(), String> {
        if self.rt.switched_off {
            tracing::warn!("journal write {name} dropped: switching is off");
            return Err("switching is off".to_owned());
        }
        self.sync_workers();
        let Some(journal) = self.rt.journal.as_mut() else {
            return Err("there is no journal".to_owned());
        };
        match self.disk.write_journal(journal, name) {
            Ok(()) => {
                self.rt.journal_stale = false;
                self.rt.journal_retry = None;
                self.not_durable = false;
                self.run_deferred();
                Ok(())
            }
            Err(open_ferry_update::journal::JournalError::NotDurable(error)) => {
                // The disk has the state, but a crash may lose it. Keep going
                // in memory, retry the same write, and do nothing irreversible
                // until a write of this state is durable (item 10).
                tracing::warn!("journal write {name} landed but could not be synced: {error}");
                self.rt.journal_stale = true;
                self.rt.journal_retry = Some(Instant::now() + self.cfg.timings.status_every);
                self.not_durable = true;
                Ok(())
            }
            Err(error) => {
                tracing::error!("journal write {name} failed: {error}");
                self.rt.journal_stale = true;
                self.rt.journal_retry = Some(Instant::now() + self.cfg.timings.status_every);
                Err(error.to_string())
            }
        }
    }

    /// Writes the journal and, on a failure, puts the in-memory journal back
    /// as it was (J1 to J3: nothing has been done that the disk must know).
    pub(crate) fn write_journal_or_undo(
        &mut self,
        before: &Journal,
        name: &str,
    ) -> Result<(), String> {
        let result = self.write_journal(name);
        if result.is_err() {
            self.rt.journal = Some(before.clone());
            // The undo is exactly the disk's state: nothing is stale, unless
            // an earlier write of it was not synced.
            self.rt.journal_stale = self.not_durable;
            self.rt.journal_retry = self
                .not_durable
                .then(|| Instant::now() + self.cfg.timings.status_every);
        }
        result
    }

    /// The pin is consumed only once a durable journal holds J1 (item 5's
    /// hook); until then it waits.
    pub(crate) fn consume_pin_when_durable(&mut self, pin: &str) {
        if self.not_durable {
            self.deferred.pin = Some(pin.to_owned());
        } else {
            self.publish_keep_before_pin_goes();
            if let Err(why) = self.disk.consume_pin(pin) {
                tracing::warn!("could not consume pin {pin}: {why}");
            }
        }
    }

    /// The pin protects its version in the `keep_set` reply until the journal
    /// does. A reply cached before the pin was written lacks that version, and
    /// would go on lacking it once the pin file is gone, until the next
    /// publish: so the reply is recomputed and published first, while the pin
    /// still stands, and the file is deleted after.
    fn publish_keep_before_pin_goes(&self) {
        self.forget_keep_reply();
        self.publish();
    }

    /// `handover.json` goes only after A has resumed or exited, and only once
    /// the journal that says so is durable.
    pub(crate) fn remove_handover_when_durable(&mut self, switch_id: &str) {
        if self.not_durable {
            self.deferred.handover = Some(switch_id.to_owned());
        } else {
            self.disk.remove_handover();
        }
    }

    /// Work held back for a durable journal is still pending.
    pub(crate) fn cleanup_pending(&self) -> bool {
        self.not_durable || self.deferred.pin.is_some() || self.deferred.handover.is_some()
    }

    fn run_deferred(&mut self) {
        if let Some(pin) = self.deferred.pin.take() {
            self.publish_keep_before_pin_goes();
            if let Err(why) = self.disk.consume_pin(&pin) {
                tracing::warn!("could not consume pin {pin}: {why}");
            }
        }
        if let Some(owner) = self.deferred.handover.take() {
            // The file belongs to the switch that deferred its delete. If a
            // newer switch is open it may have written its own: leave it
            // (oracle H2).
            let newer = self
                .rt
                .journal
                .as_ref()
                .and_then(|journal| journal.switch.as_ref())
                .is_some_and(|switch| switch.id != owner);
            if newer {
                tracing::warn!("kept the hand over file: switch {owner} is no longer the open one");
            } else {
                self.disk.remove_handover();
            }
        }
    }

    /// Gives the state `outcome`. On a failure the outcome is queued for
    /// retry in memory (and stays in the journal if it is held there).
    /// Returns whether the state has it.
    pub(crate) fn write_state(
        &mut self,
        name: &'static str,
        outcome: &Outcome,
        in_journal: bool,
    ) -> bool {
        if self.rt.switched_off {
            // Switching is off: nothing reaches the disk, and nothing is
            // queued to reach it later.
            tracing::warn!("state write {name} dropped: switching is off");
            return false;
        }
        // The state's `staged` and `previous` feed the keep set.
        self.forget_keep_reply();
        // A write may be only a patch (for example the install result). Carry
        // everything this operation still owes the state, in chronological
        // order, so a successful patch cannot acknowledge an absent result.
        let id = &outcome.record.id;
        let mut complete = self
            .rt
            .pending
            .iter()
            .find(|held| held.outcome.record.id == *id)
            .map(|held| held.outcome.clone());
        let journal_outcome = self
            .rt
            .journal
            .as_ref()
            .and_then(|journal| journal.outcomes.iter().find(|held| held.record.id == *id));
        if let Some(held) = journal_outcome {
            if let Some(complete) = complete.as_mut() {
                complete.record.merge(&held.record);
                if held.mark_failed.is_some() {
                    complete.mark_failed.clone_from(&held.mark_failed);
                }
            } else {
                complete = Some(held.clone());
            }
        }
        let mut complete = complete.unwrap_or_else(|| outcome.clone());
        complete.record.merge(&outcome.record);
        if outcome.mark_failed.is_some() {
            complete.mark_failed.clone_from(&outcome.mark_failed);
        }
        let in_journal = in_journal || journal_outcome.is_some();
        // Retire the superseded retry even when this new write fails.
        self.rt.pending.retain(|held| held.outcome.record.id != *id);
        self.remember_outcome(&complete.record);
        match self.disk.write_state(name, &complete) {
            Ok(()) => {
                if in_journal {
                    self.outcome_delivered(id);
                }
                true
            }
            Err(error) => {
                tracing::error!("state write {name} failed: {error}");
                self.queue_state(name, complete, in_journal, 0);
                false
            }
        }
    }

    fn queue_state(
        &mut self,
        name: &'static str,
        outcome: Outcome,
        in_journal: bool,
        attempts: u32,
    ) {
        if self.rt.switched_off {
            return;
        }
        let due = Instant::now() + self.retry_wait(attempts);
        if let Some(held) = self
            .rt
            .pending
            .iter_mut()
            .find(|held| held.outcome.record.id == outcome.record.id)
        {
            held.name = name;
            held.outcome.record.merge(&outcome.record);
            if outcome.mark_failed.is_some() {
                held.outcome.mark_failed = outcome.mark_failed;
            }
            held.in_journal |= in_journal;
            return;
        }
        self.rt.pending.push(PendingState {
            name,
            outcome,
            attempts,
            due,
            in_journal,
        });
    }

    /// The state has the outcome with `id`: take it out of the journal (J8).
    pub(crate) fn outcome_delivered(&mut self, id: &str) {
        let structural_stale = self.rt.journal_stale;
        let removed = self
            .rt
            .journal
            .as_mut()
            .is_some_and(|journal| journal.remove_outcome(id));
        if removed && self.write_journal("J8").is_err() && !structural_stale {
            // Harmless: replay is idempotent by id (3.6).
            self.rt.journal_stale = false;
            self.rt.journal_retry = None;
        }
    }

    // ---- snapshot ---------------------------------------------------------

    pub(crate) fn publish(&self) {
        let rt = &self.rt;
        let mut workers = Vec::new();
        for tracked in rt.workers.values() {
            workers.push(WorkerView {
                pid: tracked.pid,
                version: tracked.artifact.version.clone(),
                role: match tracked.role {
                    WorkerRole::Active => "active",
                    WorkerRole::Standby => "standby",
                    WorkerRole::Draining => "draining",
                }
                .to_owned(),
                lost: tracked.lost,
                live: tracked.live,
                signins_pending: tracked.signins,
            });
        }
        // A handshake survivor: running, with no port, until its `Exited`.
        for id in self.registry.survivors() {
            if let Some((identity, version)) = self.registry.survivor_info(id) {
                workers.push(WorkerView {
                    pid: identity.pid,
                    version,
                    role: "survivor".to_owned(),
                    lost: false,
                    live: 0,
                    signins_pending: 0,
                });
            }
        }
        let switch = rt.view.clone().or_else(|| {
            rt.watch.as_ref().map(|watch| SwitchView {
                id: watch.open.id.clone(),
                how: watch.open.how.as_str().to_owned(),
                from: watch.open.from.version.clone(),
                to: watch.open.to.version.clone(),
                phase: Phase::Watching.as_str().to_owned(),
                status: Phase::Watching.as_str().to_owned(),
                since: watch.open.started.clone(),
                cancellable: false,
                watch_until: Some(
                    self.stamp_after(watch.deadline.saturating_duration_since(Instant::now())),
                ),
            })
        });
        let state = if rt.stop_requested {
            "stopping"
        } else if rt.degraded.is_some() {
            "degraded"
        } else if rt.switching {
            "switching"
        } else if rt.watch.is_some() {
            "watching"
        } else if rt.degraded.is_some() || rt.active.is_none() {
            "degraded"
        } else {
            "idle"
        };
        let install_pending = rt
            .journal
            .as_ref()
            .and_then(|journal| journal.install_pending.as_ref())
            .map(|pending| InstallView {
                target: pending.target.version.clone(),
                attempts: pending.attempts,
                next_try: pending.next_try.clone(),
            });
        // Released workers still draining: the earliest start and cap, the
        // connections they hold.
        let now = Instant::now();
        let drains: Vec<_> = rt
            .workers
            .values()
            .filter_map(|tracked| tracked.drain.as_ref().map(|drain| (tracked.live, drain)))
            .collect();
        let drain = (!drains.is_empty()).then(|| DrainView {
            live: drains.iter().map(|(live, _)| live).sum(),
            since: self.stamp_before(
                drains
                    .iter()
                    .map(|(_, drain)| now.saturating_duration_since(drain.since))
                    .max()
                    .unwrap_or_default(),
            ),
            cap_at: self.stamp_after(
                drains
                    .iter()
                    .map(|(_, drain)| drain.deadline.saturating_duration_since(now))
                    .min()
                    .unwrap_or_default(),
            ),
        });
        self.shared.publish(Snapshot {
            state: state.to_owned(),
            supervisor_version: self.host.own_version(),
            installed_version: rt
                .journal
                .as_ref()
                .and_then(|journal| journal.installed.as_ref())
                .map(|installed| installed.version.clone()),
            commit_pending: install_pending.is_some(),
            needs_restart: rt.needs_restart,
            journal: if rt.journal_stale { "stale" } else { "ok" }.to_owned(),
            state_write: if !rt.pending.is_empty()
                || rt
                    .journal
                    .as_ref()
                    .is_some_and(|journal| !journal.outcomes.is_empty())
            {
                "pending"
            } else {
                "ok"
            }
            .to_owned(),
            drain,
            switch,
            active_version: rt
                .active
                .and_then(|id| rt.workers.get(&id))
                .map(|tracked| tracked.artifact.version.clone()),
            workers,
            journal_stale: rt.journal_stale,
            state_write_pending: !rt.pending.is_empty()
                || rt
                    .journal
                    .as_ref()
                    .is_some_and(|journal| !journal.outcomes.is_empty()),
            switching_disabled: rt.disabled.clone(),
            degraded: rt.degraded.clone(),
            install_pending,
            prune_pending: rt.prune_due.is_some(),
            keep: self.keep_reply(),
        });
    }

    // ---- keep set and prune (3.5) ----------------------------------------

    /// The versions of every worker the core tracks: active, standby,
    /// draining, and the handshake survivors the registry still holds.
    fn tracked_versions(&self) -> Vec<String> {
        let mut versions: Vec<String> = self
            .rt
            .workers
            .values()
            .map(|tracked| tracked.artifact.version.clone())
            .collect();
        for id in self.registry.survivors() {
            if let Some((_, version)) = self.registry.survivor_info(id) {
                versions.push(version);
            }
        }
        versions.sort();
        versions.dedup();
        versions
    }

    /// True while the journal or the state is not durably what memory says: the
    /// prune holds off, and so does the keep set the endpoint offers.
    fn write_gate_shut(&self) -> bool {
        self.not_durable || self.rt.journal_stale || !self.rt.pending.is_empty()
    }

    fn forget_keep_reply(&self) {
        if let Ok(mut cache) = self.keep_cache.lock() {
            *cache = None;
        }
    }

    /// What the `keep_set` reply lists, computed here (never in a handler) and
    /// published with the snapshot. It is the full 3.5 set; with `keep_all`, or
    /// when the pins or the journal cannot be read (or the journal is of a newer
    /// format), it is every version directory present. The reply is recomputed
    /// when the journal or the tracked workers change, and at least every
    /// `status_every`, which catches a worker that died, a pin that expired and
    /// a state write. It may therefore lag a pin file written since the last
    /// publish (by at most that interval): pins are files the endpoint's
    /// callers write under the update lock, and the core does not watch them. A
    /// pin protects its version from the prune regardless, which reads the
    /// pins itself under the lock; the reply is the advice a caller gets
    /// before it stages.
    ///
    /// `None` is "not available" (the snapshot's `keep`, and the endpoint's
    /// error reply): with `keep_all`, or when the keep set cannot be computed,
    /// the reply is every version directory plus every tracked worker's
    /// version, and when `versions/` cannot be listed completely there is no
    /// honest list at all. An empty or partial list would let a caller prune
    /// what is protected.
    ///
    /// Deviation, written down once: a cache miss reads the journal, the pins
    /// and the state and lists two directories, synchronously on the core's
    /// task. That is the same class as the core's synchronous journal writes
    /// (which also fsync), it runs only on a miss (a changed input, or every
    /// `status_every`), and it reads a handful of small files. It is not moved
    /// to a blocking task; a disk slow enough for it to matter stalls those
    /// writes too.
    fn keep_reply(&self) -> Option<Vec<String>> {
        let tracked = self.tracked_versions();
        let now = Instant::now();
        let journal = self.rt.journal.as_ref();
        let keep_all = self.rt.keep_all;
        // Not while the prune itself would hold off (`try_prune`): a journal
        // or state write that is not durable means the set computed from
        // memory can name fewer versions than what a crash would bring back,
        // and a caller that prunes by it would delete a protection. This is
        // checked before the cache, so a cached success is never served while
        // the gate is shut. With `keep_all` the reply is every version, which
        // protects everything whatever is durable.
        if !keep_all && self.write_gate_shut() {
            return None;
        }
        if let Ok(cache) = self.keep_cache.lock()
            && let Some(held) = cache.as_ref()
            && held.keep_all == keep_all
            && held.journal.as_ref() == journal
            && held.tracked == tracked
            && now.saturating_duration_since(held.at) < self.cfg.timings.status_every
        {
            return held.reply.clone();
        }
        // Every version on disk and every version a worker runs (a tracked
        // version may have no directory), or nothing when the listing failed.
        let every = || match self.disk.all_versions() {
            Ok(mut versions) => {
                versions.extend(tracked.iter().cloned());
                Some(versions.into_iter().collect::<Vec<_>>())
            }
            Err(why) => {
                tracing::warn!("the versions could not be listed ({why}): no keep set is offered");
                None
            }
        };
        let reply = if keep_all {
            every()
        } else {
            match self.disk.keep_set(journal, &tracked, self.clock.now()) {
                Ok(keep) => Some(keep.into_iter().collect()),
                Err(why) => {
                    tracing::warn!(
                        "the keep set could not be computed ({why}): listing every version"
                    );
                    every()
                }
            }
        };
        if let Ok(mut cache) = self.keep_cache.lock() {
            *cache = Some(KeepCache {
                journal: journal.cloned(),
                tracked,
                keep_all,
                at: now,
                reply: reply.clone(),
            });
        }
        reply
    }

    /// Prunes `versions/` (3.5): at start, after a confirm, and on the timer a
    /// busy lock or a refusal leaves. It never waits for the update lock (T3):
    /// the disk takes it with `try_lock` and, when it is held, the prune is
    /// tried again after `status_every`. The blocking work runs in
    /// `spawn_blocking` holding the lock, and nothing here waits on the
    /// endpoint. It never runs with `keep_all`, and not while switching is
    /// disabled (the environment was found unsafe to write in). Nothing is
    /// deleted while the journal or the state file is not durably what memory
    /// says (item 10: nothing irreversible until a write of this state is
    /// durable): a crash would bring back an older journal or state whose
    /// protections (`previous`, a recovery fallback) the prune has just
    /// removed. The prune then waits for its timer, and a journal or state
    /// retry that lands makes it go through.
    pub(crate) async fn try_prune(&mut self) {
        self.rt.prune_due = None;
        if self.rt.keep_all || self.rt.disabled.is_some() {
            return;
        }
        // A state write that landed but could not be synced is an error that
        // queues it in `pending`, like one that failed outright (`apply` uses
        // the plain save), so `pending` covers both.
        if self.write_gate_shut() {
            self.rt.prune_due = Some(Instant::now() + self.cfg.timings.status_every);
            return;
        }
        let request = crate::disk::PruneRequest {
            journal: self.rt.journal.clone(),
            tracked: self.tracked_versions(),
            now: self.clock.now(),
        };
        match self.disk.prune(request).await {
            crate::disk::PruneOutcome::Done {
                removed,
                pins_removed,
            } => {
                self.rt.prune_refusals = 0;
                if !removed.is_empty() || pins_removed > 0 {
                    tracing::info!(
                        "pruned the versions {removed:?} and {pins_removed} expired pin(s)"
                    );
                }
                self.forget_keep_reply();
            }
            crate::disk::PruneOutcome::Partial { removed, why } => {
                // What was deleted is gone; only the expired pins are left, and
                // they are retried on the refusal backoff.
                tracing::warn!(
                    "pruned the versions {removed:?}, but could not remove the expired pins: {why}"
                );
                self.forget_keep_reply();
                let wait = self.retry_wait(self.rt.prune_refusals);
                self.rt.prune_refusals = self.rt.prune_refusals.saturating_add(1);
                self.rt.prune_due = Some(Instant::now() + wait);
            }
            crate::disk::PruneOutcome::Busy => {
                self.rt.prune_due = Some(Instant::now() + self.cfg.timings.status_every);
            }
            crate::disk::PruneOutcome::Refused(why) => {
                tracing::warn!("the prune was refused and deleted nothing: {why}");
                let wait = self.retry_wait(self.rt.prune_refusals);
                self.rt.prune_refusals = self.rt.prune_refusals.saturating_add(1);
                self.rt.prune_due = Some(Instant::now() + wait);
            }
        }
    }

    // ---- the loop ---------------------------------------------------------

    /// Runs until a stop. Panics are caught above this (see `supervisor.rs`).
    pub async fn run(&mut self) -> Exit {
        self.publish();
        self.peers_gone_check();
        // The first run step: prune what the last run left (3.5).
        self.try_prune().await;
        self.publish();
        loop {
            self.fault_check();
            let tick = self.next_tick;
            tokio::select! {
                (id, event) = self.registry.next_event() => {
                    self.background_event(id, event).await;
                }
                command = self.commands.recv() => match command {
                    Some(Command::Switch { request, replies }) => {
                        self.handle_switch(request, replies).await;
                    }
                    Some(Command::Stop) | None => self.rt.stop_requested = true,
                },
                () = tokio::time::sleep_until(tick) => {
                    self.next_tick = Instant::now() + TICK;
                    self.on_tick().await;
                }
            }
            if self.rt.stop_requested {
                return self.stop().await;
            }
            self.publish();
        }
    }

    #[cfg(feature = "blue-green-test")]
    pub(crate) fn fault_check(&self) {
        // The test fault `supervisor-panic`: panic inside the core, once.
        let root = self.cfg.data.root();
        if open_ferry_update::test_seams::hit(root, "fault.supervisor-panic").is_err() {
            let _ = std::fs::remove_file(
                root.join(".blue-green-test")
                    .join("fault.supervisor-panic.fail"),
            );
            panic!("injected fault: the supervisor's core panicked");
        }
    }

    #[cfg(not(feature = "blue-green-test"))]
    #[inline(always)]
    pub(crate) fn fault_check(&self) {}

    /// A service stop (9.1): `stop` to every worker, the budget, then the
    /// handle's kill.
    ///
    /// A clean stop is not a failed start. When the active worker is still
    /// loading (no `loaded` yet) and the supervisor itself ended it, the try
    /// that was counted for it is taken back once the workers are gone
    /// (`clean_stop_take_back`), so repeated stops during loading (reboots
    /// during boot, quick service restarts) do not spend a healthy version.
    /// The count is journalled before a spawn only so that a crash of the
    /// supervisor or the machine while the worker runs still counts; here the
    /// supervisor stopped the worker itself.
    ///
    /// Only an end the supervisor caused is refunded (`Registry::stopped_cleanly`):
    /// an exit with success after `stop` was sent, or the exit after a kill the
    /// registry issued at the stop deadline that the worker's pump reports found
    /// the process running. A worker that had already failed (an exit, a
    /// `load_failed` the Core or the registry saw, or a lost channel before the
    /// stop), that fails during the stop budget by itself, that lost its
    /// channel after `stop` and then needed the kill, or that crashed before
    /// the kill reached it, keeps its count, and so does one whose load
    /// deadline had passed.
    ///
    /// The load deadline (`Tracked::load_deadline`) is what bounds a worker
    /// that hangs in loading: `timings.active_load` (3 minutes) after its
    /// handshake the worker is killed and the start counts as failed. Accepted trade-off: an external
    /// tool that stops the service again and again within that window while a
    /// broken version loads keeps that version on its rung; the deadline is the
    /// guard.
    ///
    /// The take-back changes only `attempts` in the ladder record. Its J-L
    /// write is a journal write like any other: it can also carry the worker
    /// list (`sync_workers`) and advances `seq`, and a failed write restores
    /// the record, not the whole journal. It is best-effort: a failed write
    /// leaves the count, logs one warning, and does not switch anything off.
    pub async fn stop(&mut self) -> Exit {
        self.rt.stop_requested = true;
        self.publish();
        // Who and which record are decided before the stop: events during
        // `stop_all` go to the registry, not to `note_message`, so a `loaded`
        // that arrives in the stop budget is not seen. What the worker did
        // during the stop is read from the registry afterwards.
        let take_back = self.clean_stop_take_back();
        self.registry
            .stop_all(self.cfg.timings.stop, Duration::from_secs(2))
            .await;
        // A stop's kill deadline is not permission to forget a surviving
        // worker: the service manager bounds the stop, not an internal timeout.
        self.registry.wait_until_empty().await;
        if let Some((id, ladder)) = take_back {
            if self.registry.stopped_cleanly(id) {
                self.journal_take_back(ladder);
            } else {
                tracing::info!(
                    worker = id,
                    "the active worker had failed, or ended for its own reasons: the start count stays"
                );
            }
        }
        // The journal is left as it is, for recovery: its `workers` list stays,
        // and the next startup clears the listed workers without checking each
        // by pid and start time. That is by design (v3 6.1 row h): a stopped or
        // crashed supervisor's workers die with it (the Windows job object) or
        // stop on EOF of the control channel within the stop budget, so none is
        // left to find.
        self.rt.workers.clear();
        self.rt.active = None;
        self.publish();
        Exit::Stopped
    }

    // ---- events outside a switch -----------------------------------------

    /// An event for a worker no switch step is waiting for.
    pub(crate) async fn background_event(&mut self, id: WorkerId, event: Event) {
        self.survivors_changed();
        match event {
            Event::Message(message) => self.note_message(id, &message),
            Event::Lost(why) => self.worker_lost(id, &why),
            Event::Exited(info) => {
                tracing::info!(worker = id, "a worker exited: {}", info.text);
                self.worker_exited(id, &info.text, info.success).await;
            }
        }
    }

    /// Bookkeeping for the messages that mean nothing to a switch step.
    pub(crate) fn note_message(&mut self, id: WorkerId, message: &Message) {
        let Some(tracked) = self.rt.workers.get_mut(&id) else {
            return;
        };
        if tracked.lost {
            // Lost is final: a message after it is not a live worker's.
            return;
        }
        match message {
            Message::Status {
                live,
                signins_pending,
                ..
            } => {
                tracked.live = *live;
                tracked.signins = *signins_pending;
                tracked.status_seq += 1;
            }
            Message::Loaded { .. } if tracked.load_timed_out => {
                // The timeout is final: the start has failed, and a `loaded`
                // that was already on its way does not undo it.
                tracing::info!(
                    worker = id,
                    "a `loaded` after the load timeout is ignored: the start has failed"
                );
            }
            Message::LoadFailed { .. } => {
                tracked.load_failed = true;
                tracing::debug!(worker = id, "ignored {message:?}");
            }
            Message::Loaded { .. } if tracked.awaiting_loaded => {
                // Now it is active: `peers_gone` is valid (L9.15).
                tracked.awaiting_loaded = false;
                tracked.load_deadline = None;
                tracked.wants_peers_gone = true;
                let loaded = tracked.rung.clone().map(|run| (run, tracked.spent.clone()));
                // While a switch is open the reset waits for its end: a write
                // before J1 would race the switch's own writes (round 8b).
                if !self.rt.switching
                    && let Some((run, spent)) = loaded
                {
                    self.journal_loaded(&run, &spent);
                }
                self.peers_gone_check();
            }
            Message::Drained => {
                // The worker exits next; its `Exited` ends the drain.
                tracing::info!(worker = id, "a released worker has drained");
            }
            other => tracing::debug!(worker = id, "ignored {other:?}"),
        }
    }

    /// A channel closed while the process may live (6.2): mark it lost and
    /// stop it by its handle. Nothing it held is taken until it has exited.
    pub(crate) fn worker_lost(&mut self, id: WorkerId, why: &str) {
        tracing::warn!(worker = id, "a worker's control channel was lost: {why}");
        let stop = self.cfg.timings.stop;
        if let Some(tracked) = self.rt.workers.get_mut(&id)
            && !tracked.lost
        {
            tracked.lost = true;
            tracked.kill_at = Some(Instant::now() + stop);
            self.registry.close(id);
            // The mark is in the journal's `workers`, so a restart knows this
            // process may still hold the lease (A-L9).
            let _ = self.write_journal("J-workers");
        }
    }

    /// Reads the active worker's progress before it is removed, during a
    /// switch, so the routes that restart it carry it on (round 8, item 4).
    pub(crate) fn save_a_progress(&mut self, id: WorkerId) {
        if self.rt.switching
            && self.rt.active == Some(id)
            && let Some(tracked) = self.rt.workers.get(&id)
        {
            self.a_progress = Some(tracked.progress_after_exit());
        }
    }

    /// A worker's process is gone. The registry has dropped its port.
    pub(crate) async fn worker_exited(&mut self, id: WorkerId, text: &str, success: bool) {
        self.save_a_progress(id);
        let Some(tracked) = self.rt.workers.remove(&id) else {
            return;
        };
        match tracked.role {
            WorkerRole::Draining => {
                self.drain_ended(&tracked);
                self.peers_gone_check();
            }
            WorkerRole::Standby => {}
            WorkerRole::Active => {
                if self.rt.active == Some(id) {
                    self.rt.active = None;
                }
                if self.rt.watch.as_ref().is_some_and(|watch| watch.b == id) {
                    let why = if tracked.lost {
                        format!("the new version's control channel was lost and it exited ({text})")
                    } else {
                        format!("the new version exited inside the watch window ({text})")
                    };
                    self.window_failure(&why).await;
                } else if self.rt.stop_requested {
                    // A service stop is under way: nothing restarts.
                    tracing::info!(worker = id, "the active worker exited ({text}) for a stop");
                } else {
                    tracing::error!(
                        worker = id,
                        "the active worker exited ({text}, success: {success}); restarting"
                    );
                    let alive = Instant::now().saturating_duration_since(tracked.since);
                    let wait = self.rt.backoff.after(alive);
                    // 6.1 g: a worker that exits before `loaded` did not
                    // start: its rung is charged a failed try. One that
                    // loaded starts again from its own rung with none.
                    self.rt.restart = Some(Restart {
                        due: Instant::now() + wait,
                        progress: tracked.progress_after_exit(),
                        recorded: false,
                    });
                    self.rt.degraded = Some(format!("the active worker exited ({text})"));
                }
            }
        }
        self.sync_workers();
    }

    /// Sends `peers_gone` to the active worker whenever no other worker of the
    /// data dir is alive: none draining, standby or lost (L9.15). It goes to a
    /// worker started at startup or by a restart too.
    pub(crate) fn peers_gone_check(&mut self) {
        let Some(id) = self.rt.active else {
            return;
        };
        if self.rt.workers.keys().any(|other| *other != id) {
            return;
        }
        // A child with no port yet (a handshake that failed or was cancelled)
        // may still run: A is not alone until its exit is seen.
        if self.spawning.is_some() || !self.registry.survivors().is_empty() {
            return;
        }
        if let Some(tracked) = self.rt.workers.get_mut(&id)
            && tracked.wants_peers_gone
            && !tracked.peers_gone_sent
            && !tracked.lost
        {
            tracked.peers_gone_sent = true;
            self.registry.send(id, Message::PeersGone);
        }
    }

    /// A released worker is gone: patch its switch's record with how long the
    /// drain took and how many connections were cut.
    fn drain_ended(&mut self, tracked: &Tracked) {
        let Some(drain) = &tracked.drain else {
            return;
        };
        let Some(record) = &drain.record else {
            return;
        };
        let mut patch = SwitchRecord::started(
            &record.id,
            record.how.as_str(),
            &record.from.version,
            &record.to.version,
            &record.started,
        );
        // A patch, not a restart of the record: `started` only fills what the
        // state already has, and the result below is the one S1 wrote.
        patch.drained_ms = Some(
            u64::try_from(
                Instant::now()
                    .saturating_duration_since(drain.since)
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
        );
        patch.cut_connections = Some(drain.cut);
        // The record may already be confirmed or failed; keep its result.
        patch.result = None;
        let outcome = Outcome {
            record: patch,
            mark_failed: None,
        };
        self.write_state("S2", &outcome, false);
    }

    // ---- timers -----------------------------------------------------------

    pub(crate) async fn on_tick(&mut self) {
        self.prune_automatic_cooldowns();
        self.tick_background().await;
        self.tick_watch().await;
        self.tick_restart().await;
        // Never inside a switch (a tick here is not one of its waits).
        if !self.rt.switching && self.rt.prune_due.is_some_and(|due| due <= Instant::now()) {
            self.try_prune().await;
        }
    }

    /// The timers that run whatever else is happening, including inside a
    /// switch's waits.
    pub(crate) async fn tick_background(&mut self) {
        self.survivors_changed();
        let now = Instant::now();
        // Lost workers that did not exit: terminate by the handle.
        let kill: Vec<WorkerId> = self
            .rt
            .workers
            .iter()
            .filter(|(_, tracked)| tracked.kill_at.is_some_and(|at| at <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in kill {
            tracing::error!(
                worker = id,
                "a lost worker did not exit in time: terminating it"
            );
            if let Some(tracked) = self.rt.workers.get_mut(&id) {
                tracked.kill_at = None;
            }
            self.registry.kill(id);
        }
        // An active start that greeted and never sent `loaded`: ended at its
        // deadline, by its handle. Its exit is a failed start, and the pre-counted
        // try stays; the ladder goes on as for any failed start.
        let expired: Vec<WorkerId> = self
            .rt
            .workers
            .iter()
            .filter(|(_, tracked)| {
                tracked.role == WorkerRole::Active
                    && tracked.awaiting_loaded
                    && !tracked.lost
                    && !tracked.load_timed_out
                    && tracked.load_deadline.is_some_and(|at| at <= now)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            tracing::warn!(
                worker = id,
                "the active worker did not report loaded within the 3-minute load timeout: terminating it"
            );
            if let Some(tracked) = self.rt.workers.get_mut(&id) {
                tracked.load_timed_out = true;
            }
            self.registry.kill(id);
        }
        if self.rt.switching {
            // While a switch is open the mode is re-read every tick (5.3).
            self.eval_mode();
        }
        self.tick_drains(now);
        self.tick_pending(now);
        if self.rt.journal_retry.is_some_and(|due| due <= now) {
            self.retry_journal();
        }
        // No install starts while a switch's foreground handler runs; the due
        // retry is kept, and runs once the handler has ended.
        if !self.rt.switching && self.rt.install_due.is_some_and(|due| due <= now) {
            self.rt.install_due = None;
            self.try_install().await;
        }
    }

    fn tick_drains(&mut self, now: Instant) {
        let budget = self.cfg.timings.stop_message_budget();
        let budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);
        let ids: Vec<WorkerId> = self
            .rt
            .workers
            .iter()
            .filter(|(_, tracked)| tracked.drain.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(tracked) = self.rt.workers.get_mut(&id) else {
                continue;
            };
            let live = tracked.live;
            let seq = tracked.status_seq;
            let Some(drain) = tracked.drain.as_mut() else {
                continue;
            };
            if drain.stopped || now < drain.deadline {
                continue;
            }
            if !drain.pinged {
                // Ask for the live count first, so the cut is counted.
                drain.pinged = true;
                drain.stop_at = Some(now + Duration::from_secs(1));
                let _ = seq;
                self.registry.send(id, Message::Ping);
                continue;
            }
            if drain.stop_at.is_some_and(|at| at <= now) {
                drain.stopped = true;
                drain.cut = live;
                tracked.kill_at = Some(now + self.cfg.timings.stop);
                tracing::warn!(
                    worker = id,
                    "the drain reached its cap: stopping the worker, cutting {live} connections"
                );
                self.registry.send(id, Message::Stop { budget_ms });
            }
        }
    }

    fn tick_pending(&mut self, now: Instant) {
        if self.rt.switched_off {
            self.rt.pending.clear();
            return;
        }
        let due: Vec<usize> = self
            .rt
            .pending
            .iter()
            .enumerate()
            .filter(|(_, held)| held.due <= now)
            .map(|(index, _)| index)
            .collect();
        for index in due.into_iter().rev() {
            let held = self.rt.pending.remove(index);
            match self.disk.write_state(held.name, &held.outcome) {
                Ok(()) => {
                    if held.in_journal {
                        self.outcome_delivered(&held.outcome.record.id);
                    }
                }
                Err(error) => {
                    // A partial outcome whose original record is gone fails the
                    // same way until it returns: the first failure was logged,
                    // and startup warned about the orphan once. The outcome and
                    // its retries stay.
                    if !crate::statefile::orphaned_partial(&self.cfg.data, &held.outcome) {
                        tracing::error!("state write {} failed again: {error}", held.name);
                    }
                    let attempts = held.attempts + 1;
                    self.queue_state(held.name, held.outcome, held.in_journal, attempts);
                }
            }
        }
    }

    fn retry_journal(&mut self) {
        // Tried again every STATUS_EVERY while the disk is stale (3.6).
        if self.write_journal("J-retry").is_ok() {
            tracing::info!("the journal was written again");
        }
    }

    // ---- the watch window and its confirmation ----------------------------

    async fn tick_watch(&mut self) {
        let Some(watch) = &self.rt.watch else {
            return;
        };
        let now = Instant::now();
        if now < watch.deadline {
            return;
        }
        if watch.confirm_due.is_some_and(|due| due > now) {
            return;
        }
        // A new version that is lost has not ended its window cleanly.
        if self
            .rt
            .workers
            .get(&watch.b)
            .is_some_and(|tracked| tracked.lost)
        {
            return;
        }
        self.confirm().await;
    }

    /// The window ended cleanly (5.1 step 11): J5, then S2, then the install.
    async fn confirm(&mut self) {
        let Some(watch) = self.rt.watch.clone() else {
            return;
        };
        let open = watch.open.clone();
        let installed_sha = self.disk.installed_sha256();
        // The file already holding the target is no proof that its directory
        // entry is durable: while an install is pending (an earlier attempt
        // may have landed the file and failed its sync), the confirmation
        // keeps a pending for the target, and the install task's
        // `AlreadyReplaced` path syncs before it acknowledges.
        let unsynced = self
            .rt
            .journal
            .as_ref()
            .is_some_and(|journal| journal.install_pending.is_some());
        let already = !unsynced
            && installed_sha
                .as_deref()
                .is_some_and(|sha| sha.eq_ignore_ascii_case(&open.to.sha256));
        let pending = if already {
            None
        } else {
            Some(InstallPending {
                target: open.to.clone(),
                source: self
                    .cfg
                    .data
                    .binary(&open.to.version, &self.cfg.binary_name)
                    .to_string_lossy()
                    .into_owned(),
                aside: self.disk.choose_aside().unwrap_or_default(),
                attempts: 0,
                next_try: None,
                started: self.stamp(),
                record_id: Some(open.id.clone()),
            })
        };
        let mut record = SwitchRecord::started(
            &open.id,
            open.how.as_str(),
            &open.from.version,
            &open.to.version,
            &open.started,
        );
        record.confirm();
        if already {
            record.installed_binary = Some(installed_binary::REPLACED.to_owned());
        }
        let outcome = Outcome {
            record,
            mark_failed: None,
        };
        let Some(journal) = self.rt.journal.as_mut() else {
            return;
        };
        let before = journal.clone();
        if !journal.confirm(outcome.clone(), pending) {
            // Nothing to confirm: the switch is not open any more.
            self.rt.watch = None;
            return;
        }
        if already {
            journal.installed = Some(Installed::from(&open.to));
        }
        if let Err(why) = self.write_journal_or_undo(&before, "J5") {
            // B keeps running and is not installed; retried with backoff. A
            // restart before it lands reverts to `from` (safe, documented).
            tracing::error!("the window ended but J5 could not be written: {why}");
            self.rt.journal_stale = true;
            self.rt.journal_retry = None;
            let attempts = self
                .rt
                .watch
                .as_ref()
                .map_or(0, |watch| watch.confirm_attempts);
            let wait = self.retry_wait(attempts);
            if let Some(watch) = self.rt.watch.as_mut() {
                watch.confirm_attempts += 1;
                watch.confirm_due = Some(Instant::now() + wait);
            }
            return;
        }
        self.rt.watch = None;
        self.write_state("S2", &outcome, true);
        // The installed file is touched only after J5 landed.
        self.try_install().await;
        // The confirm moved what the journal protects: prune after S2 (3.5).
        self.try_prune().await;
        self.peers_gone_check();
    }

    // ---- the install step (3.9) ------------------------------------------

    /// Replaces the installed file, if an install is pending and its journal
    /// write landed. A failure is retried on a timer and never undoes the
    /// switch.
    pub(crate) async fn try_install(&mut self) {
        if self.rt.switching {
            // A switch's handler is running (admission to its end, clean-up
            // and resume included): an install held at its seam would hold
            // up A's resume. It is due as soon as the handler has ended.
            if self.rt.install_due.is_none() {
                self.rt.install_due = Some(Instant::now());
            }
            return;
        }
        if self.rt.disabled.is_some() {
            // Switching is off: the environment was found unsafe to install
            // from. The pending install stays in the journal, and no retry is
            // scheduled or logged.
            return;
        }
        if self.rt.journal_stale {
            // The installed file is never touched unless the write before it
            // succeeded (answer 6). Try again when the retry may have landed.
            self.rt.install_due = Some(Instant::now() + self.cfg.timings.status_every);
            return;
        }
        let Some(journal) = self.rt.journal.as_ref() else {
            return;
        };
        let Some(pending) = journal.install_pending.clone() else {
            return;
        };
        // A pending install is for what is, or is about to be, confirmed: a
        // return's target while one is open, else `confirmed`. Anything else
        // would put a version in the installed file that is not the one the
        // service runs on.
        let expected = journal
            .switch
            .as_ref()
            .and_then(|switch| switch.return_.as_ref())
            .map_or(&journal.confirmed, |ret| &ret.target)
            .clone();
        let is_expected = pending.target.version == expected.version
            && pending.target.sha256.eq_ignore_ascii_case(&expected.sha256);
        if !is_expected {
            if let Some(outcome) = self.supersede_install_unless(&expected) {
                let _ = self.write_journal("J6");
                self.write_state("S2", &outcome, true);
            } else {
                let _ = self.write_journal("J6");
            }
            return;
        }
        if self.rt.returning.is_some() {
            // The install of the return's own target waits for J-R2: until a
            // worker of it runs, `confirmed` is not that version yet. Its
            // retry is kept.
            self.rt.install_due = Some(Instant::now() + self.cfg.timings.status_every);
            return;
        }
        let (journal, result) = self.disk.install_transaction(journal.clone()).await;
        self.rt.journal = Some(journal);
        let (text, done): (String, bool) = match result {
            crate::install::InstallResult::Replaced => {
                (installed_binary::REPLACED.to_owned(), true)
            }
            crate::install::InstallResult::ChangedElsewhere => {
                (installed_binary::CHANGED_ELSEWHERE.to_owned(), true)
            }
            crate::install::InstallResult::Failed(why) => {
                tracing::error!("the install of {} failed: {why}", pending.target.version);
                (installed_binary::failed(&why), false)
            }
        };
        let replaced = text == installed_binary::REPLACED;
        let Some(journal) = self.rt.journal.as_mut() else {
            return;
        };
        if done {
            if replaced {
                journal.installed = Some(Installed::from(&pending.target));
            }
            journal.install_pending = None;
        } else if let Some(held) = journal.install_pending.as_mut() {
            held.attempts += 1;
            let attempts = held.attempts;
            let wait = self
                .cfg
                .timings
                .install_retry
                .saturating_mul(
                    1u32.checked_shl(attempts.saturating_sub(1))
                        .unwrap_or(u32::MAX),
                )
                .min(self.cfg.timings.install_retry_max);
            held.next_try = Some(
                (self.clock.now() + chrono::Duration::from_std(wait).unwrap_or_default())
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            );
            self.rt.install_due = Some(Instant::now() + wait);
        }
        let outcome = pending.record_id.as_ref().map(|id| {
            let mut record = SwitchRecord {
                id: id.clone(),
                ..SwitchRecord::default()
            };
            record.installed_binary = Some(text.clone());
            Outcome {
                record,
                mark_failed: None,
            }
        });
        if let Some(outcome) = &outcome
            && let Some(journal) = self.rt.journal.as_mut()
        {
            journal.push_outcome(outcome.clone());
        }
        // J6. Harmless if it fails: the next recovery sees the installed file
        // equal to the target and finishes the record (3.9).
        if self.write_journal("J6").is_err() {
            self.rt.journal_stale = false;
            self.rt.journal_retry = None;
        }
        if let Some(outcome) = outcome {
            self.write_state("S2", &outcome, true);
        }
    }

    // ---- row f: the new version failed inside its window ------------------

    /// B is dead (and its lease free): return to `from` (6.1 f, j).
    pub(crate) async fn window_failure(&mut self, why: &str) {
        let Some(watch) = self.rt.watch.take() else {
            return;
        };
        tracing::warn!("{why}; returning to {}", watch.open.from.version);
        let mut open = watch.open;
        open.reached_ready = true;
        open.phase = Phase::Watching;
        self.begin_return(
            open,
            Blame::NewVersion,
            "the new version failed during its watch".to_owned(),
            None,
        )
        .await;
    }

    /// J-R1 (the no-oscillation decision), then the fresh `from`.
    ///
    /// `progress` is where the failed `from` stood, when it is the one that
    /// failed (A's own ladder); `None` is a new ladder, nothing spent.
    pub(crate) async fn begin_return(
        &mut self,
        open: Open,
        blame: Blame,
        why: String,
        progress: Option<Progress>,
    ) {
        let ret = Return {
            target: open.from.clone(),
            blame,
            why,
            decided: self.stamp(),
            installed: false,
        };
        self.decide_return(&ret);
        self.rt.returning = Some(Returning {
            initial_target: open.from.clone(),
            open,
            ret,
            unstarted_recorded: false,
        });
        self.rt.view = None;
        // A new ladder, unless A's own progress is carried on.
        self.rt.restart = progress.map(|progress| Restart {
            due: Instant::now(),
            progress,
            recorded: false,
        });
        self.start_returning().await;
    }

    /// Journals the decision before acting on it: up to three tries (J-R1);
    /// if it still fails the supervisor acts on it anyway (a worker that
    /// failed must be replaced, and a stale journal reverts to `from` at a
    /// restart, which is what the decision says).
    ///
    /// A pending install is for the version that will be confirmed, and a
    /// return confirms its target (J-R2): one for any other version is
    /// superseded in this same write, so the installed file can never be made
    /// to hold a version that is not the confirmed one.
    ///
    /// `true` when a write of it landed (a write that landed but could not be
    /// synced counts, as everywhere); `false` when all three failed.
    pub(crate) fn decide_return(&mut self, ret: &Return) -> bool {
        let (written, superseded) = self.journal_return_decision(ret);
        if let Some(outcome) = superseded {
            self.write_state("S2", &outcome, true);
        }
        written
    }

    /// `decide_return`'s journal half: the decision in memory and up to three
    /// J-R1 writes, with the outcome of a superseded install returned for the
    /// caller to deliver. Nothing is delivered here, so a caller that must put
    /// the decision back can do so with nothing having reached the state.
    fn journal_return_decision(&mut self, ret: &Return) -> (bool, Option<Outcome>) {
        let superseded = self.supersede_install_unless(&ret.target);
        if let Some(switch) = self
            .rt
            .journal
            .as_mut()
            .and_then(|journal| journal.switch.as_mut())
        {
            switch.return_ = Some(ret.clone());
        }
        let mut written = false;
        for attempt in 0..3 {
            if self.write_journal("J-R1").is_ok() {
                written = true;
                break;
            }
            tracing::error!("J-R1 failed (try {})", attempt + 1);
            if attempt == 2 {
                tracing::error!("J-R1 could not be written: acting on the decision anyway");
            }
        }
        (written, superseded)
    }

    /// Drops the pending install when its target is not `target` (by version
    /// and hash, not size), carrying `superseded` for its record in the
    /// journal. The caller's write persists it. The outcome to give the state
    /// file is returned.
    fn supersede_install_unless(&mut self, target: &Artifact) -> Option<Outcome> {
        let journal = self.rt.journal.as_mut()?;
        let pending = journal.install_pending.as_ref()?;
        if pending.target.version == target.version
            && pending.target.sha256.eq_ignore_ascii_case(&target.sha256)
        {
            return None;
        }
        let pending = journal.install_pending.take()?;
        tracing::error!(
            "the pending install of {} is not for {}, the version that will be confirmed: superseded",
            pending.target.version,
            target.version
        );
        self.rt.install_due = None;
        let id = pending.record_id?;
        let mut record = SwitchRecord {
            id,
            ..SwitchRecord::default()
        };
        record.installed_binary = Some(installed_binary::SUPERSEDED.to_owned());
        let outcome = Outcome {
            record,
            mark_failed: None,
        };
        journal.push_outcome(outcome.clone());
        Some(outcome)
    }

    /// Starts the fresh worker of the return; on success J-R2 and the outcome.
    ///
    /// The return walks a ladder of rungs (the kept copy of `from`, `previous`,
    /// the installed file by hash), three tries each, and each choice is
    /// journalled in `return.target` before it is tried.
    pub(crate) async fn start_returning(&mut self) {
        let Some(returning) = self.rt.returning.clone() else {
            return;
        };
        let progress = self
            .rt
            .restart
            .as_ref()
            .map(|restart| restart.progress.clone())
            .unwrap_or_default();
        let recorded = self
            .rt
            .restart
            .as_ref()
            .is_some_and(|restart| restart.recorded);
        let full = self.return_candidates(&returning);
        let progress = self.checked_progress(progress, recorded, &full);
        let Some((run, attempts)) = progress.pick(&full, |run| !self.is_untrusted(&run.binary))
        else {
            self.record_unstarted_return();
            self.rt.degraded =
                Some("no version could be started after the failed switch".to_owned());
            // A spent ladder starts a new cycle on the restart backoff, from
            // its first permitted rung. Exhausting the permitted ladder does
            // not reopen the rejected active version. The cycle boundary is
            // journalled, so a recovery moves back across it and no other.
            // The record ends with the cycle, in the same write.
            if let Some(first) = full.iter().find(|run| !self.is_untrusted(&run.binary)) {
                self.retarget_return(&returning, first, None);
            } else {
                self.journal_ladder(None);
            }
            self.schedule_restart(Progress::default());
            return;
        };
        let tried = Progress {
            current: Some((run.clone(), attempts)),
            spent: progress.spent.clone(),
        };
        // The start is counted before it is made, in the write that journals
        // the rung as the return's target (a clean service stop while it is
        // still loading takes the try back, `stop`).
        let ladder = record_start(self.cfg, Some(&returning.open.id), &tried, &run);
        if !self.retarget_return(&returning, &run, Some(ladder)) {
            // The count could not be written: nothing is spawned on its
            // strength, and no outcome or J-R2 is written. Switching is off
            // now, or when the open switch's handler ends, and the return is
            // abandoned in memory with it (`apply_ladder_off`): the disk keeps
            // the open return, and the run that switching off chooses starts.
            return;
        }
        let target = run.artifact.clone();
        match self
            .spawn_run(&run, Some(returning.open.id.clone()), &tried)
            .await
        {
            Ok(id) => {
                self.rt.restart = None;
                self.rt.degraded = None;
                self.rt.run = Some(run);
                let Some(returning) = self.rt.returning.take() else {
                    return;
                };
                self.finish_return(&returning, id);
            }
            // A service stop: nothing more starts.
            Err(RunError::Stopped) => {}
            // Not an attempt: the next pick passes the refused rung, and the
            // one that takes its place starts with its own three attempts.
            Err(RunError::Untrusted(why)) => {
                tracing::error!("{} is untrusted: {why}", target.version);
                self.record_unstarted_return();
                self.rt.degraded = Some(format!("{} is untrusted: {why}", target.version));
                self.schedule_restart(tried);
            }
            Err(RunError::Failed(why)) => {
                tracing::error!("the fresh {} did not start: {why}", target.version);
                self.record_unstarted_return();
                self.rt.degraded = Some(format!("{} did not start: {why}", target.version));
                let mut next = tried;
                next.failed(&run, attempts);
                self.schedule_restart(next);
            }
        }
    }

    /// Journals `run` as the return's target (J-R1) when it is not what the
    /// journal already says: each choice before it is tried (6.1 g).
    ///
    /// `ladder` is the ladder record to journal with it: in the same write
    /// when the target changes, in a write of its own when it doesn't.
    ///
    /// `false` when the ladder record could not be written (switching is off
    /// from then on): the caller must not spawn from the ladder.
    pub(crate) fn retarget_return(
        &mut self,
        returning: &Returning,
        run: &Run,
        ladder: Option<LadderRecord>,
    ) -> bool {
        let current = self
            .rt
            .returning
            .as_ref()
            .map_or(&returning.ret, |held| &held.ret);
        let installed = run.binary == self.cfg.installed;
        if current.target == run.artifact && current.installed == installed {
            return self.journal_ladder(ladder);
        }
        let persisted = self.ladder_persisted();
        let mut changes = false;
        // The journal and the pending install as they are just before the
        // retarget: a failed write of a ladder change puts all of it back.
        let mut before = None;
        if persisted && let Some(journal) = self.rt.journal.as_mut() {
            changes = journal.ladder != ladder;
            before = Some((journal.clone(), self.rt.install_due));
            journal.ladder = ladder;
        }
        let mut ret = current.clone();
        ret.target = run.artifact.clone();
        ret.installed = installed;
        let (written, superseded) = self.journal_return_decision(&ret);
        if !written
            && changes
            && let Some((journal, install_due)) = before
        {
            // The count was not made: nothing of the retarget stays. The
            // ladder record, `switch.return_`, the pending install and the held
            // outcomes are what the disk has, no superseded install's outcome
            // is delivered (no S2, no J8), and the return stands as it was.
            self.rt.journal = Some(journal);
            self.rt.install_due = install_due;
            self.unmake_ladder_state();
            self.ladder_write_failed();
            return false;
        }
        if let Some(outcome) = superseded {
            self.write_state("S2", &outcome, true);
        }
        if let Some(held) = self.rt.returning.as_mut() {
            held.ret = ret;
        }
        true
    }

    /// The rungs a return may run (`ladder::return_list`), with the installed
    /// file by hash when it is a version the supervisor knows.
    fn return_candidates(&self, returning: &Returning) -> Vec<Run> {
        return_list(
            self.cfg,
            &returning.initial_target,
            self.rt
                .journal
                .as_ref()
                .and_then(|journal| journal.previous.as_ref()),
            &returning.open.to,
            &returning.ret,
            self.installed_run(&returning.open.to.sha256),
        )
    }

    /// The installed file as a rung, when it is a version the supervisor knows
    /// and is not `skip` (a sha256).
    fn installed_run(&self, skip: &str) -> Option<Run> {
        let sha256 = self.disk.installed_sha256()?;
        if sha256.eq_ignore_ascii_case(skip) {
            return None;
        }
        let installed = self
            .rt
            .journal
            .as_ref()
            .and_then(|journal| journal.installed.as_ref());
        let version = installed
            .filter(|installed| installed.sha256.eq_ignore_ascii_case(&sha256))
            .map(|installed| installed.version.clone())
            .or_else(|| {
                self.host
                    .own_sha256()
                    .filter(|own| own.eq_ignore_ascii_case(&sha256))
                    .map(|_| self.host.own_version())
            })?;
        // A file whose size can't be read can't be run either: no rung.
        let size = self.disk.installed_size()?;
        Some(Run {
            binary: self.cfg.installed.clone(),
            artifact: Artifact {
                version,
                sha256,
                size,
            },
        })
    }

    fn is_untrusted(&self, binary: &std::path::Path) -> bool {
        self.rt.untrusted.iter().any(|refused| refused == binary)
    }

    /// The driver refused a binary as untrusted (2.5): the environment's
    /// fault. Switching is off, no ladder starts that version again, and no
    /// version is marked failed.
    pub(crate) fn refuse_untrusted(&mut self, binary: &std::path::Path, version: &str, why: &str) {
        tracing::warn!("{version} is not a trusted binary: {why}");
        if !self.is_untrusted(binary) {
            self.rt.untrusted.push(binary.to_path_buf());
        }
        self.rt
            .disabled
            .get_or_insert_with(|| "switching is off: a worker binary is not trusted".to_owned());
        self.publish();
    }

    /// The return could not start a worker (6.1 g): the forward switch's own
    /// record gets its row d or f result now, as the table says the outcome is
    /// already recorded when the supervisor goes `degraded`. It is decided at
    /// J-R1 (blame and why are in the journalled `return`), so it needs no
    /// worker; `returned_to` stays absent until one runs (J-R2 then adds it to
    /// the same record by id). The outcome rides the journal like any other
    /// (J7), so a crash replays it, once, by id; the decision stays journalled
    /// and the return goes on retrying on the restart backoff.
    fn record_unstarted_return(&mut self) {
        let Some(returning) = self.rt.returning.clone() else {
            return;
        };
        if returning.unstarted_recorded {
            return;
        }
        if let Some(held) = self.rt.returning.as_mut() {
            held.unstarted_recorded = true;
        }
        let mut outcome = self.return_outcome(&returning);
        outcome.record.returned_to = None;
        if let Some(journal) = self.rt.journal.as_mut() {
            journal.push_outcome(outcome.clone());
        }
        let in_journal = self.rt.journal.is_some();
        if in_journal {
            let _ = self.write_journal("J7");
        }
        self.write_state("S3", &outcome, in_journal);
    }

    /// J-R2 and the forward record's outcome (S3) once the fresh worker runs.
    fn finish_return(&mut self, returning: &Returning, id: WorkerId) {
        let outcome = self.return_outcome(returning);
        let mut done = false;
        let persisted = self.ladder_persisted();
        let ran = self.rt.run.clone();
        let installed = self.installed_run("");
        let installed_sha256 = self.disk.installed_sha256();
        if let Some(journal) = self.rt.journal.as_mut() {
            let held = journal.ladder.clone();
            done = journal.finish_return(outcome.clone());
            // The return's record becomes the restart ladder's in J-R2's own
            // write: the rung that started, the tries held, not a new cycle.
            if done
                && persisted
                && let Some(ran) = ran.as_ref()
            {
                journal.ladder = restart_record_after_return(
                    self.cfg,
                    journal,
                    &returning.open.id,
                    held.as_ref(),
                    ran,
                    installed,
                    installed_sha256.as_deref(),
                );
            }
        }
        if done {
            // J-R2. A failure rolls forward in memory: the supervisor already
            // runs the fresh worker.
            let _ = self.write_journal("J-R2");
            self.write_state("S3", &outcome, true);
        } else {
            self.write_state("S3", &outcome, false);
        }
        let _ = id;
        self.peers_gone_check();
    }

    /// The forward switch's own record, ended (6.1 d, e, f, j).
    pub(crate) fn return_outcome(&self, returning: &Returning) -> Outcome {
        let open = &returning.open;
        let ret = &returning.ret;
        let mut record = if open.reached_ready {
            SwitchRecord::failed_after_switch(
                &open.id,
                open.how.as_str(),
                &open.from.version,
                &open.to.version,
                &open.started,
                Phase::Watching.as_str(),
                ret.blame.as_str(),
                &ret.why,
                &ret.target.version,
            )
        } else {
            let mut record = SwitchRecord::failed_to_load(
                &open.id,
                open.how.as_str(),
                &open.from.version,
                &open.to.version,
                &open.started,
                open.phase.as_str(),
                ret.blame.as_str(),
                &ret.why,
            );
            record.returned_to = Some(ret.target.version.clone());
            record
        };
        record.ended_at = Some(self.stamp());
        // `mark_failed` only for an update: a rollback's target is an older
        // version the auto-updater never offers.
        let mark_failed =
            (ret.blame == Blame::NewVersion && open.how == How::Update).then(|| MarkFailed {
                version: open.to.version.clone(),
                why: ret.why.clone(),
            });
        Outcome {
            record,
            mark_failed,
        }
    }

    // ---- restarting the active worker (9.2) -------------------------------

    pub(crate) fn schedule_restart(&mut self, progress: Progress) {
        let wait = self.rt.backoff.after(Duration::ZERO);
        self.rt.restart = Some(Restart {
            due: Instant::now() + wait,
            progress,
            recorded: false,
        });
    }

    async fn tick_restart(&mut self) {
        let Some(restart) = self.rt.restart.clone() else {
            return;
        };
        if Instant::now() < restart.due || self.rt.switching {
            return;
        }
        // While a return is open the generic restart doesn't run: the return
        // is the ladder, and it confirms what it starts.
        if self.rt.returning.is_some() {
            self.start_returning().await;
            return;
        }
        if self.rt.active.is_some() {
            self.rt.restart = None;
            return;
        }
        self.restart_active(restart.progress).await;
    }

    // ---- the ladder record (6.1 g) ------------------------------------------

    /// Whether the ladder is journalled: there is a journal, and switching is
    /// not off (an environment found unsafe keeps nothing new on the disk).
    fn ladder_persisted(&self) -> bool {
        self.rt.journal.is_some() && self.rt.disabled.is_none() && self.rt.ladder_off.is_none()
    }

    /// Journals `ladder` as the record, in a write of its own, when it is not
    /// what the journal holds.
    ///
    /// `false` when the write failed after the store's retries, or when a
    /// failed write is waiting for its switch to end: the record goes back to
    /// what the disk has, and a caller that was about to spawn the rung the
    /// count was for must not (`ladder_write_failed`). A write that landed but
    /// could not be synced (`NotDurable`) counts as written, as it does for J1
    /// (`write_journal`).
    fn journal_ladder(&mut self, ladder: Option<LadderRecord>) -> bool {
        if self.rt.ladder_off.is_some() {
            return false;
        }
        if !self.ladder_persisted() {
            return true;
        }
        let Some(journal) = self.rt.journal.as_mut() else {
            return true;
        };
        if journal.ladder == ladder {
            return true;
        }
        let before = std::mem::replace(&mut journal.ladder, ladder);
        if self.write_journal("J-L").is_ok() {
            return true;
        }
        self.unmake_ladder(before);
        self.ladder_write_failed();
        false
    }

    /// Puts the provisional record back to what the disk has, in memory: a
    /// record that was never written must not be written later by a retry.
    fn unmake_ladder(&mut self, before: Option<LadderRecord>) {
        if let Some(journal) = self.rt.journal.as_mut() {
            journal.ladder = before;
        }
        self.unmake_ladder_state();
    }

    /// The staleness bookkeeping of an undone write (`unmake_ladder`).
    fn unmake_ladder_state(&mut self) {
        // The write that failed left the journal stale; what is held is the
        // disk's again, unless an earlier write of it was not synced.
        self.rt.journal_stale = self.not_durable;
        self.rt.journal_retry = self
            .not_durable
            .then(|| Instant::now() + self.cfg.timings.status_every);
    }

    /// A ladder record could not be journalled: what the supervisor would
    /// resume from is no longer what it holds, so switching goes off, with the
    /// reason in the status. With no switch open that is done at once; with
    /// one open it is done when the switch's handler ends, after its outcome is
    /// decided and before anything is spawned (`settle_ladder_after_switch`).
    fn ladder_write_failed(&mut self) {
        tracing::error!("the ladder record could not be written: switching is off");
        self.rt.ladder_off.get_or_insert_with(|| {
            "switching is off: the ladder record could not be written".to_owned()
        });
        if !self.rt.switching {
            self.apply_ladder_off();
        }
        self.publish();
    }

    /// The one transition of a failed ladder write: the state a Core built
    /// from start-up's `switch_off` is in. The reason is in `disabled`; no
    /// journal is kept in memory (so no provisional count, no retry, no
    /// stale mark and no pending state write); every version is kept; `run` is
    /// what start-up's `switch_off` chooses (`installed_as_own_by`), or
    /// nothing with the reason it is not (degraded). A running worker is not
    /// stopped. An open return is abandoned in memory only: no J-R2 and no
    /// outcome, so the disk keeps what was last written and the next start's
    /// recovery finishes it, as after a crash. Every later restart runs only
    /// `run`, as `restart_active`'s no-journal branch does.
    pub(crate) fn apply_ladder_off(&mut self) {
        let Some(reason) = self.rt.ladder_off.take() else {
            return;
        };
        self.rt.disabled.get_or_insert(reason);
        self.rt.switched_off = true;
        self.rt.journal = None;
        self.rt.journal_stale = false;
        self.rt.journal_retry = None;
        self.rt.pending.clear();
        self.not_durable = false;
        self.rt.keep_all = true;
        self.rt.returning = None;
        // The same measurement start-up's `choose_installed` makes.
        let (installed_sha256, installed_size) = self.disk.installed_switched_off();
        match crate::startup::installed_as_own_by(
            self.cfg,
            installed_sha256.as_deref(),
            installed_size,
            self.host.own_sha256().as_deref(),
            &self.host.own_version(),
        ) {
            Ok(run) => {
                self.rt.run = Some(run);
                self.rt.degraded = None;
            }
            Err(why) => {
                self.rt.run = None;
                self.rt.degraded = Some(why);
            }
        }
        // With no worker, the run is started by the restart: now, and not on a
        // ladder.
        // A restart pending from before (the handler's untrusted branch) loses
        // its progress too: tries or a spent installed rung are the old
        // ladder's, and a Core built from start-up's switch-off has none.
        if let Some(restart) = self.rt.restart.as_mut() {
            restart.progress = Progress::default();
            restart.recorded = false;
            if self.rt.active.is_none() {
                restart.due = Instant::now();
            }
        } else if self.rt.active.is_none() && self.rt.run.is_some() {
            self.rt.restart = Some(Restart {
                due: Instant::now(),
                progress: Progress::default(),
                recorded: false,
            });
        }
        self.publish();
    }

    /// What a switch's handler leaves when it ends, with the switch closed:
    /// the reset A's `loaded` could not write while the switch was open (A is
    /// still the loaded active worker and the switch ended before J1 cleared
    /// the record), and then the switching-off a failed ladder write waited for.
    /// A failure of the reset is the no-switch case.
    pub(crate) fn settle_ladder_after_switch(&mut self, writes: bool) {
        if writes
            && let Some(id) = self.rt.active
            && let Some(tracked) = self.rt.workers.get(&id)
            && tracked.role == WorkerRole::Active
            && !tracked.awaiting_loaded
            && !tracked.lost
            && let Some(run) = tracked.rung.clone()
        {
            let spent = tracked.spent.clone();
            self.journal_loaded(&run, &spent);
        }
        #[cfg(test)]
        {
            self.rt.settle_snapshot = self.rt.journal.clone();
        }
        self.apply_ladder_off();
    }

    /// A start the driver refused as untrusted was not an attempt: the count
    /// made before it is taken back.
    fn journal_uncount_start(&mut self) {
        if !self.ladder_persisted() {
            return;
        }
        let Some(mut ladder) = self
            .rt
            .journal
            .as_ref()
            .and_then(|journal| journal.ladder.clone())
        else {
            return;
        };
        if ladder.attempts == 0 {
            return;
        }
        ladder.attempts -= 1;
        self.journal_ladder(Some(ladder));
    }

    /// The candidate of a clean service stop: the active worker's id and the
    /// ladder record to write for it, the journal's record with one try fewer
    /// (never below 0) and everything else in the record as it is. The
    /// worker is a tracked, still-loading, not-lost one with a rung, its load
    /// deadline not passed, the ladder is journalled with switching on, the
    /// record's `current` is that rung with a non-zero count, and the disk is
    /// not behind the journal (`journal_stale`). With a switch open nothing
    /// is written (the run loop calls `stop` after `handle_switch` has
    /// returned, so that cannot happen in production). Whether the worker's
    /// end allows the refund is decided after the stop (`Registry::stopped_cleanly`).
    /// Only `attempts` changes in the record; the write that carries it is
    /// not a promise that the whole journal equals the disk afterwards.
    fn clean_stop_take_back(&self) -> Option<(WorkerId, LadderRecord)> {
        let id = self.rt.active?;
        let tracked = self.rt.workers.get(&id)?;
        if tracked.role != WorkerRole::Active || !tracked.awaiting_loaded || tracked.lost {
            return None;
        }
        // A `load_failed` the Core has read is a failure the stop did not cause.
        if tracked.load_failed {
            return None;
        }
        // Past its deadline the start has failed, whether or not the timer ran.
        if tracked.load_timed_out || tracked.load_deadline.is_some_and(|at| at <= Instant::now()) {
            return None;
        }
        let run = tracked.rung.as_ref()?;
        if !self.ladder_persisted() {
            return None;
        }
        let ladder = self.rt.journal.as_ref()?.ladder.as_ref()?;
        let current = ladder.current.as_ref()?;
        if ladder.attempts == 0 || !same_rung(&run_of(self.cfg, current), run) {
            return None;
        }
        if self.rt.journal_stale {
            // The disk is behind the journal: a write now would carry more than
            // the take-back, and a failed one could not be undone cleanly.
            return None;
        }
        if self.rt.switching {
            tracing::info!("a stop with a switch open: the start count is left as it is");
            return None;
        }
        let mut ladder = ladder.clone();
        ladder.attempts = ladder.attempts.saturating_sub(1);
        Some((id, ladder))
    }

    /// Writes the record of `clean_stop_take_back`, best-effort: a write that
    /// fails leaves the count (the ladder record goes back to what it was; the
    /// rest of the in-memory journal, such as the worker list, is not rolled
    /// back), logs one warning, and neither switches anything off nor calls
    /// `ladder_write_failed`. The write carries the whole journal, so it can
    /// also persist the worker list and advances `seq`. A write that landed
    /// but could not be synced counts as written (`write_journal`).
    fn journal_take_back(&mut self, ladder: LadderRecord) {
        let Some(journal) = self.rt.journal.as_mut() else {
            return;
        };
        let before = journal.ladder.replace(ladder);
        if self.write_journal("J-L").is_err() {
            tracing::warn!("the start count could not be taken back on a stop: it stays counted");
            self.unmake_ladder(before);
        }
    }

    /// The worker of `run` reported `loaded`: its rung has nothing against it
    /// on the disk either. Nothing is written when no record is held.
    pub(crate) fn journal_loaded(&mut self, run: &Run, spent: &[Run]) {
        if self
            .rt
            .journal
            .as_ref()
            .is_none_or(|journal| journal.ladder.is_none())
        {
            return;
        }
        self.journal_ladder(Some(record_loaded(self.cfg, run, spent)));
    }

    /// A progress read from the journal at start is used only if every rung it
    /// names is on the ladder now (the rung half of the one record check,
    /// `ladder::usable_record`); otherwise it is ignored (and logged), and the
    /// ladder starts from its first rung. A count is never carried to a list
    /// it was not earned on.
    fn checked_progress(&self, progress: Progress, recorded: bool, full: &[Run]) -> Progress {
        if !recorded || all_known(&progress, full) {
            return progress;
        }
        tracing::warn!(
            "the journalled ladder record names a rung that is not on the ladder: ignored"
        );
        Progress::default()
    }

    /// The rungs of the restart ladder (`ladder::restart_list`), with the
    /// installed file by hash when it is a version the supervisor knows.
    fn restart_list(&self, journal: &Journal, progress: &Progress) -> Vec<Run> {
        crate::ladder::restart_list(
            self.cfg,
            journal,
            self.installed_run(""),
            self.disk.installed_sha256().as_deref(),
            progress,
        )
    }

    /// Starts the active worker again: the run it had, `confirmed`,
    /// `previous`, then the installed file by hash, three tries on each rung
    /// (6.1 g). A spent ladder is degraded and starts a new cycle from its
    /// first permitted rung after the restart backoff.
    async fn restart_active(&mut self, progress: Progress) {
        let recorded = self
            .rt
            .restart
            .as_ref()
            .is_some_and(|restart| restart.recorded);
        let progress = match self.rt.journal.as_ref() {
            Some(journal) if recorded => {
                let full = self.restart_list(journal, &progress);
                self.checked_progress(progress, recorded, &full)
            }
            _ => progress,
        };
        let Some(journal) = self.rt.journal.as_ref() else {
            // No journal: the one run there is, from the installed file. The
            // real driver never refuses the installed path as untrusted; a
            // generic driver that did would be retried on the restart backoff
            // (the environment may be repaired), not dropped from a ladder.
            if let Some(run) = self.rt.run.clone() {
                self.restart_rung(&[run], &progress, false).await;
            }
            return;
        };
        let full = self.restart_list(journal, &progress);
        self.restart_rung(&full, &progress, true).await;
    }

    async fn restart_rung(&mut self, full: &[Run], progress: &Progress, refuse_untrusted: bool) {
        let permitted = |run: &Run| !(refuse_untrusted && self.is_untrusted(&run.binary));
        let Some((run, attempts)) = progress.pick(full, permitted) else {
            self.rt.degraded = Some("no version could be started".to_owned());
            // A new cycle on the backoff: never a busy loop, and the refused
            // locations stay out of it. The cycle's record ends with it.
            self.journal_ladder(None);
            self.schedule_restart(Progress::default());
            return;
        };
        let tried = Progress {
            current: Some((run.clone(), attempts)),
            spent: progress.spent.clone(),
        };
        // The start is counted before it is made: a crash while it runs has
        // used one of the rung's tries (a clean service stop while it is still
        // loading takes the try back, `stop`). When the count can't be written
        // switching is off (no switch is open here), and the spawn is not made
        // from the ladder: the run that switching off chose is started, as it
        // is for a supervisor with switching off. With none, nothing is.
        let counted = self.journal_ladder(Some(record_start(self.cfg, None, &tried, &run)));
        let (run, attempts, tried) = if counted {
            (run, attempts, tried)
        } else {
            let Some(fallback) = self.rt.run.clone() else {
                return;
            };
            let tried = Progress::on(fallback.clone(), 0);
            (fallback, 0, tried)
        };
        match self.spawn_run(&run, None, &tried).await {
            Ok(_) => {
                self.rt.restart = None;
                self.rt.degraded = None;
                self.rt.run = Some(run);
            }
            Err(RunError::Stopped) => {}
            // Not an attempt: the ladder goes on without the refused version.
            Err(RunError::Untrusted(why)) => {
                tracing::error!("the active worker's version is untrusted: {why}");
                self.rt.degraded = Some(format!("the active worker is untrusted: {why}"));
                // As in `start_returning`: the next rung gets all its attempts.
                self.schedule_restart(tried);
            }
            Err(RunError::Failed(why)) => {
                tracing::error!("the active worker did not start: {why}");
                self.rt.degraded = Some(format!("the active worker did not start: {why}"));
                let mut next = tried;
                next.failed(&run, attempts);
                self.schedule_restart(next);
            }
        }
    }

    // ---- spawning ---------------------------------------------------------

    pub(crate) fn spawn_request(
        &mut self,
        artifact: &Artifact,
        binary: std::path::PathBuf,
        role: Role,
        switch_id: Option<String>,
    ) -> SpawnRequest {
        let id = self.registry.next_id();
        SpawnRequest {
            id,
            version: artifact.version.clone(),
            sha256: artifact.sha256.clone(),
            binary,
            role,
            switch_id,
            config: self.cfg.config.clone(),
            installation: Installation {
                installed: self.cfg.installed.clone(),
                data_dir: self.cfg.data.root().to_path_buf(),
                receipt: self.cfg.receipt.clone(),
            },
            topology: self.host.topology(),
            listeners: self.rt.listeners.clone(),
            budgets: self.cfg.timings.budgets(),
            handshake_timeout: self.cfg.timings.load,
            log: None,
        }
    }

    async fn spawn_run(
        &mut self,
        run: &Run,
        switch_id: Option<String>,
        progress: &Progress,
    ) -> Result<WorkerId, RunError> {
        if self.rt.listeners.is_empty() {
            match self.host.bind() {
                Ok(listeners) => self.rt.listeners = Arc::new(listeners),
                Err(error) => {
                    return Err(RunError::Failed(format!(
                        "could not bind the listening sockets: {error}"
                    )));
                }
            }
        }
        let (id, port) = loop {
            // A child of an earlier failed spawn may still hold what this one
            // needs: wait for its exit, against Stop only (L9.15).
            if !self.wait_survivors_gone().await {
                return Err(RunError::Stopped);
            }
            let request = self.spawn_request(
                &run.artifact,
                run.binary.clone(),
                Role::Active,
                switch_id.clone(),
            );
            let id = request.id;
            match self.driver.spawn(request, self.registry.events()).await {
                Ok(port) => break (id, port),
                // Not an attempt and no version's fault: wait and retry.
                Err(SpawnError::SurvivorPending) => {
                    if !self.pause_for_survivor().await {
                        return Err(RunError::Stopped);
                    }
                }
                Err(SpawnError::Untrusted(why)) => {
                    self.survivors_changed();
                    // Not an attempt: the count made before this spawn is taken
                    // back first, while switching is still on. Once the refusal
                    // turns switching off the ladder record is no longer kept.
                    self.journal_uncount_start();
                    self.refuse_untrusted(&run.binary, &run.artifact.version, &why);
                    return Err(RunError::Untrusted(why));
                }
                // A child of the failed handshake may live on: list it now.
                Err(error) => {
                    self.survivors_changed();
                    return Err(RunError::Failed(error.to_string()));
                }
            }
        };
        let tracked = Tracked::new(
            run.artifact.clone(),
            WorkerRole::Active,
            &port,
            Instant::now(),
            false,
        );
        let mut tracked = tracked;
        tracked.awaiting_loaded = true;
        tracked.load_deadline = Some(Instant::now() + self.cfg.timings.active_load);
        tracked.rung = Some(run.clone());
        tracked.attempts = progress.attempts_on(run);
        tracked.spent.clone_from(&progress.spent);
        self.registry.add(port);
        self.rt.workers.insert(id, tracked);
        self.rt.active = Some(id);
        self.sync_workers();
        Ok(id)
    }
}
