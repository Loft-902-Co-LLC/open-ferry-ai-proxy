//! Not upstream's: recovery at supervisor start, as a pure function.
//!
//! [`plan_startup`] takes what the journal file held and the facts the
//! supervisor gathered (hashes, its own hash, the clock) and returns a
//! [`RecoveryPlan`]: which artifact to run, the one journal write to make,
//! the state records to upsert, and what to do with the installed file. It
//! reads no file and runs nothing; the supervisor executes the plan.
//!
//! The rules (design v3 3.3, as review 3's T4 and T5 amend them):
//! - Outcomes the state file has not been given are replayed first
//!   (`upserts`); an entry leaves the journal by id once its upsert lands.
//! - `switch.return` set: run `return.target`; J-R2 writes `active =
//!   confirmed = return.target`, clears `switch` and carries the outcome in
//!   one write; `previous` does not change.
//! - `switch` set, no `return`: run `confirmed`, always. When `confirmed` is
//!   `switch.to` (a rollback that ended a watch window) the restart
//!   completed the rollback; otherwise the switch failed, before `ready`
//!   (`failed-to-load`) or inside the window (`failed-after-switch`, a
//!   revert to `from`), blame `environment`, nothing marked failed.
//! - No `switch`: run `confirmed`; adopt an installed file that nobody
//!   told us about.
//! - A `confirmed` whose hash is wrong falls back to `previous`, then to the
//!   installed file but only by hash, then to nothing (`degraded`).
//! - `install_pending` follows 3.9's four hash cases, and its target is
//!   always `confirmed`: a recovery that moves `confirmed` off it clears it
//!   in the same write and reports `superseded` (Opus 11).
//! - An idle journal whose `installed` is older than the installed file that
//!   is the verified artifact running gets a `reconcile-installed` write (O5).
//! - The installed-file fallback needs a copy in `versions/`; when that can't
//!   be staged, [`RecoveryPlan::on_stage_conflict`] gives the plan that runs
//!   the installed file with switching disabled and publishes nothing (O2).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::journal::{
    Artifact, Blame, How, Installed, Journal, JournalLoad, MarkFailed, Outcome, Phase, Return,
    Switch,
};
use crate::state::SwitchRecord;

/// What the supervisor knows at start, gathered with no worker running.
#[derive(Clone, Debug)]
pub struct Facts {
    /// The clock. Tests use a fixed one.
    pub now: DateTime<Utc>,
    /// The sha256 the supervisor computed of its own file at this start.
    pub self_sha256: Option<String>,
    /// The supervisor's own version.
    pub self_version: String,
    /// The sha256 of the installed file; `None` when it is missing.
    pub installed_sha256: Option<String>,
    /// The size of the installed file, for an artifact made from it.
    pub installed_size: u64,
    /// The path of the installed file, for notes that name it.
    pub installed_path: PathBuf,
    /// `versions/<version>/` -> the sha256 of the binary in it. A version
    /// whose binary is missing is absent.
    pub versions: BTreeMap<String, String>,
}

impl Facts {
    /// Whether `artifact`'s binary is in `versions/` with the hash the
    /// journal pinned.
    pub fn verified(&self, artifact: &Artifact) -> bool {
        self.versions.get(&artifact.version) == Some(&artifact.sha256)
    }

    fn time(&self) -> String {
        self.now.to_rfc3339_opts(SecondsFormat::Secs, true)
    }
}

/// Why the installed file is what runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstalledReason {
    /// No journal: a first start (v2 3.4, `stage_exclusive`).
    FirstStart,
    /// The journal is of a newer format: one worker, no journal write.
    NewerJournal,
}

/// What to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunChoice {
    /// `versions/<version>/`, its hash checked against the artifact.
    Version(Artifact),
    /// The installed file, not yet made an artifact.
    Installed(InstalledReason),
    /// The installed file, trusted because its hash is the journal's
    /// `installed.sha256` or the supervisor's own. `versions/<version>/` is
    /// NOT known to hold it: see [`RecoveryPlan::stage_installed`].
    VerifiedInstalled(Artifact),
    /// The installed file, hash-verified, run with switching disabled: the
    /// copy that [`RecoveryPlan::stage_installed`] required could not be made
    /// because `versions/<version>/` is in use or holds another binary (O2).
    /// Only [`RecoveryPlan::on_stage_conflict`] makes this. `artifact` is NOT
    /// in `versions/` and no journal claims it is; `reason` names the
    /// conflicting directory and is also the plan's `SwitchingDisabled`
    /// status. Design v3 2.5/3.3: one worker runs from the installed file.
    InstalledSwitchingDisabled { artifact: Artifact, reason: String },
    /// Nothing can run: `degraded`.
    Nothing,
}

/// The status the plan implies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    Ok,
    /// No worker can run; the supervisor logs at error and retries with
    /// backoff.
    Degraded(String),
    /// `switching_disabled`: the reason.
    SwitchingDisabled(String),
}

/// What to do with the installed file (3.9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallAction {
    None,
    /// The file holds the old hash: replace it with `target` through the
    /// hardened step, using the journalled `aside`. Idempotent.
    Replace {
        target: Artifact,
        aside: String,
    },
    /// The file is missing: place `target` from `versions/` by the same
    /// steps.
    Place {
        target: Artifact,
        aside: String,
    },
    /// The file is missing and `target` is not verified: put back
    /// `installed` from `versions/<installed.version>/`.
    Restore {
        installed: Installed,
    },
}

/// A state record to update by id, to say how the install ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordPatch {
    pub id: String,
    pub installed_binary: String,
}

/// The installed file nobody accounted for: stage a verified copy of it
/// (`stage_exclusive`), then [`Journal::adopt`] it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adopt {
    pub sha256: String,
}

/// What recovery decided.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryPlan {
    pub run: RunChoice,
    pub status: PlanStatus,
    /// The journal file didn't parse: rename it to `.bad.<seq>`
    /// ([`crate::journal::JournalStore::quarantine`]) before anything.
    pub quarantine: bool,
    /// There is no usable journal: build one from the artifact that runs.
    pub first_start: bool,
    /// Keep every version directory out of every prune.
    ///
    /// This flag lives only in the plan, that is in memory: a crash loses
    /// it. The contract is therefore the supervisor's (phase 2a):
    /// - it takes the update lock before it quarantines an unreadable
    ///   journal and holds it through hashing the installed file and the
    ///   `versions/` entries, until the first journal write lands;
    /// - for its whole lifetime, its `keep_set` reply on the control
    ///   endpoint includes every version it kept at that start, for the
    ///   supervisor's whole lifetime, not only until the first journal write
    ///   (an on-disk `.bad.<seq>` journal does not protect anything);
    /// - a CLI prune with a supervisor present uses that reply, and fails
    ///   closed (prunes nothing) when it can't get one.
    ///
    /// It is also set by [`RecoveryPlan::on_stage_conflict`]: the journal on
    /// disk is stale for what runs, so nothing may be pruned.
    pub keep_all_versions: bool,
    /// The journal to write; `None` when nothing changes. Its `outcomes` hold
    /// every pending record, old and new. Name the write with
    /// [`journal_write_name`](Self::journal_write_name).
    pub journal_write: Option<Journal>,
    /// The name of the `journal_write`: `J-R2` after a return, `reconcile-installed`
    /// when the only change is `installed` (O5), else `recovery`.
    pub journal_write_name: &'static str,
    /// The records to upsert into the state file by id, in order, and the
    /// failed versions to mark. Call [`Journal::remove_outcome`] and write
    /// once each upsert lands.
    pub upserts: Vec<Outcome>,
    /// Updates to records that already exist in the state file.
    pub record_patches: Vec<RecordPatch>,
    pub install: InstallAction,
    pub adopt: Option<Adopt>,
    /// Set with [`RunChoice::VerifiedInstalled`]: copy the installed file
    /// into `versions/<version>/` with `stage_exclusive` (same hash reuses)
    /// and only then publish `journal_write` and spawn the worker. If
    /// staging fails because the version is in use or holds another binary,
    /// do not publish this plan: call [`RecoveryPlan::on_stage_conflict`]
    /// and execute what it returns instead.
    pub stage_installed: Option<Artifact>,
    /// What happened, for the log.
    pub notes: Vec<String>,
}

impl RecoveryPlan {
    fn empty(run: RunChoice) -> Self {
        Self {
            run,
            status: PlanStatus::Ok,
            quarantine: false,
            first_start: false,
            keep_all_versions: false,
            journal_write: None,
            journal_write_name: "recovery",
            upserts: Vec::new(),
            record_patches: Vec::new(),
            install: InstallAction::None,
            adopt: None,
            stage_installed: None,
            notes: Vec::new(),
        }
    }

    /// The plan to execute when the staging this plan requires
    /// ([`stage_installed`](Self::stage_installed)) fails because
    /// `conflicting_dir` (`versions/<version>/`) is in use or holds another
    /// binary (O2). Consumes the plan, so the original can't still be
    /// published.
    ///
    /// The result runs the hash-verified installed file with switching
    /// disabled ([`RunChoice::InstalledSwitchingDisabled`], status
    /// `SwitchingDisabled` naming `conflicting_dir`) and suppresses all else
    /// the original plan would have done on the strength of that copy: no
    /// `journal_write` (the rebuilt journal that claims the version is
    /// verified is not published; the journal on disk stays as it is), no
    /// `install`, `adopt`, `upserts` or `record_patches` (all of them replay
    /// from the unchanged journal at the next recovery), and every version
    /// stays out of every prune.
    ///
    /// `Err(self)` when the plan required no staging: nothing to fall back
    /// from.
    pub fn on_stage_conflict(
        self,
        conflicting_dir: &Path,
    ) -> Result<RecoveryPlan, Box<RecoveryPlan>> {
        let Some(artifact) = self.stage_installed.clone() else {
            return Err(Box::new(self));
        };
        let reason = format!(
            "the installed file ({} {}) can't be kept in {}: it is in use or holds another binary; \
             switching is disabled and one worker runs from the installed file",
            artifact.version,
            artifact.sha256,
            conflicting_dir.display()
        );
        let mut plan = RecoveryPlan::empty(RunChoice::InstalledSwitchingDisabled {
            artifact,
            reason: reason.clone(),
        });
        plan.status = PlanStatus::SwitchingDisabled(reason.clone());
        plan.keep_all_versions = true;
        plan.notes = self.notes;
        plan.notes.push(format!(
            "{reason}; the rebuilt journal is not published and nothing else the plan held is done"
        ));
        Ok(plan)
    }
}

/// The plan for whatever the journal file held.
pub fn plan_startup(load: &JournalLoad, facts: &Facts) -> RecoveryPlan {
    match load {
        JournalLoad::Missing => {
            let mut plan = RecoveryPlan::empty(RunChoice::Installed(InstalledReason::FirstStart));
            plan.first_start = true;
            plan.notes.push("no journal: first start".to_owned());
            plan
        }
        JournalLoad::Unreadable { reason } => {
            let mut plan = RecoveryPlan::empty(RunChoice::Installed(InstalledReason::FirstStart));
            plan.quarantine = true;
            plan.first_start = true;
            plan.keep_all_versions = true;
            plan.notes.push(format!(
                "the journal is unreadable ({reason}): it is set aside, and every kept version stays"
            ));
            plan
        }
        JournalLoad::Newer { found } => {
            let mut plan = RecoveryPlan::empty(RunChoice::Installed(InstalledReason::NewerJournal));
            plan.status = PlanStatus::SwitchingDisabled(format!(
                "journal format {found} is newer than this version supports ({})",
                crate::journal::FORMAT
            ));
            plan.keep_all_versions = true;
            plan.notes
                .push("the journal is newer: one worker, no journal write".to_owned());
            plan
        }
        JournalLoad::Loaded(journal) => plan_recovery(journal, facts),
    }
}

/// The installed file as an artifact, only when its hash is one we know:
/// the journal's `installed.sha256`, or the supervisor's own.
fn installed_by_hash(journal: &Journal, facts: &Facts) -> Option<Artifact> {
    let hash = facts.installed_sha256.as_ref()?;
    let version = match &journal.installed {
        Some(installed) if &installed.sha256 == hash => installed.version.clone(),
        _ if facts.self_sha256.as_ref() == Some(hash) => facts.self_version.clone(),
        _ => return None,
    };
    Some(Artifact {
        version,
        sha256: hash.clone(),
        size: facts.installed_size,
    })
}

/// Which artifact runs.
enum Ran {
    /// Verified in `versions/`.
    Version(Artifact),
    /// The installed file, by hash.
    Installed(Artifact),
    Nothing,
}

fn make_record(switch: &Switch, now: &str, result: &str) -> SwitchRecord {
    SwitchRecord {
        id: switch.id.clone(),
        from: switch.from.version.clone(),
        to: switch.to.version.clone(),
        at: now.to_owned(),
        how: switch.how.as_str().to_owned(),
        result: Some(result.to_owned()),
        blue_green: true,
        ..SwitchRecord::default()
    }
}

/// The outcome of a switch that was cut short, with no return journalled.
fn restart_outcome(switch: &Switch, ran: &Artifact, completed: bool, now: &str) -> Outcome {
    if completed {
        let mut record = make_record(switch, now, "rolled-back");
        record.confirmed = true;
        record.why = Some(
            "the service restarted during the rollback; it finished on the rollback's target"
                .to_owned(),
        );
        return Outcome {
            record,
            mark_failed: None,
        };
    }
    let mut record;
    if switch.phase == Phase::Watching {
        record = make_record(switch, now, "failed-after-switch");
        record.returned_to = Some(ran.version.clone());
        record.why = Some("the service restarted during the watch window".to_owned());
    } else {
        record = make_record(switch, now, "failed-to-load");
        record.why = Some("the service restarted during the switch".to_owned());
    }
    record.phase = Some(switch.phase.as_str().to_owned());
    record.blame = Some(Blame::Environment.as_str().to_owned());
    Outcome {
        record,
        mark_failed: None,
    }
}

/// The outcome of a decided return (J-R1 was written): the forward switch's
/// own record, with the blame and why the decision stored.
fn return_outcome(switch: &Switch, ret: &Return, ran: &Artifact, now: &str) -> Outcome {
    let result = if switch.phase == Phase::Watching {
        "failed-after-switch"
    } else {
        "failed-to-load"
    };
    let mut record = make_record(switch, now, result);
    record.phase = Some(switch.phase.as_str().to_owned());
    record.blame = Some(ret.blame.as_str().to_owned());
    record.why = Some(ret.why.clone());
    record.returned_to = Some(ran.version.clone());
    // `mark_failed` only when `how == Update`: the auto-updater offers
    // new versions, and a failed update target must not be offered again. A
    // rollback's target is an older version the auto-updater never offers,
    // so marking it would only block a later deliberate `update --to`.
    let mark_failed =
        (ret.blame == Blame::NewVersion && switch.how == How::Update).then(|| MarkFailed {
            version: switch.to.version.clone(),
            why: ret.why.clone(),
        });
    Outcome {
        record,
        mark_failed,
    }
}

/// Recovery for a journal that was read.
pub fn plan_recovery(journal: &Journal, facts: &Facts) -> RecoveryPlan {
    let now = facts.time();
    let mut j = journal.clone();
    let mut notes = Vec::new();

    // What recovery wants to run: `return.target` first, else `confirmed`.
    let returning = journal
        .switch
        .as_ref()
        .and_then(|switch| switch.return_.as_ref());
    let primary = match returning {
        Some(ret) => ret.target.clone(),
        None => journal.confirmed.clone(),
    };

    // The ladder: the wanted artifact, `confirmed`, `previous`, then the
    // installed file by hash. A path is never trusted, only a hash.
    // No oscillation: a return never falls back to the version that just
    // failed (`switch.to`), whichever rung of the ladder it would be.
    let failed_to = journal
        .switch
        .as_ref()
        .filter(|switch| switch.return_.is_some())
        .map(|switch| &switch.to);
    let allowed = |artifact: &Artifact| {
        failed_to.is_none_or(|failed| {
            failed.version != artifact.version && failed.sha256 != artifact.sha256
        })
    };
    let mut candidates = vec![primary.clone()];
    for next in [Some(&journal.confirmed), journal.previous.as_ref()]
        .into_iter()
        .flatten()
    {
        if !candidates.contains(next) && allowed(next) {
            candidates.push(next.clone());
        }
    }
    let ran = match candidates.iter().find(|artifact| facts.verified(artifact)) {
        Some(artifact) => Ran::Version(artifact.clone()),
        None => match installed_by_hash(journal, facts).filter(|artifact| allowed(artifact)) {
            Some(artifact) => Ran::Installed(artifact),
            None => Ran::Nothing,
        },
    };

    let (run_artifact, installed_ran) = match &ran {
        Ran::Version(artifact) => (artifact.clone(), false),
        Ran::Installed(artifact) => (artifact.clone(), true),
        Ran::Nothing => {
            let mut plan = RecoveryPlan::empty(RunChoice::Nothing);
            plan.status = PlanStatus::Degraded(format!(
                "neither {} nor any fallback matches its recorded hash, and the installed file is not known",
                primary.version
            ));
            plan.upserts = journal.outcomes.clone();
            plan.notes.push("no artifact can run: degraded".to_owned());
            // U2-O1: the repair of a missing installed file doesn't need a
            // worker, so it is returned even though none can run.
            if journal.install_pending.is_none() && facts.installed_sha256.is_none() {
                plan.install = repair_missing_installed(journal, facts, &mut plan.notes);
            }
            return plan;
        }
    };
    if run_artifact != primary {
        notes.push(format!(
            "{} failed its hash check; running {} instead",
            primary.version, run_artifact.version
        ));
    }
    let on_primary = run_artifact == primary;

    // The switch, if any.
    match &journal.switch {
        Some(switch) => {
            let outcome = if let Some(ret) = returning {
                notes.push("a journalled return is carried out".to_owned());
                return_outcome(switch, ret, &run_artifact, &now)
            } else {
                let completed = switch.how == How::Rollback
                    && on_primary
                    && journal.confirmed == switch.to
                    && switch.to != switch.from;
                notes.push(if completed {
                    "the restart completed the rollback".to_owned()
                } else {
                    "a switch was cut short: reverting to the confirmed version".to_owned()
                });
                restart_outcome(switch, &run_artifact, completed, &now)
            };
            j.switch = None;
            j.push_outcome(outcome);
            j.active = run_artifact.clone();
            if returning.is_some() || !on_primary {
                // J-R2, or a fallback: what runs is now the confirmed one.
                // `previous` does not change.
                j.confirmed = run_artifact.clone();
            }
        }
        None => {
            j.active = run_artifact.clone();
            if !on_primary {
                j.confirmed = run_artifact.clone();
            }
        }
    }

    if !installed_ran && !on_primary && journal.previous.as_ref() == Some(&run_artifact) {
        j.previous = None;
    }

    // The installed file (3.9). These rows run only when a worker will run;
    // the degraded `Ran::Nothing` return above may still restore a missing
    // installed file from hash-verified recorded bytes, so the next start
    // can find that installed file by its hash.
    let mut install = InstallAction::None;
    let mut patches = Vec::new();
    let mut pending_handled = false;
    if let Some(pending) = j.install_pending.clone() {
        pending_handled = true;
        let target = pending.target.clone();
        let old = j
            .installed
            .as_ref()
            .map(|installed| installed.sha256.as_str());
        // The result is carried durably in the same write that clears
        // `install_pending`, as an outcome (merged into one with the same id
        // if it is still held), and also offered as a `RecordPatch`.
        let mut report = |j: &mut Journal, text: &str| {
            if let Some(id) = pending.record_id.clone() {
                set_installed_binary(j, &id, text);
                patches.push(RecordPatch {
                    id,
                    installed_binary: text.to_owned(),
                });
            }
        };
        // Invariant (Opus 11): `install_pending.target` is `confirmed`. A
        // recovery that moved `confirmed` off the pending target (a fallback,
        // or a rollback's return) clears the install in the same write and
        // reports `superseded` on its record. It never queues an install of
        // the new `confirmed` (3.9 has no such row: the installed file is
        // normally that version already, A when B failed and we fell back to
        // A), so only a missing file is repaired, from what the journal says
        // is installed.
        // Identity is version and hash, not size: an artifact made from the
        // installed file carries the file's size. An input journal that
        // already breaks the invariant is cleared too.
        let superseded =
            j.confirmed.version != target.version || j.confirmed.sha256 != target.sha256;
        if superseded {
            notes.push(format!(
                "confirmed moved off {}: its pending install is superseded",
                target.version
            ));
            j.install_pending = None;
            match facts.installed_sha256.as_deref() {
                // The replace had already landed: the journal records the
                // file as it is, so the next start does not adopt it.
                Some(hash) if hash == target.sha256 => {
                    j.installed = Some(Installed::from(&target));
                }
                Some(_) => {}
                None => {
                    install = repair_missing_installed(&j, facts, &mut notes);
                }
            }
            report(&mut j, "superseded");
        } else {
            match facts.installed_sha256.as_deref() {
                Some(hash) if hash == target.sha256 => {
                    j.installed = Some(Installed::from(&target));
                    j.install_pending = None;
                    report(&mut j, "replaced");
                }
                Some(hash) if Some(hash) == old => {
                    if facts.verified(&target) {
                        install = InstallAction::Replace {
                            target,
                            aside: pending.aside.clone(),
                        };
                    } else {
                        notes.push(format!(
                            "the install of {} can't run: its copy in versions/ failed its hash check",
                            target.version
                        ));
                    }
                }
                Some(_) => {
                    notes.push(
                        "the installed file was replaced by something else: install cleared"
                            .to_owned(),
                    );
                    j.install_pending = None;
                    report(&mut j, "changed-elsewhere");
                }
                None => {
                    if facts.verified(&target) {
                        install = InstallAction::Place {
                            target,
                            aside: pending.aside.clone(),
                        };
                    } else if let Some(installed) = j.installed.clone().filter(|installed| {
                        facts.versions.get(&installed.version) == Some(&installed.sha256)
                    }) {
                        install = InstallAction::Restore { installed };
                    } else {
                        notes.push(
                            "the installed file is missing and neither it nor its target can be placed"
                                .to_owned(),
                        );
                    }
                }
            }
        }
    }

    // A missing installed file with no install pending (U2-O1): the repair
    // comes from the journal and the disk facts alone, so it survives a
    // superseding write that landed before the restore ran, and a restore
    // that failed and is planned again. The superseded branch above uses the
    // same rule when its clearing write has not landed yet.
    if !pending_handled
        && !installed_ran
        && j.install_pending.is_none()
        && facts.installed_sha256.is_none()
    {
        install = repair_missing_installed(&j, facts, &mut notes);
    }

    // Authenticate and report any completed install before rebuilding from the
    // installed fallback. Otherwise clearing the pending install would lose the
    // only operation id that lets the state file learn its result after a crash.
    if installed_ran {
        let mut rebuilt =
            Journal::first_start(run_artifact.clone(), Some(Installed::from(&run_artifact)));
        rebuilt.seq = j.seq;
        rebuilt.outcomes = std::mem::take(&mut j.outcomes);
        j = rebuilt;
        install = InstallAction::None;
        notes.push("the journal is rebuilt from the installed file".to_owned());
    }

    // Reconcile (O5): idle, no install pending, and the installed file is
    // the verified artifact that runs, while the journal still records an
    // older `installed` (J-R2 leaves `installed = A` with `confirmed = B`;
    // then the user installs B from outside). Without this the next update's
    // hash classification calls the file `changed-elsewhere` (T4).
    let mut reconciled = false;
    if !installed_ran
        && journal.switch.is_none()
        && journal.install_pending.is_none()
        && facts.installed_sha256.as_deref() == Some(run_artifact.sha256.as_str())
        && journal
            .installed
            .as_ref()
            .map(|installed| installed.sha256.as_str())
            != Some(run_artifact.sha256.as_str())
    {
        j.installed = Some(Installed::from(&run_artifact));
        reconciled = true;
        notes.push(format!(
            "the installed file is {}, which runs: the journal's installed is updated",
            run_artifact.version
        ));
    }
    let only_installed_changed = reconciled && {
        let mut only = journal.clone();
        only.installed = j.installed.clone();
        only == j
    };
    let journal_write_name = if returning.is_some() {
        "J-R2"
    } else if only_installed_changed {
        "reconcile-installed"
    } else {
        "recovery"
    };

    // Adoption (3.1 item 1): only when no switch was open and no install was
    // in flight, and the installed file is neither what the journal recorded
    // nor what runs.
    let adopt = match facts.installed_sha256.as_ref() {
        Some(hash)
            if journal.switch.is_none()
                && !pending_handled
                && !installed_ran
                && journal
                    .installed
                    .as_ref()
                    .map(|installed| &installed.sha256)
                    != Some(hash)
                && *hash != run_artifact.sha256 =>
        {
            notes.push("the installed file is new to the journal: adopting it".to_owned());
            Some(Adopt {
                sha256: hash.clone(),
            })
        }
        _ => None,
    };

    let upserts = j.outcomes.clone();
    // The installed file is trusted by hash only. Its `versions/<v>/` copy
    // does not exist (or doesn't match), so the plan says so: stage it first,
    // and publish `journal_write` only after that succeeded.
    let (run, stage_installed) = if installed_ran {
        (
            RunChoice::VerifiedInstalled(run_artifact.clone()),
            Some(run_artifact),
        )
    } else {
        (RunChoice::Version(run_artifact), None)
    };
    RecoveryPlan {
        run,
        stage_installed,
        status: PlanStatus::Ok,
        quarantine: false,
        first_start: false,
        keep_all_versions: false,
        journal_write: (j != *journal).then_some(j),
        journal_write_name,
        upserts,
        record_patches: patches,
        install,
        adopt,
        notes,
    }
}

/// The repair of a missing installed file when no install is pending: restore
/// `journal.installed` from `versions/<installed.version>/` when that copy is
/// verified. Otherwise no action, and a note says so (the pending branch does
/// the same when it can't place anything).
fn repair_missing_installed(j: &Journal, facts: &Facts, notes: &mut Vec<String>) -> InstallAction {
    match j.installed.clone() {
        Some(installed) if facts.versions.get(&installed.version) == Some(&installed.sha256) => {
            InstallAction::Restore { installed }
        }
        Some(installed) => {
            notes.push(format!(
                "the installed file {} is missing and its recorded version {} has no verified copy in versions/ to restore it from",
                facts.installed_path.display(),
                installed.version
            ));
            InstallAction::None
        }
        None => {
            notes.push(format!(
                "the installed file {} is missing and the journal records no installed version to restore it from",
                facts.installed_path.display()
            ));
            InstallAction::None
        }
    }
}

/// Records what became of the installed file for record `id` as a durable
/// outcome: an outcome still held for `id` only has its `installed_binary`
/// updated (its other fields and `mark_failed` are kept); otherwise a
/// partial record `{id, installed_binary}` is added, which the state upsert
/// merges into the existing record.
fn set_installed_binary(j: &mut Journal, id: &str, text: &str) {
    match j.outcomes.iter_mut().find(|held| held.record.id == id) {
        Some(held) => held.record.installed_binary = Some(text.to_owned()),
        None => j.push_outcome(Outcome {
            record: SwitchRecord {
                id: id.to_owned(),
                installed_binary: Some(text.to_owned()),
                ..SwitchRecord::default()
            },
            mark_failed: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Admitted, InstallPending, Worker, WorkerRole};

    fn art(version: &str) -> Artifact {
        Artifact {
            version: version.to_owned(),
            sha256: format!("{:0<64}", version.replace('.', "")),
            size: 10,
        }
    }

    fn clock(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn facts_for(journal: &Journal) -> Facts {
        let mut versions = BTreeMap::new();
        let mut add = |artifact: &Artifact| {
            versions.insert(artifact.version.clone(), artifact.sha256.clone());
        };
        add(&journal.active);
        add(&journal.confirmed);
        journal.previous.iter().for_each(&mut add);
        if let Some(pending) = &journal.install_pending {
            add(&pending.target);
        }
        if let Some(switch) = &journal.switch {
            add(&switch.from);
            add(&switch.to);
            switch.return_.iter().for_each(|ret| add(&ret.target));
        }
        Facts {
            now: clock("2026-10-09T12:00:00Z"),
            self_sha256: None,
            self_version: "9.9.9".into(),
            installed_sha256: journal
                .installed
                .as_ref()
                .map(|installed| installed.sha256.clone()),
            installed_size: 77,
            installed_path: PathBuf::from("/opt/open-ferry/open-ferry"),
            versions,
        }
    }

    fn idle(confirmed: &str) -> Journal {
        Journal::first_start(art(confirmed), Some(Installed::from(&art(confirmed))))
    }

    fn switch(how: How, from: &str, to: &str, phase: Phase) -> Switch {
        Switch {
            id: "s1".into(),
            how,
            from: art(from),
            to: art(to),
            phase,
            started: "2026-10-09T11:00:00Z".into(),
            watch_until: (phase == Phase::Watching).then(|| "2026-10-09T11:10:00Z".into()),
            admitted: Admitted::default(),
            return_: None,
        }
    }

    const PHASES: [Phase; 4] = [
        Phase::Loading,
        Phase::HandingOver,
        Phase::TakingOver,
        Phase::Watching,
    ];

    fn only_outcome(plan: &RecoveryPlan) -> &Outcome {
        assert_eq!(plan.upserts.len(), 1, "{:?}", plan.upserts);
        &plan.upserts[0]
    }

    fn written(plan: &RecoveryPlan) -> &Journal {
        plan.journal_write.as_ref().expect("a journal write")
    }

    #[test]
    fn no_file_unreadable_and_newer() {
        // Not upstream's: 3.3's first three rows.
        let facts = facts_for(&idle("0.2.0"));
        let plan = plan_startup(&JournalLoad::Missing, &facts);
        assert_eq!(plan.run, RunChoice::Installed(InstalledReason::FirstStart));
        assert!(plan.first_start && !plan.quarantine && plan.journal_write.is_none());

        let plan = plan_startup(&JournalLoad::Unreadable { reason: "x".into() }, &facts);
        assert!(plan.first_start && plan.quarantine && plan.keep_all_versions);
        assert_eq!(plan.run, RunChoice::Installed(InstalledReason::FirstStart));

        let plan = plan_startup(&JournalLoad::Newer { found: 4 }, &facts);
        assert_eq!(
            plan.run,
            RunChoice::Installed(InstalledReason::NewerJournal)
        );
        assert!(plan.journal_write.is_none() && !plan.first_start);
        match plan.status {
            PlanStatus::SwitchingDisabled(why) => {
                assert!(
                    why.contains("journal format 4") && why.contains("(1)"),
                    "{why}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_idle_journal_runs_confirmed_and_writes_nothing() {
        // Not upstream's: "switch null": verify confirmed, run it.
        let mut journal = idle("0.2.0");
        journal.workers.push(Worker {
            pid: 1,
            started: 1,
            version: "0.2.0".into(),
            role: WorkerRole::Active,
            lost: false,
        });
        let plan = plan_recovery(&journal, &facts_for(&journal));
        assert_eq!(plan.run, RunChoice::Version(art("0.2.0")));
        assert_eq!(plan.status, PlanStatus::Ok);
        assert!(plan.journal_write.is_none() && plan.upserts.is_empty());
        assert_eq!(plan.install, InstallAction::None);
        assert!(plan.adopt.is_none());
    }

    #[test]
    fn a_restart_before_ready_fails_to_load_on_confirmed() {
        // Not upstream's: loading, handing-over and taking-over run
        // `confirmed`, clear `switch`, record failed-to-load, blame
        // environment, mark nothing.
        for phase in [Phase::Loading, Phase::HandingOver, Phase::TakingOver] {
            let mut journal = idle("0.2.0");
            journal.switch = Some(switch(How::Update, "0.2.0", "0.3.0", phase));
            let plan = plan_recovery(&journal, &facts_for(&journal));
            assert_eq!(plan.run, RunChoice::Version(art("0.2.0")), "{phase:?}");
            let outcome = only_outcome(&plan);
            let record = &outcome.record;
            assert_eq!(record.id, "s1");
            assert_eq!(record.result.as_deref(), Some("failed-to-load"));
            assert_eq!(record.phase.as_deref(), Some(phase.as_str()));
            assert_eq!(record.blame.as_deref(), Some("environment"));
            assert_eq!(
                record.why.as_deref(),
                Some("the service restarted during the switch")
            );
            assert_eq!(record.at, "2026-10-09T12:00:00Z");
            assert!(outcome.mark_failed.is_none());
            let next = written(&plan);
            assert!(next.switch.is_none());
            assert_eq!(next.active, art("0.2.0"));
            assert_eq!(next.confirmed, art("0.2.0"));
            assert_eq!(next.outcomes, plan.upserts);
        }
    }

    #[test]
    fn a_restart_in_the_window_reverts_to_from() {
        // Not upstream's: Q4. The window's restart reverts, blames the
        // environment and does not mark the release failed.
        let mut journal = idle("0.2.0");
        journal.active = art("0.3.0");
        journal.switch = Some(switch(How::Update, "0.2.0", "0.3.0", Phase::Watching));
        let plan = plan_recovery(&journal, &facts_for(&journal));
        assert_eq!(plan.run, RunChoice::Version(art("0.2.0")));
        let outcome = only_outcome(&plan);
        assert_eq!(
            outcome.record.result.as_deref(),
            Some("failed-after-switch")
        );
        assert_eq!(outcome.record.returned_to.as_deref(), Some("0.2.0"));
        assert_eq!(outcome.record.blame.as_deref(), Some("environment"));
        assert_eq!(
            outcome.record.why.as_deref(),
            Some("the service restarted during the watch window")
        );
        assert!(outcome.mark_failed.is_none());
        assert_eq!(written(&plan).active, art("0.2.0"));
        assert!(written(&plan).switch.is_none());
    }

    #[test]
    fn a_rollback_that_ended_a_watch_finishes_on_its_target() {
        // Not upstream's: T4. After a forward watch (A to B) `confirmed` is
        // A; the rollback B to A has `from = B`, `to = A`. A restart at
        // any phase completes it: run A, `rolled-back`, confirmed.
        for phase in PHASES {
            let mut journal = idle("0.2.0");
            journal.active = art("0.3.0");
            // The rollback's J1 carried the forward record's superseded_by.
            journal.outcomes.push(Outcome {
                record: SwitchRecord {
                    id: "fwd".into(),
                    result: Some("switched".into()),
                    superseded_by: Some("s1".into()),
                    ..SwitchRecord::default()
                },
                mark_failed: None,
            });
            journal.switch = Some(switch(How::Rollback, "0.3.0", "0.2.0", phase));
            let plan = plan_recovery(&journal, &facts_for(&journal));
            assert_eq!(plan.run, RunChoice::Version(art("0.2.0")), "{phase:?}");
            assert_eq!(plan.upserts.len(), 2, "forward record replayed first");
            assert_eq!(plan.upserts[0].record.id, "fwd");
            let record = &plan.upserts[1].record;
            assert_eq!(record.result.as_deref(), Some("rolled-back"), "{phase:?}");
            assert!(record.confirmed);
            assert_eq!(record.how, "rollback");
            assert_eq!(
                record.why.as_deref(),
                Some(
                    "the service restarted during the rollback; it finished on the rollback's target"
                )
            );
            assert!(plan.upserts[1].mark_failed.is_none());
            let next = written(&plan);
            assert_eq!(next.active, art("0.2.0"));
            assert_eq!(next.confirmed, art("0.2.0"));
            assert!(next.switch.is_none());
        }
    }

    #[test]
    fn a_rollback_to_previous_reverts_like_an_update_until_it_is_confirmed() {
        // Not upstream's: T4. A rollback to `previous` has `confirmed =
        // from`; before its window ends a restart runs `from` and the
        // rollback fails (before ready or in the window).
        for phase in PHASES {
            let mut journal = idle("0.3.0");
            journal.previous = Some(art("0.2.0"));
            journal.switch = Some(switch(How::Rollback, "0.3.0", "0.2.0", phase));
            let plan = plan_recovery(&journal, &facts_for(&journal));
            assert_eq!(plan.run, RunChoice::Version(art("0.3.0")), "{phase:?}");
            let record = &only_outcome(&plan).record;
            let expected = if phase == Phase::Watching {
                "failed-after-switch"
            } else {
                "failed-to-load"
            };
            assert_eq!(record.result.as_deref(), Some(expected), "{phase:?}");
            assert_eq!(record.how, "rollback");
            assert!(only_outcome(&plan).mark_failed.is_none());
            let next = written(&plan);
            assert_eq!(next.previous, Some(art("0.2.0")));
            assert_eq!(next.confirmed, art("0.3.0"));
        }
    }

    fn with_return(mut journal: Journal, blame: Blame) -> Journal {
        let target = journal.switch.as_ref().unwrap().from.clone();
        journal.switch.as_mut().unwrap().return_ = Some(Return {
            target,
            blame,
            why: "the new version exited with code 101".into(),
            decided: "2026-10-09T11:05:00Z".into(),
        });
        journal
    }

    #[test]
    fn a_journalled_return_is_carried_out_in_one_write() {
        // Not upstream's: T4 and T5, J-R2. `active = confirmed =
        // return.target`, `switch` null, the outcome set, `previous`
        // unchanged; mark_failed only for blame new-version on an update.
        let mut journal = idle("0.2.0");
        journal.previous = Some(art("0.1.0"));
        journal.active = art("0.3.0");
        journal.switch = Some(switch(How::Update, "0.2.0", "0.3.0", Phase::Watching));
        let journal = with_return(journal, Blame::NewVersion);
        let plan = plan_recovery(&journal, &facts_for(&journal));
        assert_eq!(plan.run, RunChoice::Version(art("0.2.0")));
        let outcome = only_outcome(&plan);
        assert_eq!(
            outcome.record.result.as_deref(),
            Some("failed-after-switch")
        );
        assert_eq!(outcome.record.blame.as_deref(), Some("new-version"));
        assert_eq!(outcome.record.returned_to.as_deref(), Some("0.2.0"));
        assert_eq!(
            outcome.record.why.as_deref(),
            Some("the new version exited with code 101")
        );
        assert_eq!(
            outcome.mark_failed,
            Some(MarkFailed {
                version: "0.3.0".into(),
                why: "the new version exited with code 101".into()
            })
        );
        let next = written(&plan);
        assert_eq!(next.active, art("0.2.0"));
        assert_eq!(next.confirmed, art("0.2.0"));
        assert_eq!(next.previous, Some(art("0.1.0")));
        assert!(next.switch.is_none());
        assert_eq!(next.outcomes, plan.upserts);

        // The same return before ready is failed-to-load, and an
        // environment blame marks nothing.
        for phase in [Phase::Loading, Phase::HandingOver, Phase::TakingOver] {
            let mut journal = idle("0.2.0");
            journal.switch = Some(switch(How::Update, "0.2.0", "0.3.0", phase));
            let journal = with_return(journal, Blame::Environment);
            let plan = plan_recovery(&journal, &facts_for(&journal));
            let outcome = only_outcome(&plan);
            assert_eq!(outcome.record.result.as_deref(), Some("failed-to-load"));
            assert_eq!(outcome.record.phase.as_deref(), Some(phase.as_str()));
            assert!(outcome.mark_failed.is_none());
        }
    }

    #[test]
    fn a_requested_rollbacks_return_makes_the_returned_version_confirmed() {
        // Not upstream's: T4, row j. A rollback ended a watch (A confirmed);
        // its target A fails in its window, so the return goes back to B
        // (the rollback's `from`). J-R2 makes B active and confirmed, and
        // nothing is marked failed (it is a rollback).
        let mut journal = idle("0.2.0");
        journal.previous = Some(art("0.1.0"));
        journal.active = art("0.2.0");
        journal.switch = Some(switch(How::Rollback, "0.3.0", "0.2.0", Phase::Watching));
        let journal = with_return(journal, Blame::NewVersion);
        let plan = plan_recovery(&journal, &facts_for(&journal));
        assert_eq!(plan.run, RunChoice::Version(art("0.3.0")));
        let outcome = only_outcome(&plan);
        assert_eq!(outcome.record.how, "rollback");
        assert_eq!(
            outcome.record.result.as_deref(),
            Some("failed-after-switch")
        );
        assert_eq!(outcome.record.returned_to.as_deref(), Some("0.3.0"));
        assert!(outcome.mark_failed.is_none());
        let next = written(&plan);
        assert_eq!(next.active, art("0.3.0"));
        assert_eq!(next.confirmed, art("0.3.0"));
        assert_eq!(next.previous, Some(art("0.1.0")));
    }

    #[test]
    fn a_restart_after_a_return_runs_what_was_running() {
        // Not upstream's: T4. After J-R2 (and after J-R1's completion) the
        // journal is idle with `confirmed = return.target`: a restart runs
        // it and only replays the outcome.
        let mut journal = idle("0.3.0");
        journal.previous = Some(art("0.1.0"));
        journal.outcomes.push(Outcome {
            record: SwitchRecord {
                id: "s1".into(),
                result: Some("failed-after-switch".into()),
                ..SwitchRecord::default()
            },
            mark_failed: Some(MarkFailed {
                version: "0.4.0".into(),
                why: "w".into(),
            }),
        });
        let plan = plan_recovery(&journal, &facts_for(&journal));
        assert_eq!(plan.run, RunChoice::Version(art("0.3.0")));
        assert_eq!(plan.upserts, journal.outcomes);
        assert!(
            plan.journal_write.is_none(),
            "replay clears nothing by itself"
        );
    }

    #[test]
    fn a_confirm_whose_state_write_was_lost_is_replayed_exactly_once() {
        // Not upstream's: T5. A crash between J5 and S2 leaves the outcome
        // in the journal; the next start upserts `confirmed: true`, and once
        // the entry is removed by id it is not upserted again.
        let mut journal = idle("0.2.0");
        journal.switch = Some(switch(How::Update, "0.2.0", "0.3.0", Phase::Watching));
        let record = SwitchRecord {
            id: "s1".into(),
            result: Some("switched".into()),
            confirmed: true,
            ..SwitchRecord::default()
        };
        assert!(journal.confirm(
            Outcome {
                record: record.clone(),
                mark_failed: None
            },
            None
        ));
        let plan = plan_recovery(&journal, &facts_for(&journal));
        assert_eq!(plan.run, RunChoice::Version(art("0.3.0")));
        assert_eq!(plan.upserts.len(), 1);
        assert_eq!(plan.upserts[0].record, record);
        assert!(plan.upserts[0].record.confirmed);

        // Upserting is idempotent by id: planning again before the removal
        // yields the same thing; after the removal there is nothing.
        let again = plan_recovery(&journal, &facts_for(&journal));
        assert_eq!(again.upserts, plan.upserts);
        assert!(journal.remove_outcome("s1"));
        assert!(
            plan_recovery(&journal, &facts_for(&journal))
                .upserts
                .is_empty()
        );
    }

    #[test]
    fn a_confirmed_with_a_bad_hash_falls_back_by_hash_only() {
        // Not upstream's: 3.3's last row. `confirmed`, then `previous`, then
        // the installed file only by `installed.sha256` or the supervisor's
        // own hash; else nothing, `degraded`.
        let mut journal = idle("0.3.0");
        journal.previous = Some(art("0.2.0"));
        journal.installed = Some(Installed::from(&art("0.1.0")));
        let mut facts = facts_for(&journal);
        facts.versions.insert("0.3.0".into(), "f".repeat(64));
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.run, RunChoice::Version(art("0.2.0")));
        let next = written(&plan);
        assert_eq!(next.confirmed, art("0.2.0"));
        assert_eq!(next.active, art("0.2.0"));
        assert!(next.previous.is_none());

        // Both missing: the installed file, because its hash is the journal's.
        facts.versions.insert("0.2.0".into(), "e".repeat(64));
        facts.installed_sha256 = Some(art("0.1.0").sha256);
        let plan = plan_recovery(&journal, &facts);
        match &plan.run {
            RunChoice::VerifiedInstalled(artifact) => {
                assert_eq!(artifact.version, "0.1.0");
                assert_eq!(artifact.size, 77);
                // Staged before the journal is published; never claimed to
                // already be in `versions/`.
                assert_eq!(plan.stage_installed.as_ref(), Some(artifact));
            }
            other => panic!("{other:?}"),
        }
        let next = written(&plan);
        assert_eq!(next.confirmed.version, "0.1.0");
        assert!(next.previous.is_none() && next.install_pending.is_none());
        assert_eq!(next.installed, Some(Installed::from(&art("0.1.0"))));

        // The supervisor's own hash counts too.
        facts.installed_sha256 = Some("a".repeat(64));
        facts.self_sha256 = Some("a".repeat(64));
        let plan = plan_recovery(&journal, &facts);
        match &plan.run {
            RunChoice::VerifiedInstalled(artifact) => {
                assert_eq!(artifact.version, "9.9.9");
                assert!(plan.stage_installed.is_some());
            }
            other => panic!("{other:?}"),
        }

        // Any other hash is never trusted.
        facts.self_sha256 = None;
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.run, RunChoice::Nothing);
        assert!(matches!(plan.status, PlanStatus::Degraded(_)));
        assert!(plan.journal_write.is_none());

        // Missing installed file: the same.
        facts.installed_sha256 = None;
        assert_eq!(plan_recovery(&journal, &facts).run, RunChoice::Nothing);
    }

    #[test]
    fn a_return_whose_target_is_gone_never_runs_the_failed_switch_target() {
        // Not upstream's: 6.1 g plus no oscillation. A rollback 0.3.0 -> 0.2.0
        // failed, the return target 0.3.0 is gone. `confirmed` is 0.2.0, the
        // very version that just failed, so it is not a fallback: with no
        // permitted `previous` or installed file the plan is degraded.
        let mut journal = idle("0.2.0");
        journal.switch = Some(switch(How::Rollback, "0.3.0", "0.2.0", Phase::Watching));
        let journal = with_return(journal, Blame::Environment);
        let mut facts = facts_for(&journal);
        facts.versions.remove("0.3.0");
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.run, RunChoice::Nothing);
        assert!(matches!(plan.status, PlanStatus::Degraded(_)));
        assert!(plan.stage_installed.is_none());

        // The installed file whose hash is the failed target's is excluded too.
        let mut with_installed = journal.clone();
        with_installed.installed = Some(Installed::from(&art("0.2.0")));
        facts.installed_sha256 = Some(art("0.2.0").sha256);
        let plan = plan_recovery(&with_installed, &facts);
        assert_eq!(plan.run, RunChoice::Nothing);
        assert!(plan.stage_installed.is_none());

        // A permitted `previous` is used.
        let mut with_previous = journal.clone();
        with_previous.previous = Some(art("0.1.0"));
        let mut facts = facts_for(&with_previous);
        facts.versions.remove("0.3.0");
        let plan = plan_recovery(&with_previous, &facts);
        assert_eq!(plan.run, RunChoice::Version(art("0.1.0")));
        assert_eq!(written(&plan).confirmed, art("0.1.0"));

        // So is a known installed file with another hash.
        let mut with_other = journal.clone();
        with_other.installed = Some(Installed::from(&art("0.1.0")));
        let mut facts = facts_for(&with_other);
        facts.versions.remove("0.3.0");
        facts.installed_sha256 = Some(art("0.1.0").sha256);
        let plan = plan_recovery(&with_other, &facts);
        match &plan.run {
            RunChoice::VerifiedInstalled(artifact) => assert_eq!(artifact.version, "0.1.0"),
            other => panic!("{other:?}"),
        }
        assert!(plan.stage_installed.is_some());
    }

    fn pending(journal: &mut Journal, target: &str) {
        journal.install_pending = Some(InstallPending {
            target: art(target),
            source: format!("versions/{target}/open-ferry"),
            aside: "open-ferry.old.123".into(),
            attempts: 2,
            next_try: None,
            started: "2026-10-09T11:00:00Z".into(),
            record_id: Some("s1".into()),
        });
    }

    // Not upstream's: installed fallback carries a completed install through its rebuild.
    #[test]
    fn an_installed_fallback_replays_the_completed_install_after_rebuild() {
        let mut journal = idle("0.2.0");
        journal.installed = Some(Installed::from(&art("0.1.0")));
        pending(&mut journal, "0.2.0");
        let mut facts = facts_for(&journal);
        facts.versions.clear();
        facts.installed_sha256 = Some(art("0.2.0").sha256);
        facts.self_sha256 = facts.installed_sha256.clone();
        facts.self_version = "0.2.0".into();
        let plan = plan_recovery(&journal, &facts);
        assert!(matches!(plan.run, RunChoice::VerifiedInstalled(_)));
        let rebuilt = written(&plan);
        assert!(rebuilt.install_pending.is_none());
        assert_eq!(
            only_outcome(&plan).record.installed_binary.as_deref(),
            Some("replaced")
        );
        let replay = plan_recovery(rebuilt, &facts);
        assert_eq!(replay.upserts, plan.upserts);
        let mut acknowledged = rebuilt.clone();
        assert!(acknowledged.remove_outcome("s1"));
        assert!(plan_recovery(&acknowledged, &facts).upserts.is_empty());
    }

    #[test]
    fn a_cleared_install_pending_leaves_its_result_to_replay() {
        // Not upstream's: the installed_binary result is an outcome in the
        // very write that clears `install_pending`, so a crash after that
        // write loses nothing: the next recovery replays the same upsert
        // until `remove_outcome`.
        for (hash, text) in [
            (art("0.3.0").sha256, "replaced"),
            ("c".repeat(64), "changed-elsewhere"),
        ] {
            let mut journal = idle("0.3.0");
            journal.installed = Some(Installed::from(&art("0.2.0")));
            pending(&mut journal, "0.3.0");
            let mut facts = facts_for(&journal);
            facts.installed_sha256 = Some(hash);
            let first = plan_recovery(&journal, &facts);
            let on_disk = written(&first).clone();
            assert!(on_disk.install_pending.is_none());

            // Crash: the patch was never applied. Recover from the file.
            let second = plan_recovery(&on_disk, &facts);
            assert_eq!(second.upserts, first.upserts, "{text}");
            assert_eq!(second.upserts.len(), 1);
            assert_eq!(second.upserts[0].record.id, "s1");
            assert_eq!(
                second.upserts[0].record.installed_binary.as_deref(),
                Some(text)
            );

            // After a successful upsert it is gone for good.
            let mut done = on_disk;
            assert!(done.remove_outcome("s1"));
            assert!(plan_recovery(&done, &facts).upserts.is_empty());
        }
    }

    #[test]
    fn an_install_result_merges_into_a_held_outcome_with_the_same_id() {
        // Not upstream's: a pending outcome `s1` keeps its result, blame and
        // `mark_failed`; only its `installed_binary` is set.
        let mut journal = idle("0.3.0");
        journal.installed = Some(Installed::from(&art("0.2.0")));
        pending(&mut journal, "0.3.0");
        let held = Outcome {
            record: SwitchRecord {
                id: "s1".into(),
                result: Some("failed-after-switch".into()),
                blame: Some("new-version".into()),
                installed_binary: Some("pending".into()),
                ..SwitchRecord::default()
            },
            mark_failed: Some(MarkFailed {
                version: "0.4.0".into(),
                why: "it crashed".into(),
            }),
        };
        journal.push_outcome(held.clone());
        let mut facts = facts_for(&journal);
        facts.installed_sha256 = Some(art("0.3.0").sha256);
        let plan = plan_recovery(&journal, &facts);
        let next = written(&plan);
        assert_eq!(next.outcomes.len(), 1);
        let merged = &next.outcomes[0];
        assert_eq!(merged.mark_failed, held.mark_failed);
        assert_eq!(merged.record.result, held.record.result);
        assert_eq!(merged.record.blame, held.record.blame);
        assert_eq!(merged.record.installed_binary.as_deref(), Some("replaced"));
    }

    #[test]
    fn the_four_installed_hash_cases_at_recovery() {
        // Not upstream's: 3.9's table, recovery column. `old` is
        // `installed.sha256`, `new` is `install_pending.target.sha256`.
        let mut journal = idle("0.3.0");
        journal.installed = Some(Installed::from(&art("0.2.0")));
        pending(&mut journal, "0.3.0");

        // old: replace it, journal unchanged.
        let mut facts = facts_for(&journal);
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(
            plan.install,
            InstallAction::Replace {
                target: art("0.3.0"),
                aside: "open-ferry.old.123".into()
            }
        );
        assert!(plan.journal_write.is_none() && plan.record_patches.is_empty());

        // new: finish the record.
        facts.installed_sha256 = Some(art("0.3.0").sha256);
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.install, InstallAction::None);
        assert_eq!(
            plan.record_patches,
            vec![RecordPatch {
                id: "s1".into(),
                installed_binary: "replaced".into()
            }]
        );
        let next = written(&plan);
        assert!(next.install_pending.is_none());
        assert_eq!(next.installed, Some(Installed::from(&art("0.3.0"))));
        // The result is durable in the same write, not only in the patch.
        assert_eq!(next.outcomes.len(), 1);
        assert_eq!(next.outcomes[0].record.id, "s1");
        assert_eq!(
            next.outcomes[0].record.installed_binary.as_deref(),
            Some("replaced")
        );
        assert_eq!(plan.upserts, next.outcomes);

        // another hash: skip, clear, report; adoption waits for the next start.
        facts.installed_sha256 = Some("c".repeat(64));
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.install, InstallAction::None);
        assert_eq!(plan.record_patches[0].installed_binary, "changed-elsewhere");
        assert!(written(&plan).install_pending.is_none());
        assert!(plan.adopt.is_none());
        assert_eq!(
            written(&plan).installed,
            Some(Installed::from(&art("0.2.0")))
        );

        // missing: place the target...
        facts.installed_sha256 = None;
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(
            plan.install,
            InstallAction::Place {
                target: art("0.3.0"),
                aside: "open-ferry.old.123".into()
            }
        );

        // ...or, with the target unverified, restore `installed`.
        facts.versions.remove("0.3.0");
        facts.versions.insert("0.2.0".into(), art("0.2.0").sha256);
        // `confirmed` is 0.3.0 and now fails its hash, so previous or
        // installed must run: give previous.
        journal.previous = Some(art("0.2.0"));
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.run, RunChoice::Version(art("0.2.0")));
        assert_eq!(
            plan.install,
            InstallAction::Restore {
                installed: Installed::from(&art("0.2.0"))
            }
        );
    }

    #[test]
    fn a_hash_equal_to_old_with_an_unverified_target_never_replaces() {
        // Not upstream's: the replace needs a verified source. The target is
        // `confirmed` (the invariant), so an unverified target means recovery
        // falls back off it: the install is superseded, never run, and no
        // `install_pending` is left behind.
        let mut journal = idle("0.3.0");
        journal.installed = Some(Installed::from(&art("0.2.0")));
        pending(&mut journal, "0.3.0");
        let mut facts = facts_for(&journal);
        facts.versions.insert("0.3.0".into(), "d".repeat(64));
        facts.installed_sha256 = Some(art("0.2.0").sha256);
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.install, InstallAction::None);
        assert!(matches!(plan.run, RunChoice::VerifiedInstalled(_)));
        let next = written(&plan);
        assert!(next.install_pending.is_none());
        assert_eq!(
            plan.upserts[0].record.installed_binary.as_deref(),
            Some("superseded")
        );
    }

    #[test]
    fn no_recovery_leaves_a_pending_install_off_confirmed() {
        // The invariant over the ladder: whatever runs, `install_pending`
        // is absent or its target is the planned `confirmed`.
        for confirmed_ok in [true, false] {
            for previous_ok in [true, false] {
                for hash in [Some(art("0.2.0").sha256), Some(art("0.3.0").sha256), None] {
                    let mut journal = idle("0.3.0");
                    journal.previous = Some(art("0.2.0"));
                    journal.installed = Some(Installed::from(&art("0.2.0")));
                    pending(&mut journal, "0.3.0");
                    let mut facts = facts_for(&journal);
                    if !confirmed_ok {
                        facts.versions.insert("0.3.0".into(), "f".repeat(64));
                    }
                    if !previous_ok {
                        facts.versions.remove("0.2.0");
                    }
                    facts.installed_sha256 = hash.clone();
                    let plan = plan_recovery(&journal, &facts);
                    let after = plan.journal_write.as_ref().unwrap_or(&journal);
                    if let Some(left) = &after.install_pending {
                        assert_eq!(left.target.sha256, after.confirmed.sha256);
                        assert_eq!(left.target.version, after.confirmed.version);
                    }
                }
            }
        }
    }

    #[test]
    fn an_installed_file_nobody_accounted_for_is_adopted_only_when_idle() {
        // Not upstream's: 3.1 item 1 with T4's hash rules. Adopt when no
        // switch is open and no install is in flight.
        let journal = idle("0.2.0");
        let mut facts = facts_for(&journal);
        facts.installed_sha256 = Some("b".repeat(64));
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(
            plan.adopt,
            Some(Adopt {
                sha256: "b".repeat(64)
            })
        );
        assert_eq!(plan.run, RunChoice::Version(art("0.2.0")));

        // An open switch: not now.
        let mut open = idle("0.2.0");
        open.switch = Some(switch(How::Update, "0.2.0", "0.3.0", Phase::Loading));
        let mut facts = facts_for(&open);
        facts.installed_sha256 = Some("b".repeat(64));
        assert!(plan_recovery(&open, &facts).adopt.is_none());

        // The installed file equal to what runs is already known.
        let mut known = idle("0.2.0");
        known.installed = Some(Installed::from(&art("0.1.0")));
        let mut facts = facts_for(&known);
        facts.installed_sha256 = Some(art("0.2.0").sha256);
        assert!(plan_recovery(&known, &facts).adopt.is_none());

        // Adopting moves `confirmed` to `previous`.
        let mut adopted = idle("0.2.0");
        adopted.adopt(art("0.3.0"));
        assert_eq!(adopted.previous, Some(art("0.2.0")));
        assert_eq!(adopted.confirmed, art("0.3.0"));
        assert_eq!(adopted.installed, Some(Installed::from(&art("0.3.0"))));
    }

    // O2: the installed-file fallback when `versions/<v>/` can't take the copy.
    #[test]
    fn a_stage_conflict_runs_the_installed_file_with_switching_disabled() {
        let mut journal = idle("0.2.0");
        journal.installed = Some(Installed::from(&art("0.1.0")));
        pending(&mut journal, "0.2.0");
        let mut facts = facts_for(&journal);
        facts.versions.clear();
        facts.installed_sha256 = Some(art("0.2.0").sha256);
        facts.self_sha256 = facts.installed_sha256.clone();
        facts.self_version = "0.2.0".into();
        let plan = plan_recovery(&journal, &facts);

        // What the staging path would publish: a rebuilt journal that claims
        // the version, an outcome and a patch.
        assert!(matches!(plan.run, RunChoice::VerifiedInstalled(_)));
        assert!(plan.stage_installed.is_some());
        assert!(plan.journal_write.is_some());
        assert!(!plan.upserts.is_empty() && !plan.record_patches.is_empty());

        let dir = Path::new("versions/0.2.0");
        let fallback = plan.on_stage_conflict(dir).expect("staging was required");
        let RunChoice::InstalledSwitchingDisabled { artifact, reason } = &fallback.run else {
            panic!("{:?}", fallback.run);
        };
        assert_eq!(artifact.sha256, art("0.2.0").sha256);
        assert!(reason.contains("versions/0.2.0"), "{reason}");
        assert_eq!(
            fallback.status,
            PlanStatus::SwitchingDisabled(reason.clone())
        );
        // Nothing that rested on the copy survives: no rebuilt journal that
        // claims the version, no install, no adoption, no state writes.
        assert!(fallback.journal_write.is_none());
        assert!(fallback.stage_installed.is_none());
        assert_eq!(fallback.install, InstallAction::None);
        assert!(fallback.adopt.is_none());
        assert!(fallback.upserts.is_empty() && fallback.record_patches.is_empty());
        assert!(!fallback.first_start && !fallback.quarantine);
        assert!(fallback.keep_all_versions);
        assert!(
            fallback
                .notes
                .iter()
                .any(|note| note.contains("not published"))
        );
    }

    #[test]
    fn a_plan_that_needed_no_staging_has_no_conflict_fallback() {
        let journal = idle("0.2.0");
        let plan = plan_recovery(&journal, &facts_for(&journal));
        assert!(plan.stage_installed.is_none());
        let back = plan
            .clone()
            .on_stage_conflict(Path::new("versions/0.2.0"))
            .expect_err("nothing to fall back from");
        assert_eq!(*back, plan);
    }

    // O5 and T4: an outside install of what runs updates the baseline.
    #[test]
    fn an_idle_outside_install_of_the_running_version_reconciles_installed() {
        // J-R2 left `active = confirmed = B`, `installed = A`; the user then
        // installed B from outside.
        let mut journal = idle("0.3.0");
        journal.installed = Some(Installed::from(&art("0.2.0")));
        let mut facts = facts_for(&journal);
        facts.installed_sha256 = Some(art("0.3.0").sha256);
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.run, RunChoice::Version(art("0.3.0")));
        assert_eq!(plan.journal_write_name, "reconcile-installed");
        assert!(plan.adopt.is_none() && plan.stage_installed.is_none());
        let next = written(&plan).clone();
        assert_eq!(next.installed, Some(Installed::from(&art("0.3.0"))));
        assert_eq!(next.confirmed, art("0.3.0"));
        // Idempotent: nothing more to write once it landed.
        assert!(plan_recovery(&next, &facts).journal_write.is_none());

        // T4: the later B -> C update's install is classified against B, so
        // the file is replaced, not `changed-elsewhere`.
        let mut later = next;
        later.confirmed = art("0.4.0");
        later.previous = Some(art("0.3.0"));
        later.active = art("0.4.0");
        pending(&mut later, "0.4.0");
        let mut after = facts_for(&later);
        after.installed_sha256 = Some(art("0.3.0").sha256);
        let replay = plan_recovery(&later, &after);
        assert_eq!(
            replay.install,
            InstallAction::Replace {
                target: art("0.4.0"),
                aside: "open-ferry.old.123".into()
            }
        );
        assert!(replay.record_patches.is_empty());

        // The same facts without the reconcile are the false `Other`.
        let mut stale = later.clone();
        stale.installed = Some(Installed::from(&art("0.2.0")));
        let wrong = plan_recovery(&stale, &after);
        assert_eq!(
            wrong.record_patches[0].installed_binary,
            "changed-elsewhere"
        );
    }

    #[test]
    fn reconcile_waits_for_an_open_switch_or_a_pending_install() {
        let mut open = idle("0.3.0");
        open.installed = Some(Installed::from(&art("0.2.0")));
        open.switch = Some(switch(How::Update, "0.3.0", "0.4.0", Phase::Loading));
        let mut facts = facts_for(&open);
        facts.installed_sha256 = Some(art("0.3.0").sha256);
        assert_ne!(
            plan_recovery(&open, &facts).journal_write_name,
            "reconcile-installed"
        );

        let mut waiting = idle("0.3.0");
        waiting.installed = Some(Installed::from(&art("0.2.0")));
        pending(&mut waiting, "0.3.0");
        let mut facts = facts_for(&waiting);
        facts.installed_sha256 = Some(art("0.3.0").sha256);
        let plan = plan_recovery(&waiting, &facts);
        // The pending install's own case ("new") finishes it instead.
        assert_eq!(plan.record_patches[0].installed_binary, "replaced");
        assert_ne!(plan.journal_write_name, "reconcile-installed");
    }

    // Opus 11: `install_pending.target` is always `confirmed`.
    #[test]
    fn a_fallback_off_the_pending_target_supersedes_the_install() {
        // B confirmed, B's install pending, A is installed. B fails its hash
        // at start; recovery falls back to `previous` A.
        let mut journal = idle("0.3.0");
        journal.previous = Some(art("0.2.0"));
        journal.installed = Some(Installed::from(&art("0.2.0")));
        pending(&mut journal, "0.3.0");
        let mut facts = facts_for(&journal);
        facts.versions.insert("0.3.0".into(), "f".repeat(64));
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.run, RunChoice::Version(art("0.2.0")));
        let next = written(&plan);
        assert_eq!(next.confirmed, art("0.2.0"));
        assert!(next.install_pending.is_none());
        // No install of the fallback is queued: A is already installed.
        assert_eq!(plan.install, InstallAction::None);
        assert_eq!(next.installed, Some(Installed::from(&art("0.2.0"))));
        // Reported durably, in the same write, on the record's id.
        assert_eq!(next.outcomes.len(), 1);
        assert_eq!(next.outcomes[0].record.id, "s1");
        assert_eq!(
            next.outcomes[0].record.installed_binary.as_deref(),
            Some("superseded")
        );
        assert_eq!(plan.upserts, next.outcomes);
        assert_eq!(plan.record_patches[0].installed_binary, "superseded");
        // Reconcile doesn't fire alongside it.
        assert_ne!(plan.journal_write_name, "reconcile-installed");
        // A crash after the write replays the same report, once.
        assert_eq!(plan_recovery(next, &facts).upserts, plan.upserts);

        // The replace had already landed (the installed file is B): the
        // journal records the file as it is, so it is not adopted later.
        facts.installed_sha256 = Some(art("0.3.0").sha256);
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.run, RunChoice::Version(art("0.2.0")));
        let next = written(&plan);
        assert!(next.install_pending.is_none());
        assert_eq!(next.installed, Some(Installed::from(&art("0.3.0"))));
        assert!(plan.adopt.is_none());
        assert_eq!(plan.install, InstallAction::None);
        assert_eq!(
            next.outcomes[0].record.installed_binary.as_deref(),
            Some("superseded")
        );

        // A missing installed file is repaired from what was installed, not
        // by placing the failed target.
        facts.installed_sha256 = None;
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(
            plan.install,
            InstallAction::Restore {
                installed: Installed::from(&art("0.2.0"))
            }
        );
    }

    #[test]
    fn a_superseded_missing_repair_survives_replanning_the_proposed_journal() {
        // Not upstream's: U2-O1. The restore is derived from the journal and
        // the disk facts, so a crash after the clearing write, or a failed
        // restore, plans the same restore again.
        let mut journal = idle("0.3.0");
        journal.previous = Some(art("0.2.0"));
        journal.installed = Some(Installed::from(&art("0.2.0")));
        pending(&mut journal, "0.3.0");
        let mut facts = facts_for(&journal);
        facts.versions.insert("0.3.0".into(), "f".repeat(64));
        facts.installed_sha256 = None;
        let restore = InstallAction::Restore {
            installed: Installed::from(&art("0.2.0")),
        };
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.install, restore);
        let proposed = written(&plan).clone();
        assert!(proposed.install_pending.is_none());

        let again = plan_recovery(&proposed, &facts);
        assert_eq!(again.install, restore);
        assert!(again.journal_write.is_none(), "no further journal change");
        assert!(again.record_patches.is_empty());
    }

    #[test]
    fn a_missing_installed_file_with_no_pending_install_is_repaired_from_the_journal() {
        // Not upstream's: U2-O1's rule, no install pending.
        let journal = idle("0.2.0");
        let mut facts = facts_for(&journal);
        facts.installed_sha256 = None;

        // `journal.installed` verified: restore it; nothing else changes.
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(
            plan.install,
            InstallAction::Restore {
                installed: Installed::from(&art("0.2.0"))
            }
        );
        assert!(plan.journal_write.is_none() && plan.adopt.is_none());

        // Unverified: no action, a note, and the same Ok status the pending
        // branch keeps when it can't place anything.
        let mut older = idle("0.2.0");
        older.installed = Some(Installed::from(&art("0.1.0")));
        let mut facts = facts_for(&older);
        facts.installed_sha256 = None;
        let plan = plan_recovery(&older, &facts);
        assert_eq!(plan.install, InstallAction::None);
        assert!(matches!(plan.status, PlanStatus::Ok));
        assert!(
            plan.notes.iter().any(|note| note
                .contains(&facts.installed_path.display().to_string())
                && note.contains("is missing")
                && note.contains("0.1.0")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn the_missing_repair_is_returned_even_when_no_worker_can_run() {
        // Not upstream's: U2-O1 on the `Ran::Nothing` return. Neither
        // `confirmed` nor `previous` is available, and the installed file is
        // missing; only `journal.installed`'s copy may be verified.
        let mut journal = idle("0.3.0");
        journal.previous = Some(art("0.2.5"));
        journal.installed = Some(Installed::from(&art("0.2.0")));
        let mut facts = facts_for(&journal);
        facts.installed_sha256 = None;
        facts.versions.clear();

        // Verified copy of `installed` only: degraded, yet the repair is there.
        facts.versions.insert("0.2.0".into(), art("0.2.0").sha256);
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.run, RunChoice::Nothing);
        assert!(matches!(plan.status, PlanStatus::Degraded(_)));
        assert!(plan.journal_write.is_none());
        assert_eq!(
            plan.install,
            InstallAction::Restore {
                installed: Installed::from(&art("0.2.0"))
            }
        );

        // Unverified: still degraded, no action, a note naming the path.
        facts.versions.clear();
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.run, RunChoice::Nothing);
        assert!(matches!(plan.status, PlanStatus::Degraded(_)));
        assert_eq!(plan.install, InstallAction::None);
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains(&facts.installed_path.display().to_string()))
        );

        // No `installed` recorded at all: no source, and the note says so.
        journal.installed = None;
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(plan.install, InstallAction::None);
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("records no installed version"))
        );
    }

    #[test]
    fn a_present_installed_file_never_triggers_the_missing_repair() {
        // Not upstream's: U2-O1's rule needs a missing file; adoption is as
        // it was for any present hash.
        let journal = idle("0.2.0");
        for hash in [art("0.2.0").sha256, "b".repeat(64)] {
            let mut facts = facts_for(&journal);
            facts.installed_sha256 = Some(hash.clone());
            let plan = plan_recovery(&journal, &facts);
            assert_eq!(plan.install, InstallAction::None);
            assert_eq!(
                plan.adopt.is_some(),
                hash != art("0.2.0").sha256,
                "adoption unchanged for {hash}"
            );
        }
    }

    #[test]
    fn a_pending_install_stays_while_confirmed_is_its_target() {
        // The invariant's other side: nothing moved, so nothing is superseded.
        let mut journal = idle("0.3.0");
        journal.installed = Some(Installed::from(&art("0.2.0")));
        pending(&mut journal, "0.3.0");
        let mut facts = facts_for(&journal);
        facts.installed_sha256 = Some(art("0.2.0").sha256);
        let plan = plan_recovery(&journal, &facts);
        assert!(matches!(plan.install, InstallAction::Replace { .. }));
        assert!(plan.journal_write.is_none() && plan.record_patches.is_empty());
    }

    #[test]
    fn the_clock_is_the_facts_clock() {
        // Not upstream's: a fake clock decides every `at`.
        let mut journal = idle("0.2.0");
        journal.switch = Some(switch(How::Update, "0.2.0", "0.3.0", Phase::Loading));
        let mut facts = facts_for(&journal);
        facts.now = clock("2031-01-02T03:04:05Z");
        let plan = plan_recovery(&journal, &facts);
        assert_eq!(only_outcome(&plan).record.at, "2031-01-02T03:04:05Z");
    }

    #[test]
    fn planning_is_deterministic_and_leaves_the_journal_alone() {
        // Not upstream's: a crash in the middle of recovery repeats it, and
        // the plan changes nothing but its own copy.
        let mut journal = idle("0.2.0");
        journal.switch = Some(switch(How::Update, "0.2.0", "0.3.0", Phase::Watching));
        let before = journal.clone();
        let facts = facts_for(&journal);
        let a = plan_recovery(&journal, &facts);
        let b = plan_recovery(&journal, &facts);
        assert_eq!(a, b);
        assert_eq!(journal, before);
        // Planning the plan's own journal is a no-op apart from the replay.
        let after = plan_recovery(written(&a), &facts_for(written(&a)));
        assert!(after.journal_write.is_none());
        assert_eq!(after.upserts, a.upserts);
    }
}
