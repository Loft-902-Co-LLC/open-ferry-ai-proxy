// Ported from CLIProxyAPI sdk/cliproxy/auth/quota_signals.go
// (ProviderSupportsQuotaObservation, ObserveResponseHeadersForProvider,
// ClearObservationSignals, cooldownFieldsOf, applyCooldownFields,
// collectQuotaSignals, validQuotaSignalValue, quotaSignalRetentionRank,
// isQuotaSignalHeaderForProvider and mergeQuotaObservation) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Passive quota observations: what the provider's last response said of a
//! credential's quota, in its own headers.
//!
//! Claude's `Anthropic-Ratelimit-Unified-*` headers and Codex's `X-Codex-*`
//! windows, plan and credits, and `Retry-After` for both, are kept as a
//! [`QuotaState`]'s `signals`, by canonical header name, with the time they
//! came as its `observed_at`. Each response that carries any replaces the
//! snapshot whole: a watermark such as `Retry-After` is only on the
//! response that produced it, so keeping the old ones would show it after
//! it expired. A response with none, as a transport failure or a 5xx
//! usually is, leaves the snapshot as it is.
//!
//! A snapshot keeps at most 64 headers, those that say most about the
//! credential first, and no value longer than 512 bytes or holding a
//! control character. The cooldown fields are never read or written here,
//! and cooldown changes leave the snapshot alone
//! ([`apply_cooldown_fields`]).
//!
//! Deviations from upstream:
//! - Devin isn't one of the providers observed, as it isn't ported: a
//!   `devin` result clears a snapshot, where upstream's leaves it.
//! - open-ferry's `claude-cli` provider is observed as Claude is: its
//!   executor writes what Claude Code reports of the account's windows as
//!   Claude's unified headers.
//! - A value's bytes that aren't UTF-8 are kept as U+FFFD, one for each
//!   byte, as Go's JSON encoder writes them; their length is counted as
//!   the bytes that came.
//! - Codex's `codex.rate_limits` WebSocket events aren't observed: upstream
//!   merges each into the call's headers (`ParseCodexQuotaEventHeaders`),
//!   and here a WebSocket call is observed by its stream's headers alone.

use std::collections::BTreeMap;

use http::HeaderMap;

use super::credential::is_zero;
use super::text::go_lower;
use crate::auth::{QuotaState, Timestamp};
use crate::multipart::{canonical_key, lossy};

/// The most headers a snapshot keeps (upstream's `maxQuotaSignalHeaders`).
pub(crate) const MAX_QUOTA_SIGNAL_HEADERS: usize = 64;
/// The longest value a snapshot keeps, in bytes (upstream's
/// `maxQuotaSignalValue`).
pub(crate) const MAX_QUOTA_SIGNAL_VALUE: usize = 512;

/// Whether `provider`'s responses carry a quota snapshot this module reads:
/// Claude's, `claude-cli`'s and Codex's, in any case and with spaces around
/// (upstream's `ProviderSupportsQuotaObservation`).
pub fn provider_supports_quota_observation(provider: &str) -> bool {
    matches!(
        go_lower(provider.trim()).as_str(),
        "claude" | "claude-cli" | "codex"
    )
}

impl QuotaState {
    /// Replaces the snapshot with the quota headers of one `provider`
    /// response that came at `observed_at`, and says whether anything
    /// changed (upstream's `ObserveResponseHeadersForProvider`). A response
    /// with no quota header leaves the snapshot alone; a provider that
    /// isn't observed has its snapshot cleared.
    pub fn observe_response_headers_for_provider(
        &mut self,
        provider: &str,
        headers: &HeaderMap,
        observed_at: Timestamp,
    ) -> bool {
        if !provider_supports_quota_observation(provider) {
            return self.clear_observation_signals();
        }
        let next = collect_quota_signals(provider, headers);
        if next.is_empty() {
            return false;
        }
        self.signals = next;
        self.observed_at = Some(observed_at);
        true
    }

    /// Drops the snapshot, leaving the cooldown fields; says whether there
    /// was one (upstream's `ClearObservationSignals`).
    pub fn clear_observation_signals(&mut self) -> bool {
        if self.signals.is_empty() && is_zero(self.observed_at) {
            return false;
        }
        self.signals.clear();
        self.observed_at = None;
        true
    }
}

/// `quota`'s cooldown fields alone, its snapshot left out (upstream's
/// `cooldownFieldsOf`).
pub(crate) fn cooldown_fields_of(quota: &QuotaState) -> QuotaState {
    QuotaState {
        exceeded: quota.exceeded,
        reason: quota.reason.clone(),
        next_recover_at: quota.next_recover_at,
        backoff_level: quota.backoff_level,
        ..QuotaState::default()
    }
}

/// Writes `cooldown`'s cooldown fields into `dst`, leaving `dst`'s snapshot
/// as it is (upstream's `applyCooldownFields`).
pub(crate) fn apply_cooldown_fields(dst: &mut QuotaState, cooldown: QuotaState) {
    dst.exceeded = cooldown.exceeded;
    dst.reason = cooldown.reason;
    dst.next_recover_at = cooldown.next_recover_at;
    dst.backoff_level = cooldown.backoff_level;
}

/// `target` with `source`'s snapshot when that one is at least as new;
/// two snapshots are never united (upstream's `mergeQuotaObservation`).
pub(crate) fn merge_quota_observation(mut target: QuotaState, source: &QuotaState) -> QuotaState {
    if is_zero(source.observed_at) || source.observed_at < target.observed_at {
        return target;
    }
    target.observed_at = source.observed_at;
    target.signals.clone_from(&source.signals);
    target
}

/// The snapshot of one response's `headers` (upstream's
/// `collectQuotaSignals`): the last value of each quota header `provider`
/// sends, by canonical name, cut to the first 64 by retention rank and
/// then name.
fn collect_quota_signals(provider: &str, headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut found: Vec<(u8, String, String)> = Vec::new();
    for name in headers.keys() {
        let canonical = canonical_key(name.as_str().trim());
        if !is_quota_signal_header_for_provider(provider, &canonical) {
            continue;
        }
        let Some(last) = headers.get_all(name).iter().next_back() else {
            continue;
        };
        let Some(value) = quota_signal_value(last.as_bytes()) else {
            continue;
        };
        found.push((quota_signal_retention_rank(&canonical), canonical, value));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    found.truncate(MAX_QUOTA_SIGNAL_HEADERS);
    found
        .into_iter()
        .map(|(_, name, value)| (name, value))
        .collect()
}

/// A header value as a snapshot keeps it: trimmed, and `None` when that
/// leaves it empty, longer than 512 bytes or holding a control character
/// (Go's `strings.TrimSpace` and upstream's `validQuotaSignalValue`). A
/// control character could forge a line in the plain-text request log
/// these values reach.
fn quota_signal_value(raw: &[u8]) -> Option<String> {
    let text = lossy(raw);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // What was trimmed is white space, which reads the same in `raw`.
    let padding = text.len().saturating_sub(trimmed.len());
    if raw.len().saturating_sub(padding) > MAX_QUOTA_SIGNAL_VALUE {
        return None;
    }
    if trimmed.chars().any(|c| c < '\u{20}' || c == '\u{7f}') {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Which headers a full snapshot keeps first, lowest first (upstream's
/// `quotaSignalRetentionRank`): `Retry-After` and Claude's, then Codex's
/// plan, active limit and credits, its main windows, code review,
/// the rest of Codex's, and the WebSocket's additional limits.
fn quota_signal_retention_rank(name: &str) -> u8 {
    let lower = go_lower(name.trim());
    let lower = lower.as_str();
    if lower == "retry-after" || lower.starts_with("anthropic-ratelimit-unified-") {
        0
    } else if lower == "x-codex-plan-type"
        || lower == "x-codex-active-limit"
        || lower.starts_with("x-codex-credits-")
    {
        1
    } else if lower == "x-codex-allowed"
        || lower == "x-codex-limit-reached"
        || lower.starts_with("x-codex-primary-")
        || lower.starts_with("x-codex-secondary-")
    {
        2
    } else if lower.starts_with("x-codex-code-review-") {
        3
    } else if lower.starts_with("x-codex-additional-") {
        5
    } else if lower.starts_with("x-codex-") {
        4
    } else {
        6
    }
}

/// The name parts that mark one of Codex's per-limit quota headers, as in
/// `x-codex-bengalfox-primary-used-percent`.
const CODEX_QUOTA_MARKERS: &[&str] = &[
    "-allowed",
    "-limit-reached",
    "-limit-name",
    "-used-percent",
    "-window-minutes",
    "-reset-after-seconds",
    "-reset-at",
    "-over-secondary-limit-percent",
];

/// Whether `name` is a quota header of `provider`'s (upstream's
/// `isQuotaSignalHeaderForProvider`). Other headers a provider sends, its
/// workspace or turn state among them, are never kept.
fn is_quota_signal_header_for_provider(provider: &str, name: &str) -> bool {
    let provider = go_lower(provider.trim());
    let name = go_lower(name.trim());
    if name == "retry-after" {
        return provider == "claude" || provider == "claude-cli" || provider == "codex";
    }
    if name.starts_with("anthropic-ratelimit-unified-") {
        return provider == "claude" || provider == "claude-cli";
    }
    if name.starts_with("x-ratelimit-") {
        // Upstream keeps this for a future Codex rollout; Codex doesn't send
        // them today.
        return provider == "codex";
    }
    if !name.starts_with("x-codex-") || provider != "codex" {
        return false;
    }
    if name == "x-codex-active-limit"
        || name == "x-codex-plan-type"
        || name.starts_with("x-codex-credits-")
    {
        return true;
    }
    CODEX_QUOTA_MARKERS
        .iter()
        .any(|marker| name.contains(marker))
}
