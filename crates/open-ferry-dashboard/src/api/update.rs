//! open-ferry's own updates (see `docs/updates.md`).
//!
//! - `GET /update`: what updates are doing: the mode and what set it, the
//!   running, installed, latest, staged and previous versions, the last
//!   check and its result, the next, and whether this install updates
//!   itself and why not.
//! - `POST /update/check`: starts a check now, in the background, as the
//!   server's own checks run; `GET /update` shows how it went. While
//!   updates are off nothing is checked, and the answer says so.
//!
//! - `POST /update/install` and `POST /update/rollback`: prepare in a shared
//!   background task and answer at acceptance, or after about 900 ms.
//!
//! All answer `updates_unavailable` when the server runs no update checks,
//! as a TUI's own server doesn't.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::Response;
use futures_util::FutureExt;
use http::StatusCode;
use open_ferry_update::{CheckNow, SwitchError, SwitchHandle, UpdateError, UpdateService};
use serde::Serialize;
use serde_json::{Value, json as value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::{ApiError, json, ok};
use crate::DashboardState;

/// The server's update checks, or the error saying there are none.
fn updates(state: &DashboardState) -> Result<&UpdateService, ApiError> {
    state.management.updates().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "updates_unavailable",
            "this server doesn't check for updates",
        )
    })
}

/// `GET /update`.
pub(crate) async fn status(State(state): State<DashboardState>) -> Result<Response, ApiError> {
    let updates = updates(&state)?;
    let status = updates.status();
    let read_started = Instant::now();
    let view = updates.switch_view().await;
    let mut live = state
        .update_requests
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    live.recover_finished_task();
    let supervisor = match view {
        Err(SwitchError::NotSupervised) => Value::Null,
        view => {
            let (snapshot, error, rollback_target, last) = match view {
                Ok(view) => {
                    if let Some(snapshot) = &view.supervisor {
                        live.supervisor = Some(snapshot.clone());
                        live.snapshot_at = Some(Instant::now());
                        live.failed_reads = 0;
                    }
                    // Admission publishes the live switch before returning its id.
                    // Only a read begun after acceptance can clear the pending flag.
                    if let Some(request) = live.request.as_mut()
                        && view.supervisor.is_some()
                        && read_started >= request.finished_at
                        && matches!(request.state, "unknown" | "accepted")
                    {
                        request.pending = false;
                    }
                    (
                        view.supervisor
                            .or_else(|| live.supervisor.clone())
                            .unwrap_or(Value::Null),
                        view.unreachable,
                        view.rollback_target,
                        value!(view.last),
                    )
                }
                Err(error) => {
                    tracing::warn!("dashboard API: read update switch records: {error}");
                    (
                        live.supervisor.clone().unwrap_or(Value::Null),
                        Some("the update records could not be read".to_owned()),
                        None,
                        Value::Null,
                    )
                }
            };
            let stale = error.is_some();
            if stale {
                live.failed_reads = live.failed_reads.saturating_add(1);
            }
            let snapshot_age_seconds = live.snapshot_at.map(|at| at.elapsed().as_secs_f64());
            let switching = &snapshot["switch"];
            let phase = switching["status"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| switching["phase"].as_str())
                .unwrap_or_else(|| snapshot["state"].as_str().unwrap_or("unknown"));
            value!({
                "form": "supervised",
                "phase": phase,
                "running_version": snapshot["active_version"].as_str().unwrap_or(&status.running_version),
                "rollback_target": rollback_target,
                "watch_until": switching["watch_until"],
                "switch": switching,
                "last_switch": last,
                "switching_disabled": snapshot["switching_disabled"],
                "stale": stale,
                "snapshot_age_seconds": snapshot_age_seconds,
                "failed_reads": live.failed_reads,
                "error": error,
            })
        }
    };
    let mut body = value!(status);
    body["supervisor"] = supervisor;
    body["switch_request"] = value!(live.request);
    Ok(ok(&body))
}

/// `POST /update/check`'s answer.
#[derive(Debug, Serialize)]
struct Started {
    /// `started`, or `running` when a check was running already.
    check: &'static str,
}

/// `POST /update/check`.
pub(crate) async fn check(State(state): State<DashboardState>) -> Result<Response, ApiError> {
    let check = match updates(&state)?.check_now() {
        CheckNow::Started => "started",
        CheckNow::Running => "running",
        CheckNow::Off => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "updates_off",
                "updates are off (self-update.mode or OPEN_FERRY_SELF_UPDATE), so open-ferry makes no update request; run `open-ferry update -check` to check by hand",
            ));
        }
    };
    Ok(json(StatusCode::ACCEPTED, &Started { check }))
}

/// Shared by router clones; a second request cannot start another preparation.
#[derive(Clone, Default)]
pub(crate) struct Requests(Arc<Mutex<Live>>);

#[derive(Default)]
struct Live {
    request: Option<SwitchRequest>,
    task: Option<JoinHandle<()>>,
    supervisor: Option<Value>,
    snapshot_at: Option<Instant>,
    failed_reads: u32,
}

impl Live {
    fn recover_finished_task(&mut self) {
        if self.task.as_ref().is_some_and(JoinHandle::is_finished) {
            self.task = None;
            if let Some(request) = &self.request
                && request.state == "preparing"
            {
                tracing::warn!("dashboard API: switch preparation task ended without a result");
                let mut stopped = request.clone();
                stopped.finished_at = Instant::now();
                stopped.state = "failed";
                stopped.pending = false;
                stopped.reason = Some("preparing the switch stopped".to_owned());
                self.request = Some(stopped);
            }
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct SwitchRequest {
    request_id: String,
    action: &'static str,
    state: &'static str,
    id: Option<String>,
    reason: Option<String>,
    pending: bool,
    #[serde(skip)]
    finished_at: Instant,
    #[serde(skip)]
    code: &'static str,
}

impl SwitchRequest {
    fn preparing(action: &'static str) -> Self {
        Self {
            request_id: format!("{:032x}", rand::random::<u128>()),
            action,
            state: "preparing",
            id: None,
            reason: None,
            pending: true,
            finished_at: Instant::now(),
            code: "switch_failed",
        }
    }

    fn finish(mut self, result: Result<SwitchHandle, SwitchError>) -> Self {
        let request = &mut self;
        request.finished_at = Instant::now();
        let action = request.action;
        match result {
            Ok(handle) => {
                request.state = "accepted";
                request.id = Some(handle.id.clone());
                // The supervisor owns the operation; disconnecting does not cancel it.
                drop(handle);
            }
            Err(error) => {
                tracing::warn!("dashboard API: prepare {action} switch: {error}");
                request.pending = false;
                request.state = if matches!(error, SwitchError::Refused(_)) {
                    "refused"
                } else if matches!(
                    error,
                    SwitchError::TimedOut | SwitchError::Stopped | SwitchError::Protocol(_)
                ) {
                    "unknown"
                } else {
                    "failed"
                };
                request.pending = request.state == "unknown";
                request.code = if matches!(error, SwitchError::NotSupervised) {
                    "not_supervised"
                } else if matches!(error, SwitchError::Update(UpdateError::Busy)) {
                    "update_busy"
                } else if matches!(error, SwitchError::Ended(_)) {
                    "switch_ended"
                } else if matches!(error, SwitchError::Refused(_)) {
                    "switch_refused"
                } else {
                    "switch_failed"
                };
                request.reason = Some(match error {
                    SwitchError::NotSupervised => "this install is not supervised; run `open-ferry update` (or `open-ferry update -rollback`) on the server's computer, then restart the server".to_owned(),
                    SwitchError::TimedOut => "the supervisor did not answer in time; the request may have been accepted; read update status before trying again".to_owned(),
                    SwitchError::Stopped => "the supervisor connection ended without an answer; the outcome is unknown; read update status before trying again".to_owned(),
                    SwitchError::Update(UpdateError::Busy) => "busy, try again shortly".to_owned(),
                    SwitchError::Ended(reason) | SwitchError::Refused(reason) => reason,
                    SwitchError::Untrusted(_) => "the supervisor endpoint was not trusted".to_owned(),
                    SwitchError::Unreachable(_) => "the supervisor cannot be reached; the request was not sent".to_owned(),
                    SwitchError::Protocol(_) => "the supervisor reply was not understood; read update status before trying again".to_owned(),
                    SwitchError::Update(_) => "the release could not be prepared; check the server log".to_owned(),
                    SwitchError::UpdatesOff => "automatic updates are off".to_owned(),
                    SwitchError::Skipped(_) => "the release was skipped".to_owned(),
                    SwitchError::Disabled(_) => "switching is disabled".to_owned(),
                });
            }
        }
        self
    }

    fn response(&self) -> Result<Response, ApiError> {
        match self.state {
            "preparing" => Ok(json(StatusCode::ACCEPTED, &value!({"preparing": true}))),
            "accepted" => Ok(json(
                StatusCode::ACCEPTED,
                &value!({"accepted": true, "id": self.id}),
            )),
            "unknown" => Ok(json(StatusCode::ACCEPTED, &value!({"uncertain": true}))),
            // Defensive: current install/rollback clients return at Accepted.
            // Later failures are read from supervisor.last_switch.
            "failed" if self.code == "switch_ended" => Ok(json(
                StatusCode::ACCEPTED,
                &value!({"failed": true, "reason": self.reason}),
            )),
            _ => Err(ApiError::new(
                StatusCode::CONFLICT,
                self.code,
                self.reason
                    .as_deref()
                    .unwrap_or("the switch could not start"),
            )),
        }
    }
}

impl Requests {
    #[cfg(test)]
    pub(crate) async fn ended_preparation_for_test(&self, action: &'static str) {
        let task = tokio::spawn(async {});
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut live = self.0.lock().unwrap();
        live.request = Some(SwitchRequest::preparing(action));
        live.task = Some(task);
    }

    async fn start(
        &self,
        updates: UpdateService,
        action: &'static str,
    ) -> Result<Response, ApiError> {
        let (send, receive) = oneshot::channel();
        {
            let mut live = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            live.recover_finished_task();
            if let Some(request) = &live.request {
                if request.state == "preparing" {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "already_preparing",
                        "already preparing",
                    ));
                }
                if request.state == "unknown" && request.pending {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "switch_uncertain",
                        "the previous request may have been accepted; read update status before trying again",
                    ));
                }
            }
            let preparing = SwitchRequest::preparing(action);
            live.request = Some(preparing.clone());
            let requests = self.clone();
            live.task = Some(tokio::spawn(async move {
                let result = AssertUnwindSafe(async {
                    if action == "install" {
                        updates.install_now(false).await
                    } else {
                        let target = updates
                            .switch_view()
                            .await?
                            .rollback_target
                            .ok_or(UpdateError::NoPrevious)?;
                        updates.roll_back(&target).await
                    }
                })
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(SwitchError::Stopped));
                let request = preparing.finish(result);
                requests
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .request = Some(request.clone());
                let _ = send.send(request);
            }));
        }
        match tokio::time::timeout(Duration::from_millis(900), receive).await {
            Ok(Ok(request)) => request.response(),
            _ => SwitchRequest::preparing(action).response(),
        }
    }
}

/// `POST /update/install`: never retry an ambiguous endpoint request.
pub(crate) async fn install(State(state): State<DashboardState>) -> Result<Response, ApiError> {
    state
        .update_requests
        .start(updates(&state)?.clone(), "install")
        .await
}

/// `POST /update/rollback`.
pub(crate) async fn rollback(State(state): State<DashboardState>) -> Result<Response, ApiError> {
    state
        .update_requests
        .start(updates(&state)?.clone(), "rollback")
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_task_ending_without_a_result_reports_failed_preparation() {
        let task = tokio::spawn(async {});
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut live = Live {
            request: Some(SwitchRequest::preparing("install")),
            task: Some(task),
            ..Default::default()
        };
        live.recover_finished_task();
        assert!(live.task.is_none());
        let request = live.request.unwrap();
        assert_eq!(request.state, "failed");
        assert!(!request.pending);
        assert_eq!(
            request.reason.as_deref(),
            Some("preparing the switch stopped")
        );
    }

    fn assert_error(error: SwitchError, state: &str, code: &str, status: StatusCode) {
        let request = SwitchRequest::preparing("install").finish(Err(error));
        assert_eq!(request.state, state);
        assert_eq!(request.pending, state == "unknown");
        if status == StatusCode::CONFLICT {
            let error = request.response().unwrap_err();
            assert_eq!(error.code, code);
        } else {
            assert_eq!(request.response().unwrap().status(), status);
        }
    }

    #[test]
    fn not_supervised_is_a_refusal_to_start() {
        assert_error(
            SwitchError::NotSupervised,
            "failed",
            "not_supervised",
            StatusCode::CONFLICT,
        );
    }
    #[test]
    fn updates_off_is_a_failure_to_start() {
        assert_error(
            SwitchError::UpdatesOff,
            "failed",
            "switch_failed",
            StatusCode::CONFLICT,
        );
    }
    #[test]
    fn update_errors_are_failures_to_prepare() {
        assert_error(
            SwitchError::Update(UpdateError::NothingStaged),
            "failed",
            "switch_failed",
            StatusCode::CONFLICT,
        );
    }
    #[test]
    fn skipped_is_a_failure_to_start() {
        assert_error(
            SwitchError::Skipped(open_ferry_update::Report::SkippedFailed {
                version: "0.2.0".into(),
            }),
            "failed",
            "switch_failed",
            StatusCode::CONFLICT,
        );
    }
    #[test]
    fn timed_out_might_have_been_accepted() {
        assert_error(SwitchError::TimedOut, "unknown", "", StatusCode::ACCEPTED);
    }
    #[test]
    fn unreachable_means_the_request_was_not_sent() {
        assert_error(
            SwitchError::Unreachable("offline".into()),
            "failed",
            "switch_failed",
            StatusCode::CONFLICT,
        );
    }
    #[test]
    fn untrusted_is_a_failure_to_start() {
        assert_error(
            SwitchError::Untrusted("untrusted".into()),
            "failed",
            "switch_failed",
            StatusCode::CONFLICT,
        );
    }
    #[test]
    fn protocol_might_have_been_accepted() {
        assert_error(
            SwitchError::Protocol("bad reply".into()),
            "unknown",
            "",
            StatusCode::ACCEPTED,
        );
    }
    #[test]
    fn refused_is_a_known_refusal() {
        assert_error(
            SwitchError::Refused("busy".into()),
            "refused",
            "switch_refused",
            StatusCode::CONFLICT,
        );
    }
    #[test]
    fn defensive_ended_mapping_is_a_known_failed_switch() {
        assert_error(
            SwitchError::Ended("exited".into()),
            "failed",
            "",
            StatusCode::ACCEPTED,
        );
    }
    #[test]
    fn stopped_might_have_been_accepted() {
        assert_error(SwitchError::Stopped, "unknown", "", StatusCode::ACCEPTED);
    }

    #[test]
    fn preparation_errors_use_fixed_reasons_and_refusals_pass_through() {
        for error in [
            SwitchError::Untrusted("private endpoint path".into()),
            SwitchError::Unreachable("private socket path".into()),
            SwitchError::Protocol("private reply details".into()),
            SwitchError::Update(UpdateError::Io {
                what: "read",
                error: "private state path".into(),
            }),
            SwitchError::Update(UpdateError::Journal("private journal path".into())),
        ] {
            let request = SwitchRequest::preparing("install").finish(Err(error));
            assert!(!request.reason.unwrap().contains("private"));
        }
        let request = SwitchRequest::preparing("install")
            .finish(Err(SwitchError::Refused("sign-ins are pending".into())));
        assert_eq!(request.reason.as_deref(), Some("sign-ins are pending"));
    }
}
