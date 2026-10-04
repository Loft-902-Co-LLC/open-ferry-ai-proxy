//! `POST /v0/management/quota/fetch`: a credential's quota, fetched with
//! the declarative probe of its credential file (upstream's
//! internal/api/handlers/management/plugin_quota.go, without the plugin
//! host). Not ported yet (P3 WP-E).

use crate::Route;

/// The module's routes: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
