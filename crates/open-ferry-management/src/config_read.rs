// To be ported from CLIProxyAPI internal/api/handlers/management/
// config_basic.go (GetConfig, GetConfigYAML and the getters),
// config_lists.go (the getters), config_auth_index.go and config_v8.go
// (ConfigV8's reads) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Reading the config: as JSON, as the file it was loaded from, and one
//! setting or list at a time.
//!
//! Not ported yet: `GET /v0/management/config`, `config.yaml` and the
//! getters of single settings and key lists, and `GET
//! /v8/management/config`, `config.yaml` and `config/*path`. The config is
//! only ever read: open-ferry never writes it, so its `PUT`, `PATCH` and
//! `DELETE` routes stay unported.

use crate::Route;

/// The routes this module serves: none yet.
pub(crate) fn routes() -> Vec<Route> {
    Vec::new()
}
