// Ported from CLIProxyAPI internal/thinking/summary_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use serde_json::json;

use super::*;

const SHOWN: Summary = Summary::Shown(Detail::Auto);
const HIDDEN: Summary = Summary::Hidden;
const UNSPECIFIED: Summary = Summary::Unspecified;

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn applied(body: &str, format: &str, summary: Summary) -> Value {
    let mut body = parse(body);
    apply_for_model(&mut body, format, "", summary, ModelCatalog::embedded());
    body
}

#[test]
fn extract_summary_config() {
    let cases: &[(&str, &str, &str, Summary)] = &[
        (
            "chat effort enables",
            "openai",
            r#"{"reasoning_effort":"high"}"#,
            SHOWN,
        ),
        (
            "chat none disables",
            "openai",
            r#"{"reasoning_effort":"none"}"#,
            HIDDEN,
        ),
        ("chat missing unspecified", "openai", "{}", UNSPECIFIED),
        (
            "chat null effort unspecified",
            "openai",
            r#"{"reasoning_effort":null}"#,
            UNSPECIFIED,
        ),
        (
            "chat non-string effort unspecified",
            "openai",
            r#"{"reasoning_effort":17}"#,
            UNSPECIFIED,
        ),
        (
            "chat google extension false overrides effort",
            "openai",
            r#"{"reasoning_effort":"high","extra_body":{"google":{"thinking_config":{"include_thoughts":false}}}}"#,
            HIDDEN,
        ),
        (
            "chat google extension true",
            "openai",
            r#"{"extra_body":{"google":{"thinking_config":{"include_thoughts":true}}}}"#,
            SHOWN,
        ),
        (
            "chat exclude disables",
            "openai",
            r#"{"reasoning_effort":"high","reasoning":{"exclude":true}}"#,
            HIDDEN,
        ),
        (
            "chat exclude false enables",
            "openai",
            r#"{"reasoning":{"effort":"high","exclude":false}}"#,
            SHOWN,
        ),
        (
            "chat legacy include_reasoning false disables",
            "openai",
            r#"{"reasoning_effort":"high","include_reasoning":false}"#,
            HIDDEN,
        ),
        (
            "chat legacy include_reasoning true enables",
            "openai",
            r#"{"include_reasoning":true}"#,
            SHOWN,
        ),
        (
            "chat reasoning enabled false disables",
            "openai",
            r#"{"reasoning":{"enabled":false}}"#,
            HIDDEN,
        ),
        (
            "chat reasoning enabled true enables",
            "openai",
            r#"{"reasoning":{"enabled":true}}"#,
            SHOWN,
        ),
        (
            "chat exclude wins over include_reasoning",
            "openai",
            r#"{"reasoning":{"exclude":true},"include_reasoning":true}"#,
            HIDDEN,
        ),
        (
            "chat non-boolean include_reasoning unspecified",
            "openai",
            r#"{"include_reasoning":"false"}"#,
            UNSPECIFIED,
        ),
        (
            "responses effort alone unspecified",
            "openai-response",
            r#"{"reasoning":{"effort":"high"}}"#,
            UNSPECIFIED,
        ),
        (
            "responses summary auto",
            "openai-response",
            r#"{"reasoning":{"effort":"high","summary":"auto"}}"#,
            SHOWN,
        ),
        (
            "responses summary concise",
            "openai-response",
            r#"{"reasoning":{"summary":"concise"}}"#,
            Summary::Shown(Detail::Concise),
        ),
        (
            "responses summary null",
            "openai-response",
            r#"{"reasoning":{"summary":null}}"#,
            HIDDEN,
        ),
        (
            "responses boolean summary invalid",
            "openai-response",
            r#"{"reasoning":{"summary":true}}"#,
            UNSPECIFIED,
        ),
        (
            "responses deprecated generate summary",
            "openai-response",
            r#"{"reasoning":{"generate_summary":"detailed"}}"#,
            Summary::Shown(Detail::Detailed),
        ),
        (
            "claude summarized",
            "claude",
            r#"{"thinking":{"type":"adaptive","display":"summarized"}}"#,
            SHOWN,
        ),
        (
            "claude omitted",
            "claude",
            r#"{"thinking":{"type":"enabled","budget_tokens":2048,"display":"omitted"}}"#,
            HIDDEN,
        ),
        (
            "claude display without type is invalid",
            "claude",
            r#"{"thinking":{"display":"summarized"}}"#,
            UNSPECIFIED,
        ),
        (
            "claude display with auto type is invalid",
            "claude",
            r#"{"thinking":{"type":"auto","display":"summarized"}}"#,
            UNSPECIFIED,
        ),
        // Summaries are applied before thinking settings fill in
        // budget_tokens, so a missing budget doesn't mean thinking is off.
        (
            "claude enabled display without budget is valid",
            "claude",
            r#"{"thinking":{"type":"enabled","display":"summarized"}}"#,
            SHOWN,
        ),
        (
            "claude enabled display with zero budget is invalid",
            "claude",
            r#"{"thinking":{"type":"enabled","budget_tokens":0,"display":"summarized"}}"#,
            UNSPECIFIED,
        ),
        (
            "claude auto compatibility budget summarized",
            "claude",
            r#"{"thinking":{"type":"enabled","budget_tokens":-1,"display":"summarized"}}"#,
            SHOWN,
        ),
        (
            "claude auto compatibility budget omitted",
            "claude",
            r#"{"thinking":{"type":"enabled","budget_tokens":-1,"display":"omitted"}}"#,
            HIDDEN,
        ),
        (
            "gemini include true",
            "gemini",
            r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":true}}}"#,
            SHOWN,
        ),
        (
            "gemini include false",
            "gemini",
            r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":false}}}"#,
            HIDDEN,
        ),
        (
            "antigravity include true",
            "antigravity",
            r#"{"request":{"generationConfig":{"thinkingConfig":{"includeThoughts":true}}}}"#,
            SHOWN,
        ),
        (
            "interactions auto",
            "interactions",
            r#"{"generation_config":{"thinking_summaries":"auto"}}"#,
            SHOWN,
        ),
        (
            "interactions none",
            "interactions",
            r#"{"generation_config":{"thinking_summaries":"none"}}"#,
            HIDDEN,
        ),
        (
            "interactions nested snake include false",
            "interactions",
            r#"{"generation_config":{"thinking_config":{"include_thoughts":false}}}"#,
            HIDDEN,
        ),
        (
            "interactions nested camel include true",
            "interactions",
            r#"{"generation_config":{"thinking_config":{"includeThoughts":true}}}"#,
            SHOWN,
        ),
        (
            "interactions camel config snake include true",
            "interactions",
            r#"{"generation_config":{"thinkingConfig":{"include_thoughts":true}}}"#,
            SHOWN,
        ),
        (
            "interactions camel config camel include false",
            "interactions",
            r#"{"generation_config":{"thinkingConfig":{"includeThoughts":false}}}"#,
            HIDDEN,
        ),
        (
            "interactions enum wins over compatibility reasoning",
            "interactions",
            r#"{"generation_config":{"thinking_summaries":"none"},"reasoning":{"summary":"auto"}}"#,
            HIDDEN,
        ),
        (
            "interactions compatibility reasoning auto",
            "interactions",
            r#"{"reasoning":{"summary":"auto"}}"#,
            SHOWN,
        ),
        (
            "interactions compatibility reasoning none",
            "interactions",
            r#"{"reasoning":{"summary":"none"}}"#,
            HIDDEN,
        ),
        (
            "interactions compatibility reasoning takes only auto or none",
            "interactions",
            r#"{"reasoning":{"summary":"detailed"},"generation_config":{"thinking_config":{"include_thoughts":false}}}"#,
            HIDDEN,
        ),
        (
            "interactions compatibility reasoning null is invalid",
            "interactions",
            r#"{"reasoning":{"summary":null}}"#,
            UNSPECIFIED,
        ),
        (
            "interactions enum wins over include alias",
            "interactions",
            r#"{"generation_config":{"thinking_summaries":"none","thinking_config":{"include_thoughts":true}}}"#,
            HIDDEN,
        ),
        (
            "interactions string include alias is invalid",
            "interactions",
            r#"{"generation_config":{"thinking_config":{"include_thoughts":"false"}}}"#,
            UNSPECIFIED,
        ),
        (
            "interactions detailed is invalid",
            "interactions",
            r#"{"generation_config":{"thinking_summaries":"detailed"}}"#,
            UNSPECIFIED,
        ),
        (
            "interactions boolean is invalid",
            "interactions",
            r#"{"generation_config":{"thinking_summaries":true}}"#,
            UNSPECIFIED,
        ),
        (
            "gemini string bool is invalid",
            "gemini",
            r#"{"generationConfig":{"thinkingConfig":{"includeThoughts":"true"}}}"#,
            UNSPECIFIED,
        ),
    ];
    for (name, format, body, want) in cases {
        assert_eq!(extract(&parse(body), format), *want, "{name}");
    }
}

#[test]
fn extract_explicit_summary_config_does_not_use_chat_effort() {
    let body = parse(r#"{"reasoning_effort":"high"}"#);
    assert_eq!(extract_explicit(&body, "openai"), UNSPECIFIED);
    let body = parse(r#"{"reasoning_effort":"high","reasoning":{"exclude":true}}"#);
    assert_eq!(extract_explicit(&body, "openai"), HIDDEN);
}

#[test]
fn apply_summary_config() {
    let cases: &[(&str, &str, &str, Summary, &str, &str)] = &[
        (
            "chat enabled invents no effort",
            "openai",
            "",
            SHOWN,
            "reasoning_effort",
            "",
        ),
        (
            "chat enabled preserves active effort",
            "openai",
            r#"{"reasoning_effort":"high"}"#,
            SHOWN,
            "reasoning_effort",
            "high",
        ),
        (
            "chat enabled preserves disabled effort",
            "openai",
            r#"{"reasoning_effort":"none"}"#,
            SHOWN,
            "reasoning_effort",
            "none",
        ),
        // Chat can't say "reason but hide", so hiding mustn't fall back to
        // reasoning_effort "none", which turns reasoning off altogether.
        (
            "chat disabled preserves requested effort",
            "openai",
            r#"{"reasoning_effort":"high"}"#,
            HIDDEN,
            "reasoning_effort",
            "high",
        ),
        (
            "chat disabled sets openrouter exclude when present",
            "openai",
            r#"{"reasoning":{"effort":"high","exclude":false}}"#,
            HIDDEN,
            "reasoning.exclude",
            "true",
        ),
        (
            "chat enabled clears openrouter exclude when present",
            "openai",
            r#"{"reasoning":{"effort":"high","exclude":true}}"#,
            SHOWN,
            "reasoning.exclude",
            "false",
        ),
        (
            "chat disabled updates legacy include_reasoning when present",
            "openai",
            r#"{"reasoning_effort":"high","include_reasoning":true}"#,
            HIDDEN,
            "include_reasoning",
            "false",
        ),
        (
            "chat disabled invents no openrouter field",
            "openai",
            r#"{"reasoning_effort":"high"}"#,
            HIDDEN,
            "reasoning",
            "",
        ),
        (
            "claude enabled",
            "claude",
            r#"{"thinking":{"type":"adaptive"}}"#,
            SHOWN,
            "thinking.display",
            "summarized",
        ),
        (
            "claude disabled",
            "claude",
            r#"{"thinking":{"type":"enabled","budget_tokens":2048}}"#,
            HIDDEN,
            "thinking.display",
            "omitted",
        ),
        (
            "gemini enabled",
            "gemini",
            "",
            SHOWN,
            "generationConfig.thinkingConfig.includeThoughts",
            "true",
        ),
        (
            "gemini disabled",
            "gemini",
            "",
            HIDDEN,
            "generationConfig.thinkingConfig.includeThoughts",
            "false",
        ),
        (
            "antigravity enabled",
            "antigravity",
            "",
            SHOWN,
            "request.generationConfig.thinkingConfig.includeThoughts",
            "true",
        ),
        (
            "interactions detail collapses to auto",
            "interactions",
            "",
            Summary::Shown(Detail::Detailed),
            "generation_config.thinking_summaries",
            "auto",
        ),
        (
            "interactions disabled",
            "interactions",
            "",
            HIDDEN,
            "generation_config.thinking_summaries",
            "none",
        ),
        (
            "responses concise",
            "openai-response",
            "",
            Summary::Shown(Detail::Concise),
            "reasoning.summary",
            "concise",
        ),
    ];
    for (name, format, body, summary, key, want) in cases {
        let body = if body.is_empty() { "{}" } else { body };
        let out = applied(body, format, *summary);
        assert_eq!(str_of(path(&out, key)), *want, "{name}: {out}");
    }
}

#[test]
fn apply_summary_config_openai_chat_provider_dialects() {
    // (provider, body, summary, reasoning.exclude, reasoning_effort)
    let cases = [
        ("openai", "{}", SHOWN, None, None),
        ("openrouter", "{}", SHOWN, Some("false"), None),
        ("prod-openrouter", "{}", HIDDEN, Some("true"), None),
        (
            "deepseek",
            r#"{"reasoning_effort":"high"}"#,
            HIDDEN,
            None,
            Some("high"),
        ),
        (
            "kimi",
            r#"{"reasoning_effort":"max"}"#,
            SHOWN,
            None,
            Some("max"),
        ),
        (
            "moonshot",
            r#"{"thinking":{"type":"enabled"}}"#,
            SHOWN,
            None,
            None,
        ),
        (
            "openai-compatibility",
            r#"{"reasoning":{"exclude":false}}"#,
            HIDDEN,
            Some("true"),
            None,
        ),
    ];
    for (provider, body, summary, exclude, effort) in cases {
        let mut out = parse(body);
        apply_for_provider(
            &mut out,
            "openai",
            "model",
            provider,
            None,
            summary,
            ModelCatalog::embedded(),
        );
        assert_eq!(
            path(&out, "reasoning.exclude")
                .map(|v| v.to_string())
                .as_deref(),
            exclude,
            "{provider}: {out}"
        );
        assert_eq!(
            out.get("reasoning_effort").and_then(Value::as_str),
            effort,
            "{provider}: {out}"
        );
    }
}

#[test]
fn apply_summary_config_normalizes_target_aliases() {
    let cases = [
        (
            "gemini",
            r#"{"generationConfig":{"thinkingConfig":{"include_thoughts":true}}}"#,
            "generationConfig.thinkingConfig.includeThoughts",
            "generationConfig.thinkingConfig.include_thoughts",
        ),
        (
            "antigravity",
            r#"{"request":{"generationConfig":{"thinkingConfig":{"include_thoughts":true}}}}"#,
            "request.generationConfig.thinkingConfig.includeThoughts",
            "request.generationConfig.thinkingConfig.include_thoughts",
        ),
        (
            "interactions",
            r#"{"generation_config":{"thinkingSummaries":"auto"}}"#,
            "generation_config.thinking_summaries",
            "generation_config.thinkingSummaries",
        ),
    ];
    for (format, body, canonical, alias) in cases {
        let out = applied(body, format, SHOWN);
        assert!(path(&out, canonical).is_some(), "{format}: {out}");
        assert!(path(&out, alias).is_none(), "{format}: {out}");
    }
}

/// Claude needs `thinking.type`, and rejects `display` on disabled thinking,
/// so `display` is only written where thinking is already on.
#[test]
fn apply_summary_config_claude_display_requires_active_thinking() {
    for summary in [SHOWN, HIDDEN] {
        for body in [
            "{}",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            r#"{"thinking":{"type":"disabled"}}"#,
        ] {
            assert_eq!(
                applied(body, "claude", summary),
                parse(body),
                "{summary:?} {body}"
            );
        }
    }
}

#[test]
fn apply_summary_config_for_model_claude_enabled_summary_uses_valid_thinking_mode() {
    let apply = |model: &str| {
        let mut body = json!({"model": model, "max_tokens": 32000});
        apply_for_model(&mut body, "claude", model, SHOWN, ModelCatalog::embedded());
        body
    };
    assert_eq!(
        apply("claude-opus-5")["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(
        apply("claude-haiku-4-5-20251001")["thinking"],
        json!({"type": "enabled", "budget_tokens": 1024, "display": "summarized"})
    );
}

/// Hiding summaries mustn't add a thinking block. Leaving it out keeps the
/// model's default: newer models may still think, older ones don't.
#[test]
fn apply_summary_config_for_model_claude_disabled_summary_does_not_enable_thinking() {
    for model in ["claude-opus-5", "claude-haiku-4-5-20251001"] {
        let mut body = json!({"model": model, "max_tokens": 32000});
        apply_for_model(&mut body, "claude", model, HIDDEN, ModelCatalog::embedded());
        assert!(body.get("thinking").is_none(), "{model}: {body}");
    }
}

#[test]
fn apply_summary_config_responses_normalizes_deprecated_generate_summary() {
    let out = applied(
        r#"{"reasoning":{"generate_summary":"detailed"}}"#,
        "openai-response",
        Summary::Shown(Detail::Detailed),
    );
    assert_eq!(out, json!({"reasoning": {"summary": "detailed"}}));
}

#[test]
fn apply_summary_config_responses_disabled_omits_summary() {
    let out = applied(
        r#"{"reasoning":{"effort":"high","summary":"auto"}}"#,
        "openai-response",
        HIDDEN,
    );
    assert_eq!(out, json!({"reasoning": {"effort": "high"}}));
}

#[test]
fn apply_summary_config_responses_disabled_drops_empty_reasoning() {
    let out = applied(
        r#"{"model":"gpt-5.4","reasoning":{"summary":"auto"}}"#,
        "openai-response",
        HIDDEN,
    );
    assert_eq!(out, json!({"model": "gpt-5.4"}));
}

#[test]
fn apply_summary_config_unspecified_leaves_body_unchanged() {
    let body = r#"{"thinking":{"type":"adaptive"}}"#;
    assert_eq!(applied(body, "claude", UNSPECIFIED), parse(body));
}

// Not in upstream's tests.

#[test]
fn formats_are_trimmed_and_case_insensitive() {
    let body = parse(r#"{"reasoning":{"summary":"auto"}}"#);
    assert_eq!(extract(&body, " OpenAI-Response "), SHOWN);
    assert_eq!(extract(&body, "unknown"), UNSPECIFIED);
    assert_eq!(
        applied("{}", " CODEX", SHOWN),
        json!({"reasoning": {"summary": "auto"}})
    );
    assert_eq!(applied("{}", "unknown", SHOWN), json!({}));
}

#[test]
fn applying_through_a_non_object_follows_sjson() {
    // A scalar on the way becomes an object; an array stops the change.
    assert_eq!(
        applied(r#"{"reasoning":"x"}"#, "codex", SHOWN),
        json!({"reasoning": {"summary": "auto"}})
    );
    assert_eq!(
        applied(r#"{"reasoning":[1]}"#, "codex", SHOWN),
        json!({"reasoning": [1]})
    );
    assert_eq!(applied("[1]", "gemini", HIDDEN), json!([1]));
    assert_eq!(
        applied(r#""s""#, "gemini", HIDDEN),
        json!({"generationConfig": {"thinkingConfig": {"includeThoughts": false}}})
    );
}

#[test]
fn translated_chat_to_claude_ignores_effort() {
    let body = parse(r#"{"reasoning_effort":"high"}"#);
    assert_eq!(extract_translated(&body, "openai", "claude"), UNSPECIFIED);
    assert_eq!(extract_translated(&body, "openai", "codex"), SHOWN);
    let body = parse(r#"{"reasoning":{"summary":"concise"}}"#);
    assert_eq!(
        extract_translated(&body, "openai", "claude"),
        Summary::Shown(Detail::Concise)
    );
}

fn apply(mut body: Value, show: bool, model: &str) -> Value {
    let summary = if show { SHOWN } else { HIDDEN };
    apply_for_model(
        &mut body,
        "claude",
        model,
        summary,
        ModelCatalog::embedded(),
    );
    body
}

#[test]
fn showing_summaries_turns_thinking_on_by_model() {
    assert_eq!(
        apply(json!({"max_tokens": 32000}), true, "claude-opus-4-6"),
        json!({"max_tokens": 32000, "thinking": {"type": "adaptive", "display": "summarized"}})
    );
    assert_eq!(
        apply(
            json!({"max_tokens": 32000}),
            true,
            "claude-sonnet-4-5-20250929(high)"
        ),
        json!({
            "max_tokens": 32000,
            "thinking": {"type": "enabled", "budget_tokens": 1024, "display": "summarized"}
        })
    );
    // The body's model is used when none is given.
    assert_eq!(
        apply(json!({"model": "claude-opus-4-6"}), true, ""),
        json!({"model": "claude-opus-4-6", "thinking": {"type": "adaptive", "display": "summarized"}})
    );
    // No room for the smallest budget, or an unknown model: left alone.
    assert_eq!(
        apply(
            json!({"max_tokens": 1024}),
            true,
            "claude-sonnet-4-5-20250929"
        ),
        json!({"max_tokens": 1024})
    );
    assert_eq!(apply(json!({}), true, "gpt-x"), json!({}));
}

#[test]
fn hiding_summaries_needs_active_thinking() {
    assert_eq!(
        apply(
            json!({"thinking": {"type": "disabled"}}),
            false,
            "claude-opus-4-6"
        ),
        json!({"thinking": {"type": "disabled"}})
    );
    assert_eq!(apply(json!({}), false, "claude-opus-4-6"), json!({}));
    assert_eq!(
        apply(json!({"thinking": {"type": "enabled"}}), false, "x"),
        json!({"thinking": {"type": "enabled", "display": "omitted"}})
    );
    assert_eq!(
        apply(
            json!({"thinking": {"type": "enabled", "budget_tokens": 0}}),
            false,
            "x"
        ),
        json!({"thinking": {"type": "enabled", "budget_tokens": 0}})
    );
    assert_eq!(
        apply(
            json!({"thinking": {"type": "Enabled", "budget_tokens": -1}}),
            true,
            "x"
        ),
        json!({"thinking": {"type": "Enabled", "budget_tokens": -1, "display": "summarized"}})
    );
}
