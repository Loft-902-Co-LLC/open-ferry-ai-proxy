//! The request log's routes: `GET /v0/management/request-error-logs`,
//! `request-error-logs/:name` and `request-log-by-id/:id`
//! (`/v8/management/observability/logs/errors`, `errors/:name` and
//! `requests/:id`) (upstream's internal/api/handlers/management/logs.go).
//! Not ported yet (P3 WP-A).

use crate::Route;

/// The module's routes: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
