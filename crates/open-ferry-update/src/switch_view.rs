//! A read-only view shared by the CLI and the dashboard. The endpoint owns
//! the live picture; the journal and state carry durable switch outcomes.

use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::journal::Phase;
use crate::state::{State, SwitchRecord};
use crate::{SwitchError, UpdateService, Updater};

#[derive(Debug, Clone, Serialize)]
pub struct SwitchView {
    pub supervisor: Option<Value>,
    /// A short reason, without endpoint paths or transport details.
    pub unreachable: Option<String>,
    /// Local policy disables switching rather than failing a status read.
    #[serde(skip)]
    pub switching_disabled: bool,
    pub rollback_target: Option<String>,
    pub last: Option<LastSwitch>,
    /// Optional durable records could not be read; live status remains useful.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LastSwitch {
    pub how: String,
    /// Unknown unless the live snapshot identifies this operation and says.
    pub auto: Option<bool>,
    pub from: Option<String>,
    pub to: String,
    pub result: String,
    pub reason: Option<String>,
    pub at: Option<String>,
}

impl UpdateService {
    /// Read live status and durable outcomes without taking the update lock
    /// or writing records. An unavailable endpoint goes in `unreachable`;
    /// without an attached switcher this returns `NotSupervised`.
    pub async fn switch_view(&self) -> Result<SwitchView, SwitchError> {
        let switcher = self.switcher()?;
        let updater = self.inner.updater.clone();
        let records = tokio::task::spawn_blocking(move || records(&updater));
        let (live, records) = tokio::join!(
            tokio::time::timeout(
                Duration::from_millis(750),
                switcher.status(Instant::now() + Duration::from_millis(750)),
            ),
            tokio::time::timeout(Duration::from_secs(1), records),
        );
        let (rollback_target, last, notes, missing_endpoint) = match records {
            Ok(Ok(records)) => records,
            Ok(Err(_)) => (
                None,
                None,
                vec!["reading switch records stopped".into()],
                false,
            ),
            Err(_) => (
                None,
                None,
                vec!["reading switch records timed out".into()],
                false,
            ),
        };
        let (supervisor, unreachable, switching_disabled) =
            match live.unwrap_or(Err(SwitchError::TimedOut)) {
                Ok(snapshot) => (Some(snapshot), None, false),
                Err(error) => (
                    None,
                    Some(unreachable_reason(&error, missing_endpoint).into()),
                    matches!(error, SwitchError::Disabled(_)),
                ),
            };
        let last = last.map(|record| last_switch(record, supervisor.as_ref()));
        Ok(SwitchView {
            supervisor,
            unreachable,
            switching_disabled,
            rollback_target,
            last,
            notes,
        })
    }
}

fn unreachable_reason(error: &SwitchError, missing_endpoint: bool) -> &'static str {
    match error {
        SwitchError::Disabled(_) => "switching is disabled",
        SwitchError::TimedOut => "the supervisor did not answer in time",
        SwitchError::Untrusted(_) if cfg!(unix) && missing_endpoint => "no supervisor is running",
        SwitchError::Untrusted(_) => "the supervisor endpoint was not trusted",
        SwitchError::Protocol(_) => "the supervisor reply was not understood",
        SwitchError::Refused(_) => "the supervisor refused the status request",
        SwitchError::Stopped => "the supervisor stopped",
        _ => "the supervisor cannot be reached",
    }
}

fn records(updater: &Updater) -> (Option<String>, Option<SwitchRecord>, Vec<String>, bool) {
    let missing_endpoint = std::fs::symlink_metadata(updater.data.root().join("supervisor.json"))
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    let journal = crate::switching::pending_journal(updater);
    let mut notes = Vec::new();
    let state = State::load_strict(&updater.data.state_file()).unwrap_or_else(|_| {
        notes.push("the update state could not be read; its history is unavailable".into());
        State::default()
    });
    // Reuse the state merger in memory: pending terminal patches retain the
    // original operation's fields, and late older outcomes stay behind it.
    let mut merged = State::default();
    for record in state
        .last_switch
        .into_iter()
        .chain(state.history.into_iter().rev())
    {
        merged.upsert_record(record);
    }
    let mut rollback_target = None;
    if let Some(view) = journal
        && let Some(journal) = view.journal()
    {
        rollback_target = journal
            .switch
            .as_ref()
            .filter(|switch| switch.phase == Phase::Watching)
            .map(|switch| &switch.from)
            .or(journal.previous.as_ref())
            .map(|artifact| artifact.version.clone());
        for outcome in &journal.outcomes {
            merged.upsert_record(outcome.record.clone());
        }
    }
    (rollback_target, merged.last_switch, notes, missing_endpoint)
}

fn last_switch(record: SwitchRecord, snapshot: Option<&Value>) -> LastSwitch {
    let auto = snapshot
        .and_then(|snapshot| snapshot.get("switch"))
        .filter(|switch| !record.id.is_empty() && switch["id"].as_str() == Some(&record.id))
        .and_then(|switch| switch["auto"].as_bool());
    let at = DateTime::parse_from_rfc3339(&record.at).ok().map(|time| {
        time.with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true)
    });
    LastSwitch {
        result: record.result.unwrap_or_else(|| "unknown".into()),
        how: record.how,
        auto,
        from: (!record.from.is_empty()).then_some(record.from),
        to: record.to,
        reason: record.why,
        at,
    }
}
