// To be ported from CLIProxyAPI internal/api/handlers/management/
// config_basic.go (GetLatestVersion, setLatestReleaseRequestHeaders)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The latest open-ferry release, as GitHub names it.
//!
//! Not ported yet: `GET /v0/management/latest-version` (also
//! `/v8/management/server/latest-version`).

use crate::Route;

/// Where open-ferry's latest release is found. Upstream asks for
/// CLIProxyAPI's.
#[cfg_attr(not(test), allow(dead_code, reason = "for the latest-version route"))]
pub(crate) const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest";

/// The routes this module serves: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
