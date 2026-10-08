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
//! Both answer `updates_unavailable` when the server runs no update checks,
//! as a TUI's own server doesn't.

use axum::extract::State;
use axum::response::Response;
use http::StatusCode;
use open_ferry_update::{CheckNow, UpdateService};
use serde::Serialize;

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
    Ok(ok(&updates(&state)?.status()))
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
