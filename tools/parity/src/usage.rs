//! Our side of the executor helpers' harness's `usage/parse` entry: the
//! usage parsed from an upstream's response body or stream line by
//! `open_ferry_core::observe::usage`, written as the harness writes
//! upstream's (see `go/helps/parity_usage.go`).

use open_ferry_core::observe::usage::{
    Detail, StreamUsageBuffer, TokenBreakdown, merge_stream_usage_detail,
    parse_claude_stream_usage, parse_claude_usage, parse_codex_usage, parse_gemini_stream_usage,
    parse_gemini_usage, parse_interactions_stream_usage, parse_interactions_usage,
    parse_openai_stream_usage, parse_openai_usage,
};
use serde_json::{Value, json};

use crate::cases::Case;

/// `usage/parse`: the usage the case's parser, its `parser` option, reads
/// from its body or line, or null where it finds none.
pub fn parse(case: &Case) -> Result<Value, String> {
    let parser = case.options["parser"].as_str().unwrap_or_default();
    if parser == "merge" {
        let mut merged = None;
        let mut steps = Vec::new();
        for step in case.options["steps"].as_array().into_iter().flatten() {
            let detail = parsed(
                step["parser"].as_str().unwrap_or_default(),
                step["body"].as_str().unwrap_or_default().as_bytes(),
            )?
            .unwrap_or_default();
            let detail = match merged.as_ref() {
                Some(existing) => merge_stream_usage_detail(existing, detail),
                None => detail,
            };
            steps.push(detail_json(&detail));
            merged = Some(detail);
        }
        return Ok(Value::Array(steps));
    }
    if parser == "sequence" {
        let protocol = case.options["protocol"].as_str().unwrap_or_default();
        let mut buffer = StreamUsageBuffer::default();
        let mut steps = Vec::new();
        for line in case.options["lines"].as_array().into_iter().flatten() {
            let line = line.as_str().unwrap_or_default().as_bytes();
            match protocol {
                "openai" => buffer.observe_openai_stream(line),
                "claude" => buffer.observe_claude_stream(line),
                "interactions" => buffer.observe(parse_interactions_stream_usage(line)),
                other => return Err(format!("unknown sequence protocol {other:?}")),
            }
            steps.push(json!({
                "detail": buffer.detail().map_or(Value::Null, detail_json),
                "model": buffer.response_model(),
            }));
        }
        return Ok(Value::Array(steps));
    }
    let body = case.request.as_bytes();
    Ok(parsed(parser, body)?
        .as_ref()
        .map_or(Value::Null, detail_json))
}

/// Reads one detail without discarding the parser's accounting semantics.
fn parsed(parser: &str, body: &[u8]) -> Result<Option<Detail>, String> {
    Ok(match parser {
        "codex" => parse_codex_usage(body),
        "openai" => Some(parse_openai_usage(body)),
        "openai-stream" => parse_openai_stream_usage(body),
        "claude" => Some(parse_claude_usage(body)),
        "claude-stream" => parse_claude_stream_usage(body),
        "gemini" => Some(parse_gemini_usage(body)),
        "gemini-stream" => parse_gemini_stream_usage(body),
        "interactions" => Some(parse_interactions_usage(body)),
        "interactions-stream" => parse_interactions_stream_usage(body),
        other => return Err(format!("unknown parser {other:?}")),
    })
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
