// Ported from CLIProxyAPI internal/runtime/executor/helps/usage_helpers_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the parsers of upstream answers' token counts, the stream
//! buffer and the usage reporter.
//!
//! Upstream builds its reporter's records from the Go context and calls
//! its `buildRecord` directly; open-ferry's records are made by the usage
//! tap from the traffic it sees, so those tests run the traffic through
//! the tap and read the record it queues, on a clock the tests move.
//!
//! Dropped:
//! - TestParseInteractionsUsage, TestParseInteractionsUsageNormalizesCacheWriteAlias,
//!   TestParseInteractionsUsageIncludesToolUseTokens,
//!   TestParseInteractionsStreamUsage and
//!   TestParseInteractionsStreamUsageOfficialMetadata: the Interactions
//!   parsers aren't ported (see the module's docs).
//! - TestUsageReporterBuildRecordIncludesReasoningEffort,
//!   TestUsageReporterSetTranslatedReasoningEffortCodexConfigurationUpdate
//!   and TestUsageReporterSetTranslatedReasoningEffortConfigurationUpdateAfterCleanup:
//!   records don't carry a reasoning effort (see the reporter's module).
//! - TestUsageReporterSetStream: the stream flag is the client's option,
//!   which nothing changes later; TestUsageReporterBuildRecordIncludesStreamTrue
//!   covers it.
//! - TestUsageReporterBuildAdditionalModelRecordSkipsZeroTokens: the Codex
//!   image tool's records aren't ported.
//! - TestUsageReporterPropagatesSessionHierarchy: open-ferry never derives
//!   a session or its hierarchy (policy).
//! - TestUsageReporterPropagatesBaseURL: records have no base URL.
//!
//! Deviations from upstream:
//! - A failure's body is the error's text, trimmed; upstream prefers the
//!   error's response body as it came. Go's wrapped errors have no
//!   counterpart, so the `url.Error` case of
//!   TestFailFromErrorsMapsContextStatuses isn't here, and a failure
//!   without a status is written with 500 as the record writes it.
//! - An execution ID is a version 7 UUID; upstream's is version 4.
//! - The TTFT tests check the time a record carries, as the tap keeps no
//!   state a test can read between events.

use std::time::{Duration, Instant};

use super::super::accounting::{Detail, Quality, ensure_token_breakdown_for_provider};
use super::super::parse::{
    StreamUsageBuffer, parse_claude_stream_usage, parse_claude_usage, parse_codex_usage,
    parse_gemini_stream_usage, parse_gemini_usage, parse_openai_stream_usage, parse_openai_usage,
};
use super::super::record_json::Record;
use super::super::ttft::Ttft;
use super::support::{ClientCall, Harness, auth, bool_at, int_at, str_field};
use crate::exec::{ErrorKind, ExecError, Format};
use crate::observe::{AttemptKind, Outcome};

/// Ports TestParseOpenAIUsageChatCompletions.
#[test]
fn parse_openai_usage_chat_completions() {
    let detail = parse_openai_usage(
        br#"{"usage":{"prompt_tokens":10,"completion_tokens":6,"total_tokens":16,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":5}}}"#,
    );
    assert_eq!(detail.input_tokens, 10);
    assert_eq!(detail.output_tokens, 6);
    assert_eq!(detail.total_tokens, 16);
    assert_eq!(detail.cached_tokens, 4);
    assert_eq!(detail.cache_read_tokens, 4);
    assert_eq!(detail.reasoning_tokens, 5);
    let breakdown = detail.token_breakdown;
    assert!(breakdown.is_valid());
    assert_eq!(breakdown.quality, Quality::Complete);
    assert_eq!(breakdown.input.uncached_tokens, 6);
    assert_eq!(breakdown.output.non_reasoning_tokens, 1);
}

/// Ports TestParseOpenAIUsageResponses.
#[test]
fn parse_openai_usage_responses() {
    let detail = parse_openai_usage(
        br#"{"service_tier":"default","usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"input_tokens_details":{"cached_tokens":7},"output_tokens_details":{"reasoning_tokens":9}}}"#,
    );
    assert_eq!(detail.input_tokens, 10);
    assert_eq!(detail.output_tokens, 20);
    assert_eq!(detail.total_tokens, 30);
    assert_eq!(detail.cached_tokens, 7);
    assert_eq!(detail.cache_read_tokens, 7);
    assert_eq!(detail.reasoning_tokens, 9);
    assert_eq!(detail.response_service_tier, "default");
    assert_eq!(detail.token_breakdown.input.uncached_tokens, 3);
    assert_eq!(detail.token_breakdown.output.non_reasoning_tokens, 11);
}

/// Ports TestParseOpenAIUsageTotalOnlyIsUnclassified.
#[test]
fn parse_openai_usage_total_only_is_unclassified() {
    let detail = parse_openai_usage(br#"{"usage":{"total_tokens":42}}"#);
    let breakdown = detail.token_breakdown;
    assert!(breakdown.is_valid());
    assert_eq!(breakdown.quality, Quality::Unclassified);
    assert_eq!(detail.total_tokens, 42);
    assert_eq!(breakdown.unclassified_tokens, 42);
}

/// Ports TestParseOpenAIUsagePartialBucketsPreserveKnownTokens.
#[test]
fn parse_openai_usage_partial_buckets_preserve_known_tokens() {
    let detail = parse_openai_usage(br#"{"usage":{"input_tokens":10,"total_tokens":15}}"#);
    let breakdown = detail.token_breakdown;
    assert!(breakdown.is_valid());
    assert_eq!(breakdown.quality, Quality::Unclassified);
    assert_eq!(breakdown.input.total_tokens, 10);
    assert_eq!(breakdown.unclassified_tokens, 5);
}

/// Ports TestParseOpenAIUsageExplicitZeroBucketsRemainInconsistent.
#[test]
fn parse_openai_usage_explicit_zero_buckets_remain_inconsistent() {
    let detail =
        parse_openai_usage(br#"{"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":42}}"#);
    assert!(detail.token_breakdown.is_valid());
    assert_eq!(detail.token_breakdown.quality, Quality::Inconsistent);
}

/// Ports TestParseCodexUsageIncludesCacheWriteTokens.
#[test]
fn parse_codex_usage_includes_cache_write_tokens() {
    let detail = parse_codex_usage(
        br#"{"response":{"service_tier":"priority","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":40}}}}"#,
    )
    .expect("usage");
    assert_eq!(detail.input_tokens, 100);
    assert_eq!(detail.output_tokens, 20);
    assert_eq!(detail.cached_tokens, 30);
    assert_eq!(detail.cache_read_tokens, 30);
    assert_eq!(detail.cache_creation_tokens, 40);
    assert_eq!(detail.total_tokens, 120);
    assert_eq!(detail.response_service_tier, "priority");
    assert_eq!(detail.token_breakdown.input.uncached_tokens, 30);
    assert_eq!(detail.token_breakdown.input.cache_write_tokens, 40);
}

/// Ports TestParseOpenAIUsageNormalizesCacheCreationAlias.
#[test]
fn parse_openai_usage_normalizes_cache_creation_alias() {
    let detail = parse_openai_usage(
        br#"{"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12,"input_tokens_details":{"cache_creation_tokens":4}}}"#,
    );
    assert_eq!(detail.cache_creation_tokens, 4);
}

/// Ports TestParseOpenAIUsageIgnoresNullUsage.
#[test]
fn parse_openai_usage_ignores_null_usage() {
    assert_eq!(parse_openai_usage(br#"{"usage":null}"#), Detail::default());
}

/// Ports TestParseOpenAIUsagePreservesResponseTierWithoutUsage.
#[test]
fn parse_openai_usage_preserves_response_tier_without_usage() {
    let detail = parse_openai_usage(br#"{"service_tier":"default"}"#);
    assert_eq!(detail.response_service_tier, "default");
}

/// Ports TestParseCodexUsagePreservesResponseTierWithoutUsage.
#[test]
fn parse_codex_usage_preserves_response_tier_without_usage() {
    let detail = parse_codex_usage(br#"{"response":{"service_tier":"default"}}"#).expect("tier");
    assert_eq!(detail.response_service_tier, "default");
}

/// Ports TestParseOpenAIStreamUsageIgnoresNullUsage.
#[test]
fn parse_openai_stream_usage_ignores_null_usage() {
    let line = br#"data: {"id":"chunk_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}],"usage":null}"#;
    assert_eq!(parse_openai_stream_usage(line), None);
}

/// Ports TestParseOpenAIStreamUsageResponsesFields.
#[test]
fn parse_openai_stream_usage_responses_fields() {
    let line = br#"data: {"id":"chunk_1","object":"chat.completion.chunk","service_tier":"flex","choices":[],"usage":{"input_tokens":8,"output_tokens":5,"total_tokens":13,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}}"#;
    let detail = parse_openai_stream_usage(line).expect("usage");
    assert_eq!(detail.input_tokens, 8);
    assert_eq!(detail.output_tokens, 5);
    assert_eq!(detail.total_tokens, 13);
    assert_eq!(detail.cached_tokens, 3);
    assert_eq!(detail.cache_read_tokens, 3);
    assert_eq!(detail.reasoning_tokens, 2);
    assert_eq!(detail.response_service_tier, "flex");
}

/// Ports TestStreamUsageBufferKeepsLastUsage.
#[test]
fn stream_usage_buffer_keeps_last_usage() {
    let mut buffer = StreamUsageBuffer::default();
    buffer.observe(Some(Detail::default()));
    buffer.observe(None);
    buffer.observe(Some(Detail {
        input_tokens: 39320,
        output_tokens: 26,
        total_tokens: 39346,
        cached_tokens: 33280,
        ..Detail::default()
    }));
    let detail = buffer.detail().expect("usage");
    assert_eq!(detail.input_tokens, 39320);
    assert_eq!(detail.output_tokens, 26);
    assert_eq!(detail.total_tokens, 39346);
    assert_eq!(detail.cached_tokens, 33280);
}

/// Ports TestStreamUsageBufferPreservesTierAcrossChunks.
#[test]
fn stream_usage_buffer_preserves_tier_across_chunks() {
    let mut buffer = StreamUsageBuffer::default();
    buffer.observe_openai_stream(br#"data: {"service_tier":"default"}"#);
    buffer.observe_openai_stream(
        br#"data: {"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
    );
    let detail = buffer.detail().expect("usage");
    assert_eq!(detail.input_tokens, 1);
    assert_eq!(detail.output_tokens, 1);
    assert_eq!(detail.response_service_tier, "default");
}

/// Ports TestStreamUsageBufferObserveOpenAIStreamStateTransitions.
#[test]
fn stream_usage_buffer_observe_openai_stream_state_transitions() {
    let buffer_of = |lines: &[&[u8]]| {
        let mut buffer = StreamUsageBuffer::default();
        for line in lines {
            buffer.observe_openai_stream(line);
        }
        buffer
    };

    // same chunk
    let buffer = buffer_of(&[
        br#"data: {"service_tier":"flex","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}"#,
    ]);
    let detail = buffer.detail().expect("same chunk");
    assert_eq!(detail.input_tokens, 2);
    assert_eq!(detail.response_service_tier, "flex");

    // usage before tier
    let buffer = buffer_of(&[
        br#"data: {"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}"#,
        br#"data: {"service_tier":"default"}"#,
    ]);
    let detail = buffer.detail().expect("usage before tier");
    assert_eq!(detail.input_tokens, 2);
    assert_eq!(detail.response_service_tier, "default");

    // final usage tier overrides early tier
    let buffer = buffer_of(&[
        br#"data: {"service_tier":"default"}"#,
        br#"data: {"service_tier":"priority","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}"#,
    ]);
    let detail = buffer.detail().expect("final tier");
    assert_eq!(detail.response_service_tier, "priority");

    // irrelevant and invalid chunks do not change state
    let buffer = buffer_of(&[
        br#"data: {"content":"the word \"usage\" appears here"}"#,
        br#"data: {"usage":"#,
        br#"data: {"usage":null}"#,
    ]);
    assert_eq!(buffer.detail(), None);

    // zero token usage is retained
    let buffer =
        buffer_of(&[br#"data: {"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}"#]);
    assert!(buffer.detail().is_some());
}

/// Ports TestStreamUsageBufferPreservesOnlyZeroUsage.
#[test]
fn stream_usage_buffer_preserves_only_zero_usage() {
    let mut buffer = StreamUsageBuffer::default();
    buffer.observe(Some(Detail::default()));
    assert_eq!(buffer.detail(), Some(&Detail::default()));
}

/// Ports TestParseClaudeUsageIncludesCacheTokensInTotal.
#[test]
fn parse_claude_usage_includes_cache_tokens_in_total() {
    let detail = parse_claude_usage(
        br#"{"usage":{"input_tokens":3085,"output_tokens":253,"cache_read_input_tokens":7,"cache_creation_input_tokens":19514}}"#,
    );
    assert_eq!(detail.input_tokens, 3085);
    assert_eq!(detail.output_tokens, 253);
    assert_eq!(detail.cache_read_tokens, 7);
    assert_eq!(detail.cache_creation_tokens, 19514);
    assert_eq!(detail.cached_tokens, 7);
    assert_eq!(detail.total_tokens, 22859);
    assert_eq!(detail.token_breakdown.input.total_tokens, 22606);
    assert_eq!(detail.token_breakdown.input.uncached_tokens, 3085);
}

/// Ports TestParseClaudeUsageFallsBackCachedTokensToCacheCreation.
#[test]
fn parse_claude_usage_falls_back_cached_tokens_to_cache_creation() {
    let detail = parse_claude_usage(
        br#"{"usage":{"input_tokens":3085,"output_tokens":253,"cache_creation_input_tokens":19514}}"#,
    );
    assert_eq!(detail.cached_tokens, 19514);
    assert_eq!(detail.total_tokens, 22852);
}

/// Ports TestParseClaudeUsagePreservesThinkingTokensAsReasoningSubset.
#[test]
fn parse_claude_usage_preserves_thinking_tokens_as_reasoning_subset() {
    let detail = parse_claude_usage(
        br#"{"usage":{"input_tokens":2,"cache_creation_input_tokens":831,"cache_read_input_tokens":44225,"output_tokens":244,"output_tokens_details":{"thinking_tokens":40}}}"#,
    );
    assert_eq!(detail.output_tokens, 244);
    assert_eq!(detail.reasoning_tokens, 40);
    assert_eq!(detail.total_tokens, 45302);
    let output = detail.token_breakdown.output;
    assert!(detail.token_breakdown.is_valid());
    assert_eq!(output.total_tokens, 244);
    assert_eq!(output.non_reasoning_tokens, 204);
    assert_eq!(output.reasoning_tokens, 40);
}

/// Ports TestParseClaudeStreamUsagePreservesThinkingTokensAsReasoningSubset.
#[test]
fn parse_claude_stream_usage_preserves_thinking_tokens_as_reasoning_subset() {
    let detail = parse_claude_stream_usage(
        br#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":2,"cache_creation_input_tokens":831,"cache_read_input_tokens":44225,"output_tokens":244,"output_tokens_details":{"thinking_tokens":40}}}"#,
    )
    .expect("usage");
    assert_eq!(detail.output_tokens, 244);
    assert_eq!(detail.reasoning_tokens, 40);
    assert_eq!(detail.total_tokens, 45302);
    assert!(detail.token_breakdown.is_valid());
    assert_eq!(detail.token_breakdown.output.non_reasoning_tokens, 204);
}

/// Ports TestParseClaudeStreamUsage_MessageStart.
#[test]
fn parse_claude_stream_usage_message_start() {
    let detail = parse_claude_stream_usage(
        br#"data: {"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[],"model":"claude-opus-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":2095,"cache_creation_input_tokens":7185,"cache_read_input_tokens":355598,"output_tokens":1}}}"#,
    )
    .expect("usage");
    assert_eq!(detail.input_tokens, 2095);
    assert_eq!(detail.cache_read_tokens, 355598);
    assert_eq!(detail.cache_creation_tokens, 7185);
    assert_eq!(detail.cached_tokens, 355598);
}

/// Ports TestParseClaudeUsageFallsBackToTopLevelThinkingTokens.
#[test]
fn parse_claude_usage_falls_back_to_top_level_thinking_tokens() {
    let detail = parse_claude_usage(
        br#"{"usage":{"input_tokens":3,"output_tokens":10,"thinking_tokens":4}}"#,
    );
    assert_eq!(detail.output_tokens, 10);
    assert_eq!(detail.reasoning_tokens, 4);
    assert_eq!(detail.total_tokens, 13);
    assert_eq!(detail.token_breakdown.output.non_reasoning_tokens, 6);
}

/// Ports TestParseGeminiUsageNormalizesCachedContent.
#[test]
fn parse_gemini_usage_normalizes_cached_content() {
    let detail = parse_gemini_usage(
        br#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"cachedContentTokenCount":4,"totalTokenCount":12}}"#,
    );
    assert_eq!(detail.cached_tokens, 4);
    assert_eq!(detail.cache_read_tokens, 4);
    assert_eq!(detail.token_breakdown.input.uncached_tokens, 6);
    assert_eq!(detail.token_breakdown.total_tokens, 12);
}

/// Ports TestParseGeminiUsageIncludesToolUsePromptTokens.
#[test]
fn parse_gemini_usage_includes_tool_use_prompt_tokens() {
    let detail = parse_gemini_usage(
        br#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":3,"toolUsePromptTokenCount":5,"totalTokenCount":20}}"#,
    );
    assert_eq!(detail.input_tokens, 15);
    assert_eq!(detail.total_tokens, 20);
    let breakdown = detail.token_breakdown;
    assert!(breakdown.is_valid());
    assert_eq!(breakdown.quality, Quality::Complete);
    assert_eq!(breakdown.input.uncached_tokens, 15);
    assert_eq!(breakdown.output.reasoning_tokens, 3);
}

/// Ports TestParseGeminiStreamUsageSkipsZeroPlaceholder.
#[test]
fn parse_gemini_stream_usage_skips_zero_placeholder() {
    let lines: [&[u8]; 2] = [
        br#"data: {"usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":0,"thoughtsTokenCount":0,"totalTokenCount":0}}"#,
        br#"data: {"usageMetadata":{"promptTokenCount":17984,"candidatesTokenCount":2668,"thoughtsTokenCount":1028,"totalTokenCount":21680}}"#,
    ];
    let accepted: Vec<Detail> = lines
        .iter()
        .filter_map(|line| parse_gemini_stream_usage(line))
        .collect();
    assert_eq!(accepted.len(), 1);
    let detail = &accepted[0];
    assert_eq!(detail.input_tokens, 17984);
    assert_eq!(detail.output_tokens, 2668);
    assert_eq!(detail.reasoning_tokens, 1028);
    assert_eq!(detail.total_tokens, 21680);
}

/// Ports TestParseGeminiUsageRejectsInvalidToolUseSums.
#[test]
fn parse_gemini_usage_rejects_invalid_tool_use_sums() {
    let cases = [
        (
            "negative",
            r#"{"usageMetadata":{"promptTokenCount":10,"toolUsePromptTokenCount":-1,"totalTokenCount":10}}"#,
        ),
        (
            "overflow",
            r#"{"usageMetadata":{"promptTokenCount":9223372036854775807,"toolUsePromptTokenCount":1,"totalTokenCount":9223372036854775807}}"#,
        ),
    ];
    for (name, payload) in cases {
        let detail = parse_gemini_usage(payload.as_bytes());
        assert!(detail.input_tokens >= 0, "{name}");
        assert!(detail.token_breakdown.is_valid(), "{name}");
        assert_eq!(
            detail.token_breakdown.quality,
            Quality::Inconsistent,
            "{name}"
        );
    }
}

/// Ports TestNormalizeUsageDetailTotalDoesNotDoubleCountReasoning
/// (upstream's `normalizeUsageDetailTotal` is
/// `EnsureTokenBreakdownForProvider`).
#[test]
fn normalize_usage_detail_total_does_not_double_count_reasoning() {
    let detail = ensure_token_breakdown_for_provider(
        Detail {
            input_tokens: 100,
            output_tokens: 30,
            reasoning_tokens: 12,
            ..Detail::default()
        },
        "openai",
        "",
    );
    assert_eq!(detail.total_tokens, 130);
    assert_eq!(detail.token_breakdown.quality, Quality::Complete);
    assert_eq!(detail.token_breakdown.output.reasoning_tokens, 12);
}

/// The record of `call` making an OpenAI-compatible call that answers
/// `answer` whole, with `before` run after the attempt is sent.
fn openai_record(
    call: ClientCall,
    answer: &str,
    before: impl FnOnce(&Harness),
) -> serde_json::Value {
    let harness = Harness::new();
    let driver = call.tap(&harness);
    driver.attempt(
        AttemptKind::Execute,
        "openai",
        "gpt-5.4",
        &auth("openai-1", "0", "openai"),
    );
    before(&harness);
    driver.chunk(answer);
    driver.finish(Outcome::Completed);
    harness.record()
}

/// The answer of three tokens the reporter tests give.
const THREE_TOKENS: &str = r#"{"usage":{"total_tokens":3}}"#;

/// Ports TestUsageReporterBuildRecordIncludesLatency.
#[test]
fn build_record_includes_latency() {
    let record = openai_record(ClientCall::new("gpt-5.4"), THREE_TOKENS, |harness| {
        harness.advance_ms(1500);
    });
    assert_eq!(int_at(&record, "/latency_ms"), 1500);
}

/// Ports TestUsageReporterTrackHTTPClientStartsTTFTBeforeRoundTrip: the
/// time to the answer's first byte counts from when the attempt is sent.
#[test]
fn ttft_starts_before_round_trip() {
    let record = openai_record(ClientCall::new("gpt-5.4"), "ok", |harness| {
        harness.advance_ms(40);
    });
    assert_eq!(int_at(&record, "/ttft_ms"), 40);
}

/// A Codex stream's record, the answer's `chunks` each given `after_ms`
/// after the one before.
fn codex_stream_record(chunks: &[(u64, &str)], end: Option<ExecError>) -> serde_json::Value {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.6-luna").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Stream,
        "codex",
        "gpt-5.6-luna",
        &auth("codex-1", "0", "codex"),
    );
    for (after_ms, chunk) in chunks {
        harness.advance_ms(*after_ms);
        driver.chunk(chunk);
    }
    match end {
        Some(error) => driver.fail(&error),
        None => driver.finish(Outcome::Completed),
    }
    harness.record()
}

/// Ports TestUsageReporterTrackHTTPClientRoundTripOnly_DoesNotTriggerOnBodyRead:
/// a Codex stream's metadata event gives only the first-packet fallback,
/// and its first token event sets the time to first token.
#[test]
fn codex_stream_ttft_waits_for_a_token_event() {
    let created = "data: {\"type\":\"response.created\"}\n\n";
    let record = codex_stream_record(&[(10, created)], None);
    assert_eq!(int_at(&record, "/ttft_ms"), 10, "fallback");

    let delta = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
    let record = codex_stream_record(&[(10, created), (5, delta)], None);
    assert_eq!(int_at(&record, "/ttft_ms"), 15, "token event");
}

/// Ports TestUsageReporterTrackHTTPClientRoundTripOnly_ErrorResponseRecordsFirstPacketFallback.
#[test]
fn codex_error_answer_records_first_packet_fallback() {
    let body = r#"{"error":{"message":"rate limit"}}"#;
    let record = codex_stream_record(&[(3, body)], Some(ExecError::upstream(429, body)));
    assert!(bool_at(&record, "/failed"));
    assert_eq!(int_at(&record, "/ttft_ms"), 3);
}

/// Ports TestUsageReporterObserveTokenEvent_FastPathNonTokenAndToken.
#[test]
fn observe_token_event_fast_path_non_token_and_token() {
    let start = Instant::now();
    let at = |millis| start + Duration::from_millis(millis);
    let mut ttft = Ttft::default();
    ttft.start(start);
    assert!(!ttft.is_set());

    ttft.observe_token_event(false, at(5));
    assert!(!ttft.is_set());
    assert!(ttft.is_first_packet_set());
    ttft.observe_token_event(false, at(8));
    assert_eq!(ttft.duration(), Duration::from_millis(5));

    ttft.observe_token_event(true, at(12));
    assert!(ttft.is_set());
    assert_eq!(ttft.duration(), Duration::from_millis(12));
    ttft.observe_token_event(true, at(20));
    assert_eq!(ttft.duration(), Duration::from_millis(12));
}

/// Ports TestUsageReporterBuildRecordIncludesRequestedModelAlias.
#[test]
fn build_record_includes_requested_model_alias() {
    let record = openai_record(ClientCall::new("client-gpt"), THREE_TOKENS, |_| {});
    assert_eq!(str_field(&record, "model"), "gpt-5.4");
    assert_eq!(str_field(&record, "alias"), "client-gpt");
}

/// Ports TestNewExecutorUsageReporterIncludesExecutorType: the executor
/// type is named from the provider and how it was called.
#[test]
fn executor_type_follows_the_provider() {
    let codex_completed =
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"total_tokens\":3}}}\n";
    let cases = [
        (
            "codex",
            AttemptKind::Execute,
            codex_completed,
            "CodexExecutor",
        ),
        (
            "codex",
            AttemptKind::Websocket,
            r#"{"type":"response.completed","response":{"usage":{"total_tokens":3}}}"#,
            "CodexWebsocketsExecutor",
        ),
        ("claude", AttemptKind::Execute, "{}", "ClaudeExecutor"),
        ("gemini", AttemptKind::Execute, "{}", "GeminiExecutor"),
        ("vertex", AttemptKind::Execute, "{}", "GeminiVertexExecutor"),
        (
            "test-provider",
            AttemptKind::Execute,
            "{}",
            "OpenAICompatExecutor",
        ),
    ];
    for (provider, kind, answer, executor_type) in cases {
        let harness = Harness::new();
        let driver = ClientCall::new("gpt-5.4").tap(&harness);
        driver.attempt(kind, provider, "gpt-5.4", &auth("auth-1", "0", provider));
        driver.chunk(answer);
        driver.finish(Outcome::Completed);
        let record = harness.record();
        assert_eq!(str_field(&record, "provider"), provider);
        assert_eq!(
            str_field(&record, "executor_type"),
            executor_type,
            "{provider}"
        );
    }
}

/// Ports TestUsageReporterBuildRecordIncludesServiceTier.
#[test]
fn build_record_includes_service_tier() {
    for tier in ["auto", "priority"] {
        let call = ClientCall::new("gpt-5.4").body(&format!(r#"{{"service_tier":"{tier}"}}"#));
        let record = openai_record(
            call,
            r#"{"service_tier":"default","usage":{"total_tokens":3}}"#,
            |_| {},
        );
        assert_eq!(str_field(&record, "service_tier"), tier);
        assert_eq!(str_field(&record, "response_service_tier"), "default");
    }
}

/// Ports TestUsageReporterBuildRecordDefaultsGenerateTrue.
#[test]
fn build_record_defaults_generate_true() {
    let record = openai_record(ClientCall::new("gpt-5.4"), THREE_TOKENS, |_| {});
    assert!(bool_at(&record, "/generate"));
}

/// Ports TestUsageReporterBuildRecordIncludesGenerateFalse.
#[test]
fn build_record_includes_generate_false() {
    let call = ClientCall::new("gpt-5.4").body(r#"{"generate":false}"#);
    let record = openai_record(call, THREE_TOKENS, |_| {});
    assert!(!bool_at(&record, "/generate"));
}

/// Ports TestUsageReporterBuildRecordDefaultsStreamFalse.
#[test]
fn build_record_defaults_stream_false() {
    let record = openai_record(ClientCall::new("gpt-5.4"), THREE_TOKENS, |_| {});
    assert!(!bool_at(&record, "/stream"));
}

/// Ports TestUsageReporterBuildRecordIncludesStreamTrue.
#[test]
fn build_record_includes_stream_true() {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.4").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Stream,
        "openai",
        "gpt-5.4",
        &auth("openai-1", "0", "openai"),
    );
    driver.chunk("data: {\"usage\":{\"total_tokens\":3}}\n");
    driver.finish(Outcome::Completed);
    assert!(bool_at(&harness.record(), "/stream"));
}

/// Ports TestUsageReporterSetTranslatedReasoningEffortPreservesClientServiceTier:
/// the tier the translated request names doesn't replace the client's.
#[test]
fn translated_request_keeps_the_client_service_tier() {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.4")
        .body(r#"{"service_tier":"auto"}"#)
        .tap(&harness);
    driver.attempt_with(
        AttemptKind::Execute,
        "openai",
        "gpt-5.4",
        &Format::OPENAI,
        &auth("openai-1", "0", "openai"),
        &[],
        r#"{"service_tier":"priority"}"#,
    );
    driver.chunk(THREE_TOKENS);
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_eq!(str_field(&record, "service_tier"), "auto");
    assert_eq!(str_field(&record, "reasoning_effort"), "");
}

/// The failed record of an OpenAI-compatible call that ends with `error`.
fn failed_record(error: &ExecError) -> serde_json::Value {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.4").tap(&harness);
    driver.attempt(
        AttemptKind::Execute,
        "openai",
        "gpt-5.4",
        &auth("openai-1", "0", "openai"),
    );
    driver.head(401, &[]);
    driver.fail(error);
    harness.record()
}

/// Ports TestFailFromErrorsPrefersResponseBody.
#[test]
fn fail_from_errors_prefers_response_body() {
    let record = failed_record(&ExecError::upstream(
        401,
        " \n{\"error\":\"upstream rejected request\"}\r\n",
    ));
    assert!(bool_at(&record, "/failed"));
    assert_eq!(int_at(&record, "/fail/status_code"), 401);
    assert_eq!(
        record.pointer("/fail/body").and_then(|body| body.as_str()),
        Some(r#"{"error":"upstream rejected request"}"#)
    );

    let record = failed_record(&ExecError::upstream(401, "generic upstream error"));
    assert_eq!(int_at(&record, "/fail/status_code"), 401);
    assert_eq!(
        record.pointer("/fail/body").and_then(|body| body.as_str()),
        Some("generic upstream error")
    );
}

/// Ports TestFailFromErrorsMapsContextStatuses.
#[test]
fn fail_from_errors_maps_context_statuses() {
    let cases = [
        ("canceled", ExecError::canceled(), 499),
        (
            "deadline",
            ExecError::new(ErrorKind::DeadlineExceeded, "context deadline exceeded"),
            504,
        ),
        (
            "plain error",
            ExecError::new(ErrorKind::Upstream, "boom"),
            500,
        ),
    ];
    for (name, error, status) in cases {
        let record = failed_record(&error);
        assert_eq!(int_at(&record, "/fail/status_code"), status, "{name}");
        let body = record
            .pointer("/fail/body")
            .and_then(|body| body.as_str())
            .unwrap_or_default();
        assert!(!body.trim().is_empty(), "{name}");
    }

    // A call that failed without an error.
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.4").tap(&harness);
    driver.attempt(
        AttemptKind::Execute,
        "openai",
        "gpt-5.4",
        &auth("openai-1", "0", "openai"),
    );
    driver.finish(Outcome::Failed);
    let record = harness.record();
    assert_eq!(int_at(&record, "/fail/status_code"), 500);
    assert_eq!(str_field(&record["fail"], "body"), "");
}

/// Ports TestStreamUsageBufferPublishFailure: a failed record keeps the
/// counts it is given.
#[test]
fn stream_usage_buffer_publish_failure() {
    let mut buffer = StreamUsageBuffer::default();
    buffer.observe(Some(Detail {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
        ..Detail::default()
    }));
    let record = Record {
        provider: "openai".to_owned(),
        model: "gpt-5.4".to_owned(),
        failed: true,
        fail_status: 499,
        fail_body: "context canceled".to_owned(),
        detail: buffer.detail().cloned().unwrap_or_default(),
        ..Record::default()
    };
    let record: serde_json::Value = serde_json::from_str(&record.encode()).expect("JSON");
    assert!(bool_at(&record, "/failed"));
    assert_eq!(int_at(&record, "/fail/status_code"), 499);
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 15);
}

const CLAUDE_MESSAGE_START: &str = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":2095,\"cache_creation_input_tokens\":7185,\"cache_read_input_tokens\":355598,\"output_tokens\":1}}}\n";

/// Ports TestStreamUsageBufferObserveClaudeStream_MergesStartAndDelta.
#[test]
fn stream_usage_buffer_observe_claude_stream_merges_start_and_delta() {
    let mut buffer = StreamUsageBuffer::default();
    buffer.observe_claude_stream(CLAUDE_MESSAGE_START.trim_end().as_bytes());
    buffer.observe_claude_stream(
        br#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#,
    );
    let detail = buffer.detail().expect("usage");
    assert_eq!(detail.input_tokens, 2095);
    assert_eq!(detail.output_tokens, 15);
    assert_eq!(detail.cache_read_tokens, 355598);
    assert_eq!(detail.cache_creation_tokens, 7185);
    assert_eq!(detail.cached_tokens, 355598);
    assert_eq!(detail.total_tokens, 2095 + 15 + 355598 + 7185);
}

/// Ports TestStreamUsageBufferObserveClaudeStream_FailurePreservesUsage: a
/// Claude stream that fails after its `message_start` publishes a failure
/// with the counts read.
#[test]
fn claude_stream_failure_preserves_usage() {
    let harness = Harness::new();
    let driver = ClientCall::new("claude-opus-5").stream().tap(&harness);
    driver.attempt(
        AttemptKind::Stream,
        "claude",
        "claude-opus-5",
        &auth("claude-1", "0", "claude"),
    );
    driver.chunk(CLAUDE_MESSAGE_START);
    driver.fail(&ExecError::canceled());
    let record = harness.record();
    assert!(bool_at(&record, "/failed"));
    assert_eq!(int_at(&record, "/fail/status_code"), 499);
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 2095);
    assert_eq!(int_at(&record, "/tokens/cache_read_tokens"), 355598);
    assert_eq!(int_at(&record, "/tokens/cache_creation_tokens"), 7185);
}

/// Ports TestUsageReporter_AllocatesUniqueUUIDPerAttemptWithSharedTraceID:
/// each executor call of a request gets its own execution ID, and all
/// share the request's ID as their trace.
#[test]
fn allocates_unique_uuid_per_attempt_with_shared_trace_id() {
    let harness = Harness::new();
    let driver = ClientCall::new("gpt-5.4").tap(&harness);
    let request_id = driver.context.id.as_str().to_owned();
    for (provider, model) in [("openai", "gpt-5.4"), ("claude", "claude-3-7-sonnet")] {
        driver.attempt(
            AttemptKind::Execute,
            provider,
            model,
            &auth("auth-1", "0", provider),
        );
        driver.chunk(r#"{"usage":{"total_tokens":10}}"#);
        driver.finish(Outcome::Completed);
    }
    let records = harness.records();
    assert_eq!(records.len(), 2);
    let mut ids = Vec::new();
    for record in &records {
        assert_eq!(str_field(record, "trace_id"), request_id);
        assert_eq!(str_field(record, "request_id"), request_id);
        let id = str_field(record, "execution_id");
        let parsed = uuid::Uuid::parse_str(id).expect("a UUID");
        assert_eq!(parsed.get_version_num(), 7, "{id}");
        ids.push(id.to_owned());
    }
    assert_ne!(ids[0], ids[1]);
}

/// Ports TestUsageReporter_ExplicitTraceIDPrecedenceOverLogRequestID.
#[test]
fn explicit_trace_id_precedence_over_request_id() {
    let record = Record {
        request_id: "log-id-1".to_owned(),
        trace_id: "explicit-trace-1".to_owned(),
        ..Record::default()
    };
    let record: serde_json::Value = serde_json::from_str(&record.encode()).expect("JSON");
    assert_eq!(str_field(&record, "trace_id"), "explicit-trace-1");
    assert_eq!(str_field(&record, "request_id"), "log-id-1");
}
