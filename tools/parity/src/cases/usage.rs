//! Hand-written cases for usage parsing: the bodies and lines upstream's
//! tests parse (internal/runtime/executor/helps/usage_helpers_test.go),
//! and, not upstream's, the corners of each parser: counts written as
//! floats, strings, booleans or past `i64`, usage that isn't an object,
//! the service tier's places and types, and the SSE framings the stream
//! parsers read.

use serde_json::json;

use super::Case;

/// The hand-written cases for `usage/parse`.
pub fn parses() -> Vec<Case> {
    UPSTREAM
        .iter()
        .chain(MORE)
        .map(|(parser, name, body)| {
            Case::new(format!("{parser}-{name}"), "", *body)
                .with_options(json!({ "parser": parser }))
        })
        .collect()
}

/// The parser, a name, and the body or line, from upstream's tests.
const UPSTREAM: &[(&str, &str, &str)] = &[
    (
        "openai",
        "chat-completions",
        r#"{"usage":{"prompt_tokens":10,"completion_tokens":6,"total_tokens":16,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":5}}}"#,
    ),
    (
        "openai",
        "responses",
        r#"{"service_tier":"default","usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"input_tokens_details":{"cached_tokens":7},"output_tokens_details":{"reasoning_tokens":9}}}"#,
    ),
    (
        "openai",
        "total-only-is-unclassified",
        r#"{"usage":{"total_tokens":42}}"#,
    ),
    (
        "openai",
        "partial-buckets-preserve-known-tokens",
        r#"{"usage":{"input_tokens":10,"total_tokens":15}}"#,
    ),
    (
        "openai",
        "explicit-zero-buckets-remain-inconsistent",
        r#"{"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":42}}"#,
    ),
    (
        "codex",
        "includes-cache-write-tokens",
        r#"{"response":{"service_tier":"priority","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":40}}}}"#,
    ),
    (
        "openai",
        "normalizes-cache-creation-alias",
        r#"{"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12,"input_tokens_details":{"cache_creation_tokens":4}}}"#,
    ),
    ("openai", "ignores-null-usage", r#"{"usage":null}"#),
    (
        "openai",
        "preserves-response-tier-without-usage",
        r#"{"service_tier":"default"}"#,
    ),
    (
        "codex",
        "preserves-response-tier-without-usage",
        r#"{"response":{"service_tier":"default"}}"#,
    ),
    (
        "openai-stream",
        "ignores-null-usage",
        r#"data: {"id":"chunk_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}],"usage":null}"#,
    ),
    (
        "openai-stream",
        "responses-fields",
        r#"data: {"id":"chunk_1","object":"chat.completion.chunk","service_tier":"flex","choices":[],"usage":{"input_tokens":8,"output_tokens":5,"total_tokens":13,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}}"#,
    ),
    (
        "openai-stream",
        "buffer-tier-only",
        r#"data: {"service_tier":"default"}"#,
    ),
    (
        "openai-stream",
        "buffer-usage-only",
        r#"data: {"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
    ),
    (
        "openai-stream",
        "buffer-tier-and-usage",
        r#"data: {"service_tier":"flex","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}"#,
    ),
    (
        "openai-stream",
        "buffer-usage-in-text",
        r#"data: {"content":"the word \"usage\" appears here"}"#,
    ),
    ("openai-stream", "buffer-cut-short", r#"data: {"usage":"#),
    (
        "openai-stream",
        "buffer-null-usage",
        r#"data: {"usage":null}"#,
    ),
    (
        "openai-stream",
        "buffer-zero-usage",
        r#"data: {"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}"#,
    ),
    (
        "claude",
        "includes-cache-tokens-in-total",
        r#"{"usage":{"input_tokens":3085,"output_tokens":253,"cache_read_input_tokens":7,"cache_creation_input_tokens":19514}}"#,
    ),
    (
        "claude",
        "falls-back-cached-tokens-to-cache-creation",
        r#"{"usage":{"input_tokens":3085,"output_tokens":253,"cache_creation_input_tokens":19514}}"#,
    ),
    (
        "claude",
        "preserves-thinking-tokens-as-reasoning-subset",
        r#"{"usage":{"input_tokens":2,"cache_creation_input_tokens":831,"cache_read_input_tokens":44225,"output_tokens":244,"output_tokens_details":{"thinking_tokens":40}}}"#,
    ),
    (
        "claude-stream",
        "preserves-thinking-tokens-as-reasoning-subset",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":2,"cache_creation_input_tokens":831,"cache_read_input_tokens":44225,"output_tokens":244,"output_tokens_details":{"thinking_tokens":40}}}"#,
    ),
    (
        "claude-stream",
        "message-start",
        r#"data: {"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[],"model":"claude-opus-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":2095,"cache_creation_input_tokens":7185,"cache_read_input_tokens":355598,"output_tokens":1}}}"#,
    ),
    (
        "claude",
        "falls-back-to-top-level-thinking-tokens",
        r#"{"usage":{"input_tokens":3,"output_tokens":10,"thinking_tokens":4}}"#,
    ),
    (
        "claude-stream",
        "merge-start",
        r#"data: {"type":"message_start","message":{"id":"msg_123","model":"claude-opus-5","usage":{"input_tokens":2095,"cache_creation_input_tokens":7185,"cache_read_input_tokens":355598,"output_tokens":1}}}"#,
    ),
    (
        "claude-stream",
        "merge-delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#,
    ),
    (
        "gemini",
        "normalizes-cached-content",
        r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"cachedContentTokenCount":4,"totalTokenCount":12}}"#,
    ),
    (
        "gemini",
        "includes-tool-use-prompt-tokens",
        r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":3,"toolUsePromptTokenCount":5,"totalTokenCount":20}}"#,
    ),
    (
        "gemini-stream",
        "skips-zero-placeholder",
        r#"data: {"usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":0,"thoughtsTokenCount":0,"totalTokenCount":0}}"#,
    ),
    (
        "gemini-stream",
        "real-usage",
        r#"data: {"usageMetadata":{"promptTokenCount":17984,"candidatesTokenCount":2668,"thoughtsTokenCount":1028,"totalTokenCount":21680}}"#,
    ),
    (
        "gemini",
        "rejects-negative-tool-use-sum",
        r#"{"usageMetadata":{"promptTokenCount":10,"toolUsePromptTokenCount":-1,"totalTokenCount":10}}"#,
    ),
    (
        "gemini",
        "rejects-overflowing-tool-use-sum",
        r#"{"usageMetadata":{"promptTokenCount":9223372036854775807,"toolUsePromptTokenCount":1,"totalTokenCount":9223372036854775807}}"#,
    ),
];

/// Not upstream's: the parser, a name, and the body or line.
const MORE: &[(&str, &str, &str)] = &[
    ("openai", "empty", ""),
    ("openai", "not-json", "upstream error"),
    ("openai", "array", r#"[{"usage":{"total_tokens":3}}]"#),
    (
        "openai",
        "leading-space",
        " \n {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}",
    ),
    ("openai", "usage-string", r#"{"usage":"12"}"#),
    ("openai", "usage-array", r#"{"usage":[{"total_tokens":3}]}"#),
    ("openai", "usage-empty", r#"{"usage":{}}"#),
    (
        "openai",
        "both-input-names",
        r#"{"usage":{"prompt_tokens":5,"input_tokens":9,"completion_tokens":1,"output_tokens":7}}"#,
    ),
    (
        "openai",
        "both-cache-names",
        r#"{"usage":{"input_tokens":20,"output_tokens":2,"input_tokens_details":{"cache_creation_tokens":3,"cache_write_tokens":4},"prompt_tokens_details":{"cache_creation_tokens":5,"cached_tokens":6}}}"#,
    ),
    (
        "openai",
        "output-only",
        r#"{"usage":{"output_tokens":4,"output_tokens_details":{"reasoning_tokens":1},"input_tokens_details":{"cached_tokens":2}}}"#,
    ),
    (
        "openai",
        "reasoning-over-output",
        r#"{"usage":{"input_tokens":2,"output_tokens":3,"output_tokens_details":{"reasoning_tokens":9}}}"#,
    ),
    (
        "openai",
        "cache-over-input",
        r#"{"usage":{"input_tokens":2,"output_tokens":3,"input_tokens_details":{"cached_tokens":9}}}"#,
    ),
    (
        "openai",
        "total-too-small",
        r#"{"usage":{"input_tokens":20,"output_tokens":30,"total_tokens":10}}"#,
    ),
    (
        "openai",
        "total-too-large",
        r#"{"usage":{"input_tokens":20,"output_tokens":30,"total_tokens":90}}"#,
    ),
    (
        "openai",
        "negative-counts",
        r#"{"usage":{"input_tokens":-5,"output_tokens":3,"total_tokens":-2}}"#,
    ),
    (
        "openai",
        "float-counts",
        r#"{"usage":{"input_tokens":10.9,"output_tokens":1e1,"total_tokens":"21"}}"#,
    ),
    (
        "openai",
        "string-counts",
        r#"{"usage":{"input_tokens":"10","output_tokens":"x","total_tokens":"-3"}}"#,
    ),
    (
        "openai",
        "bool-counts",
        r#"{"usage":{"input_tokens":true,"output_tokens":false,"total_tokens":null}}"#,
    ),
    (
        "openai",
        "huge-counts",
        r#"{"usage":{"input_tokens":9223372036854775807,"output_tokens":1,"total_tokens":1e30}}"#,
    ),
    (
        "openai",
        "past-i64",
        r#"{"usage":{"input_tokens":9223372036854775808,"output_tokens":-1e400,"total_tokens":18446744073709551616}}"#,
    ),
    (
        "openai",
        "tier-trimmed",
        r#"{"service_tier":"  priority  ","usage":{"total_tokens":1}}"#,
    ),
    ("openai", "tier-number", r#"{"service_tier":5}"#),
    ("openai", "tier-float", r#"{"service_tier":1.50}"#),
    ("openai", "tier-bool", r#"{"service_tier":false}"#),
    (
        "openai",
        "tier-blank-then-response",
        r#"{"service_tier":" ","response":{"service_tier":"flex"}}"#,
    ),
    (
        "openai",
        "tier-interaction",
        r#"{"interaction":{"service_tier":"scale"}}"#,
    ),
    (
        "openai",
        "pretty",
        "{\n  \"service_tier\": \"default\",\n  \"usage\": {\n    \"prompt_tokens\": 3,\n    \"completion_tokens\": 4\n  }\n}\n",
    ),
    (
        "codex",
        "completed",
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":100},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":12},"total_tokens":160}}}"#,
    ),
    (
        "codex",
        "no-usage",
        r#"{"type":"response.completed","response":{"id":"resp_1"}}"#,
    ),
    (
        "codex",
        "null-usage",
        r#"{"type":"response.completed","response":{"usage":null}}"#,
    ),
    (
        "codex",
        "top-level-usage",
        r#"{"usage":{"total_tokens":3}}"#,
    ),
    (
        "codex",
        "tier-top-level",
        r#"{"service_tier":"flex","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    ),
    ("codex", "empty", ""),
    ("codex", "not-json", "event: response.completed"),
    ("openai-stream", "empty", ""),
    ("openai-stream", "done", "data: [DONE]"),
    ("openai-stream", "bare-done", "[DONE]"),
    ("openai-stream", "event-line", "event: message"),
    (
        "openai-stream",
        "data-no-space",
        r#"data:{"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    ),
    (
        "openai-stream",
        "bare-json",
        r#"  {"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}  "#,
    ),
    (
        "openai-stream",
        "data-array",
        r#"data: [{"usage":{"total_tokens":2}}]"#,
    ),
    (
        "openai-stream",
        "trailing-text",
        r#"data: {"usage":{"total_tokens":2}} trailing"#,
    ),
    (
        "openai-stream",
        "chat-final-chunk",
        r#"data: {"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"prompt_tokens_details":{"cached_tokens":5,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":3}}}"#,
    ),
    (
        "openai-stream",
        "tier-in-response",
        r#"data: {"response":{"service_tier":"priority"}}"#,
    ),
    ("claude", "empty", ""),
    ("claude", "no-usage", r#"{"type":"message","content":[]}"#),
    ("claude", "null-usage", r#"{"usage":null}"#),
    ("claude", "usage-string", r#"{"usage":"5"}"#),
    (
        "claude",
        "reasoning-details",
        r#"{"usage":{"input_tokens":3,"output_tokens":10,"output_tokens_details":{"reasoning_tokens":6}}}"#,
    ),
    (
        "claude",
        "thinking-over-output",
        r#"{"usage":{"input_tokens":3,"output_tokens":10,"thinking_tokens":40}}"#,
    ),
    (
        "claude",
        "negative-thinking",
        r#"{"usage":{"input_tokens":3,"output_tokens":10,"thinking_tokens":-4}}"#,
    ),
    (
        "claude",
        "zero-thinking-first",
        r#"{"usage":{"input_tokens":3,"output_tokens":10,"output_tokens_details":{"thinking_tokens":0},"thinking_tokens":4}}"#,
    ),
    (
        "claude",
        "negative-counts",
        r#"{"usage":{"input_tokens":-3,"output_tokens":10,"cache_read_input_tokens":-1}}"#,
    ),
    (
        "claude",
        "overflowing-total",
        r#"{"usage":{"input_tokens":9223372036854775807,"output_tokens":10}}"#,
    ),
    ("claude-stream", "empty", ""),
    ("claude-stream", "ping", r#"data: {"type":"ping"}"#),
    ("claude-stream", "event-line", "event: message_delta"),
    (
        "claude-stream",
        "both-usages",
        r#"data: {"type":"message_delta","usage":{"output_tokens":3},"message":{"usage":{"input_tokens":9}}}"#,
    ),
    (
        "claude-stream",
        "null-usage",
        r#"data: {"type":"message_delta","usage":null}"#,
    ),
    (
        "claude-stream",
        "cut-short",
        r#"data: {"type":"message_delta","usage":{"output_tokens":3}"#,
    ),
    ("gemini", "empty", ""),
    ("gemini", "no-usage", r#"{"candidates":[]}"#),
    (
        "gemini",
        "snake-case",
        r#"{"usage_metadata":{"promptTokenCount":4,"candidatesTokenCount":5,"totalTokenCount":9}}"#,
    ),
    (
        "gemini",
        "both-names",
        r#"{"usageMetadata":{"promptTokenCount":1},"usage_metadata":{"promptTokenCount":4}}"#,
    ),
    (
        "gemini",
        "snake-tool-use",
        r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"tool_use_prompt_token_count":5}}"#,
    ),
    (
        "gemini",
        "no-total",
        r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":3,"cachedContentTokenCount":4}}"#,
    ),
    (
        "gemini",
        "negative-no-total",
        r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":-20}}"#,
    ),
    (
        "gemini",
        "total-too-small",
        r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":20,"totalTokenCount":5}}"#,
    ),
    (
        "gemini",
        "cache-over-prompt",
        r#"{"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"cachedContentTokenCount":9,"totalTokenCount":5}}"#,
    ),
    (
        "gemini",
        "cli-wrapped",
        r#"{"response":{"usageMetadata":{"promptTokenCount":4,"totalTokenCount":4}}}"#,
    ),
    ("gemini", "usage-null", r#"{"usageMetadata":null}"#),
    ("gemini-stream", "empty", ""),
    ("gemini-stream", "done", "data: [DONE]"),
    (
        "gemini-stream",
        "bare-json",
        r#"{"usageMetadata":{"promptTokenCount":4,"totalTokenCount":4}}"#,
    ),
    (
        "gemini-stream",
        "array-chunk",
        r#"[{"usageMetadata":{"promptTokenCount":4,"totalTokenCount":4}}]"#,
    ),
    (
        "gemini-stream",
        "snake-case",
        r#"data: {"usage_metadata":{"candidatesTokenCount":2}}"#,
    ),
    (
        "gemini-stream",
        "usage-null",
        r#"data: {"usageMetadata":null}"#,
    ),
    (
        "gemini-stream",
        "cached-only",
        r#"data: {"usageMetadata":{"cachedContentTokenCount":4}}"#,
    ),
    (
        "gemini-stream",
        "cli-wrapped",
        r#"data: {"response":{"usageMetadata":{"promptTokenCount":4,"totalTokenCount":4}}}"#,
    ),
];
