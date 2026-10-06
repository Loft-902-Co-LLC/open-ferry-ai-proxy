// Ported from CLIProxyAPI internal/runtime/executor/helps/usage_helpers.go
// (StreamUsageBuffer, ParseCodexUsage, ParseCodexImageToolUsage,
// ParseOpenAIUsage,
// hasOpenAIStyleUsageTokenFields, hasOpenAIStyleUsageBucketFields,
// parseOpenAIStyleUsageNode, ParseOpenAIStreamUsage, ParseClaudeUsage,
// ParseClaudeStreamUsage, parseClaudeUsageNode,
// parseGeminiFamilyUsageDetail, ParseGeminiUsage, ParseGeminiStreamUsage,
// parseInteractionsUsageDetail, ParseInteractionsUsage,
// ParseInteractionsStreamUsage, hasNonZeroTokenUsage, extractResponseServiceTier,
// extractResponseServiceTierFromValidJSON, firstExistingUsageNode,
// safeUsageTokenSum, jsonPayload) and plugin_executor_usage.go
// (ObserveMergedStreamUsage, MergeStreamUsageDetail) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The token counts in an upstream's answer: a whole answer's, or a
//! stream's, line by line, in each provider's protocol.
//!
//! OpenAI's protocols (Chat Completions, Responses and Codex) write
//! `usage` with `prompt_tokens` or `input_tokens` and their details;
//! Claude's write `usage` with its cache buckets apart, in
//! `message_start` and again in `message_delta`; Gemini's write
//! `usageMetadata`; Gemini Interactions' write `usage` with `input_tokens`
//! or `total_input_tokens`, beside the interaction or inside it. A
//! stream's counts are kept in a [`StreamUsageBuffer`], the latest winning,
//! Claude's merged.
//!
//! Codex's image generation tool writes its own counts, OpenAI-style, in
//! the terminal event's `response.tool_usage.image_gen`
//! ([`parse_codex_image_tool_usage`]).
//!
//! Deviations from upstream:
//! - JSON that doesn't parse whole has no counts (see [`super::json`]).
//! - Antigravity's counts, and the plugin executors', aren't parsed: those
//!   aren't ported yet.

use super::accounting::{Detail, TokenBreakdown, non_negative_sum};
use super::json::{self, Doc, Node};
use super::response_model::{
    extract_claude_response_model_event, extract_generic_response_model_event,
};

/// The JSON object an SSE line or frame carries, without `data:`; none for
/// a blank line, `[DONE]`, an `event:` line or anything not an object
/// (upstream's `jsonPayload`).
pub(crate) fn json_payload(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = json::trim_space(line);
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = json::trim_space(rest);
    }
    (trimmed.first() == Some(&b'{')).then_some(trimmed)
}

/// The service tier an answer says it was served at, empty when it says
/// none (upstream's `extractResponseServiceTier`).
pub(crate) fn extract_response_service_tier(payload: &[u8]) -> String {
    if payload.is_empty() || !json::valid(payload) {
        return String::new();
    }
    service_tier_of(&Doc::scan(payload))
}

/// The first service tier written, trimmed (upstream's
/// `extractResponseServiceTierFromValidJSON`).
fn service_tier_of(doc: &Doc) -> String {
    [
        "response.service_tier",
        "service_tier",
        "interaction.service_tier",
    ]
    .into_iter()
    .map(|path| doc.get(path).string().trim().to_owned())
    .find(|tier| !tier.is_empty())
    .unwrap_or_default()
}

/// The first of `paths` below `node` that exists.
fn first_existing<'a>(node: Node<'a>, paths: &[&str]) -> Node<'a> {
    paths
        .iter()
        .map(|path| node.get(path))
        .find(|found| found.exists())
        .unwrap_or_default()
}

/// The token buckets an OpenAI-style `usage` may have.
const OPENAI_BUCKET_FIELDS: [&str; 12] = [
    "prompt_tokens",
    "input_tokens",
    "completion_tokens",
    "output_tokens",
    "prompt_tokens_details.cached_tokens",
    "input_tokens_details.cached_tokens",
    "prompt_tokens_details.cache_write_tokens",
    "prompt_tokens_details.cache_creation_tokens",
    "input_tokens_details.cache_write_tokens",
    "input_tokens_details.cache_creation_tokens",
    "completion_tokens_details.reasoning_tokens",
    "output_tokens_details.reasoning_tokens",
];

/// Whether `usage` is an object with a total or a bucket (upstream's
/// `hasOpenAIStyleUsageTokenFields`).
fn has_openai_token_fields(usage: Node<'_>) -> bool {
    usage.is_object() && (usage.get("total_tokens").exists() || has_openai_bucket_fields(usage))
}

/// Whether `usage` has a bucket (upstream's
/// `hasOpenAIStyleUsageBucketFields`).
fn has_openai_bucket_fields(usage: Node<'_>) -> bool {
    OPENAI_BUCKET_FIELDS
        .iter()
        .any(|path| usage.get(path).exists())
}

/// The counts of an OpenAI-style `usage`, cache and reasoning counted
/// inside input and output (upstream's `parseOpenAIStyleUsageNode`).
fn parse_openai_usage_node(usage: Node<'_>) -> Detail {
    let input = first_existing(usage, &["prompt_tokens", "input_tokens"]);
    let output = first_existing(usage, &["completion_tokens", "output_tokens"]);
    let mut detail = Detail {
        input_tokens: input.int(),
        output_tokens: output.int(),
        total_tokens: usage.get("total_tokens").int(),
        ..Detail::default()
    };
    let cached = first_existing(
        usage,
        &[
            "prompt_tokens_details.cached_tokens",
            "input_tokens_details.cached_tokens",
        ],
    );
    if cached.exists() {
        detail.cached_tokens = cached.int();
        detail.cache_read_tokens = cached.int();
    }
    let cache_creation = first_existing(
        usage,
        &[
            "input_tokens_details.cache_creation_tokens",
            "input_tokens_details.cache_write_tokens",
            "prompt_tokens_details.cache_creation_tokens",
            "prompt_tokens_details.cache_write_tokens",
        ],
    );
    if cache_creation.exists() {
        detail.cache_creation_tokens = cache_creation.int();
    }
    let reasoning = first_existing(
        usage,
        &[
            "completion_tokens_details.reasoning_tokens",
            "output_tokens_details.reasoning_tokens",
        ],
    );
    if reasoning.exists() {
        detail.reasoning_tokens = reasoning.int();
    }
    detail.token_breakdown = if !has_openai_bucket_fields(usage) {
        TokenBreakdown::unclassified(detail.total_tokens)
    } else if input.exists() && output.exists() {
        TokenBreakdown::subset(
            detail.input_tokens,
            detail.cache_read_tokens,
            detail.cache_creation_tokens,
            detail.output_tokens,
            detail.reasoning_tokens,
            detail.total_tokens,
        )
    } else {
        let (cache_read, cache_creation) = if input.exists() {
            (detail.cache_read_tokens, detail.cache_creation_tokens)
        } else {
            (0, 0)
        };
        let reasoning = if output.exists() {
            detail.reasoning_tokens
        } else {
            0
        };
        TokenBreakdown::partial_subset(
            detail.input_tokens,
            cache_read,
            cache_creation,
            detail.output_tokens,
            reasoning,
            detail.total_tokens,
        )
    };
    if detail.total_tokens == 0 {
        detail.total_tokens = detail.token_breakdown.total_tokens;
    }
    detail
}

/// The counts of an OpenAI-style answer's `usage` at `path`, and its
/// service tier; `None` with neither.
fn parse_openai_style(doc: &Doc, path: &str, tier: String) -> Option<Detail> {
    let usage = doc.get(path);
    if !has_openai_token_fields(usage) {
        return (!tier.is_empty()).then(|| Detail {
            response_service_tier: tier,
            ..Detail::default()
        });
    }
    let mut detail = parse_openai_usage_node(usage);
    detail.response_service_tier = tier;
    Some(detail)
}

/// The counts in a Codex `response.completed` event, or its service tier
/// alone; `None` with neither (upstream's `ParseCodexUsage`).
pub fn parse_codex_usage(data: &[u8]) -> Option<Detail> {
    let tier = extract_response_service_tier(data);
    parse_openai_style(&Doc::parse(data), "response.usage", tier)
}

/// The image generation tool's counts in a Codex terminal event's
/// `response.tool_usage.image_gen`; `None` when it has neither a total nor
/// a bucket (upstream's `ParseCodexImageToolUsage`).
pub fn parse_codex_image_tool_usage(data: &[u8]) -> Option<Detail> {
    let doc = Doc::parse(data);
    let usage = doc.get("response.tool_usage.image_gen");
    has_openai_token_fields(usage).then(|| parse_openai_usage_node(usage))
}

/// The counts in an OpenAI-style answer, or nothing but its service tier
/// (upstream's `ParseOpenAIUsage`).
pub fn parse_openai_usage(data: &[u8]) -> Detail {
    let tier = extract_response_service_tier(data);
    match parse_openai_style(&Doc::parse(data), "usage", tier.clone()) {
        Some(detail) => detail,
        None => Detail {
            response_service_tier: tier,
            ..Detail::default()
        },
    }
}

/// The counts in an OpenAI-style stream line, or its service tier alone;
/// `None` with neither (upstream's `ParseOpenAIStreamUsage`).
pub fn parse_openai_stream_usage(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line).filter(|payload| json::valid(payload))?;
    let tier = extract_response_service_tier(payload);
    parse_openai_style(&Doc::scan(payload), "usage", tier)
}

/// The counts in a Claude answer's `usage` (upstream's `ParseClaudeUsage`).
pub fn parse_claude_usage(data: &[u8]) -> Detail {
    let doc = Doc::parse(data);
    let usage = doc.get("usage");
    if usage.exists() {
        parse_claude_usage_node(usage)
    } else {
        Detail::default()
    }
}

/// The counts in a Claude stream line's `usage` or `message.usage`
/// (upstream's `ParseClaudeStreamUsage`).
pub fn parse_claude_stream_usage(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line).filter(|payload| json::valid(payload))?;
    let doc = Doc::scan(payload);
    let usage = first_existing(doc.root(), &["usage", "message.usage"]);
    usage.exists().then(|| parse_claude_usage_node(usage))
}

/// The counts of a Claude `usage`, its cache buckets apart from the input
/// and thinking inside the output (upstream's `parseClaudeUsageNode`).
fn parse_claude_usage_node(usage: Node<'_>) -> Detail {
    let cache_read = usage.get("cache_read_input_tokens").int();
    let cache_creation = usage.get("cache_creation_input_tokens").int();
    let raw_output = usage.get("output_tokens").int();
    let reasoning = first_existing(
        usage,
        &[
            "output_tokens_details.thinking_tokens",
            "output_tokens_details.reasoning_tokens",
            "thinking_tokens",
        ],
    )
    .int()
    .max(0);
    let non_reasoning = if reasoning > 0 && reasoning <= raw_output {
        raw_output - reasoning
    } else if reasoning > raw_output {
        0
    } else {
        raw_output
    };
    let mut detail = Detail {
        input_tokens: usage.get("input_tokens").int(),
        output_tokens: raw_output,
        reasoning_tokens: reasoning,
        cached_tokens: cache_read,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        ..Detail::default()
    };
    if detail.cached_tokens == 0 {
        detail.cached_tokens = detail.cache_creation_tokens;
    }
    detail.total_tokens = detail
        .input_tokens
        .wrapping_add(raw_output)
        .wrapping_add(detail.cache_read_tokens)
        .wrapping_add(detail.cache_creation_tokens);
    detail.token_breakdown = TokenBreakdown::independent(
        detail.input_tokens,
        detail.cache_read_tokens,
        detail.cache_creation_tokens,
        non_reasoning,
        detail.reasoning_tokens,
        detail.total_tokens,
    );
    detail
}

/// The counts of a Gemini `usageMetadata`, cache inside the input and
/// thoughts beside the output (upstream's `parseGeminiFamilyUsageDetail`).
fn parse_gemini_usage_node(node: Node<'_>) -> Detail {
    let cached = node.get("cachedContentTokenCount").int();
    let tool_use = first_existing(
        node,
        &["toolUsePromptTokenCount", "tool_use_prompt_token_count"],
    )
    .int();
    let input = non_negative_sum(&[node.get("promptTokenCount").int(), tool_use]);
    let mut detail = Detail {
        input_tokens: input.unwrap_or(0),
        output_tokens: node.get("candidatesTokenCount").int(),
        reasoning_tokens: node.get("thoughtsTokenCount").int(),
        total_tokens: node.get("totalTokenCount").int(),
        cached_tokens: cached,
        cache_read_tokens: cached,
        ..Detail::default()
    };
    if input.is_none() {
        detail.token_breakdown = TokenBreakdown::invalid(detail.total_tokens);
        return detail;
    }
    if detail.total_tokens == 0 {
        match non_negative_sum(&[
            detail.input_tokens,
            detail.output_tokens,
            detail.reasoning_tokens,
        ]) {
            Some(total) => detail.total_tokens = total,
            None => {
                detail.total_tokens = 0;
                detail.token_breakdown = TokenBreakdown::invalid(0);
                return detail;
            }
        }
    }
    detail.token_breakdown = TokenBreakdown::separate_reasoning(
        detail.input_tokens,
        detail.cache_read_tokens,
        detail.cache_creation_tokens,
        detail.output_tokens,
        detail.reasoning_tokens,
        detail.total_tokens,
    );
    detail
}

/// The counts in a Gemini answer's `usageMetadata` or `usage_metadata`
/// (upstream's `ParseGeminiUsage`).
pub fn parse_gemini_usage(data: &[u8]) -> Detail {
    let doc = Doc::parse(data);
    let node = first_existing(doc.root(), &["usageMetadata", "usage_metadata"]);
    if node.exists() {
        parse_gemini_usage_node(node)
    } else {
        Detail::default()
    }
}

/// The counts in a Gemini stream line, when it has any (upstream's
/// `ParseGeminiStreamUsage`).
pub fn parse_gemini_stream_usage(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line).filter(|payload| json::valid(payload))?;
    let doc = Doc::scan(payload);
    let node = first_existing(doc.root(), &["usageMetadata", "usage_metadata"]);
    if !node.exists() {
        return None;
    }
    let detail = parse_gemini_usage_node(node);
    detail.has_tokens().then_some(detail)
}

/// Where an Interactions answer or event keeps its counts, in the order
/// they are looked for.
const INTERACTIONS_USAGE_PATHS: [&str; 9] = [
    "usage",
    "total_usage",
    "metadata.total_usage",
    "metadata.usage",
    "usageMetadata",
    "usage_metadata",
    "interaction.usage",
    "interaction.total_usage",
    "interaction.metadata.total_usage",
];

/// The counts of an Interactions `usage`, tool use counted in the input,
/// cache inside it and reasoning beside the output (upstream's
/// `parseInteractionsUsageDetail`).
fn parse_interactions_usage_detail(node: Node<'_>) -> Detail {
    let cache_read = first_existing(node, &["cache_read_tokens", "cacheReadTokens"]);
    let tool_use = first_existing(
        node,
        &[
            "tool_use_tokens",
            "total_tool_use_tokens",
            "toolUseTokens",
            "totalToolUseTokens",
        ],
    )
    .int();
    let input = non_negative_sum(&[
        first_existing(
            node,
            &["input_tokens", "prompt_tokens", "total_input_tokens"],
        )
        .int(),
        tool_use,
    ]);
    let mut detail = Detail {
        input_tokens: input.unwrap_or(0),
        output_tokens: first_existing(
            node,
            &["output_tokens", "completion_tokens", "total_output_tokens"],
        )
        .int(),
        reasoning_tokens: first_existing(
            node,
            &[
                "reasoning_tokens",
                "thoughtsTokenCount",
                "total_thought_tokens",
            ],
        )
        .int(),
        total_tokens: first_existing(node, &["total_tokens", "totalTokenCount"]).int(),
        cached_tokens: first_existing(
            node,
            &[
                "cached_tokens",
                "cachedContentTokenCount",
                "total_cached_tokens",
            ],
        )
        .int(),
        cache_read_tokens: cache_read.int(),
        cache_creation_tokens: first_existing(
            node,
            &[
                "cache_creation_tokens",
                "cacheCreationTokens",
                "cache_write_tokens",
                "cacheWriteTokens",
            ],
        )
        .int(),
        ..Detail::default()
    };
    if input.is_none() {
        detail.token_breakdown = TokenBreakdown::invalid(detail.total_tokens);
        return detail;
    }
    if !cache_read.exists() && detail.cached_tokens > 0 {
        detail.cache_read_tokens = detail.cached_tokens;
    }
    if detail.total_tokens == 0 {
        match non_negative_sum(&[
            detail.input_tokens,
            detail.output_tokens,
            detail.reasoning_tokens,
        ]) {
            Some(total) => detail.total_tokens = total,
            None => {
                detail.total_tokens = 0;
                detail.token_breakdown = TokenBreakdown::invalid(0);
                return detail;
            }
        }
    }
    detail.token_breakdown = TokenBreakdown::separate_reasoning(
        detail.input_tokens,
        detail.cache_read_tokens,
        detail.cache_creation_tokens,
        detail.output_tokens,
        detail.reasoning_tokens,
        detail.total_tokens,
    );
    detail
}

/// The counts in a Gemini Interactions answer or event: its `usage`, its
/// `total_usage` or its interaction's, read as Gemini's `usageMetadata`
/// when it has Gemini's counts (upstream's `ParseInteractionsUsage`).
pub fn parse_interactions_usage(data: &[u8]) -> Detail {
    let doc = Doc::parse(data);
    let node = first_existing(doc.root(), &INTERACTIONS_USAGE_PATHS);
    if !node.exists() {
        return Detail::default();
    }
    let mut detail =
        if node.get("promptTokenCount").exists() || node.get("candidatesTokenCount").exists() {
            parse_gemini_usage_node(node)
        } else {
            parse_interactions_usage_detail(node)
        };
    detail.response_service_tier = extract_response_service_tier(data);
    detail
}

/// The counts in a Gemini Interactions stream line, a `data:` line or bare
/// JSON, when it has any (upstream's `ParseInteractionsStreamUsage`).
pub fn parse_interactions_stream_usage(line: &[u8]) -> Option<Detail> {
    let payload = json_payload(line).unwrap_or(line);
    if payload.is_empty() || !json::valid(payload) {
        return None;
    }
    let detail = parse_interactions_usage(payload);
    detail.has_tokens().then_some(detail)
}

/// `update` with what `existing` knew and it lacks, its total at least
/// their sum, and its breakdown rebuilt with Claude's semantics (upstream's
/// `MergeStreamUsageDetail`).
pub fn merge_stream_usage_detail(existing: &Detail, update: Detail) -> Detail {
    let mut merged = update;
    let keep = |merged: &mut i64, existing: i64| {
        if *merged == 0 && existing > 0 {
            *merged = existing;
        }
    };
    keep(&mut merged.input_tokens, existing.input_tokens);
    keep(&mut merged.cached_tokens, existing.cached_tokens);
    keep(&mut merged.cache_read_tokens, existing.cache_read_tokens);
    keep(
        &mut merged.cache_creation_tokens,
        existing.cache_creation_tokens,
    );
    keep(&mut merged.output_tokens, existing.output_tokens);
    keep(&mut merged.reasoning_tokens, existing.reasoning_tokens);
    if merged.response_service_tier.is_empty() {
        merged
            .response_service_tier
            .clone_from(&existing.response_service_tier);
    }
    let mut cached = merged
        .cache_read_tokens
        .wrapping_add(merged.cache_creation_tokens);
    if cached == 0 {
        cached = merged.cached_tokens;
    }
    let calculated = merged
        .input_tokens
        .wrapping_add(merged.output_tokens)
        .wrapping_add(cached);
    if merged.total_tokens == 0 || merged.total_tokens < calculated {
        merged.total_tokens = calculated;
    }
    let non_reasoning = merged
        .output_tokens
        .wrapping_sub(merged.reasoning_tokens)
        .max(0);
    merged.token_breakdown = TokenBreakdown::independent(
        merged.input_tokens,
        merged.cache_read_tokens,
        merged.cache_creation_tokens,
        non_reasoning,
        merged.reasoning_tokens,
        merged.total_tokens,
    );
    merged
}

/// The latest token counts a stream gave, and the model it named
/// (upstream's `StreamUsageBuffer`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamUsageBuffer {
    detail: Detail,
    ok: bool,
    response_model: String,
}

impl StreamUsageBuffer {
    /// Keeps `detail`, when given: counts replace what was kept, keeping
    /// the service tier when they name none, and a tier alone only updates
    /// the tier (upstream's `Observe`).
    pub fn observe(&mut self, detail: Option<Detail>) {
        let Some(detail) = detail else {
            return;
        };
        let tier = detail.response_service_tier.trim();
        if tier.is_empty() || detail.has_tokens() {
            let preserved = std::mem::take(&mut self.detail.response_service_tier);
            self.detail = detail;
            if self.detail.response_service_tier.is_empty() {
                self.detail.response_service_tier = preserved;
            }
        } else {
            self.detail.response_service_tier = tier.to_owned();
        }
        self.ok = true;
    }

    /// Reads an OpenAI-style stream line: its model, the first time one is
    /// named, its counts and its service tier, parsing only lines that may
    /// hold them (upstream's `ObserveOpenAIStream`).
    pub fn observe_openai_stream(&mut self, line: &[u8]) {
        let Some(payload) = json_payload(line) else {
            return;
        };
        let has_usage = contains(payload, br#""usage""#);
        let need_tier = self.detail.response_service_tier.is_empty() || has_usage;
        let has_tier = need_tier && contains(payload, br#""service_tier""#);
        if self.response_model.is_empty() {
            let (model, _) = extract_generic_response_model_event(payload);
            if !model.is_empty() {
                self.response_model = model;
            }
        }
        if (!has_usage && !has_tier) || !json::valid(payload) {
            return;
        }
        let doc = Doc::scan(payload);
        let mut detail = Detail::default();
        let mut usage_ok = false;
        if has_usage {
            let usage = doc.get("usage");
            if has_openai_token_fields(usage) {
                detail = parse_openai_usage_node(usage);
                usage_ok = true;
            }
        }
        if has_tier {
            detail.response_service_tier = service_tier_of(&doc);
        }
        if usage_ok || !detail.response_service_tier.is_empty() {
            self.observe(Some(detail));
        }
    }

    /// Reads a Claude stream line: its model, the first time one is named,
    /// and its counts, merged with those kept (upstream's
    /// `ObserveClaudeStream`).
    pub fn observe_claude_stream(&mut self, line: &[u8]) {
        if self.response_model.is_empty()
            && let Some(payload) = json_payload(line)
        {
            let (model, _) = extract_claude_response_model_event(payload);
            if !model.is_empty() {
                self.response_model = model;
            }
        }
        if let Some(update) = parse_claude_stream_usage(line) {
            self.observe_merged(update);
        }
    }

    /// Keeps `update` merged with the counts kept (upstream's
    /// `ObserveMergedStreamUsage`).
    pub fn observe_merged(&mut self, update: Detail) {
        let merged = match self.detail() {
            Some(existing) => merge_stream_usage_detail(existing, update),
            None => update,
        };
        self.observe(Some(merged));
    }

    /// The counts kept, if any were (upstream's `Detail`).
    pub fn detail(&self) -> Option<&Detail> {
        self.ok.then_some(&self.detail)
    }

    /// The counts kept, given or not.
    pub(crate) fn raw_detail(&self) -> &Detail {
        &self.detail
    }

    /// The model the stream named, empty when none (upstream's
    /// `ResponseModel`).
    pub fn response_model(&self) -> &str {
        &self.response_model
    }
}

/// Whether `haystack` holds `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
