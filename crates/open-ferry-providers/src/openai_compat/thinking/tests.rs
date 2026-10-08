// Ported from CLIProxyAPI test/thinking_conversion_test.go and
// internal/thinking/summary_test.go
// (TestApplySummaryConfig_OpenAIChatProviderDialects) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The OpenAI Chat Completions cases of the thinking conversion matrix,
//! and the target's own rules.
//!
//! Changed:
//! - The test models are registered in a model registry of their own,
//!   rather than the global one (see `codex::thinking`'s tests, whose
//!   runner these share).
//! - `TestApplySummaryConfig_OpenAIChatProviderDialects` checks this
//!   module's copy of the Chat Completions summary writer.
//!
//! Dropped:
//! - The matrix cases for other targets, and the Kimi and xAI cases of
//!   `TestThinkingE2EProviderTargets`, whose targets aren't ported.

use std::sync::Arc;

use bytes::Bytes;
use open_ferry_core::config::{Config as ProxyConfig, OpenAiCompatibility};
use open_ferry_core::exec::{Options, Request};
use open_ferry_core::models::ThinkingSupport;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_translate::registry::Format;
use open_ferry_translate::thinking::summary::Detail;
use serde_json::json;

use super::*;
use crate::codex::thinking::tests::{Want, run_matrix};
use crate::openai_compat::OpenAiCompatExecutor;

/// What a body without a Chat Completions thinking setting lacks.
const NO_THINKING: [&str; 1] = ["reasoning_effort"];

// TestThinkingE2EMatrix_Suffix: the cases with an OpenAI target.
#[test]
fn suffix_matrix() {
    run_matrix::<OpenAi>(
        "openai",
        &NO_THINKING,
        &[
            (
                "11",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Nothing,
            ),
            (
                "12",
                "claude",
                "level-model(8192)",
                r#"{"model":"level-model(8192)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning_effort", "medium"),
            ),
            (
                "13",
                "claude",
                "level-model(64000)",
                r#"{"model":"level-model(64000)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning_effort", "high"),
            ),
            (
                "14",
                "claude",
                "level-model(0)",
                r#"{"model":"level-model(0)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning_effort", "minimal"),
            ),
            (
                "15",
                "claude",
                "level-model(-1)",
                r#"{"model":"level-model(-1)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning_effort", "medium"),
            ),
            (
                "16",
                "gemini",
                "level-subset-model(8192)",
                r#"{"model":"level-subset-model(8192)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning_effort", "low"),
            ),
            (
                "58",
                "gemini",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Nothing,
            ),
            (
                "59",
                "gemini",
                "no-thinking-model(8192)",
                r#"{"model":"no-thinking-model(8192)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Nothing,
            ),
            (
                "60",
                "gemini",
                "no-thinking-model(0)",
                r#"{"model":"no-thinking-model(0)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Nothing,
            ),
            (
                "61",
                "gemini",
                "no-thinking-model(-1)",
                r#"{"model":"no-thinking-model(-1)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Nothing,
            ),
            (
                "62",
                "claude",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Nothing,
            ),
            (
                "63",
                "claude",
                "no-thinking-model(8192)",
                r#"{"model":"no-thinking-model(8192)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Nothing,
            ),
            (
                "64",
                "claude",
                "no-thinking-model(0)",
                r#"{"model":"no-thinking-model(0)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Nothing,
            ),
            (
                "65",
                "claude",
                "no-thinking-model(-1)",
                r#"{"model":"no-thinking-model(-1)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Nothing,
            ),
            (
                "66",
                "gemini",
                "user-defined-model",
                r#"{"model":"user-defined-model","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Nothing,
            ),
            (
                "67",
                "gemini",
                "user-defined-model(8192)",
                r#"{"model":"user-defined-model(8192)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning_effort", "medium"),
            ),
            (
                "68",
                "gemini",
                "user-defined-model(64000)",
                r#"{"model":"user-defined-model(64000)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning_effort", "xhigh"),
            ),
            (
                "69",
                "gemini",
                "user-defined-model(0)",
                r#"{"model":"user-defined-model(0)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning_effort", "none"),
            ),
            (
                "70",
                "gemini",
                "user-defined-model(-1)",
                r#"{"model":"user-defined-model(-1)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning_effort", "auto"),
            ),
            (
                "80",
                "openai",
                "level-model(high)",
                r#"{"model":"level-model(high)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning_effort", "high"),
            ),
            (
                "81",
                "openai",
                "level-model(xhigh)",
                r#"{"model":"level-model(xhigh)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Error,
            ),
        ],
    );
}

// TestThinkingE2EMatrix_Body: the cases with an OpenAI target.
#[test]
fn body_matrix() {
    run_matrix::<OpenAi>(
        "openai",
        &NO_THINKING,
        &[
            (
                "11",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Nothing,
            ),
            (
                "12",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":8192}}"#,
                Want::Field("reasoning_effort", "medium"),
            ),
            (
                "13",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":64000}}"#,
                Want::Field("reasoning_effort", "high"),
            ),
            (
                "14",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":0}}"#,
                Want::Field("reasoning_effort", "minimal"),
            ),
            (
                "15",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":-1}}"#,
                Want::Field("reasoning_effort", "medium"),
            ),
            (
                "16",
                "gemini",
                "level-subset-model",
                r#"{"model":"level-subset-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":8192}}}"#,
                Want::Field("reasoning_effort", "low"),
            ),
            (
                "58",
                "gemini",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Nothing,
            ),
            (
                "59",
                "gemini",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":8192}}}"#,
                Want::Nothing,
            ),
            (
                "60",
                "gemini",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":0}}}"#,
                Want::Nothing,
            ),
            (
                "61",
                "gemini",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":-1}}}"#,
                Want::Nothing,
            ),
            (
                "62",
                "claude",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Nothing,
            ),
            (
                "63",
                "claude",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":8192}}"#,
                Want::Nothing,
            ),
            (
                "64",
                "claude",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":0}}"#,
                Want::Nothing,
            ),
            (
                "65",
                "claude",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":-1}}"#,
                Want::Nothing,
            ),
            (
                "66",
                "gemini",
                "user-defined-model",
                r#"{"model":"user-defined-model","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Nothing,
            ),
            (
                "67",
                "gemini",
                "user-defined-model",
                r#"{"model":"user-defined-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":8192}}}"#,
                Want::Field("reasoning_effort", "medium"),
            ),
            (
                "68",
                "gemini",
                "user-defined-model",
                r#"{"model":"user-defined-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":64000}}}"#,
                Want::Field("reasoning_effort", "xhigh"),
            ),
            (
                "69",
                "gemini",
                "user-defined-model",
                r#"{"model":"user-defined-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":0}}}"#,
                Want::Field("reasoning_effort", "none"),
            ),
            (
                "70",
                "gemini",
                "user-defined-model",
                r#"{"model":"user-defined-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":-1}}}"#,
                Want::Field("reasoning_effort", "auto"),
            ),
            (
                "80",
                "openai",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high"}"#,
                Want::Field("reasoning_effort", "high"),
            ),
            (
                "81",
                "openai",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"xhigh"}"#,
                Want::Error,
            ),
        ],
    );
}

// TestThinkingE2EProviderTargets: the cases with an OpenAI target.
#[test]
fn provider_targets_matrix() {
    run_matrix::<OpenAi>(
        "openai",
        &NO_THINKING,
        &[
            (
                "R1",
                "openai-response",
                "level-model",
                r#"{"model":"level-model","input":"hi","reasoning":{"effort":"high"}}"#,
                Want::Field("reasoning_effort", "high"),
            ),
            (
                "R2",
                "openai-response",
                "level-model",
                r#"{"model":"level-model","input":"hi","reasoning":{"effort":"none"}}"#,
                Want::Field("reasoning_effort", "minimal"),
            ),
        ],
    );
}

// TestThinkingE2EClaudeAdaptive_Body: the cases with an OpenAI target.
#[test]
fn claude_adaptive_matrix() {
    run_matrix::<OpenAi>(
        "openai",
        &NO_THINKING,
        &[
            (
                "C1",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"minimal"}}"#,
                Want::Field("reasoning_effort", "minimal"),
            ),
            (
                "C2",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}}"#,
                Want::Field("reasoning_effort", "low"),
            ),
            (
                "C3",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"medium"}}"#,
                Want::Field("reasoning_effort", "medium"),
            ),
            (
                "C4",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"high"}}"#,
                Want::Field("reasoning_effort", "high"),
            ),
            (
                "C5",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"xhigh"}}"#,
                Want::Field("reasoning_effort", "high"),
            ),
            (
                "C6",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"max"}}"#,
                Want::Field("reasoning_effort", "high"),
            ),
            (
                "C7",
                "claude",
                "no-thinking-model",
                r#"{"model":"no-thinking-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"high"}}"#,
                Want::Nothing,
            ),
        ],
    );
}

// TestApplySummaryConfig_OpenAIChatProviderDialects.
#[test]
fn shows_summaries_in_each_dialect() {
    const SHOWN: Summary = Summary::Shown(Detail::Auto);
    // (provider, body, summary, reasoning.exclude, reasoning_effort)
    let cases = [
        ("openai", "{}", SHOWN, None, None),
        ("openrouter", "{}", SHOWN, Some(false), None),
        ("prod-openrouter", "{}", Summary::Hidden, Some(true), None),
        (
            "deepseek",
            r#"{"reasoning_effort":"high"}"#,
            Summary::Hidden,
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
            Summary::Hidden,
            Some(true),
            None,
        ),
    ];
    for (provider, body, summary, exclude, effort) in cases {
        let mut out: Value = serde_json::from_str(body).unwrap();
        OpenAi::apply_summary(&mut out, "model", provider, summary);
        assert_eq!(
            json::get(&out, "reasoning.exclude"),
            exclude.map(Value::Bool).as_ref(),
            "{provider}: {out}"
        );
        assert_eq!(
            json::get(&out, "reasoning_effort").and_then(Value::as_str),
            effort,
            "{provider}: {out}"
        );
    }
}

#[test]
fn summaries_follow_the_request() {
    let mut body = json!({"include_reasoning": false, "reasoning": {"exclude": true}});
    OpenAi::apply_summary(&mut body, "model", "openai", Summary::Shown(Detail::Auto));
    assert_eq!(
        body,
        json!({"include_reasoning": true, "reasoning": {"exclude": false}})
    );
    // Only booleans are rewritten, and nothing is said for an unspecified
    // summary.
    let mut body = json!({"include_reasoning": "yes", "reasoning": {"exclude": 1}});
    OpenAi::apply_summary(&mut body, "model", "openai", Summary::Hidden);
    assert_eq!(
        body,
        json!({"include_reasoning": "yes", "reasoning": {"exclude": 1}})
    );
    OpenAi::apply_summary(&mut body, "model", "openrouter", Summary::Unspecified);
    assert_eq!(
        body,
        json!({"include_reasoning": "yes", "reasoning": {"exclude": 1}})
    );
}

#[test]
fn recognises_openrouter() {
    for name in [
        "openrouter",
        " OpenRouter ",
        "prod-openrouter",
        "a_openrouter",
        "x/openrouter/y",
        "openrouter.ai",
        "eu:openrouter",
    ] {
        assert!(is_openrouter(name), "{name}");
    }
    for name in ["", "openrouterx", "open-router", "openai"] {
        assert!(!is_openrouter(name), "{name}");
    }
}

#[test]
fn strips_the_setting() {
    let mut body = json!({"reasoning_effort": "high", "reasoning": {"exclude": true}, "n": 1});
    OpenAi::strip(&mut body);
    assert_eq!(body, json!({"n": 1}));
}

#[test]
fn writes_efforts() {
    let support = ThinkingSupport {
        levels: vec!["low".into(), "high".into()],
        ..ThinkingSupport::default()
    };
    let mut body = json!({});
    OpenAi::apply_known(
        &mut body,
        Config::level("high"),
        &Model::default(),
        &support,
    );
    assert_eq!(body, json!({"reasoning_effort": "high"}));
    let mut body = json!({});
    OpenAi::apply_compatible(&mut body, &Config::budget(1024));
    assert_eq!(body, json!({"reasoning_effort": "low"}));
}

/// An executor for the provider `provider` with `models`.
fn executor(provider: &str, models: Arc<ModelRegistry>) -> OpenAiCompatExecutor {
    let mut config = ProxyConfig::default();
    config.proxy_url = "direct".into();
    config.openai_compatibility = vec![OpenAiCompatibility {
        name: provider.into(),
        ..OpenAiCompatibility::default()
    }];
    OpenAiCompatExecutor::new(provider, Arc::new(config)).with_models(models)
}

// openai_compat_executor.go:121, :341 and :723: a call applies the setting,
// with the models the executor was given, in Chat Completions or, for a
// compact call, in OpenAI Responses.
#[test]
fn the_executor_applies_the_setting() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "compat-client",
        "openrouter",
        &[ModelInfo {
            id: "level-model".into(),
            model_type: "openai".into(),
            thinking: Some(ThinkingSupport {
                levels: vec!["low".into(), "high".into()],
                ..ThinkingSupport::default()
            }),
            ..ModelInfo::default()
        }],
    );
    let executor = executor("openrouter", registry);
    let request = |model: &str| {
        Request {
        model: model.into(),
        payload: Bytes::from_static(
            br#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"low"}"#,
        ),
    }
    };
    let options = Options::new(Format::OPENAI);

    let mut body = json!({"messages": [], "reasoning_effort": "low"});
    executor
        .apply_thinking(
            &mut body,
            &request("level-model(high)"),
            &options,
            &Format::OPENAI,
        )
        .unwrap();
    // OpenRouter is told to show the summaries the effort asks for.
    assert_eq!(
        body,
        json!({"messages": [], "reasoning_effort": "high", "reasoning": {"exclude": false}})
    );

    let mut body = json!({"input": [], "reasoning": {"effort": "low"}});
    executor
        .apply_thinking(
            &mut body,
            &request("level-model(high)"),
            &options,
            &Format::OPENAI_RESPONSE,
        )
        .unwrap();
    assert_eq!(json::get(&body, "reasoning.effort"), Some(&json!("high")));

    let mut body = json!({"messages": []});
    let error = executor
        .apply_thinking(
            &mut body,
            &request("level-model(medium)"),
            &options,
            &Format::OPENAI,
        )
        .unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(
        error.message,
        r#"level "medium" not supported, valid levels: low, high"#
    );
}
