// Ported from CLIProxyAPI internal/runtime/executor/helps/responses_tool_search.go,
// helps/claude_tool_search_model.go and claude_executor.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The native-search fallback and the value retained across both translations.

use std::borrow::Cow;

use crate::thinking;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_translate::go;
use serde_json::Value;

use super::super::request::{credentials, is_anthropic_url};
use super::{ClaudeExecutor, parse_object};

/// Whether the model predates Anthropic native tool search. Unknown models
/// remain eligible, as upstream does.
pub fn model_lacks_tool_search(model: &str) -> bool {
    let model = go::to_lower(thinking::parse_suffix(model).0.trim());
    let model = model.rsplit('/').next().unwrap_or(&model);
    let model = model.split('[').next().unwrap_or(model).trim();
    ["claude-3", "claude-2", "claude-instant"]
        .iter()
        .any(|p| model.starts_with(p))
        || matches!(
            model,
            "claude-opus-4"
                | "claude-opus-4-0"
                | "claude-opus-4-20250514"
                | "claude-opus-4-1"
                | "claude-opus-4-1-20250805"
                | "claude-sonnet-4"
                | "claude-sonnet-4-0"
                | "claude-sonnet-4-20250514"
        )
}

/// A credential's support with a caller-pinned switch. A custom base URL
/// requires the credential attribute or its matching configured key's flag.
fn supports_native_tool_search(
    config: Option<&Config>,
    enabled: bool,
    auth: &Auth,
    model: &str,
) -> bool {
    if !enabled || model_lacks_tool_search(model) {
        return false;
    }
    let (_, base) = credentials(auth);
    if base.trim().is_empty() || is_anthropic_url(&base) {
        return true;
    }
    if auth
        .attribute("native_tool_search")
        .is_some_and(|v| go::equal_fold(v.trim(), "true"))
    {
        return true;
    }
    let Some(config) = config else {
        return false;
    };
    let (key, _) = credentials(auth);
    if key.is_empty() {
        return false;
    }
    // Upstream takes the first matching key whose endpoint agrees or is empty.
    config
        .claude_api_key
        .iter()
        .find(|entry| {
            go::equal_fold(entry.api_key.trim(), &key)
                && (entry.base_url.trim().is_empty()
                    || go::equal_fold(entry.base_url.trim(), &base))
        })
        .is_some_and(|entry| entry.native_tool_search)
}

fn is_client_search(tool: &Value) -> bool {
    tool.get("type")
        .and_then(Value::as_str)
        .is_some_and(|v| v.trim() == "tool_search")
        && tool
            .get("execution")
            .and_then(Value::as_str)
            .is_some_and(|v| v.trim() == "client")
}

fn strip_tools(tools: Option<&mut Value>) {
    if let Some(Value::Array(tools)) = tools {
        tools.retain(|tool| !is_client_search(tool));
    }
}

/// Removes client declarations and search history; preserves hosted search
/// and all other tool declarations, including deferred ones.
pub fn strip_responses_tool_search(body: &mut Value) {
    strip_tools(body.get_mut("tools"));
    if let Some(Value::Array(items)) = body.get_mut("input") {
        items.retain(|item| {
            !matches!(
                item.get("type").and_then(Value::as_str).map(str::trim),
                Some("tool_search_call" | "tool_search_output")
            )
        });
        for item in items {
            if item.get("type").and_then(Value::as_str).map(str::trim) == Some("additional_tools") {
                strip_tools(item.get_mut("tools"));
            }
        }
    }
}

fn stripped(raw: &bytes::Bytes) -> bytes::Bytes {
    let mut body = parse_object(raw);
    let original = body.clone();
    strip_responses_tool_search(&mut body);
    if body == original {
        raw.clone()
    } else {
        body.to_string().into()
    }
}

impl ClaudeExecutor {
    /// Whether this executor and credential can receive native tool search.
    pub fn supports_native_tool_search(&self, auth: &Auth, upstream_model: &str) -> bool {
        supports_native_tool_search(
            self.config.as_deref(),
            self.tool_search_enabled(),
            auth,
            upstream_model,
        )
    }

    pub(super) fn tool_search_enabled(&self) -> bool {
        self.config
            .as_ref()
            .is_some_and(|config| config.claude.tool_search)
    }

    /// The immutable executor config supplies one value for fallback, request
    /// translation and response translation, even when a reload replaces it.
    pub(super) fn tool_search_fallback<'a>(
        &self,
        auth: &Auth,
        request: &'a Request,
        options: &'a Options,
    ) -> (Cow<'a, Request>, Cow<'a, Options>) {
        if options.source_format != Format::OPENAI_RESPONSE
            || !self.tool_search_enabled()
            || self.supports_native_tool_search(auth, &request.model)
        {
            return (Cow::Borrowed(request), Cow::Borrowed(options));
        }
        let payload = stripped(&request.payload);
        let original = stripped(&options.original_request);
        let request = if payload == request.payload {
            Cow::Borrowed(request)
        } else {
            Cow::Owned(Request {
                payload,
                ..request.clone()
            })
        };
        let options = if original == options.original_request {
            Cow::Borrowed(options)
        } else {
            Cow::Owned(Options {
                original_request: original,
                ..options.clone()
            })
        };
        (request, options)
    }
}

/// Deferred tools cannot carry cache markers, including ones added by the
/// executor's automatic cache policy after translation.
pub(super) fn drop_deferred_cache_control(body: &mut Value) {
    if let Some(Value::Array(tools)) = body.get_mut("tools") {
        for tool in tools {
            if tool.get("defer_loading") == Some(&Value::Bool(true))
                && let Some(tool) = tool.as_object_mut()
            {
                tool.remove("cache_control");
            }
        }
    }
}
