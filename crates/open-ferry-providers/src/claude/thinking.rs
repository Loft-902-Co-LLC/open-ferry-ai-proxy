// Ported from CLIProxyAPI internal/thinking/strip.go and
// provider/claude/apply.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings on a request going to Claude: a token budget, or an
//! adaptive effort. [`crate::thinking`] reads and checks the setting.
//!
//! Deviations from upstream:
//! - Models are looked up in the built-in catalog only. Models registered at
//!   run time, and models configured for an API key (upstream's
//!   `ResolvedModelInfo`), aren't seen; such a model is treated as unknown,
//!   so its setting goes to Claude unchecked, as upstream does for unknown
//!   models.
//! - A model found in the catalog is taken to be a Claude model. Upstream
//!   compares the model's provider type with the request's formats, which
//!   gives the same answer for Claude models.

use open_ferry_core::exec::ExecError;
use open_ferry_translate::models::{ModelCatalog, ThinkingSupport};
use open_ferry_translate::thinking::level_to_budget;
use serde_json::Value;

use crate::json::{self, Body};
pub(crate) use crate::thinking::parse_suffix;
use crate::thinking::{self as shared, Config, Mode, Model, Target};

/// The Claude target.
struct Claude;

impl Target for Claude {
    const NAME: &'static str = "claude";

    /// `StripThinkingConfig` for Claude.
    fn strip(body: &mut Value) {
        json::delete(body, "thinking");
        json::delete(body, "output_config.effort");
        drop_empty_output_config(body);
    }

    fn apply_known(body: &mut Value, config: Config, model: &Model, support: &ThinkingSupport) {
        apply_known(body, config, model, support);
    }

    fn apply_compatible(body: &mut Value, config: &Config) {
        apply_compatible(body, config);
    }
}

/// `ApplyRequestThinking` for a request translated from `from` into the
/// Claude `body`: applies the thinking setting that `model`'s suffix or the
/// request asks for. `payload` and `original_request` are the client's
/// request as the executor got it and as the client first sent it.
///
/// A setting the model can't take is a 400 error.
pub(crate) fn apply_request(
    body: &mut Value,
    model: &str,
    from: &str,
    payload: &Body,
    original_request: &Body,
) -> Result<(), ExecError> {
    shared::apply_request::<Claude>(body, model, from, payload, original_request, lookup)
}

/// The built-in catalog's model `id`, taken to be a Claude model.
fn lookup(id: &str) -> Option<Model> {
    ModelCatalog::embedded().lookup(id).map(|info| Model {
        id: info.id.clone(),
        model_type: String::new(),
        thinking: info.thinking.clone(),
        user_defined: false,
        max_completion_tokens: info.max_completion_tokens,
    })
}

fn drop_empty_output_config(body: &mut Value) {
    if json::get(body, "output_config")
        .is_some_and(|config| config.as_object().is_some_and(serde_json::Map::is_empty))
    {
        json::delete(body, "output_config");
    }
}

/// Sets `thinking.type`, removing the budget.
fn set_kind(body: &mut Value, kind: &str) {
    json::set(body, "thinking.type", kind.into());
    json::delete(body, "thinking.budget_tokens");
}

/// Turns thinking off. `display` is only valid with thinking on.
fn disable(body: &mut Value, drop_display: bool) {
    set_kind(body, "disabled");
    if drop_display {
        json::delete(body, "thinking.display");
    }
    json::delete(body, "output_config.effort");
    drop_empty_output_config(body);
}

/// Enabled thinking with `budget`, or with Claude's default budget.
fn enable(body: &mut Value, budget: Option<i64>) {
    match budget {
        Some(budget) => {
            json::set(body, "thinking.type", "enabled".into());
            json::set(body, "thinking.budget_tokens", budget.into());
        }
        None => set_kind(body, "enabled"),
    }
    json::delete(body, "output_config.effort");
    drop_empty_output_config(body);
}

/// Adaptive thinking at `effort`, or at Claude's default effort.
fn adaptive(body: &mut Value, effort: Option<&str>) {
    set_kind(body, "adaptive");
    match effort {
        Some(effort) => {
            json::set(body, "output_config.effort", effort.into());
        }
        None => {
            json::delete(body, "output_config.effort");
            drop_empty_output_config(body);
        }
    }
}

/// The Claude applier's `Apply` for a model the catalog knows, with a
/// validated setting. A model with levels takes adaptive thinking; one
/// without takes a budget.
fn apply_known(body: &mut Value, mut config: Config, info: &Model, support: &ThinkingSupport) {
    let supports_adaptive = !support.levels.is_empty();
    match config.mode {
        Mode::None => return disable(body, true),
        Mode::Level => {
            if supports_adaptive && !config.level.is_empty() {
                return adaptive(body, Some(&config.level));
            }
            match level_to_budget(&config.level) {
                Some(budget) => config = Config::budget(budget),
                None => return,
            }
        }
        Mode::Auto if supports_adaptive => return adaptive(body, None),
        Mode::Auto => return enable(body, None),
        Mode::Budget => {}
    }
    if config.budget == 0 {
        return disable(body, false);
    }
    enable(body, Some(config.budget));
    normalize_budget(body, config.budget, info, support);
}

/// `normalizeClaudeBudget`: Claude needs `max_tokens` above the budget. The
/// budget comes down to fit, unless that would put it under the model's
/// minimum. A missing `max_tokens` takes the model's.
fn normalize_budget(body: &mut Value, budget: i64, info: &Model, support: &ThinkingSupport) {
    if budget <= 0 {
        return;
    }
    let requested = json::get(body, "max_tokens").map_or(0, |value| json::int_of(Some(value)));
    let max_tokens = if requested > 0 {
        requested
    } else if info.max_completion_tokens > 0 {
        json::set(body, "max_tokens", info.max_completion_tokens.into());
        info.max_completion_tokens
    } else {
        0
    };
    let adjusted = if max_tokens > 0 && budget >= max_tokens {
        max_tokens - 1
    } else {
        budget
    };
    if support.min > 0 && adjusted > 0 && adjusted < support.min {
        return;
    }
    if adjusted != budget {
        json::set(body, "thinking.budget_tokens", adjusted.into());
    }
}

/// `applyCompatibleClaude`: the setting as it is, for a model the catalog
/// doesn't know.
fn apply_compatible(body: &mut Value, config: &Config) {
    match config.mode {
        Mode::None => disable(body, true),
        Mode::Auto => enable(body, None),
        Mode::Level if config.level.is_empty() => {}
        Mode::Level => adaptive(body, Some(&config.level)),
        Mode::Budget => enable(body, Some(config.budget)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thinking::claude_config;
    use serde_json::json;

    fn run(body: Value, model: &str, from: &str) -> Result<Value, ExecError> {
        let payload = Body::Json(body.clone());
        let mut body = body;
        apply_request(&mut body, model, from, &payload, &Body::Empty)?;
        Ok(body)
    }

    fn ok(body: Value, model: &str, from: &str) -> Value {
        match run(body, model, from) {
            Ok(body) => body,
            Err(error) => panic!("apply failed: {}", error.message),
        }
    }

    fn err(body: Value, model: &str, from: &str) -> ExecError {
        match run(body, model, from) {
            Ok(body) => panic!("apply should fail: {body}"),
            Err(error) => error,
        }
    }

    // TestApplyThinking_ClaudeEnabledWithOutputConfigEffort, as read by the
    // Claude extractor.
    #[test]
    fn reads_enabled_thinking_with_effort() {
        let cases = [
            (
                json!({"thinking": {"type": "enabled"}, "output_config": {"effort": "high"}}),
                Config::level("high"),
            ),
            (
                json!({"thinking": {"type": "enabled", "budget_tokens": 8192}, "output_config": {"effort": "high"}}),
                Config::budget(8192),
            ),
            (json!({"thinking": {"type": "enabled"}}), Config::auto()),
            (
                json!({"thinking": {"type": "enabled"}, "output_config": {"effort": ""}}),
                Config::auto(),
            ),
            (
                json!({"thinking": {"type": "enabled"}, "output_config": {"effort": "   "}}),
                Config::auto(),
            ),
            (
                json!({"thinking": {"type": "enabled"}, "output_config": {"effort": 123}}),
                Config::auto(),
            ),
            (
                json!({"thinking": {"type": "disabled", "budget_tokens": 4096}}),
                Config::none(),
            ),
            (json!({"thinking": {"type": "adaptive"}}), Config::unset()),
            (
                json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": " MAX "}}),
                Config::level("max"),
            ),
            (json!({"thinking": {"budget_tokens": -1}}), Config::auto()),
        ];
        for (body, want) in cases {
            assert_eq!(claude_config(&body), want, "{body}");
        }
    }

    #[test]
    fn suffix_overrides_the_request_budget() {
        let body = ok(
            json!({"max_tokens": 32000, "thinking": {"type": "enabled", "budget_tokens": 2048}}),
            "claude-sonnet-4-5-20250929(16384)",
            "claude",
        );
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 16384})
        );
    }

    #[test]
    fn budget_stays_under_max_tokens() {
        let body = ok(
            json!({"max_tokens": 4096, "thinking": {"type": "enabled", "budget_tokens": 8192}}),
            "claude-sonnet-4-5-20250929",
            "claude",
        );
        assert_eq!(body["thinking"]["budget_tokens"], 4095);

        // Without max_tokens the model's is written.
        let body = ok(
            json!({"thinking": {"type": "enabled", "budget_tokens": 8192}}),
            "claude-sonnet-4-5-20250929",
            "claude",
        );
        assert_eq!(body["max_tokens"], 64000);
        assert_eq!(body["thinking"]["budget_tokens"], 8192);

        // A budget pushed under the minimum is left alone.
        let body = ok(
            json!({"max_tokens": 1000, "thinking": {"type": "enabled", "budget_tokens": 2048}}),
            "claude-sonnet-4-5-20250929",
            "claude",
        );
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
    }

    #[test]
    fn claude_budget_out_of_range_is_rejected() {
        let error = err(
            json!({"thinking": {"type": "enabled", "budget_tokens": 512}}),
            "claude-sonnet-4-5-20250929",
            "claude",
        );
        assert_eq!(error.status, 400);
        assert_eq!(error.message, "budget 512 out of range [1024,128000]");

        // From another format, or from a suffix, the budget is clamped.
        let body = ok(json!({}), "claude-sonnet-4-5-20250929(512)", "claude");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
    }

    #[test]
    fn levels_become_budgets_on_budget_models() {
        let body = ok(
            json!({"max_tokens": 64000}),
            "claude-sonnet-4-5-20250929(high)",
            "claude",
        );
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 24576})
        );
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn levels_become_adaptive_effort_on_level_models() {
        let body = ok(
            json!({"thinking": {"type": "enabled", "budget_tokens": 4096}, "output_config": {"format": {}}}),
            "claude-opus-4-6(max)",
            "claude",
        );
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(
            body["output_config"],
            json!({"format": {}, "effort": "max"})
        );
    }

    #[test]
    fn unsupported_levels() {
        // A Claude request's own level must be one the model has.
        let error = err(
            json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "xhigh"}}),
            "claude-opus-4-6",
            "claude",
        );
        assert_eq!(error.status, 400);
        assert_eq!(
            error.message,
            r#"level "xhigh" not supported, valid levels: low, medium, high, max"#
        );

        // One translated from another format is clamped to the nearest.
        let body = ok(
            json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "minimal"}}),
            "claude-opus-4-6",
            "openai",
        );
        assert_eq!(body["output_config"]["effort"], "low");
    }

    #[test]
    fn auto_thinking() {
        // A model that decides for itself gets adaptive thinking at its
        // default effort; others think at mid-range.
        let body = ok(
            json!({"thinking": {"type": "enabled"}, "output_config": {"effort": "auto"}}),
            "claude-opus-5",
            "claude",
        );
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert!(body.get("output_config").is_none());

        let body = ok(
            json!({"thinking": {"type": "enabled"}, "output_config": {"effort": "auto"}}),
            "claude-opus-4-6",
            "claude",
        );
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 64512})
        );
        assert_eq!(body["max_tokens"], 128000);
        assert!(body.get("output_config").is_none());

        let body = ok(
            json!({"max_tokens": 100000}),
            "claude-opus-4-1-20250805(auto)",
            "claude",
        );
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 64512})
        );
    }

    #[test]
    fn none_disables_and_wins_over_summaries() {
        let body = ok(
            json!({"thinking": {"type": "enabled", "budget_tokens": 4096, "display": "summarized"}, "output_config": {"effort": "low"}}),
            "claude-sonnet-4-5-20250929(none)",
            "claude",
        );
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn summaries_stay_shown() {
        let body = ok(
            json!({"thinking": {"type": "enabled", "budget_tokens": 4096, "display": "summarized"}}),
            "claude-sonnet-4-5-20250929(8192)",
            "claude",
        );
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 8192, "display": "summarized"})
        );
    }

    #[test]
    fn models_without_thinking_lose_the_setting() {
        let body = ok(
            json!({"thinking": {"type": "enabled", "budget_tokens": 4096}, "output_config": {"effort": "low"}}),
            "claude-3-5-haiku-20241022",
            "claude",
        );
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());

        let untouched = json!({"messages": []});
        assert_eq!(
            ok(untouched.clone(), "claude-3-5-haiku-20241022", "claude"),
            untouched
        );
    }

    #[test]
    fn unknown_models_get_the_setting_unchecked() {
        let body = ok(json!({}), "my-claude(xhigh)", "claude");
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(body["output_config"]["effort"], "xhigh");

        let body = ok(json!({}), "my-claude(300)", "claude");
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 300})
        );

        let body = ok(
            json!({"thinking": {"type": "enabled"}}),
            "my-claude",
            "claude",
        );
        assert_eq!(body["thinking"], json!({"type": "enabled"}));

        // A Responses request's effort is read from the client's request.
        let mut body = json!({"thinking": {"type": "enabled", "display": "summarized"}});
        let payload = Body::Json(json!({"reasoning": {"effort": "none"}}));
        apply_request(
            &mut body,
            "my-claude",
            "openai-response",
            &payload,
            &Body::Empty,
        )
        .unwrap();
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn responses_configuration_updates_set_the_effort() {
        let source = json!({
            "reasoning": {"effort": "low"},
            "input": [
                {"type": "configuration_update", "reasoning": {"effort": "high"}},
                {"type": "configuration_update", "reasoning": {"effort": "  "}},
                {"role": "user", "content": "ok"}
            ]
        });
        let mut body =
            json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "low"}});
        apply_request(
            &mut body,
            "claude-opus-4-6",
            "openai-response",
            &Body::Json(source),
            &Body::Empty,
        )
        .unwrap();
        assert_eq!(body["output_config"]["effort"], "high");
    }
}
