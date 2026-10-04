//! The usage routes: `GET /v0/management/api-key-usage` and `usage-queue`
//! (`/v8/management/observability/usage/api-keys` and `usage/queue`)
//! (upstream's internal/api/handlers/management/api_key_usage.go and
//! usage.go). Not ported yet (P3 WP-C).

use crate::Route;

/// The module's routes: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
