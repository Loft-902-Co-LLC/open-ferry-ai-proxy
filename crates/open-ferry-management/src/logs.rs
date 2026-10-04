//! The main log's routes: `GET` and `DELETE /v0/management/logs`
//! (`/v8/management/observability/logs`), a tail of `main.log` with a
//! cursor, and removing the rotated logs (upstream's
//! internal/api/handlers/management/logs.go). Not ported yet (P3 WP-B).

use crate::Route;

/// The module's routes: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
