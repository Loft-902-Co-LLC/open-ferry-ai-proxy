//! What management hands to the next version of open-ferry when it switches
//! (v3 12.3): the failed-attempt bans, in the `handover.json` file, and the
//! "updating" answer management gives while the switch is under way.
//!
//! The service writes and reads the file; this module is its format and the
//! two operations on the record:
//!
//! 1. [`ManagementState::set_updating`] turns the outermost layer of the
//!    management routers on: every route answers 503
//!    `{"error":"updating, try again"}` with `Connection: close`, before the
//!    key is looked at, so a refused request is not a failed attempt.
//! 2. [`ManagementState::freeze_attempts`] stops the record of failed
//!    attempts and returns it as a [`Handover`]. A failure counted after it,
//!    by a request that was already past the layer, is not recorded.
//!
//! If the switch does not happen, [`ManagementState::unfreeze_attempts`] and
//! `set_updating(false)` put management back as it was. The next version
//! calls [`ManagementState::restore_attempts`] with what it read.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderValue, StatusCode, header};
use serde::{Deserialize, Serialize};

use crate::json;
use crate::state::ManagementState;

/// What every management route answers while open-ferry is switching.
const UPDATING: &str = "updating, try again";

/// The file `<data dir>/handover.json`: the state one version hands to the
/// next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handover {
    /// Names this hand over, so the version that reads the file can tell it
    /// is the one it was told about.
    pub id: String,
    /// When the snapshot was taken, in milliseconds since the Unix epoch.
    pub at: u64,
    /// The addresses with failed attempts or a ban, by address.
    pub bans: Vec<Ban>,
}

/// One address's failed attempts. Durations rather than `Instant`s, which
/// mean nothing to another process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ban {
    /// The client address, as the record keys it.
    pub ip: String,
    /// Failures counted toward a ban.
    pub count: u32,
    /// How long the ban had left at `at`, in milliseconds; 0 for none.
    pub blocked_for_ms: u64,
    /// How long the address had been idle at `at`, in milliseconds.
    pub idle_ms: u64,
}

impl Handover {
    /// The file's contents.
    ///
    /// # Panics
    /// Never: the types serialize.
    #[must_use]
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("a Handover serializes")
    }

    /// The `Handover` `data` holds.
    ///
    /// # Errors
    /// When `data` isn't one.
    pub fn from_json(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }
}

/// `duration` in whole milliseconds, saturating.
pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Milliseconds since the Unix epoch.
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, millis)
}

/// Whether management answers "updating". Cloning it gives another handle
/// to the same switch.
#[derive(Clone, Debug, Default)]
pub(crate) struct Updating(Arc<AtomicBool>);

impl Updating {
    pub(crate) fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }

    pub(crate) fn is_on(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 503 `{"error":"updating, try again"}` with `Connection: close`.
pub(crate) fn updating_response() -> Response {
    let mut response = json::error(StatusCode::SERVICE_UNAVAILABLE, UPDATING);
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

/// The outermost layer of every management route: answers
/// [`updating_response`] while the state's switch is on, and otherwise
/// passes the request on.
pub(crate) async fn updating(
    State(state): State<ManagementState>,
    request: Request,
    next: Next,
) -> Response {
    if state.updating().is_on() {
        return updating_response();
    }
    next.run(request).await
}

impl ManagementState {
    /// Turns the "updating" answer on or off for every management route.
    /// Turn it on first when a switch begins; turn it off if the switch does
    /// not happen.
    pub fn set_updating(&self, on: bool) {
        self.updating().set(on);
    }

    /// Whether management answers "updating".
    pub fn is_updating(&self) -> bool {
        self.updating().is_on()
    }

    /// Stops the record of failed attempts and returns it, named `id`, as of
    /// now. Call it with the "updating" answer on, so no request that
    /// would have failed is refused after it. A failure counted later is not
    /// recorded.
    pub fn freeze_attempts(&self, id: impl Into<String>) -> Handover {
        let now = std::time::Instant::now();
        let at = unix_millis();
        let bans = self.attempts().freeze_and_snapshot(now);
        Handover {
            id: id.into(),
            at,
            bans,
        }
    }

    /// Records failed attempts again, with the record as it was before
    /// [`freeze_attempts`](Self::freeze_attempts).
    pub fn unfreeze_attempts(&self) {
        self.attempts().unfreeze();
    }

    /// Takes in the bans another version handed over, as of now: each has
    /// the time since `handover.at` less to run and has been idle that much
    /// longer.
    pub fn restore_attempts(&self, handover: &Handover) {
        self.restore_attempts_at(handover, unix_millis(), std::time::Instant::now());
    }

    pub(crate) fn restore_attempts_at(
        &self,
        handover: &Handover,
        now_unix_ms: u64,
        now: std::time::Instant,
    ) {
        let elapsed = now_unix_ms.saturating_sub(handover.at);
        self.attempts().restore(&handover.bans, elapsed, now);
    }
}
