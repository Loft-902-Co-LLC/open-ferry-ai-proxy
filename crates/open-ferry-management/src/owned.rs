//! The handlers that change credentials run as tasks the active gate owns.
//!
//! Such a handler asks the gate for an [`Operation`] first, and answers 503
//! `{"error":"open-ferry is switching versions; try again in a minute"}`,
//! having changed nothing, when the gate isn't open. Its whole body then runs
//! as a task of the gate (`ActiveGate::spawn`) holding that operation, so
//! the work finishes, writes included, even when the client goes away, and
//! the gate does not close under it: a switch waits for the task, and the
//! task's writes pass a gate that is only quiescing.
//!
//! Without a gate set, as in a state made only with
//! [`ManagementState::new`], the gate is always open and a handler runs as it
//! did, on a task of its own.

use std::future::Future;

use axum::response::Response;
use http::StatusCode;

use crate::config_write::SWITCHING;
use crate::json;
use crate::state::ManagementState;

/// 503 `{"error":"open-ferry is switching versions; try again in a minute"}`.
pub(crate) fn switching() -> Response {
    json::error(StatusCode::SERVICE_UNAVAILABLE, SWITCHING)
}

/// Runs `body` as an owned task of the state's gate and answers what it
/// answers, or [`switching`] when the gate doesn't admit it. A panic in
/// `body` is passed on.
pub(crate) async fn owned<F>(state: &ManagementState, body: F) -> Response
where
    F: Future<Output = Response> + Send + 'static,
{
    let gate = state.gate();
    let Ok(operation) = gate.begin_operation() else {
        return switching();
    };
    let Ok(task) = gate.spawn(&operation, body) else {
        return switching();
    };
    // The task holds its own clone from here.
    drop(operation);
    match task.await {
        Ok(response) => response,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(error) => panic!("management task failed: {error}"),
        },
    }
}
