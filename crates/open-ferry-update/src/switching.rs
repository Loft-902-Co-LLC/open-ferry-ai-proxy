//! The supervised switch contract. Preparing an artifact holds the update
//! lock; calling a switcher never does. The supervisor owns an accepted
//! switch, including after its caller drops the progress handle.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use serde_json::Value;

use crate::fetch::BoxFuture;
use crate::journal::{Artifact, JournalView, Phase};
use crate::pins::Pin;
use crate::state::{State, how, result};
use crate::{Report, UpdateError, Updater};

/// Why preparing, asking for, or following a supervised switch failed.
#[derive(Debug)]
pub enum SwitchError {
    NotSupervised,
    UpdatesOff,
    Update(UpdateError),
    /// A failed or rolled-back release is not retried.
    Skipped(Report),
    /// The request may have arrived. Read status before trying again.
    TimedOut,
    Unreachable(String),
    Untrusted(String),
    Protocol(String),
    Refused(String),
    /// The stream ended without a terminal reply.
    Stopped,
}

impl fmt::Display for SwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupervised => f.write_str("this install is not supervised"),
            Self::UpdatesOff => f.write_str("automatic updates are off"),
            Self::Update(error) => error.fmt(f),
            Self::Skipped(Report::SkippedFailed { version }) => {
                write!(f, "open-ferry {version} failed here and is skipped")
            }
            Self::Skipped(Report::SkippedRolledBack { version }) => {
                write!(f, "open-ferry {version} was rolled back and is skipped")
            }
            Self::Skipped(report) => write!(f, "the release is skipped: {report:?}"),
            Self::TimedOut => f.write_str(
                "the supervisor did not answer in time; check its status before trying again",
            ),
            Self::Unreachable(why) => write!(f, "the supervisor cannot be reached: {why}"),
            Self::Untrusted(why) => write!(f, "the supervisor endpoint was refused: {why}"),
            Self::Protocol(why) => write!(f, "the supervisor reply was not understood: {why}"),
            Self::Refused(why) => write!(f, "the supervisor refused the request: {why}"),
            Self::Stopped => f.write_str("the supervisor stopped before the switch finished"),
        }
    }
}

impl std::error::Error for SwitchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Update(error) => Some(error),
            _ => None,
        }
    }
}

impl From<UpdateError> for SwitchError {
    fn from(error: UpdateError) -> Self {
        Self::Update(error)
    }
}

/// A reply after acceptance. `Done` is the endpoint's result, which can
/// describe a failed switch; it does not promise a confirmed watch window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchEvent {
    Progress { phase: String },
    Done { result: Value },
}

/// Whether cancellation was accepted, missed its window, or found no switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    Cancelled,
    TooLate,
    Nothing,
}

/// The progress transport. Implementations preserve a partial frame when a
/// deadline expires, and report EOF as `SwitchError::Stopped`.
pub trait SwitchProgress: Send {
    fn next(&mut self, deadline: Instant) -> BoxFuture<'_, Result<SwitchEvent, SwitchError>>;
}

/// An accepted switch. Dropping it closes the stream and never cancels.
pub struct SwitchHandle {
    pub id: String,
    progress: Box<dyn SwitchProgress>,
    finished: bool,
}

impl fmt::Debug for SwitchHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SwitchHandle")
            .field("id", &self.id)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl SwitchHandle {
    pub fn new(id: String, progress: Box<dyn SwitchProgress>) -> Self {
        Self {
            id,
            progress,
            finished: false,
        }
    }

    /// The next progress or terminal reply, or `None` after the terminal
    /// reply. A timeout leaves the handle open for another call.
    pub async fn next(&mut self, deadline: Instant) -> Result<Option<SwitchEvent>, SwitchError> {
        if self.finished {
            return Ok(None);
        }
        let event = self.progress.next(deadline).await;
        self.finished = !matches!(
            &event,
            Ok(SwitchEvent::Progress { .. }) | Err(SwitchError::TimedOut)
        );
        event.map(Some)
    }
}

/// Local supervisor access, implemented by the supervisor crate. All methods
/// have one absolute deadline. No method may be called under the update lock.
/// Status is the complete endpoint snapshot as JSON, without a dependency
/// from this crate back to the supervisor crate.
pub trait Switcher: Send + Sync {
    /// The full endpoint snapshot; no journal or update-lock probe.
    fn status(&self, deadline: Instant) -> BoxFuture<'_, Result<Value, SwitchError>>;
    /// Request an update with an already durable, live staged pin.
    fn update<'a>(
        &'a self,
        pin: &'a Pin,
        auto: bool,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>>;
    /// Request the validated journal target, without a pin.
    fn rollback<'a>(
        &'a self,
        target: &'a Artifact,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<SwitchHandle, SwitchError>>;
    fn cancel(&self, deadline: Instant) -> BoxFuture<'_, Result<CancelOutcome, SwitchError>>;
}

/// Read fresh state without replaying supervisor-owned outcomes onto disk.
pub(crate) fn read_state(updater: &Updater) -> Result<State, UpdateError> {
    State::load_strict(&updater.data.state_file()).map_err(|error| UpdateError::Io {
        what: "read the update state",
        error: error.to_string(),
    })
}

/// Pending exclusions supplement durable state without replacing its newer
/// fields or replaying old install patches. Only the supervisor writes outcomes.
#[derive(Default)]
pub(crate) struct Exclusions {
    failed: Vec<String>,
    rolled_back: Vec<String>,
}

impl Exclusions {
    pub(crate) fn report(&self, state: &State, version: &str) -> Option<Report> {
        if state.has_failed(version) || self.failed.iter().any(|failed| failed == version) {
            Some(Report::SkippedFailed {
                version: version.into(),
            })
        } else if state.rolled_back.as_deref() == Some(version)
            || self
                .rolled_back
                .iter()
                .any(|rolled_back| rolled_back == version)
        {
            Some(Report::SkippedRolledBack {
                version: version.into(),
            })
        } else {
            None
        }
    }
}

/// Optional outcome information must not turn a readable state file into a
/// failed check. Mutating stage/pin retention checks still fail closed.
pub(crate) fn pending_journal(updater: &Updater) -> Option<JournalView> {
    match journal(updater) {
        Ok(view) if view.journal().is_some() => Some(view),
        other => {
            static WARNED: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
            if WARNED
                .get_or_init(Mutex::default)
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(updater.data.root().to_owned())
            {
                let reason = match other {
                    Err(error) => error.to_string(),
                    Ok(_) => "the switch journal is missing".into(),
                };
                tracing::warn!(%reason, "pending switch outcomes could not be read; using the update state's retry exclusions");
            }
            None
        }
    }
}

pub(crate) fn pending_exclusions(updater: &Updater) -> Exclusions {
    let mut exclusions = Exclusions::default();
    if let Some(view) = pending_journal(updater)
        && let Some(journal) = view.journal()
    {
        for outcome in &journal.outcomes {
            if let Some(failed) = &outcome.mark_failed {
                exclusions.failed.push(failed.version.clone());
            }
            let record = &outcome.record;
            if record.how == how::ROLLBACK
                && record.result.as_deref() == Some(result::ROLLED_BACK)
                && !record.from.is_empty()
            {
                exclusions.rolled_back.push(record.from.clone());
            }
        }
    }
    exclusions
}

pub(crate) fn journal(updater: &Updater) -> Result<JournalView, UpdateError> {
    let view = JournalView::load(&updater.data.root().join(crate::journal::JOURNAL_FILE))
        .map_err(|error| UpdateError::Journal(error.to_string()))?;
    view.ensure_supported()
        .map_err(|error| UpdateError::Journal(error.to_string()))?;
    Ok(view)
}

pub(crate) fn skipped(updater: &Updater, state: &State, version: &str) -> Result<(), SwitchError> {
    if let Some(report) = pending_exclusions(updater).report(state, version) {
        return Err(SwitchError::Skipped(report));
    }
    Ok(())
}

/// Read only the supervisor journal's rollback target under the update lock.
/// The endpoint revalidates it against its live view when the request arrives.
pub(crate) fn rollback_target(updater: &Updater) -> Result<Artifact, UpdateError> {
    let view = journal(updater)?;
    let journal = view.journal().ok_or(UpdateError::NoPrevious)?;
    journal
        .switch
        .as_ref()
        .filter(|switch| switch.phase == Phase::Watching)
        .map(|switch| switch.from.clone())
        .or_else(|| journal.previous.clone())
        .ok_or(UpdateError::NoPrevious)
}
