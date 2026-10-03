// Ported from CLIProxyAPI internal/runtime/executor/claude_executor.go,
// claude_executor_request.go and claude_executor_cloaking.go (the cache-control
// helpers), helps/claude_upstream.go, helps/claude_diagnostics.go
// (ClaudePayloadHas1hTTL) and sdk/cliproxy/auth/classification.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential's key and base URL, and the changes a Messages body needs
//! before it goes to Claude: `max_tokens`, thinking that a forced tool choice
//! rules out, sampling settings Claude rejects, cache breakpoints, body
//! `betas`, replayed thinking signatures and empty web-search domain lists.
//!
//! Deviations from upstream:
//! - Upstream recognises a native Claude Code client by its headers and
//!   body, and then leaves its sampling settings and cache breakpoints alone.
//!   That detection isn't ported, so every request is handled as upstream
//!   handles a client it doesn't recognise: `temperature` and `top_p` are
//!   dropped, and breakpoints are placed when the request has none.
//! - The 1-hour cache upgrade and TTL stripping that copy what Claude Code
//!   sends for probes, helpers and subagents aren't ported. TTLs stay as the
//!   client wrote them, apart from the ordering fix Claude needs.
//! - Breakpoint text blocks are built as JSON values, so the body's bytes
//!   follow `serde_json`'s escaping rather than Go's.

use serde_json::{Map, Value, json};
use tracing::debug;

use open_ferry_core::auth::Auth;
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::signature::{Provider, sanitize_claude_messages_for_claude_upstream};

use crate::json;

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

pub(crate) const OAUTH_BETA: &str = "oauth-2025-04-20";
pub(crate) const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
pub(crate) const TOKEN_COUNTING_BETA: &str = "token-counting-2024-11-01";
pub(crate) const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";
pub(crate) const EXTENDED_CACHE_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";
pub(crate) const ADVISOR_TOOL_BETA: &str = "advisor-tool-2026-03-01";

/// Betas the advisor beta goes in front of, where Anthropic lists them.
pub(crate) const AFTER_ADVISOR_BETAS: [&str; 9] = [
    "advanced-tool-use-2025-11-20",
    "effort-2025-11-24",
    "server-side-fallback-2026-06-01",
    "fallback-credit-2026-06-01",
    "structured-outputs-2025-12-15",
    FAST_MODE_BETA,
    "afk-mode-2026-01-31",
    EXTENDED_CACHE_TTL_BETA,
    "cache-diagnosis-2026-04-07",
];

/// `max_tokens` for a Claude model the catalog has no limit for.
const DEFAULT_MAX_TOKENS: u64 = 1024;

/// Anthropic's limit on cache breakpoints in one request.
pub(crate) const MAX_CACHE_BREAKPOINTS: usize = 4;

/// `claudeCreds`: the credential's API key, or else its OAuth access token,
/// and its base URL.
pub(crate) fn credentials(auth: &Auth) -> (String, String) {
    let mut api_key = auth.attribute("api_key").unwrap_or_default().to_owned();
    let base_url = auth.attribute("base_url").unwrap_or_default().to_owned();
    if api_key.is_empty() {
        api_key = auth
            .metadata_str("access_token")
            .unwrap_or_default()
            .to_owned();
    }
    (api_key, base_url)
}

/// `isClaudeOAuthToken`.
pub(crate) fn is_oauth_token(key: &str) -> bool {
    key.contains("sk-ant-oat")
}

/// `claudeCredentialUsesOAuth`: whether the key goes in a Bearer header
/// rather than `x-api-key`.
pub(crate) fn uses_bearer(auth: &Auth, key: &str) -> bool {
    if is_oauth_token(key) {
        return true;
    }
    if auth_kind(auth) == Some(AuthKind::ApiKey) {
        return false;
    }
    auth.attribute("api_key")
        .is_none_or(|key| key.trim().is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthKind {
    ApiKey,
    OAuth,
}

/// `Auth.AuthKind`: what the record says it is, or what its fields suggest.
fn auth_kind(auth: &Auth) -> Option<AuthKind> {
    let normalize = |kind: &str| match json::lower_trim(kind).as_str() {
        "apikey" | "api_key" | "api-key" => Some(AuthKind::ApiKey),
        "oauth" | "oauth2" => Some(AuthKind::OAuth),
        _ => None,
    };
    if let Some(kind) = auth.attribute("auth_kind").and_then(normalize) {
        return Some(kind);
    }
    if let Some(kind) = auth.metadata_str("auth_kind").and_then(normalize) {
        return Some(kind);
    }
    if auth
        .attribute("api_key")
        .is_some_and(|key| !key.trim().is_empty())
    {
        return Some(AuthKind::ApiKey);
    }
    let oauth_field = [
        "access_token",
        "refresh_token",
        "id_token",
        "email",
        "token_type",
        "expires_at",
        "expired",
    ]
    .iter()
    .any(|key| {
        auth.metadata_str(key)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let token_object = auth
        .metadata
        .get("token")
        .and_then(Value::as_object)
        .is_some_and(|token| !token.is_empty());
    (oauth_field || token_object).then_some(AuthKind::OAuth)
}

/// `IsAnthropicUpstreamURL`: plain HTTPS to `api.anthropic.com` on the
/// default port, with no user info.
pub(crate) fn is_anthropic_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw.trim()) else {
        return false;
    };
    url.scheme().eq_ignore_ascii_case("https")
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.anthropic.com"))
        && url.port().is_none_or(|port| port == 443)
}

/// `ensureModelMaxTokens`: a request without `max_tokens` for a model a
/// Claude credential serves gets the model's limit, or 1024. Some
/// Anthropic-compatible upstreams fail without it.
pub(crate) fn ensure_model_max_tokens(
    body: &mut Value,
    model: &str,
    models: Option<&dyn ModelCatalog>,
) {
    if json::exists(body, "max_tokens") {
        return;
    }
    let Some(models) = models else {
        return;
    };
    let model = model.trim();
    if !models
        .model_providers(model)
        .iter()
        .any(|provider| json::eq_fold(provider, "claude"))
    {
        return;
    }
    let max_tokens = models
        .available_models()
        .into_iter()
        .find(|info| info.id == model && info.max_completion_tokens > 0)
        .map_or(DEFAULT_MAX_TOKENS, |info| info.max_completion_tokens);
    json::set(body, "max_tokens", max_tokens.into());
}

/// `disableThinkingIfToolChoiceForced`: Claude doesn't allow thinking when
/// `tool_choice` forces a tool.
pub(crate) fn disable_thinking_if_tool_choice_forced(body: &mut Value) {
    let choice = json::str_at(body, "tool_choice.type");
    if choice == "any" || choice == "tool" {
        json::delete(body, "thinking");
        json::delete(body, "output_config.effort");
        drop_empty_object(body, "output_config");
    }
}

/// `normalizeClaudeSamplingForUpstream` for a client upstream doesn't
/// recognise as Claude Code: `temperature` and `top_p` go, as Claude rejects
/// several combinations of them, and so does `top_k` while thinking.
pub(crate) fn normalize_sampling(body: &mut Value) {
    let thinking = matches!(
        json::lower_trim(&json::str_at(body, "thinking.type")).as_str(),
        "enabled" | "adaptive" | "auto"
    );
    json::delete(body, "temperature");
    json::delete(body, "top_p");
    if thinking {
        json::delete(body, "top_k");
    }
}

/// `extractAndRemoveBetas`: the body's `betas`, which belong in the
/// `anthropic-beta` header.
pub(crate) fn extract_and_remove_betas(body: &mut Value) -> Vec<String> {
    let Some(betas) = json::get(body, "betas") else {
        return Vec::new();
    };
    let betas: Vec<String> = match betas {
        Value::Array(items) => items
            .iter()
            .map(|item| json::str_of(Some(item)).trim().to_owned())
            .filter(|beta| !beta.is_empty())
            .collect(),
        other => {
            let beta = json::str_of(Some(other)).trim().to_owned();
            if beta.is_empty() {
                Vec::new()
            } else {
                vec![beta]
            }
        }
    };
    json::delete(body, "betas");
    betas
}

/// `sanitizeClaudeMessagesForClaudeUpstreamWithDebug`: keeps the thinking
/// signatures a Claude model can replay and drops the rest, then removes
/// empty web-search domain lists.
pub(crate) fn sanitize_for_upstream(body: &mut Value, base_model: &str) {
    if Provider::from_model_name(base_model) == Provider::Claude {
        let report = sanitize_claude_messages_for_claude_upstream(body, base_model, false);
        if report.dropped_blocks > 0
            || report.dropped_signatures > 0
            || report.replaced_signatures > 0
        {
            let first = report.decisions.first();
            debug!(
                target_model = base_model,
                target_provider = report.target_provider.as_str(),
                preserved = report.preserved,
                dropped_blocks = report.dropped_blocks,
                dropped_signatures = report.dropped_signatures,
                replaced_signatures = report.replaced_signatures,
                first_block_kind = first.map_or("", |decision| decision.block_kind.as_str()),
                first_detected_provider =
                    first.map_or("", |decision| decision.detected_provider.as_str()),
                first_reason = first.map_or("", |decision| decision.reason.as_str()),
                "claude executor: sanitized signature history before upstream"
            );
        }
    }
    sanitize_web_search_domains(body);
}

/// `sanitizeClaudeWebSearchDomains`: Claude rejects an empty
/// `allowed_domains` or `blocked_domains` on a web-search tool, where
/// leaving it out means the same.
fn sanitize_web_search_domains(body: &mut Value) {
    let Some(Value::Array(tools)) = body.get_mut("tools") else {
        return;
    };
    for tool in tools {
        if !json::str_at(tool, "type").starts_with("web_search_") {
            continue;
        }
        let Value::Object(tool) = tool else {
            continue;
        };
        for field in ["allowed_domains", "blocked_domains"] {
            if tool
                .get(field)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                tool.shift_remove(field);
            }
        }
    }
}

/// Deletes the object at `path` if it is empty.
pub(crate) fn drop_empty_object(body: &mut Value, path: &str) {
    if json::get(body, path)
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        json::delete(body, path);
    }
}

/// The breakpoint the executor adds: Claude's default five-minute cache.
fn cache_marker() -> Value {
    json!({"type": "ephemeral"})
}

/// A text block marked as a cache breakpoint.
fn marked_text_block(text: &str) -> Value {
    json!([{"type": "text", "text": text, "cache_control": cache_marker()}])
}

fn has_cache_control(block: &Value) -> bool {
    block.get("cache_control").is_some()
}

fn array_at<'v>(body: &'v Value, key: &str) -> Option<&'v Vec<Value>> {
    body.get(key).and_then(Value::as_array)
}

/// Every message's content array, in order.
fn message_contents(body: &Value) -> impl Iterator<Item = &Vec<Value>> {
    array_at(body, "messages")
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
}

/// `countCacheControls`: breakpoints on system blocks, tools and message
/// content blocks.
pub(crate) fn count_cache_controls(body: &Value) -> usize {
    let count = |blocks: &Vec<Value>| {
        blocks
            .iter()
            .filter(|block| has_cache_control(block))
            .count()
    };
    array_at(body, "system").map_or(0, count)
        + array_at(body, "tools").map_or(0, count)
        + message_contents(body).map(count).sum::<usize>()
}

/// `ensureCacheControl`: breakpoints on the tools (when there is no system
/// prompt to cover them), the system prompt and the latest turn, each where
/// the request has none.
pub(crate) fn ensure_cache_control(body: &mut Value) {
    if !has_cacheable_system(body) {
        inject_tools_cache_control(body);
    }
    inject_system_cache_control(body);
    inject_messages_cache_control(body);
}

/// `claudePayloadHasCacheableSystem`.
fn has_cacheable_system(body: &Value) -> bool {
    match body.get("system") {
        Some(Value::Array(blocks)) => !blocks.is_empty(),
        Some(Value::String(text)) => !text.trim().is_empty(),
        _ => false,
    }
}

/// `injectToolsCacheControl`: marks the last tool that isn't deferred, if no
/// tool is marked.
fn inject_tools_cache_control(body: &mut Value) {
    let Some(Value::Array(tools)) = body.get_mut("tools") else {
        return;
    };
    if tools.iter().any(has_cache_control) {
        return;
    }
    let last = tools
        .iter()
        .rposition(|tool| !json::bool_of(tool.get("defer_loading")));
    if let Some(tool) = last.and_then(|index| tools.get_mut(index)) {
        json::set(tool, "cache_control", cache_marker());
    }
}

/// `injectSystemCacheControl`: marks the last system block, if none is
/// marked, turning a string system prompt into one text block.
fn inject_system_cache_control(body: &mut Value) {
    match body.get_mut("system") {
        Some(Value::Array(blocks)) => {
            if blocks.iter().any(has_cache_control) {
                return;
            }
            if let Some(last) = blocks.last_mut() {
                json::set(last, "cache_control", cache_marker());
            }
        }
        Some(system) => {
            let block = system
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .map(marked_text_block);
            if let Some(block) = block {
                *system = block;
            }
        }
        None => {}
    }
}

/// `claudeMessageEligibleForRollingCache`: a turn that can carry the rolling
/// breakpoint. An assistant turn ending in thinking can't.
fn eligible_for_rolling_cache(message: &Value) -> bool {
    match message.get("content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => {
            if json::str_at(message, "role") != "assistant" {
                return !blocks.is_empty();
            }
            blocks.last().is_some_and(|last| {
                !matches!(
                    json::str_at(last, "type").as_str(),
                    "thinking" | "redacted_thinking"
                )
            })
        }
        _ => false,
    }
}

/// `injectMessagesCacheControl`: marks the last block of the latest user or
/// assistant turn that can carry it, or a final system turn's string
/// content, unless that turn already has a breakpoint.
fn inject_messages_cache_control(body: &mut Value) {
    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return;
    };
    let Some(eligible) = messages.iter().rposition(|message| {
        matches!(json::str_at(message, "role").as_str(), "user" | "assistant")
            && eligible_for_rolling_cache(message)
    }) else {
        return;
    };

    if let Some(last) = messages.last_mut() {
        let final_system_block = (json::str_at(last, "role") == "system")
            .then(|| last.get("content").and_then(Value::as_str))
            .flatten()
            .filter(|text| !text.trim().is_empty())
            .map(marked_text_block);
        if let Some(block) = final_system_block {
            json::set(last, "content", block);
            return;
        }
    }

    let Some(content) = messages
        .get_mut(eligible)
        .and_then(|message| message.get_mut("content"))
    else {
        return;
    };
    if let Value::Array(blocks) = content {
        if blocks.iter().any(has_cache_control) {
            return;
        }
        if let Some(last) = blocks.last_mut() {
            json::set(last, "cache_control", cache_marker());
        }
        return;
    }
    if let Some(block) = content.as_str().map(marked_text_block) {
        *content = block;
    }
}

/// Removes up to `excess` breakpoints from `blocks`, earliest first, sparing
/// the one at `keep`.
fn strip_cache_controls(blocks: &mut [Value], keep: Option<usize>, excess: &mut usize) {
    for (index, block) in blocks.iter_mut().enumerate() {
        if *excess == 0 {
            return;
        }
        if Some(index) == keep {
            continue;
        }
        if let Some(block) = block.as_object_mut()
            && block.shift_remove("cache_control").is_some()
        {
            *excess -= 1;
        }
    }
}

/// `enforceCacheControlLimit`: removes breakpoints beyond `max`, least
/// valuable first: system blocks but the last, tools but the last, message
/// blocks, then the last system block and the last tool.
pub(crate) fn enforce_cache_control_limit(body: &mut Value, max: usize) {
    let total = count_cache_controls(body);
    if total <= max {
        return;
    }
    let mut excess = total - max;
    let Value::Object(object) = body else {
        return;
    };

    for key in ["system", "tools"] {
        if let Some(Value::Array(blocks)) = object.get_mut(key) {
            let last = blocks.iter().rposition(has_cache_control);
            strip_cache_controls(blocks, last, &mut excess);
        }
        if excess == 0 {
            return;
        }
    }

    if let Some(Value::Array(messages)) = object.get_mut("messages") {
        for message in messages {
            if excess == 0 {
                return;
            }
            if let Some(Value::Array(content)) = message.get_mut("content") {
                strip_cache_controls(content, None, &mut excess);
            }
        }
    }

    for key in ["system", "tools"] {
        if excess == 0 {
            return;
        }
        if let Some(Value::Array(blocks)) = object.get_mut(key) {
            strip_cache_controls(blocks, None, &mut excess);
        }
    }
}

/// `normalizeCacheControlTTL`: Claude rejects a one-hour breakpoint after a
/// five-minute one (tools, then system, then messages), so a one-hour TTL
/// after any five-minute breakpoint is dropped.
pub(crate) fn normalize_cache_control_ttl(body: &mut Value) {
    let mut seen_short = false;
    let mut visit = |block: &mut Value| {
        let Some(cache_control) = block.get_mut("cache_control") else {
            return;
        };
        let one_hour = cache_control.get("ttl").and_then(Value::as_str) == Some("1h");
        match cache_control {
            Value::Object(cache_control) if one_hour => {
                if seen_short {
                    cache_control.shift_remove("ttl");
                }
            }
            _ => seen_short = true,
        }
    };
    let Value::Object(object) = body else {
        return;
    };
    for key in ["tools", "system"] {
        if let Some(Value::Array(blocks)) = object.get_mut(key) {
            blocks.iter_mut().for_each(&mut visit);
        }
    }
    if let Some(Value::Array(messages)) = object.get_mut("messages") {
        for message in messages {
            if let Some(Value::Array(content)) = message.get_mut("content") {
                content.iter_mut().for_each(&mut visit);
            }
        }
    }
}

/// `ClaudePayloadHas1hTTL`: whether any breakpoint asks for the one-hour
/// cache, which needs the extended cache TTL beta.
pub(crate) fn payload_has_1h_ttl(body: &Value) -> bool {
    let one_hour = |block: &Value| {
        block
            .get("cache_control")
            .filter(|cache_control| cache_control.is_object())
            .is_some_and(|cache_control| json::str_at(cache_control, "ttl") == "1h")
    };
    let any = |blocks: &Vec<Value>| blocks.iter().any(one_hour);
    array_at(body, "tools").is_some_and(any)
        || array_at(body, "system").is_some_and(any)
        || message_contents(body).any(any)
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_ferry_core::models::ModelInfo;

    fn auth(attributes: &[(&str, &str)], metadata: Value) -> Auth {
        Auth {
            attributes: attributes
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            metadata: metadata.as_object().cloned().unwrap_or_default(),
            ..Auth::default()
        }
    }

    #[test]
    fn reads_credentials() {
        let key = auth(
            &[
                ("api_key", "sk-ant-api"),
                ("base_url", "https://gw.example"),
            ],
            json!({}),
        );
        assert_eq!(
            credentials(&key),
            ("sk-ant-api".into(), "https://gw.example".into())
        );
        assert!(!uses_bearer(&key, "sk-ant-api"));

        let oauth = auth(
            &[],
            json!({"access_token": "sk-ant-oat01-x", "type": "claude"}),
        );
        assert_eq!(
            credentials(&oauth),
            ("sk-ant-oat01-x".into(), String::new())
        );
        assert!(uses_bearer(&oauth, "sk-ant-oat01-x"));

        // An OAuth-looking key wins over the kind; an API-key kind wins over
        // the fields.
        let forced = auth(&[("auth_kind", "API-Key")], json!({"access_token": "t"}));
        assert!(!uses_bearer(&forced, "t"));
        assert!(uses_bearer(&forced, "sk-ant-oat-1"));
        // A record that is neither still sends its token as a Bearer.
        assert!(uses_bearer(&auth(&[], json!({})), "x"));
        assert_eq!(
            auth_kind(&auth(&[], json!({"token": {"a": 1}}))),
            Some(AuthKind::OAuth)
        );
        assert_eq!(auth_kind(&auth(&[], json!({"email": " "}))), None);
    }

    #[test]
    fn recognises_anthropic() {
        assert!(is_anthropic_url("https://api.anthropic.com"));
        assert!(is_anthropic_url(
            "HTTPS://API.Anthropic.com:443/v1/messages?beta=true"
        ));
        assert!(!is_anthropic_url("http://api.anthropic.com"));
        assert!(!is_anthropic_url("https://api.anthropic.com:8443"));
        assert!(!is_anthropic_url("https://user@api.anthropic.com"));
        assert!(!is_anthropic_url("https://api.anthropic.com.example"));
        assert!(!is_anthropic_url("http://127.0.0.1:1234"));
        assert!(!is_anthropic_url(""));
    }

    struct Catalog;

    impl ModelCatalog for Catalog {
        fn model_providers(&self, model: &str) -> Vec<String> {
            match model {
                "claude-known" | "claude-unlimited" => vec!["Claude".into()],
                _ => vec!["codex".into()],
            }
        }
        fn first_available_model(&self) -> Option<String> {
            None
        }
        fn available_models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "claude-known".into(),
                max_completion_tokens: 64000,
                ..ModelInfo::default()
            }]
        }
    }

    // TestEnsureModelMaxTokens_*.
    #[test]
    fn fills_in_max_tokens() {
        let mut body = json!({});
        ensure_model_max_tokens(&mut body, " claude-known ", Some(&Catalog));
        assert_eq!(body["max_tokens"], 64000);

        let mut body = json!({});
        ensure_model_max_tokens(&mut body, "claude-unlimited", Some(&Catalog));
        assert_eq!(body["max_tokens"], 1024);

        let mut body = json!({"max_tokens": null});
        ensure_model_max_tokens(&mut body, "claude-known", Some(&Catalog));
        assert_eq!(body["max_tokens"], Value::Null);

        let mut body = json!({});
        ensure_model_max_tokens(&mut body, "gpt-5", Some(&Catalog));
        ensure_model_max_tokens(&mut body, "claude-known", None);
        assert_eq!(body, json!({}));
    }

    // TestDisableThinkingIfToolChoiceForced and the sampling cases of
    // TestNormalizeClaudeSamplingForUpstream for an unrecognised client.
    #[test]
    fn forced_tool_choice_and_sampling() {
        for choice in ["any", "tool"] {
            let mut body = json!({
                "tool_choice": {"type": choice},
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "high"}
            });
            disable_thinking_if_tool_choice_forced(&mut body);
            assert_eq!(body, json!({"tool_choice": {"type": choice}}));
        }
        let mut body = json!({
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "enabled"},
            "output_config": {"effort": "high", "format": {}}
        });
        let unchanged = body.clone();
        disable_thinking_if_tool_choice_forced(&mut body);
        assert_eq!(body, unchanged);
        let mut body = json!({
            "tool_choice": {"type": "tool"},
            "output_config": {"effort": "high", "format": {}}
        });
        disable_thinking_if_tool_choice_forced(&mut body);
        assert_eq!(body["output_config"], json!({"format": {}}));

        let mut body = json!({
            "temperature": 0.2, "top_p": 0.9, "top_k": 5,
            "thinking": {"type": " Adaptive "}
        });
        normalize_sampling(&mut body);
        assert_eq!(body, json!({"thinking": {"type": " Adaptive "}}));
        let mut body = json!({"temperature": 0.2, "top_k": 5, "thinking": {"type": "disabled"}});
        normalize_sampling(&mut body);
        assert_eq!(body, json!({"top_k": 5, "thinking": {"type": "disabled"}}));
    }

    #[test]
    fn lifts_body_betas() {
        let mut body = json!({"betas": [" a ", "", 3], "model": "m"});
        assert_eq!(extract_and_remove_betas(&mut body), ["a", "3"]);
        assert_eq!(body, json!({"model": "m"}));
        let mut body = json!({"betas": " b "});
        assert_eq!(extract_and_remove_betas(&mut body), ["b"]);
        assert_eq!(body, json!({}));
        let mut body = json!({"model": "m"});
        assert!(extract_and_remove_betas(&mut body).is_empty());
    }

    // TestSanitizeClaudeWebSearchDomains.
    #[test]
    fn drops_empty_web_search_domains() {
        let mut body = json!({"tools": [
            {"type": "web_search_20250305", "name": "web_search", "allowed_domains": [], "blocked_domains": ["x.com"]},
            {"type": "web_search_20260209", "name": "web_search", "blocked_domains": []},
            {"type": "custom", "allowed_domains": []}
        ]});
        sanitize_web_search_domains(&mut body);
        assert_eq!(
            body,
            json!({"tools": [
                {"type": "web_search_20250305", "name": "web_search", "blocked_domains": ["x.com"]},
                {"type": "web_search_20260209", "name": "web_search"},
                {"type": "custom", "allowed_domains": []}
            ]})
        );
    }

    // TestEnsureCacheControl, TestInjectToolsCacheControl,
    // TestInjectSystemCacheControl and TestInjectMessagesCacheControl.
    #[test]
    fn places_cache_breakpoints() {
        let mut body = json!({
            "system": "be brief",
            "tools": [{"name": "a"}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "text", "text": "x"}, {"type": "thinking", "thinking": "t"}]}
            ]
        });
        assert_eq!(count_cache_controls(&body), 0);
        ensure_cache_control(&mut body);
        assert_eq!(
            body,
            json!({
                "system": [{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}],
                "tools": [{"name": "a"}],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "x"}, {"type": "thinking", "thinking": "t"}]}
                ]
            })
        );
        assert_eq!(count_cache_controls(&body), 2);

        // Without a system prompt the last tool that isn't deferred covers
        // the prefix, and a final system turn takes the rolling breakpoint.
        let mut body = json!({
            "system": " ",
            "tools": [{"name": "a"}, {"name": "b"}, {"name": "c", "defer_loading": true}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}]},
                {"role": "system", "content": "late"}
            ]
        });
        ensure_cache_control(&mut body);
        assert_eq!(body["system"], " ");
        assert_eq!(
            body["tools"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(body["tools"][2].get("cache_control").is_none());
        assert_eq!(
            body["messages"][1]["content"],
            json!([{"type": "text", "text": "late", "cache_control": {"type": "ephemeral"}}])
        );
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );

        // Existing breakpoints are left alone in each place.
        let mut body = json!({
            "system": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}, {"type": "text", "text": "b"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "q", "cache_control": {"type": "ephemeral"}}, {"type": "text", "text": "r"}]}]
        });
        let unchanged = body.clone();
        ensure_cache_control(&mut body);
        assert_eq!(body, unchanged);

        // No user or assistant turn can carry it: nothing in the messages.
        let mut body = json!({"messages": [{"role": "user", "content": []}, {"role": "system", "content": "x"}]});
        let unchanged = body.clone();
        ensure_cache_control(&mut body);
        assert_eq!(body, unchanged);
    }

    // TestEnforceCacheControlLimit.
    #[test]
    fn enforces_the_breakpoint_limit() {
        let cc = || json!({"type": "ephemeral"});
        let mut body = json!({
            "system": [{"text": "s1", "cache_control": cc()}, {"text": "s2", "cache_control": cc()}],
            "tools": [{"name": "t1", "cache_control": cc()}, {"name": "t2", "cache_control": cc()}],
            "messages": [{"role": "user", "content": [{"text": "m1", "cache_control": cc()}, {"text": "m2", "cache_control": cc()}]}]
        });
        enforce_cache_control_limit(&mut body, MAX_CACHE_BREAKPOINTS);
        assert_eq!(count_cache_controls(&body), 4);
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(body["tools"][0].get("cache_control").is_none());
        assert!(body["system"][1].get("cache_control").is_some());
        assert!(body["tools"][1].get("cache_control").is_some());

        enforce_cache_control_limit(&mut body, 1);
        assert_eq!(count_cache_controls(&body), 1);
        assert!(body["tools"][1].get("cache_control").is_some());
    }

    // TestNormalizeCacheControlTTL and TestClaudePayloadHas1hTTL.
    #[test]
    fn orders_cache_ttls() {
        let mut body = json!({
            "tools": [{"cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "system": [{"cache_control": {"type": "ephemeral"}}, {"cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "messages": [{"content": [{"cache_control": {"type": "ephemeral", "ttl": "1h"}}]}]
        });
        assert!(payload_has_1h_ttl(&body));
        normalize_cache_control_ttl(&mut body);
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
        assert!(body["system"][1]["cache_control"].get("ttl").is_none());
        assert!(
            body["messages"][0]["content"][0]["cache_control"]
                .get("ttl")
                .is_none()
        );
        assert!(payload_has_1h_ttl(&body));
        assert!(!payload_has_1h_ttl(
            &json!({"system": "x", "messages": [{"content": "y"}]})
        ));
    }
}
