// Ported from CLIProxyAPI internal/translator/common/request.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers shared by translators of every format.

pub(crate) mod cache_control;
pub(crate) mod claude;
pub(crate) mod openai_tools;
pub(crate) mod responses;
pub(crate) mod tool_names;

use serde_json::Value;

use crate::json::path;

/// The model the client asked for: `model`, or else `request.model`, from the
/// client's request, then from the translated request. Blank names are skipped.
pub(crate) fn request_model_name<'r>(
    original_request: &'r Value,
    request: &'r Value,
) -> Option<&'r str> {
    [original_request, request].into_iter().find_map(|request| {
        ["model", "request.model"].into_iter().find_map(|key| {
            path(request, key)?
                .as_str()
                .filter(|name| !name.trim().is_empty())
        })
    })
}
