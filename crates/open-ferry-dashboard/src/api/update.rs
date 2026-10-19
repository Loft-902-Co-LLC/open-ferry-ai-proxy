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
                Err(error) => (
                    live.supervisor.clone().unwrap_or(Value::Null),
                    Some(error.to_string()),
                    None,
                    Value::Null,
                ),
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

#[derive(Clone, Debug, Serialize)]
struct SwitchRequest {
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
            action,
            state: "preparing",
            id: None,
            reason: None,
            pending: true,
            finished_at: Instant::now(),
            code: "switch_failed",
        }
    }

    fn finished(action: &'static str, result: Result<SwitchHandle, SwitchError>) -> Self {
        let mut request = Self::preparing(action);
        match result {
            Ok(handle) => {
                request.state = "accepted";
                request.id = Some(handle.id.clone());
                // The supervisor owns the operation; disconnecting does not cancel it.
                drop(handle);
            }
            Err(error) => {
                request.pending = false;
                request.state = if matches!(error, SwitchError::Refused(_)) {
                    "refused"
                } else if matches!(error, SwitchError::TimedOut | SwitchError::Stopped) {
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
                } else {
                    "switch_refused"
                };
                request.reason = Some(match error {
                    SwitchError::NotSupervised => "this install is not supervised; run `open-ferry update` (or `open-ferry update -rollback`) on the server's computer, then restart the server".to_owned(),
                    SwitchError::TimedOut => "the supervisor did not answer in time; the request may have been accepted; read update status before trying again".to_owned(),
                    SwitchError::Stopped => "the supervisor connection ended without an answer; the outcome is unknown; read update status before trying again".to_owned(),
                    SwitchError::Update(UpdateError::Busy) => "busy, try again shortly".to_owned(),
                    SwitchError::Ended(reason) => reason,
                    error => error.to_string(),
                });
            }
        }
        request
    }

    fn response(&self) -> Result<Response, ApiError> {
        match self.state {
            "preparing" => Ok(json(StatusCode::ACCEPTED, &value!({"preparing": true}))),
            "accepted" => Ok(json(
                StatusCode::ACCEPTED,
                &value!({"accepted": true, "id": self.id}),
            )),
            "unknown" => Ok(json(StatusCode::ACCEPTED, &value!({"uncertain": true}))),
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
            live.request = Some(SwitchRequest::preparing(action));
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
                let request = SwitchRequest::finished(action, result);
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
