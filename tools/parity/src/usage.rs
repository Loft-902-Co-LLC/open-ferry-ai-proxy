//! Our side of the harness's `usage/parse` entry: the usage parsed from an
//! upstream's response body or stream line by
//! `open_ferry_core::observe::usage`, written as the harness writes
//! upstream's (see `go/parity_usage.go`).

use open_ferry_core::observe::usage::{
    Detail, TokenBreakdown, parse_claude_stream_usage, parse_claude_usage, parse_codex_usage,
    parse_gemini_stream_usage, parse_gemini_usage, parse_openai_stream_usage, parse_openai_usage,
};
use serde_json::{Value, json};

use crate::cases::Case;

/// `usage/parse`: the usage the case's parser, its `parser` option, reads
/// from its body or line, or null where it finds none.
pub fn parse(case: &Case) -> Result<Value, String> {
    let body = case.request.as_bytes();
    let detail = match case.options["parser"].as_str().unwrap_or_default() {
        "codex" => parse_codex_usage(body),
        "openai" => Some(parse_openai_usage(body)),
        "openai-stream" => parse_openai_stream_usage(body),
        "claude" => Some(parse_claude_usage(body)),
        "claude-stream" => parse_claude_stream_usage(body),
        "gemini" => Some(parse_gemini_usage(body)),
        "gemini-stream" => parse_gemini_stream_usage(body),
        other => return Err(format!("case {}: unknown parser {other:?}", case.name)),
    };
    Ok(detail.as_ref().map_or(Value::Null, detail_json))
}

/// `detail` as the harness writes upstream's `usage.Detail`.
fn detail_json(detail: &Detail) -> Value {
    json!({
        "input_tokens": detail.input_tokens,
        "output_tokens": detail.output_tokens,
        "reasoning_tokens": detail.reasoning_tokens,
        "cached_tokens": detail.cached_tokens,
        "cache_read_tokens": detail.cache_read_tokens,
        "cache_creation_tokens": detail.cache_creation_tokens,
        "total_tokens": detail.total_tokens,
        "token_breakdown": breakdown_json(&detail.token_breakdown),
        "response_service_tier": detail.response_service_tier,
    })
}

/// `breakdown` as Go marshals a `usage.TokenBreakdown`.
fn breakdown_json(breakdown: &TokenBreakdown) -> Value {
    let input = &breakdown.input;
    let output = &breakdown.output;
    json!({
        "schema_version": breakdown.schema_version,
        "quality": breakdown.quality.as_str(),
        "total_tokens": breakdown.total_tokens,
        "input": {
            "total_tokens": input.total_tokens,
            "uncached_tokens": input.uncached_tokens,
            "cache_read_tokens": input.cache_read_tokens,
            "cache_write_tokens": input.cache_write_tokens,
        },
        "output": {
            "total_tokens": output.total_tokens,
            "non_reasoning_tokens": output.non_reasoning_tokens,
            "reasoning_tokens": output.reasoning_tokens,
        },
        "unclassified_tokens": breakdown.unclassified_tokens,
    })
}
