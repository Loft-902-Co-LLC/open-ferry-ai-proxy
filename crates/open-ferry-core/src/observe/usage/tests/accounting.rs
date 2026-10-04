// Ported from CLIProxyAPI sdk/cliproxy/usage/accounting_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the token breakdown. All of upstream's are ported.
//!
//! Deviations from upstream: none.

use super::super::accounting::{
    Detail, InputBreakdown, OutputBreakdown, Quality, TOKEN_ACCOUNTING_SCHEMA_VERSION,
    TokenBreakdown, ensure_token_breakdown_for_provider,
};

/// Ports TestNewSubsetTokenBreakdownAvoidsCacheAndReasoningDoubleCount.
#[test]
fn subset_avoids_cache_and_reasoning_double_count() {
    let breakdown = TokenBreakdown::subset(100, 40, 10, 30, 12, 130);
    assert!(breakdown.is_valid(), "{breakdown:?}");
    assert_eq!(breakdown.input.uncached_tokens, 50);
    assert_eq!(breakdown.output.non_reasoning_tokens, 18);
    assert_eq!(breakdown.total_tokens, 130);
}

/// Ports TestNewPartialSubsetTokenBreakdownPreservesKnownBuckets.
#[test]
fn partial_subset_preserves_known_buckets() {
    let breakdown = TokenBreakdown::partial_subset(10, 4, 0, 0, 0, 15);
    assert!(breakdown.is_valid(), "{breakdown:?}");
    assert_eq!(breakdown.quality, Quality::Unclassified);
    assert_eq!(breakdown.input.total_tokens, 10);
    assert_eq!(breakdown.unclassified_tokens, 5);
}

/// Ports TestNewIndependentTokenBreakdownKeepsClaudeCacheBucketsIndependent.
#[test]
fn independent_keeps_claude_cache_buckets_independent() {
    let breakdown = TokenBreakdown::independent(30, 7, 13, 5, 0, 55);
    assert!(breakdown.is_valid(), "{breakdown:?}");
    assert_eq!(breakdown.input.total_tokens, 50);
    assert_eq!(breakdown.total_tokens, 55);
}

/// Ports TestNewSeparateReasoningTokenBreakdownAddsReasoningToOutput.
#[test]
fn separate_reasoning_adds_reasoning_to_output() {
    let breakdown = TokenBreakdown::separate_reasoning(20, 5, 0, 7, 3, 30);
    assert!(breakdown.is_valid(), "{breakdown:?}");
    assert_eq!(breakdown.output.total_tokens, 10);
    assert_eq!(breakdown.total_tokens, 30);
}

/// Ports TestTokenBreakdownMarksContradictoryParentsInconsistent.
#[test]
fn marks_contradictory_parents_inconsistent() {
    let breakdown = TokenBreakdown::subset(10, 4, 0, 3, 1, 20);
    assert!(breakdown.is_valid(), "{breakdown:?}");
    assert_eq!(breakdown.quality, Quality::Inconsistent);
    assert_eq!(breakdown.unclassified_tokens, 20);
}

/// Ports TestNewUnclassifiedTokenBreakdownDoesNotGuessBuckets.
#[test]
fn unclassified_does_not_guess_buckets() {
    let breakdown = TokenBreakdown::unclassified(42);
    assert!(breakdown.is_valid(), "{breakdown:?}");
    assert_eq!(breakdown.quality, Quality::Unclassified);
    assert_eq!(breakdown.unclassified_tokens, 42);
}

/// Ports TestEnsureTokenBreakdownForProviderUsesKnownSemantics.
#[test]
fn ensure_for_provider_uses_known_semantics() {
    let detail = || Detail {
        input_tokens: 100,
        output_tokens: 30,
        reasoning_tokens: 12,
        cache_read_tokens: 40,
        cache_creation_tokens: 10,
        ..Detail::default()
    };
    let cases = [
        (
            "OpenAI subsets cache and reasoning",
            "openai",
            "",
            130,
            100,
            30,
        ),
        (
            "OpenAI compatible executor takes precedence",
            "anthropic",
            "OpenAICompatExecutor",
            130,
            100,
            30,
        ),
        (
            "Gemini keeps reasoning separate",
            "gemini",
            "",
            142,
            100,
            42,
        ),
        (
            "Claude keeps cache and reasoning independent",
            "anthropic",
            "",
            192,
            150,
            42,
        ),
    ];
    for (name, provider, executor_type, total, input, output) in cases {
        let got = ensure_token_breakdown_for_provider(detail(), provider, executor_type);
        let breakdown = got.token_breakdown;
        assert!(breakdown.is_valid(), "{name}: {breakdown:?}");
        assert_eq!(breakdown.quality, Quality::Complete, "{name}");
        assert_eq!(got.total_tokens, total, "{name}");
        assert_eq!(breakdown.total_tokens, total, "{name}");
        assert_eq!(breakdown.input.total_tokens, input, "{name}");
        assert_eq!(breakdown.output.total_tokens, output, "{name}");
    }
}

/// Ports TestEnsureTokenBreakdownForUnknownProviderDoesNotGuessReasoning.
#[test]
fn ensure_for_unknown_provider_does_not_guess_reasoning() {
    let detail = ensure_token_breakdown_for_provider(
        Detail {
            input_tokens: 100,
            output_tokens: 30,
            reasoning_tokens: 12,
            ..Detail::default()
        },
        "plugin-provider",
        "",
    );
    assert_eq!(detail.total_tokens, 130);
    assert_eq!(detail.token_breakdown.quality, Quality::Unclassified);
    assert_eq!(detail.token_breakdown.unclassified_tokens, 130);
}

/// Ports TestEnsureTokenBreakdownForUnknownProviderPreservesAuxiliaryOnlyUsage.
#[test]
fn ensure_for_unknown_provider_preserves_auxiliary_only_usage() {
    let detail = ensure_token_breakdown_for_provider(
        Detail {
            reasoning_tokens: 12,
            cache_read_tokens: 7,
            ..Detail::default()
        },
        "plugin-provider",
        "",
    );
    assert_eq!(detail.total_tokens, 19);
    assert_eq!(detail.token_breakdown.quality, Quality::Unclassified);
    assert_eq!(detail.token_breakdown.unclassified_tokens, 19);
}

/// Ports TestEnsureTokenBreakdownForGeminiClassifiesReasoningOnlyUsage.
#[test]
fn ensure_for_gemini_classifies_reasoning_only_usage() {
    let detail = ensure_token_breakdown_for_provider(
        Detail {
            reasoning_tokens: 12,
            ..Detail::default()
        },
        "gemini",
        "",
    );
    assert_eq!(detail.total_tokens, 12);
    assert_eq!(detail.token_breakdown.quality, Quality::Complete);
    assert_eq!(detail.token_breakdown.output.reasoning_tokens, 12);
}

/// Ports TestEnsureTokenBreakdownPreservesLegacyCachedOnlyUsage.
#[test]
fn ensure_preserves_legacy_cached_only_usage() {
    let detail = ensure_token_breakdown_for_provider(
        Detail {
            cached_tokens: 13,
            ..Detail::default()
        },
        "openai",
        "",
    );
    assert_eq!(detail.total_tokens, 13);
    assert_eq!(detail.cache_read_tokens, 13);
    assert_eq!(detail.token_breakdown.quality, Quality::Unclassified);
    assert_eq!(detail.token_breakdown.unclassified_tokens, 13);
}

/// Ports TestEnsureTokenBreakdownDoesNotOverrideCanonicalZeroCacheRead.
#[test]
fn ensure_does_not_override_canonical_zero_cache_read() {
    let detail = ensure_token_breakdown_for_provider(
        Detail {
            cached_tokens: 13,
            cache_creation_tokens: 13,
            ..Detail::default()
        },
        "openai",
        "",
    );
    assert_eq!(detail.cache_read_tokens, 0);
}

/// Ports TestTokenBreakdown_Valid_RejectsArithmeticOverflow.
#[test]
fn valid_rejects_arithmetic_overflow() {
    let input = TokenBreakdown {
        schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
        quality: Quality::Complete,
        input: InputBreakdown {
            total_tokens: 0,
            uncached_tokens: i64::MAX,
            cache_read_tokens: i64::MAX,
            cache_write_tokens: 2,
        },
        ..TokenBreakdown::default()
    };
    assert!(!input.is_valid());
    let output = TokenBreakdown {
        schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
        quality: Quality::Complete,
        output: OutputBreakdown {
            total_tokens: -2,
            non_reasoning_tokens: i64::MAX,
            reasoning_tokens: i64::MAX,
        },
        ..TokenBreakdown::default()
    };
    assert!(!output.is_valid());
}

/// Ports TestNewSubsetTokenBreakdown_RejectsArithmeticOverflow.
#[test]
fn subset_rejects_arithmetic_overflow() {
    let breakdown = TokenBreakdown::subset(i64::MAX, i64::MAX, i64::MAX, 0, 0, i64::MAX);
    assert_ne!(breakdown.quality, Quality::Complete, "{breakdown:?}");
    assert!(breakdown.input.uncached_tokens >= 0);
}

/// Ports TestNewSeparateReasoningTokenBreakdown_RejectsArithmeticOverflow.
#[test]
fn separate_reasoning_rejects_arithmetic_overflow() {
    let breakdown =
        TokenBreakdown::separate_reasoning(i64::MAX, i64::MAX, i64::MAX, 0, 0, i64::MAX);
    assert_ne!(breakdown.quality, Quality::Complete, "{breakdown:?}");
    assert!(breakdown.input.uncached_tokens >= 0);
}
