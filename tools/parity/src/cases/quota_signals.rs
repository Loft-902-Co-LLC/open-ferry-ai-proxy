//! Hand-written cases for the quota snapshot of one response's headers: the
//! headers upstream's tests observe (sdk/cliproxy/auth/quota_signals_test.go),
//! with a tab for the CR and LF a header value here can't hold, and, not
//! upstream's, the corners: values at the length limit with and without
//! padding, Unicode white space and bytes that aren't UTF-8, names in other
//! cases and repeated, providers in other cases and with spaces around, and
//! prior snapshots kept, replaced or cleared.

use serde_json::{Value, json};

use super::Case;

/// The hand-written cases for `quota-signals/observe`.
pub fn observations() -> Vec<Case> {
    let mut cases = vec![
        case(
            "upstream-keeps-provider-scoped-signals",
            "codex",
            texts(&[
                ("X-Codex-Active-Limit", "codex_bengalfox"),
                ("X-Codex-Primary-Used-Percent", "2"),
                ("X-Codex-Turn-State", "opaque-state"),
                ("X-Codex-Safety-Buffering-Enabled", "true"),
                ("Retry-After", "120"),
                ("Authorization", "Bearer secret"),
            ]),
            none(),
        ),
        case(
            "upstream-drops-empty-and-long-values",
            "codex",
            texts(&[("X-Codex-Empty", ""), ("X-Codex-Long", &"x".repeat(513))]),
            none(),
        ),
        case(
            "upstream-canonicalizes-names",
            "codex",
            texts(&[("x-codex-plan-type", "pro")]),
            none(),
        ),
        case(
            "upstream-claude-watermarks",
            "claude",
            texts(&[
                ("Anthropic-Ratelimit-Unified-5h-Status", "allowed"),
                ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.0"),
                ("Anthropic-Ratelimit-Unified-5h-Reset", "1787296800"),
                ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
                ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.53"),
                ("Anthropic-Ratelimit-Unified-7d-Reset", "1787695200"),
                ("Anthropic-Ratelimit-Unified-Fallback-Percentage", "0.5"),
                (
                    "Anthropic-Ratelimit-Unified-Overage-Disabled-Reason",
                    "member_zero_credit_limit",
                ),
                ("Anthropic-Ratelimit-Unified-Overage-Status", "rejected"),
                (
                    "Anthropic-Ratelimit-Unified-Representative-Claim",
                    "five_hour",
                ),
                ("Anthropic-Ratelimit-Unified-Reset", "1787296800"),
                ("Anthropic-Ratelimit-Unified-Status", "allowed"),
                (
                    "Anthropic-Workspace-Id",
                    "workspace-must-not-be-quota-signal",
                ),
            ]),
            none(),
        ),
        case(
            "upstream-codex-watermarks",
            "codex",
            texts(&[
                ("X-Codex-Plan-Type", "pro"),
                ("X-Codex-Primary-Used-Percent", "51"),
                ("X-Codex-Primary-Window-Minutes", "10080"),
                ("X-Codex-Primary-Reset-After-Seconds", "309718"),
                ("X-Codex-Primary-Reset-At", "1787588999"),
                ("X-Codex-Bengalfox-Limit-Name", "GPT-5.3-Codex-Spark"),
                ("X-Codex-Bengalfox-Secondary-Used-Percent", "35"),
                ("X-Codex-Credits-Has-Credits", "False"),
            ]),
            none(),
        ),
        case(
            "upstream-replaces-stale-watermarks",
            "codex",
            texts(&[("X-Codex-Primary-Used-Percent", "5")]),
            prior(
                10,
                &[
                    ("Retry-After", "120"),
                    ("X-Codex-Primary-Used-Percent", "99"),
                ],
            ),
        ),
        case(
            "upstream-keeps-snapshot-without-signal",
            "codex",
            texts(&[("Content-Type", "application/json")]),
            prior(10, &[("X-Codex-Primary-Used-Percent", "5")]),
        ),
        case(
            "upstream-advances-observed-at-on-repeated-values",
            "codex",
            texts(&[("X-Codex-Primary-Used-Percent", "5")]),
            prior(10, &[("X-Codex-Primary-Used-Percent", "5")]),
        ),
        case(
            "upstream-rejects-control-characters",
            "codex",
            texts(&[("X-Codex-Bengalfox-Limit-Name", "evil\tX-Injected: 1")]),
            none(),
        ),
        case(
            "upstream-truncates-deterministically",
            "codex",
            (0..70)
                .rev()
                .map(|i| {
                    text(
                        &format!("X-Codex-L{i:03}-Primary-Used-Percent"),
                        &i.to_string(),
                    )
                })
                .collect(),
            none(),
        ),
        case(
            "upstream-keeps-primary-when-truncating-additional",
            "codex",
            texts(&[
                ("X-Codex-Plan-Type", "pro"),
                ("X-Codex-Primary-Used-Percent", "81"),
                ("X-Codex-Credits-Balance", "0"),
            ])
            .into_iter()
            .chain((0..64).map(|i| {
                text(
                    &format!("X-Codex-Additional-L{i:03}-Primary-Used-Percent"),
                    &i.to_string(),
                )
            }))
            .collect(),
            none(),
        ),
        case(
            "codex-keeps-x-ratelimit-headers",
            "codex",
            texts(&[
                ("X-Ratelimit-Remaining-Requests", "0"),
                ("X-Ratelimit-Remaining-Tokens", "0"),
                ("Retry-After", "60"),
            ]),
            prior(10, &[("old", "value")]),
        ),
    ];
    for provider in ["kimi", "grok", "antigravity"] {
        cases.push(case(
            &format!("upstream-drops-{provider}-signals"),
            provider,
            texts(&[
                ("X-Ratelimit-Remaining-Requests", "0"),
                ("X-Ratelimit-Remaining-Tokens", "0"),
                ("Retry-After", "60"),
            ]),
            prior(10, &[("old", "value")]),
        ));
    }
    cases.extend(corners());
    cases.push(
        case(
            "devin-is-not-observed",
            "devin",
            texts(&[("Retry-After", "60")]),
            prior(10, &[("Retry-After", "30")]),
        )
        .known_difference(
            "Devin isn't ported, so its results clear a snapshot upstream keeps (see UPSTREAM.md)",
        ),
    );
    cases
}

/// Not upstream's: the corners of names, values, providers and prior
/// snapshots.
fn corners() -> Vec<Case> {
    let long = "9".repeat(512);
    vec![
        case(
            "value-at-the-limit",
            "codex",
            texts(&[
                ("X-Codex-Primary-Used-Percent", &long),
                ("X-Codex-Secondary-Used-Percent", &format!("{long}9")),
            ]),
            none(),
        ),
        case(
            "padding-is-not-counted",
            "claude",
            texts(&[
                (
                    "Anthropic-Ratelimit-Unified-Status",
                    &format!("  {long}\t "),
                ),
                ("Retry-After", " \t "),
            ]),
            none(),
        ),
        case(
            "unicode-white-space-is-trimmed",
            "codex",
            vec![
                hex("X-Codex-Plan-Type", "c2a070726fc2a0"),
                hex("X-Codex-Active-Limit", "e3808061e38080"),
                hex("X-Codex-Credits-Balance", "c2853130c285"),
                hex("Retry-After", "e2808b3330e2808b"),
            ],
            none(),
        ),
        case(
            "bytes-that-are-not-utf-8",
            "codex",
            vec![
                hex("X-Codex-Plan-Type", "70ff726f"),
                hex("X-Codex-Active-Limit", "e28061"),
                hex("X-Codex-Credits-Balance", "20eda080f09f9820"),
                hex("Retry-After", &"ff".repeat(512)),
                hex("X-Codex-Primary-Used-Percent", &"ff".repeat(513)),
            ],
            none(),
        ),
        case(
            "repeated-names-keep-the-last-value",
            "codex",
            texts(&[
                ("X-Codex-Plan-Type", "plus"),
                ("x-codex-plan-type", "pro"),
                ("Retry-After", "30"),
                ("RETRY-AFTER", " "),
                ("X-Codex-Active-Limit", "a"),
                ("x-CODEX-active-LIMIT", "b"),
            ]),
            none(),
        ),
        case(
            "provider-in-another-case",
            " Claude\t",
            texts(&[
                ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.25"),
                ("Retry-After", "5"),
                ("X-Codex-Plan-Type", "pro"),
            ]),
            none(),
        ),
        case(
            "claude-ignores-codex-headers",
            "claude",
            texts(&[("X-Codex-Plan-Type", "pro"), ("X-Ratelimit-Limit", "1")]),
            prior(10, &[("Anthropic-Ratelimit-Unified-Status", "allowed")]),
        ),
        case(
            "codex-name-markers",
            "codex",
            texts(&[
                ("X-Codex-Code-Review-Allowed", "true"),
                ("X-Codex-Limit-Reached", "false"),
                ("X-Codex-Foo-Over-Secondary-Limit-Percent", "3"),
                ("X-Codex-Foo-Limit-Name-Extra", "x"),
                ("X-Codex-Foo-Reset-At-Next", "y"),
                ("X-Codex-Credits", "no-dash"),
                ("X-Codex-Allowed", "true"),
                ("X-Codex-Ratelimit", "nope"),
            ]),
            none(),
        ),
        case(
            "an-unobserved-provider-with-no-snapshot",
            "gemini",
            texts(&[("Retry-After", "60")]),
            none(),
        ),
        case(
            "an-unobserved-provider-with-only-a-time",
            "",
            texts(&[]),
            prior(10, &[]),
        ),
        case(
            "an-unobserved-provider-with-only-signals",
            "openai",
            texts(&[]),
            prior(0, &[("Retry-After", "1")]),
        ),
        case(
            "an-older-time-is-replaced",
            "codex",
            texts(&[("Retry-After", "1")]),
            prior(5000, &[("X-Codex-Plan-Type", "pro")]),
        ),
    ]
}

fn case(name: &str, provider: &str, headers: Vec<Value>, prior: Value) -> Case {
    Case::new(name, "", "").with_options(json!({
        "provider": provider,
        "headers": headers,
        "prior": prior,
    }))
}

fn text(name: &str, value: &str) -> Value {
    json!({ "name": name, "value": value })
}

fn texts(pairs: &[(&str, &str)]) -> Vec<Value> {
    pairs
        .iter()
        .map(|(name, value)| text(name, value))
        .collect()
}

fn hex(name: &str, hex: &str) -> Value {
    json!({ "name": name, "hex": hex })
}

fn none() -> Value {
    json!({ "observed_at": 0, "signals": {} })
}

fn prior(observed_at: i64, signals: &[(&str, &str)]) -> Value {
    let signals: serde_json::Map<String, Value> = signals
        .iter()
        .map(|(name, value)| ((*name).to_owned(), json!(value)))
        .collect();
    json!({ "observed_at": observed_at, "signals": signals })
}
