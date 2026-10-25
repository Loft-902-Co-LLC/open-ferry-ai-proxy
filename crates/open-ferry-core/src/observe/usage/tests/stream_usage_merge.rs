//! Interactions stream merges retain subset cache and separate reasoning.

use super::super::accounting::{Detail, Quality, TokenBreakdown};
use super::super::parse::{
    merge_stream_usage_detail, parse_claude_usage, parse_interactions_usage,
};
use super::support::{ClientCall, Harness, auth, int_at};
use crate::exec::Format;
use crate::observe::{AttemptKind, Outcome};

fn interactions(body: &str) -> Detail {
    parse_interactions_usage(body.as_bytes())
}

fn initial() -> Detail {
    interactions(
        r#"{"usage":{"input_tokens":100,"cache_read_tokens":20,"output_tokens":0,"reasoning_tokens":0,"total_tokens":100}}"#,
    )
}

#[test]
fn merge_interactions_cumulative_identical_and_partial_updates() {
    let initial = initial();
    let final_detail = interactions(
        r#"{"usage":{"input_tokens":100,"cache_read_tokens":20,"output_tokens":30,"reasoning_tokens":10,"total_tokens":140}}"#,
    );
    for (existing, update) in [
        (initial.clone(), final_detail.clone()),
        (final_detail.clone(), final_detail.clone()),
        (
            initial,
            interactions(r#"{"usage":{"output_tokens":30,"reasoning_tokens":10}}"#),
        ),
    ] {
        let merged = merge_stream_usage_detail(&existing, update);
        assert_eq!(merged, final_detail);
        assert_eq!(
            merged.token_breakdown,
            TokenBreakdown::separate_reasoning(100, 20, 0, 30, 10, 140)
        );
        assert!(merged.token_breakdown.is_valid());
        assert_eq!(merged.token_breakdown.quality, Quality::Complete);
    }
}

#[test]
fn merge_interactions_cache_only_and_reasoning_only_evidence() {
    let merged = merge_stream_usage_detail(
        &initial(),
        Detail {
            output_tokens: 30,
            ..Detail::default()
        },
    );
    assert_eq!(merged.total_tokens, 130);
    assert_eq!(
        merged.token_breakdown,
        TokenBreakdown::separate_reasoning(100, 20, 0, 30, 0, 130)
    );

    let reasoning_only = interactions(r#"{"usage":{"reasoning_tokens":10}}"#);
    let merged = merge_stream_usage_detail(&initial(), reasoning_only.clone());
    assert_eq!(merged.total_tokens, 110);
    assert_eq!(
        merged.token_breakdown,
        TokenBreakdown::separate_reasoning(100, 20, 0, 0, 10, 110)
    );
    let merged = merge_stream_usage_detail(
        &reasoning_only,
        Detail {
            input_tokens: 100,
            output_tokens: 30,
            ..Detail::default()
        },
    );
    assert_eq!(
        merged.token_breakdown,
        TokenBreakdown::separate_reasoning(100, 0, 0, 30, 10, 140)
    );
}

#[test]
fn merge_subset_with_inclusive_reasoning() {
    let detail = Detail {
        input_tokens: 100,
        cache_read_tokens: 20,
        cached_tokens: 20,
        output_tokens: 40,
        reasoning_tokens: 10,
        total_tokens: 140,
        token_breakdown: TokenBreakdown::subset(100, 20, 0, 40, 10, 140),
        ..Detail::default()
    };
    assert_eq!(merge_stream_usage_detail(&detail, detail.clone()), detail);
    let merged = merge_stream_usage_detail(
        &detail,
        Detail {
            output_tokens: 50,
            ..Detail::default()
        },
    );
    assert_eq!(merged.total_tokens, 150);
    assert_eq!(
        merged.token_breakdown,
        TokenBreakdown::subset(100, 20, 0, 50, 10, 150)
    );
}

#[test]
fn merge_uses_update_semantics_before_existing_semantics() {
    let existing = parse_claude_usage(br#"{"usage":{"input_tokens":100,"cache_read_input_tokens":20,"output_tokens":40,"thinking_tokens":10}}"#);
    let update = interactions(
        r#"{"usage":{"input_tokens":100,"cache_read_tokens":20,"output_tokens":30,"reasoning_tokens":10,"total_tokens":140}}"#,
    );
    let mut expected = update.clone();
    expected.cached_tokens = existing.cached_tokens;
    assert_eq!(
        merge_stream_usage_detail(&existing, update.clone()),
        expected
    );
    assert_eq!(
        merge_stream_usage_detail(&update, existing.clone()),
        existing
    );
}

#[test]
fn merge_keeps_claude_independent_cache_and_inconsistent_delta() {
    let initial = parse_claude_usage(
        br#"{"usage":{"input_tokens":100,"cache_read_input_tokens":20,"output_tokens":0}}"#,
    );
    let valid = parse_claude_usage(br#"{"usage":{"output_tokens":30,"thinking_tokens":10}}"#);
    let merged = merge_stream_usage_detail(&initial, valid);
    assert_eq!(merged.total_tokens, 150);
    assert_eq!(
        merged.token_breakdown,
        TokenBreakdown::independent(100, 20, 0, 20, 10, 150)
    );
    let invalid = parse_claude_usage(br#"{"usage":{"cache_read_input_tokens":20,"output_tokens":10,"output_tokens_details":{"thinking_tokens":20}}}"#);
    let merged = merge_stream_usage_detail(&initial, invalid);
    assert!(merged.token_breakdown.is_valid());
    assert_eq!(merged.token_breakdown.quality, Quality::Inconsistent);
}

#[test]
fn merge_interactions_overflow_and_invalid_boundaries() {
    for update in [
        Detail {
            input_tokens: i64::MAX,
            output_tokens: 1,
            ..Detail::default()
        },
        Detail {
            output_tokens: i64::MAX,
            reasoning_tokens: 1,
            ..Detail::default()
        },
        Detail {
            input_tokens: -1,
            output_tokens: 1,
            ..Detail::default()
        },
        Detail {
            output_tokens: -1,
            ..Detail::default()
        },
        Detail {
            reasoning_tokens: -1,
            ..Detail::default()
        },
        Detail {
            cache_read_tokens: i64::MAX,
            cache_creation_tokens: 1,
            ..Detail::default()
        },
        Detail {
            cache_read_tokens: -1,
            ..Detail::default()
        },
        Detail {
            cache_read_tokens: 101,
            ..Detail::default()
        },
        Detail {
            total_tokens: 200,
            ..Detail::default()
        },
    ] {
        let merged = merge_stream_usage_detail(&initial(), update);
        assert!(merged.token_breakdown.is_valid(), "{merged:?}");
        assert_eq!(
            merged.token_breakdown.quality,
            Quality::Inconsistent,
            "{merged:?}"
        );
    }
    let merged = merge_stream_usage_detail(
        &initial(),
        Detail {
            input_tokens: i64::MAX,
            cache_read_tokens: 1,
            ..Detail::default()
        },
    );
    assert_eq!(merged.total_tokens, i64::MAX);
    assert_eq!(merged.token_breakdown.quality, Quality::Complete);
}

#[test]
fn merge_subset_overflow_is_inconsistent() {
    let existing = Detail {
        input_tokens: 100,
        cache_read_tokens: 20,
        output_tokens: 40,
        reasoning_tokens: 10,
        total_tokens: 140,
        token_breakdown: TokenBreakdown::subset(100, 20, 0, 40, 10, 140),
        ..Detail::default()
    };
    let merged = merge_stream_usage_detail(
        &existing,
        Detail {
            input_tokens: i64::MAX,
            output_tokens: 1,
            ..Detail::default()
        },
    );
    assert_eq!(merged.token_breakdown.quality, Quality::Inconsistent);
    assert!(merged.token_breakdown.is_valid());
}

#[test]
fn native_interactions_keeps_replacement_policy() {
    let harness = Harness::new();
    let driver = ClientCall::new("model").stream().tap(&harness);
    driver.attempt_with(
        AttemptKind::Stream,
        "gemini-interactions",
        "model",
        &Format::INTERACTIONS,
        &auth("id", "0", "gemini-interactions"),
        &[],
        "{}",
    );
    driver.chunk("data: {\"usage\":{\"input_tokens\":100,\"cache_read_tokens\":20}}\n\n");
    driver.chunk("data: {\"usage\":{\"output_tokens\":30,\"reasoning_tokens\":10}}\n\n");
    driver.finish(Outcome::Completed);
    let record = harness.record();
    assert_eq!(int_at(&record, "/tokens/input_tokens"), 0);
    assert_eq!(int_at(&record, "/tokens/output_tokens"), 30);
    assert_eq!(int_at(&record, "/tokens/reasoning_tokens"), 10);
    assert_eq!(int_at(&record, "/tokens/total_tokens"), 40);
}
