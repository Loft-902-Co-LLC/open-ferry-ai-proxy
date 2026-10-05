// Ported from prepareCodexMultiAgentV2Tools and prepareCodexOrphanDelegation
// in CLIProxyAPI sdk/api/handlers/openai/openai_responses_handlers.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A Codex client's Responses request, readied before it is routed.
//!
//! With `client.codex.optimize-multi-agent-v2`, an official Codex client's
//! collaboration tools lose the `encrypted` mark on their `message`
//! parameter, and `spawn_agent`'s description lists the models the proxy
//! serves ([`open_ferry_core::codex_models::spawn_agent`]); with
//! `codex.orphan-delegation-compatibility`, a sub-agent's delegation outputs
//! without their call become user messages. The rewrites themselves are in
//! `open_ferry_translate::codex_client`; the executors make them again where
//! upstream's do.
//!
//! Deviations from upstream:
//! - Nothing notes for the Codex executor that the tools were prepared
//!   (`CodexMultiAgentV2ToolsPreparedContextKey`), so it prepares them
//!   again. That writes the same model list over the one written here,
//!   unless the models changed in between, when the newer list wins where
//!   upstream keeps the older.
//! - Upstream v8.0.10 skips orphan delegation here when a v8 document put
//!   the setting under `oauth.providers`. The config here, as v8.0.11's,
//!   shares that spelling with API keys, and v8.0.11 drops the check.
//! - A body that isn't a JSON object is left as it is, as is one that
//!   doesn't change; one that changes is written again, as the
//!   `codex_client` module says, each number as the client wrote it.

use http::HeaderMap;
use http::header::{HeaderValue, USER_AGENT};
use open_ferry_core::codex_models::spawn_agent::spawn_agent_model_list;
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::codex_client::{header_value, multi_agent_v2, orphan_delegation};
use open_ferry_translate::json::exact;
use serde_json::Value;

use crate::config::ServerConfig;

/// The first non-blank value of the client's header `name`, trimmed.
fn header(headers: &HeaderMap, name: &str) -> String {
    header_value(headers.get_all(name).iter().map(HeaderValue::as_bytes))
}

/// `raw` readied as `config` says for a client that sent `headers`: its
/// collaboration tools when `tools` (not for `responses/compact`), with the
/// models of `catalog`, then its orphan delegation outputs. `None` when
/// nothing changes.
pub(crate) fn prepare(
    config: &ServerConfig,
    catalog: &dyn ModelCatalog,
    headers: &HeaderMap,
    raw: &[u8],
    tools: bool,
) -> Option<Vec<u8>> {
    let user_agent = header(headers, USER_AGENT.as_str());
    let subagent = header(headers, orphan_delegation::SUBAGENT_HEADER);
    let optimize = tools && config.codex_client.optimize_multi_agent_v2;
    let orphans = config.codex_orphan_delegation;
    // Skip parsing a body neither rewrite can apply to.
    if !(optimize && multi_agent_v2::is_codex_client_user_agent(&user_agent))
        && !(orphans && !subagent.is_empty())
    {
        return None;
    }
    let mut body = match exact::from_slice(raw) {
        Ok(Value::Object(fields)) => Value::Object(fields),
        _ => return None,
    };
    let mut changed = false;
    if tools {
        let models = || spawn_agent_model_list(catalog);
        changed |= multi_agent_v2::prepare_tools(&mut body, &user_agent, optimize, models);
    }
    changed |= orphan_delegation::rewrite(&mut body, &subagent, orphans);
    changed.then(|| body.to_string().into_bytes())
}

#[cfg(test)]
mod tests;
