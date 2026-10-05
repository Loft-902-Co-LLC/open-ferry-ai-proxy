// Ported from CLIProxyAPI sdk/cliproxy/usage/accounting.go (TokenBreakdown,
// Valid, NewSubsetTokenBreakdown, NewPartialSubsetTokenBreakdown,
// NewIndependentTokenBreakdown, NewSeparateReasoningTokenBreakdown,
// NewUnclassifiedTokenBreakdown, EnsureTokenBreakdownForProvider,
// tokenBreakdownForSemantics, unclassifiedTokenLowerBound,
// tokenAccountingSemanticsFor, inconsistentTokenBreakdown,
// resolveAccountingTotal, nonNegativeSum) and sdk/cliproxy/usage/manager.go
// (Detail) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A call's token counts ([`Detail`]), and their canonical breakdown
//! ([`TokenBreakdown`], version 2): buckets that don't overlap, built for
//! how each provider counts.
//!
//! Providers count tokens three ways. OpenAI's protocols count cache reads
//! and writes inside the input, and reasoning inside the output
//! ([`TokenBreakdown::subset`]); Anthropic's count each apart
//! ([`TokenBreakdown::independent`]); Gemini's count the cache inside the
//! input and reasoning beside the output
//! ([`TokenBreakdown::separate_reasoning`]). An unknown provider's tokens
//! stay unclassified rather than be guessed at.
//!
//! Deviations from upstream: none.

use open_ferry_translate::go;

/// The version of the breakdown's contract (upstream's
/// `TokenAccountingSchemaVersion`).
pub const TOKEN_ACCOUNTING_SCHEMA_VERSION: i64 = 2;

/// How surely a breakdown's total is classified (upstream's
/// `TokenAccountingQuality`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// None given: a breakdown not built yet (Go's empty string).
    #[default]
    Unset,
    /// Every token is in a bucket.
    Complete,
    /// The counts contradict each other; the total is unclassified.
    Inconsistent,
    /// Some or all tokens are in no bucket.
    Unclassified,
}

impl Quality {
    /// The quality as upstream writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unset => "",
            Self::Complete => "complete",
            Self::Inconsistent => "inconsistent",
            Self::Unclassified => "unclassified",
        }
    }
}

/// The input's buckets, which don't overlap (upstream's
/// `TokenInputBreakdown`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputBreakdown {
    /// The input's tokens.
    pub total_tokens: i64,
    /// Those not read from or written to the cache.
    pub uncached_tokens: i64,
    /// Those read from the cache.
    pub cache_read_tokens: i64,
    /// Those written to the cache.
    pub cache_write_tokens: i64,
}

/// The output's buckets, which don't overlap (upstream's
/// `TokenOutputBreakdown`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputBreakdown {
    /// The output's tokens.
    pub total_tokens: i64,
    /// Those that aren't reasoning.
    pub non_reasoning_tokens: i64,
    /// The reasoning's.
    pub reasoning_tokens: i64,
}

/// A call's tokens in buckets that don't overlap (upstream's
/// `TokenBreakdown`). The default is Go's zero value, which isn't valid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenBreakdown {
    /// [`TOKEN_ACCOUNTING_SCHEMA_VERSION`] once built.
    pub schema_version: i64,
    /// How surely the total is classified.
    pub quality: Quality,
    /// Every token.
    pub total_tokens: i64,
    /// The input's.
    pub input: InputBreakdown,
    /// The output's.
    pub output: OutputBreakdown,
    /// Those in no bucket.
    pub unclassified_tokens: i64,
}

/// A call's token counts as its provider reported them, and their
/// breakdown (upstream's `usage.Detail`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detail {
    /// The input's tokens.
    pub input_tokens: i64,
    /// The output's tokens.
    pub output_tokens: i64,
    /// The reasoning's tokens.
    pub reasoning_tokens: i64,
    /// The cached tokens, as providers used to report them.
    pub cached_tokens: i64,
    /// The tokens read from the cache.
    pub cache_read_tokens: i64,
    /// The tokens written to the cache.
    pub cache_creation_tokens: i64,
    /// Every token.
    pub total_tokens: i64,
    /// The breakdown.
    pub token_breakdown: TokenBreakdown,
    /// The service tier the answer reports.
    pub response_service_tier: String,
}

impl Detail {
    /// Whether any count is not zero (upstream's `hasNonZeroTokenUsage`).
    pub fn has_tokens(&self) -> bool {
        self.input_tokens != 0
            || self.output_tokens != 0
            || self.reasoning_tokens != 0
            || self.cached_tokens != 0
            || self.cache_read_tokens != 0
            || self.cache_creation_tokens != 0
            || self.total_tokens != 0
            || self.token_breakdown.total_tokens != 0
    }
}

/// How a provider's counts overlap (upstream's
/// `tokenAccountingSemantics`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Semantics {
    Unknown,
    Subset,
    Independent,
    SeparateReasoning,
}

impl TokenBreakdown {
    /// Whether it keeps version 2's rules: every count at least zero, each
    /// total the sum of its buckets, and nothing unclassified when complete
    /// (upstream's `Valid`).
    pub fn is_valid(&self) -> bool {
        if self.schema_version != TOKEN_ACCOUNTING_SCHEMA_VERSION || self.quality == Quality::Unset
        {
            return false;
        }
        let counts = [
            self.total_tokens,
            self.unclassified_tokens,
            self.input.total_tokens,
            self.input.uncached_tokens,
            self.input.cache_read_tokens,
            self.input.cache_write_tokens,
            self.output.total_tokens,
            self.output.non_reasoning_tokens,
            self.output.reasoning_tokens,
        ];
        if counts.iter().any(|&count| count < 0) {
            return false;
        }
        let input = non_negative_sum(&[
            self.input.uncached_tokens,
            self.input.cache_read_tokens,
            self.input.cache_write_tokens,
        ]);
        if input != Some(self.input.total_tokens) {
            return false;
        }
        let output = non_negative_sum(&[
            self.output.non_reasoning_tokens,
            self.output.reasoning_tokens,
        ]);
        if output != Some(self.output.total_tokens) {
            return false;
        }
        let total = non_negative_sum(&[
            self.input.total_tokens,
            self.output.total_tokens,
            self.unclassified_tokens,
        ]);
        if total != Some(self.total_tokens) {
            return false;
        }
        !(self.quality == Quality::Complete && self.unclassified_tokens != 0)
    }

    /// The breakdown of a protocol that counts the cache inside the input
    /// and reasoning inside the output (upstream's
    /// `NewSubsetTokenBreakdown`).
    pub fn subset(
        input_total: i64,
        cache_read: i64,
        cache_write: i64,
        output_total: i64,
        reasoning: i64,
        total: i64,
    ) -> Self {
        let cache_total = non_negative_sum(&[cache_read, cache_write]);
        let expected = non_negative_sum(&[input_total, output_total]);
        let (Some(cache_total), Some(expected)) = (cache_total, expected) else {
            return inconsistent(total, expected.unwrap_or(0));
        };
        if reasoning < 0 || cache_total > input_total || reasoning > output_total {
            return inconsistent(total, expected);
        }
        let Some(resolved) = resolve_total(total, expected) else {
            return inconsistent(total, expected);
        };
        Self {
            schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
            quality: Quality::Complete,
            total_tokens: resolved,
            input: InputBreakdown {
                total_tokens: input_total,
                uncached_tokens: input_total - cache_total,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
            },
            output: OutputBreakdown {
                total_tokens: output_total,
                non_reasoning_tokens: output_total - reasoning,
                reasoning_tokens: reasoning,
            },
            unclassified_tokens: 0,
        }
    }

    /// [`Self::subset`] for counts that may leave part of the total in no
    /// bucket: the rest is unclassified (upstream's
    /// `NewPartialSubsetTokenBreakdown`).
    pub fn partial_subset(
        input_total: i64,
        cache_read: i64,
        cache_write: i64,
        output_total: i64,
        reasoning: i64,
        total: i64,
    ) -> Self {
        let cache_total = non_negative_sum(&[cache_read, cache_write]);
        let expected = non_negative_sum(&[input_total, output_total]);
        let (Some(cache_total), Some(expected)) = (cache_total, expected) else {
            return inconsistent(total, expected.unwrap_or(0));
        };
        if input_total < 0
            || output_total < 0
            || reasoning < 0
            || cache_total > input_total
            || reasoning > output_total
            || total < 0
        {
            return inconsistent(total, expected);
        }
        let resolved = if total == 0 { expected } else { total };
        if resolved < expected {
            return inconsistent(total, expected);
        }
        let unclassified = resolved - expected;
        Self {
            schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
            quality: if unclassified > 0 {
                Quality::Unclassified
            } else {
                Quality::Complete
            },
            total_tokens: resolved,
            input: InputBreakdown {
                total_tokens: input_total,
                uncached_tokens: input_total - cache_total,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
            },
            output: OutputBreakdown {
                total_tokens: output_total,
                non_reasoning_tokens: output_total - reasoning,
                reasoning_tokens: reasoning,
            },
            unclassified_tokens: unclassified,
        }
    }

    /// The breakdown of a protocol that counts uncached input, cache reads,
    /// cache writes, output and reasoning apart (upstream's
    /// `NewIndependentTokenBreakdown`).
    pub fn independent(
        uncached_input: i64,
        cache_read: i64,
        cache_write: i64,
        non_reasoning_output: i64,
        reasoning: i64,
        total: i64,
    ) -> Self {
        let input_total = non_negative_sum(&[uncached_input, cache_read, cache_write]);
        let output_total = non_negative_sum(&[non_reasoning_output, reasoning]);
        let expected = non_negative_sum(&[input_total.unwrap_or(0), output_total.unwrap_or(0)]);
        let (Some(input_total), Some(output_total), Some(expected)) =
            (input_total, output_total, expected)
        else {
            return inconsistent(total, expected.unwrap_or(0));
        };
        let Some(resolved) = resolve_total(total, expected) else {
            return inconsistent(total, expected);
        };
        Self {
            schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
            quality: Quality::Complete,
            total_tokens: resolved,
            input: InputBreakdown {
                total_tokens: input_total,
                uncached_tokens: uncached_input,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
            },
            output: OutputBreakdown {
                total_tokens: output_total,
                non_reasoning_tokens: non_reasoning_output,
                reasoning_tokens: reasoning,
            },
            unclassified_tokens: 0,
        }
    }

    /// The breakdown of a protocol that counts the cache inside the input
    /// and reasoning beside the output (upstream's
    /// `NewSeparateReasoningTokenBreakdown`).
    pub fn separate_reasoning(
        input_total: i64,
        cache_read: i64,
        cache_write: i64,
        non_reasoning_output: i64,
        reasoning: i64,
        total: i64,
    ) -> Self {
        let Some(cache_total) = non_negative_sum(&[cache_read, cache_write])
            .filter(|&cache_total| input_total >= 0 && cache_total <= input_total)
        else {
            return inconsistent(total, 0);
        };
        let output_total = non_negative_sum(&[non_reasoning_output, reasoning]);
        let expected = non_negative_sum(&[input_total, output_total.unwrap_or(0)]);
        let (Some(output_total), Some(expected)) = (output_total, expected) else {
            return inconsistent(total, expected.unwrap_or(0));
        };
        let Some(resolved) = resolve_total(total, expected) else {
            return inconsistent(total, expected);
        };
        Self {
            schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
            quality: Quality::Complete,
            total_tokens: resolved,
            input: InputBreakdown {
                total_tokens: input_total,
                uncached_tokens: input_total - cache_total,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
            },
            output: OutputBreakdown {
                total_tokens: output_total,
                non_reasoning_tokens: non_reasoning_output,
                reasoning_tokens: reasoning,
            },
            unclassified_tokens: 0,
        }
    }

    /// A total kept without guessing how it splits (upstream's
    /// `NewUnclassifiedTokenBreakdown`).
    pub fn unclassified(total: i64) -> Self {
        if total <= 0 {
            return Self {
                schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
                quality: if total < 0 {
                    Quality::Inconsistent
                } else {
                    Quality::Complete
                },
                ..Self::default()
            };
        }
        Self {
            schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
            quality: Quality::Unclassified,
            total_tokens: total,
            unclassified_tokens: total,
            ..Self::default()
        }
    }

    /// An inconsistent breakdown of `total`, as the usage parsers give for
    /// counts that overflow (upstream's `invalidUsageTokenBreakdown`).
    pub(crate) fn invalid(total: i64) -> Self {
        let total = total.max(0);
        Self {
            schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
            quality: Quality::Inconsistent,
            total_tokens: total,
            unclassified_tokens: total,
            ..Self::default()
        }
    }
}

/// `detail` with a valid breakdown, built for how `provider` and
/// `executor_type` count tokens unless it has one, and its total filled in
/// from the breakdown when it has none (upstream's
/// `EnsureTokenBreakdownForProvider`).
pub fn ensure_token_breakdown_for_provider(
    mut detail: Detail,
    provider: &str,
    executor_type: &str,
) -> Detail {
    if !detail.token_breakdown.is_valid() {
        let semantics = semantics_for(provider, executor_type);
        if detail.cache_read_tokens == 0
            && detail.cached_tokens > 0
            && detail.input_tokens == 0
            && detail.output_tokens == 0
            && detail.reasoning_tokens == 0
            && detail.cache_creation_tokens == 0
            && detail.total_tokens == 0
            && matches!(semantics, Semantics::Subset | Semantics::SeparateReasoning)
        {
            detail.cache_read_tokens = detail.cached_tokens;
        }
        detail.token_breakdown = breakdown_for(&detail, semantics);
    }
    if detail.total_tokens == 0 {
        detail.total_tokens = detail.token_breakdown.total_tokens;
    }
    detail
}

/// The breakdown of `detail` under `semantics` (upstream's
/// `tokenBreakdownForSemantics`).
fn breakdown_for(detail: &Detail, semantics: Semantics) -> TokenBreakdown {
    if detail.total_tokens == 0 && detail.input_tokens == 0 && detail.output_tokens == 0 {
        let Some(total) = unclassified_lower_bound(detail) else {
            return inconsistent(detail.total_tokens, 0);
        };
        let has_cache = detail.cache_read_tokens > 0
            || detail.cache_creation_tokens > 0
            || detail.cached_tokens > 0;
        if total > 0
            && (matches!(semantics, Semantics::Unknown | Semantics::Subset)
                || (semantics == Semantics::SeparateReasoning && has_cache))
        {
            return TokenBreakdown::unclassified(total);
        }
    }
    let build = match semantics {
        Semantics::Subset => TokenBreakdown::subset,
        Semantics::Independent => TokenBreakdown::independent,
        Semantics::SeparateReasoning => TokenBreakdown::separate_reasoning,
        Semantics::Unknown => {
            let mut total = detail.total_tokens;
            if total == 0 {
                let Some(bound) = unclassified_lower_bound(detail) else {
                    return inconsistent(detail.total_tokens, 0);
                };
                total = bound;
            }
            return TokenBreakdown::unclassified(total);
        }
    };
    build(
        detail.input_tokens,
        detail.cache_read_tokens,
        detail.cache_creation_tokens,
        detail.output_tokens,
        detail.reasoning_tokens,
        detail.total_tokens,
    )
}

/// The fewest tokens `detail`'s counts can add up to, however they overlap
/// (upstream's `unclassifiedTokenLowerBound`).
fn unclassified_lower_bound(detail: &Detail) -> Option<i64> {
    let cache = non_negative_sum(&[detail.cache_read_tokens, detail.cache_creation_tokens])?;
    if detail.input_tokens < 0
        || detail.output_tokens < 0
        || detail.reasoning_tokens < 0
        || detail.cached_tokens < 0
    {
        return None;
    }
    let input = detail.input_tokens.max(cache).max(detail.cached_tokens);
    let output = detail.output_tokens.max(detail.reasoning_tokens);
    non_negative_sum(&[input, output])
}

/// How `provider` and `executor_type` count tokens (upstream's
/// `tokenAccountingSemanticsFor`).
fn semantics_for(provider: &str, executor_type: &str) -> Semantics {
    let provider = go::to_lower(provider.trim());
    let executor = go::to_lower(executor_type.trim());
    let value = format!("{provider} {executor}");
    let value = value.trim();
    if value.is_empty() || value == "unknown" || value == "unknown unknown" {
        return Semantics::Unknown;
    }
    if executor == "openaicompatexecutor"
        || provider == "openai-compatibility"
        || provider.starts_with("openai-compatible-")
    {
        return Semantics::Subset;
    }
    if value.contains("claude") || value.contains("anthropic") {
        return Semantics::Independent;
    }
    let separate = ["gemini", "aistudio", "antigravity", "vertex", "interaction"];
    if separate.iter().any(|marker| value.contains(marker)) {
        return Semantics::SeparateReasoning;
    }
    let subset = [
        "openai",
        "codex",
        "xai",
        "grok",
        "kimi",
        "qwen",
        "deepseek",
        "openrouter",
    ];
    if subset.iter().any(|marker| value.contains(marker)) {
        return Semantics::Subset;
    }
    Semantics::Unknown
}

/// An inconsistent breakdown of `total`, or of `fallback` when `total`
/// isn't positive (upstream's `inconsistentTokenBreakdown`).
fn inconsistent(total: i64, fallback: i64) -> TokenBreakdown {
    let mut resolved = if total <= 0 { fallback } else { total };
    if resolved < 0 {
        resolved = 0;
    }
    TokenBreakdown {
        schema_version: TOKEN_ACCOUNTING_SCHEMA_VERSION,
        quality: Quality::Inconsistent,
        total_tokens: resolved,
        unclassified_tokens: resolved,
        ..TokenBreakdown::default()
    }
}

/// The total to keep: `expected` when none was given, else `total` if it
/// matches; `None` for a mismatch or a negative count (upstream's
/// `resolveAccountingTotal`).
fn resolve_total(total: i64, expected: i64) -> Option<i64> {
    if total < 0 || expected < 0 {
        return None;
    }
    if total == 0 {
        return Some(expected);
    }
    (total == expected).then_some(total)
}

/// The sum of `values`, or `None` if one is negative or the sum overflows
/// (upstream's `nonNegativeSum` and `safeUsageTokenSum`).
pub(crate) fn non_negative_sum(values: &[i64]) -> Option<i64> {
    values.iter().try_fold(0_i64, |total, &value| {
        if value < 0 {
            return None;
        }
        total.checked_add(value)
    })
}
