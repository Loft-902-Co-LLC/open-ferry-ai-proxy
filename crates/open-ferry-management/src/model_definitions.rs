// To be ported from CLIProxyAPI internal/api/handlers/management/
// model_definitions.go (GetStaticModelDefinitions) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The models a channel serves, from the model catalog.
//!
//! Not ported yet: `GET /v0/management/model-definitions/:channel` (also
//! `/v8/management/routing/model-definitions/:channel`).

use crate::Route;

/// The routes this module serves: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
