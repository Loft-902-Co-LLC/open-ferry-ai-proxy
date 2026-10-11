//! The ladders a worker is started from (6.1 g), as pure data.
//!
//! A rung is a location (a kept copy `versions/<v>/` or the installed path)
//! and the artifact run from it. Progress is tied to a rung, never to an index
//! of a list: the list a ladder is walked over can change between attempts (a
//! refused location leaves it, the journal moves), and a count that was earned
//! on one rung must not be spent on another.

use crate::startup::{Config, Run};
use open_ferry_update::journal::{Artifact, Journal, LadderRecord, Return, Rung};

/// Failed starts a rung is given before the ladder moves on.
pub const ATTEMPTS: u32 = 3;

/// `run` as a journalled rung: the installed file by location, never by bytes.
pub fn rung_of(cfg: &Config, run: &Run) -> Rung {
    Rung {
        installed: run.binary == cfg.installed,
        artifact: run.artifact.clone(),
    }
}

/// The run a journalled rung stands for.
pub fn run_of(cfg: &Config, rung: &Rung) -> Run {
    let binary = if rung.installed {
        cfg.installed.clone()
    } else {
        cfg.data.binary(&rung.artifact.version, &cfg.binary_name)
    };
    Run {
        binary,
        artifact: rung.artifact.clone(),
    }
}

/// The record of `progress` with one more start on `run` (its attempts
/// counted before it is spawned, so a crash while it runs counts it; a clean
/// service stop while the worker is still loading takes the try back,
/// `Core::stop`).
pub fn record_start(
    cfg: &Config,
    switch: Option<&str>,
    progress: &Progress,
    run: &Run,
) -> LadderRecord {
    LadderRecord {
        switch: switch.map(str::to_owned),
        current: Some(rung_of(cfg, run)),
        attempts: progress.attempts_on(run).saturating_add(1),
        spent: progress
            .spent
            .iter()
            .map(|spent| rung_of(cfg, spent))
            .collect(),
    }
}

/// The record of a worker that reported `loaded`: its rung with nothing
/// against it, and the rungs this cycle has spent.
pub fn record_loaded(cfg: &Config, run: &Run, spent: &[Run]) -> LadderRecord {
    LadderRecord {
        switch: None,
        current: Some(rung_of(cfg, run)),
        attempts: 0,
        spent: spent.iter().map(|rung| rung_of(cfg, rung)).collect(),
    }
}

/// The ladder a journal is on: the return of the open switch when it has
/// decided one, otherwise the restart ladder (`None`).
pub fn ladder_of(journal: &Journal) -> Option<&str> {
    journal
        .switch
        .as_ref()
        .filter(|switch| switch.return_.is_some())
        .map(|switch| switch.id.as_str())
}

/// The one check of a ladder record against the ladder it is used on: its key
/// is this ladder's, and every rung it names (the current one and each spent
/// one) is on `full`. A record that fails is ignored with one log line; the
/// next count is a fresh record. Start-up's count, the core's start-up
/// runtime and the core's restarts all use this one.
pub fn usable_record<'a>(
    cfg: &Config,
    journal: &'a Journal,
    full: &[Run],
) -> Option<&'a LadderRecord> {
    let record = journal.ladder.as_ref()?;
    let ladder = ladder_of(journal);
    if record.switch.as_deref() != ladder {
        tracing::warn!(
            "the journalled ladder record is for {:?}, not for this ladder ({ladder:?}): ignored",
            record.switch
        );
        return None;
    }
    let known = |rung: &Rung| {
        let run = run_of(cfg, rung);
        full.iter().any(|held| same_rung(held, &run))
    };
    if record.current.iter().chain(record.spent.iter()).all(known) {
        return Some(record);
    }
    tracing::warn!("the journalled ladder record names a rung that is not on the ladder: ignored");
    None
}

/// Whether every rung `progress` names is on `full`. The core's restarts hold
/// their progress in memory; this is the rung half of [`usable_record`] for it.
pub fn all_known(progress: &Progress, full: &[Run]) -> bool {
    progress
        .current
        .iter()
        .map(|(run, _)| run)
        .chain(progress.spent.iter())
        .all(|run| full.iter().any(|held| same_rung(held, run)))
}

/// The rungs of the restart ladder, in order: the run it had (`active`),
/// `confirmed`, `previous` (kept copies, by location), then the installed
/// file by hash (`installed`, when it is a version that is known). The one
/// location a record may add is the installed path: a rung `known` has at
/// `cfg.installed` that is not among them (the installed file a return started,
/// whose bytes are one of the kept artifacts) goes in before the kept copy it
/// matches, at its own location, and only when the installed file's bytes,
/// measured now (`installed_sha256`), are that rung's artifact. It is never
/// mapped onto the kept copy, and no other location a record names can put
/// itself on the list: a record that names one is not usable.
pub fn restart_list(
    cfg: &Config,
    journal: &Journal,
    installed: Option<Run>,
    installed_sha256: Option<&str>,
    known: &Progress,
) -> Vec<Run> {
    let mut list: Vec<Run> = Vec::new();
    for artifact in [
        Some(&journal.active),
        Some(&journal.confirmed),
        journal.previous.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let run = Run {
            binary: cfg.data.binary(&artifact.version, &cfg.binary_name),
            artifact: artifact.clone(),
        };
        if !list.iter().any(|held| held.binary == run.binary) {
            list.push(run);
        }
    }
    if let Some(run) = installed
        && !list.iter().any(|held| held.binary == run.binary)
    {
        list.push(run);
    }
    let named = known
        .current
        .iter()
        .map(|(run, _)| run)
        .chain(known.spent.iter());
    for run in named {
        if list.iter().any(|held| same_rung(held, run)) {
            continue;
        }
        let measured = installed_sha256
            .is_some_and(|sha256| sha256.eq_ignore_ascii_case(&run.artifact.sha256));
        if run.binary != cfg.installed || !measured {
            continue;
        }
        if let Some(at) = list.iter().position(|held| {
            held.artifact
                .sha256
                .eq_ignore_ascii_case(&run.artifact.sha256)
        }) {
            list.insert(at, run.clone());
        }
    }
    list
}

/// The rungs a return may run, in order: the kept copy of `initial` (the
/// switch's `from`), then `previous` (hash-checked at spawn), then the
/// installed file by hash (`installed`, never the version that failed, `to`).
/// The rung the journal says the return stands on (`held`) is always on the
/// list, at its place: a kept copy goes before the installed rung, the
/// installed file goes last. Without it a spent-looking ladder would start a
/// new cycle at the rung that just failed.
pub fn return_list(
    cfg: &Config,
    initial: &Artifact,
    previous: Option<&Artifact>,
    to: &Artifact,
    held: &Return,
    installed: Option<Run>,
) -> Vec<Run> {
    let kept = |artifact: &Artifact| Run {
        binary: cfg.data.binary(&artifact.version, &cfg.binary_name),
        artifact: artifact.clone(),
    };
    let mut candidates = vec![kept(initial)];
    if let Some(previous) = previous
        && previous != to
        && previous.sha256 != to.sha256
        && !candidates.iter().any(|run| &run.artifact == previous)
    {
        candidates.push(kept(previous));
    }
    if let Some(run) = installed.filter(|run| !run.artifact.sha256.eq_ignore_ascii_case(&to.sha256))
    {
        candidates.push(run);
    }
    let held_run = if held.installed {
        Run {
            binary: cfg.installed.clone(),
            artifact: held.target.clone(),
        }
    } else {
        kept(&held.target)
    };
    if !candidates.iter().any(|run| same_rung(run, &held_run)) {
        if held.installed {
            candidates.push(held_run);
        } else {
            let at = candidates
                .iter()
                .position(|run| run.binary == cfg.installed)
                .unwrap_or(candidates.len());
            candidates.insert(at, held_run);
        }
    }
    candidates
}

/// The list of the ladder `journal` is on: the return's when it has decided
/// one, otherwise the restart ladder's.
pub fn list_of(
    cfg: &Config,
    journal: &Journal,
    installed: Option<Run>,
    installed_sha256: Option<&str>,
    known: &Progress,
) -> Vec<Run> {
    match journal
        .switch
        .as_ref()
        .and_then(|switch| switch.return_.as_ref().map(|ret| (switch, ret)))
    {
        Some((switch, ret)) => return_list(
            cfg,
            &switch.from,
            journal.previous.as_ref(),
            &switch.to,
            ret,
            installed,
        ),
        None => restart_list(cfg, journal, installed, installed_sha256, known),
    }
}

/// J-R2: a return ends and `ran` is the rung that started. Its ladder record
/// becomes the restart ladder's instead of being cleared: the same rung at the
/// location it ran from, with the tries held (the in-flight one counted), and
/// the spent rungs that are on the restart ladder. It is not a new cycle, so
/// a restart that follows goes on from it. `journal` is the journal after
/// J-R2's changes and `held` is the record the return held. `None` when no
/// record is held for this return.
pub fn restart_record_after_return(
    cfg: &Config,
    journal: &Journal,
    switch_id: &str,
    held: Option<&LadderRecord>,
    ran: &Run,
    installed: Option<Run>,
    installed_sha256: Option<&str>,
) -> Option<LadderRecord> {
    let held = held.filter(|held| held.switch.as_deref() == Some(switch_id))?;
    let attempts = match &held.current {
        Some(rung) if same_rung(&run_of(cfg, rung), ran) => held.attempts.max(1),
        _ => 1,
    };
    let spent: Vec<Run> = held.spent.iter().map(|rung| run_of(cfg, rung)).collect();
    let on = Progress {
        current: Some((ran.clone(), 0)),
        spent: spent.clone(),
    };
    let list = restart_list(cfg, journal, installed, installed_sha256, &on);
    Some(LadderRecord {
        switch: None,
        current: Some(rung_of(cfg, ran)),
        attempts,
        spent: spent
            .iter()
            .filter(|run| list.iter().any(|held| same_rung(held, run)))
            .map(|run| rung_of(cfg, run))
            .collect(),
    })
}

/// The progress a record holds, as failed starts: a try the record counts is
/// one that did not end well. A rung with all its tries counted is spent.
pub fn progress_of(cfg: &Config, record: &LadderRecord) -> Progress {
    let mut progress = Progress {
        current: None,
        spent: record.spent.iter().map(|rung| run_of(cfg, rung)).collect(),
    };
    if let Some(rung) = &record.current {
        let run = run_of(cfg, rung);
        if record.attempts >= ATTEMPTS {
            progress.spent.push(run);
        } else {
            progress.current = Some((run, record.attempts));
        }
    }
    progress
}

/// Whether two runs are the same rung: the same location holding the same
/// bytes. Never by bytes alone: a kept copy and the installed file may hold
/// the same artifact and are two rungs.
pub fn same_rung(a: &Run, b: &Run) -> bool {
    a.binary == b.binary && a.artifact.sha256.eq_ignore_ascii_case(&b.artifact.sha256)
}

/// Where a ladder stands: the rung being tried with its failed attempts, and
/// the rungs this cycle has used up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub current: Option<(Run, u32)>,
    pub spent: Vec<Run>,
}

impl Progress {
    /// Progress on `run` with `attempts` failed starts.
    pub fn on(run: Run, attempts: u32) -> Self {
        Self {
            current: Some((run, attempts)),
            spent: Vec::new(),
        }
    }

    /// The failed attempts `run` has already had.
    pub fn attempts_on(&self, run: &Run) -> u32 {
        match &self.current {
            Some((current, attempts)) if same_rung(current, run) => *attempts,
            _ => 0,
        }
    }

    /// A start of `run` failed after `before` earlier failures on it: the
    /// next try is on the same rung, or on the next one once it is spent.
    pub fn failed(&mut self, run: &Run, before: u32) {
        let attempts = before.saturating_add(1);
        if attempts >= ATTEMPTS {
            self.spent.push(run.clone());
            self.current = None;
        } else {
            self.current = Some((run.clone(), attempts));
        }
    }

    /// Where `run` stands when it started and did not fail: tried again from
    /// its own rung, with nothing counted against it.
    pub fn loaded(&mut self, run: &Run) {
        self.current = Some((run.clone(), 0));
    }

    /// The rung to try next over the whole `full` list (refused rungs
    /// included, so a refused rung is known to be a rung that was passed), with
    /// the failed attempts it has had; `None` when the ladder is spent.
    pub fn pick(&self, full: &[Run], permitted: impl Fn(&Run) -> bool) -> Option<(Run, u32)> {
        let place = |run: &Run| full.iter().position(|rung| same_rung(rung, run));
        let from = match &self.current {
            Some((run, attempts)) => {
                // A rung that is no longer on the list (its file changed) has
                // no place to go on from: the ladder is spent, and a new cycle
                // starts again from the first rung.
                let at = place(run)?;
                if permitted(run) {
                    return Some((full[at].clone(), *attempts));
                }
                // A refused rung is off the ladder; the next gets all its tries.
                at + 1
            }
            None => self
                .spent
                .iter()
                .filter_map(place)
                .max()
                .map_or(0, |at| at + 1),
        };
        full.iter()
            .skip(from)
            .find(|run| permitted(run) && !self.spent.iter().any(|spent| same_rung(spent, run)))
            .map(|run| (run.clone(), 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_ferry_update::journal::Artifact;
    use std::path::PathBuf;

    fn rung(version: &str, sha: &str, at: &str) -> Run {
        Run {
            binary: PathBuf::from(at),
            artifact: Artifact {
                version: version.to_owned(),
                sha256: sha.to_owned(),
                size: 1,
            },
        }
    }

    fn all(_: &Run) -> bool {
        true
    }

    #[test]
    fn three_failures_spend_a_rung_and_the_next_starts_fresh() {
        let (a, b) = (rung("1", "aa", "/k/1"), rung("0", "bb", "/k/0"));
        let full = vec![a.clone(), b.clone()];
        let mut progress = Progress::default();
        let (mut run, mut before) = progress.pick(&full, all).unwrap();
        assert_eq!((&run, before), (&a, 0));
        for tries in 1..=3 {
            progress.failed(&run, before);
            let next = progress.pick(&full, all).unwrap();
            if tries < 3 {
                assert_eq!(next, (a.clone(), tries));
            } else {
                assert_eq!(next, (b.clone(), 0));
            }
            (run, before) = next;
        }
    }

    #[test]
    fn a_rung_that_is_spent_everywhere_is_none() {
        let a = rung("1", "aa", "/k/1");
        let mut progress = Progress::default();
        for before in 0..3 {
            progress.failed(&a, before);
        }
        assert_eq!(progress.pick(std::slice::from_ref(&a), all), None);
    }

    #[test]
    fn a_refused_rung_is_passed_and_the_next_gets_every_attempt() {
        let (a, b) = (rung("1", "aa", "/k/1"), rung("0", "bb", "/k/0"));
        let full = vec![a.clone(), b.clone()];
        let progress = Progress::on(a.clone(), 2);
        let not_a = |run: &Run| !same_rung(run, &a);
        assert_eq!(progress.pick(&full, not_a), Some((b, 0)));
    }

    #[test]
    fn a_count_is_not_carried_to_another_list() {
        // Progress on `b` stays on `b` even when `a` leaves the list.
        let (a, b) = (rung("1", "aa", "/k/1"), rung("0", "bb", "/k/0"));
        let progress = Progress::on(b.clone(), 2);
        assert_eq!(progress.pick(&[a, b.clone()], all), Some((b.clone(), 2)));
        assert_eq!(progress.pick(std::slice::from_ref(&b), all), Some((b, 2)));
    }

    #[test]
    fn a_rung_that_left_the_list_is_spent_and_not_found_again_at_the_top() {
        let (a, b) = (rung("1", "aa", "/k/1"), rung("0", "bb", "/k/0"));
        let gone = rung("2", "cc", "/k/2");
        let progress = Progress::on(gone, 1);
        assert_eq!(progress.pick(&[a, b], all), None);
    }

    #[test]
    fn nothing_moves_back_within_a_cycle() {
        let (a, b, c) = (
            rung("2", "aa", "/k/2"),
            rung("1", "bb", "/k/1"),
            rung("0", "cc", "/k/0"),
        );
        let mut progress = Progress::default();
        for before in 0..3 {
            progress.failed(&b, before);
        }
        // `a` was never tried in this cycle but sits above the spent `b`.
        assert_eq!(progress.pick(&[a, b, c.clone()], all), Some((c, 0)));
    }

    #[test]
    fn a_kept_copy_and_the_installed_file_with_the_same_bytes_are_two_rungs() {
        let kept = rung("1", "aa", "/k/1");
        let installed = rung("1", "aa", "/bin/ferry");
        assert!(!same_rung(&kept, &installed));
        let full = vec![kept.clone(), installed.clone()];
        let mut progress = Progress::default();
        for before in 0..3 {
            progress.failed(&kept, before);
        }
        assert_eq!(progress.pick(&full, all), Some((installed, 0)));
    }

    #[test]
    fn a_new_cycle_starts_from_the_first_permitted_rung() {
        let (a, b) = (rung("1", "aa", "/k/1"), rung("0", "bb", "/k/0"));
        let full = vec![a.clone(), b.clone()];
        let not_a = |run: &Run| !same_rung(run, &a);
        assert_eq!(Progress::default().pick(&full, not_a), Some((b, 0)));
        assert_eq!(Progress::default().pick(&full, all), Some((a, 0)));
    }
}
