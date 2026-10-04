//! The observability settings' reads: `GET /v0/management/
//! usage-statistics-enabled`, `logs-max-total-size-mb` and
//! `error-logs-max-files` (upstream's
//! internal/api/handlers/management/config_basic.go). Their writes aren't
//! ported, as open-ferry never writes the config. Not ported yet (P3 WP-B).

use crate::Route;

/// The module's routes: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
