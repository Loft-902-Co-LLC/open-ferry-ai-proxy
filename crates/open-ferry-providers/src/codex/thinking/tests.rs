// Ported from CLIProxyAPI internal/thinking/apply_codex_usage_test.go and
// test/thinking_conversion_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The Codex cases of the thinking conversion matrix, how a Responses
//! request's `configuration_update` items are handled, and the Codex
//! target's own rules. The matrix runner and test models here serve the
//! OpenAI target's tests too.
//!
//! Changed:
//! - The test models are registered in a model registry of their own,
//!   rather than the global one.
//! - Upstream reads a body's effective effort with
//!   `ExtractTranslatedReasoningEffort`, which fills a usage record's
//!   `reasoning_effort`; usage records here carry none (see the reporter's
//!   module in `open-ferry-core`), so the tests read the setting it is made
//!   from (`codex_usage_config`).
//! - Where upstream checks that a body is unchanged byte for byte, the
//!   tests here compare JSON values, as a body here is one.
//! - `TestApplyThinkingLogsNativeResponsesEffectiveEffort` checks that the
//!   body goes on unchanged with its effective effort, but not the debug
//!   lines logged about it.
//!
//! Dropped:
//! - The matrix cases for other targets (the OpenAI ones are in
//!   `openai_compat::thinking`'s tests, the Gemini ones in
//!   `gemini::thinking`'s), and the Interactions matrix.
//! - `ExtractReasoningEffort`'s checks in
//!   `TestExtractCodexReasoningEffortWithConfigurationUpdate` and
//!   `...TargetRouting`: a usage record's `reasoning_effort` is always
//!   empty, so there is no `ExtractReasoningEffort` to check. The former's
//!   invalid JSON case goes with them.
//! - `TestApplyConfigurationUpdateRouting`'s "invalid JSON is untouched":
//!   the body here is always JSON, as the translators give it.

use std::sync::Arc;

use open_ferry_core::exec::{Options, Request};
use open_ferry_core::registry::ModelRegistry;
use open_ferry_translate::registry::{Format, Registry};
use serde_json::json;

use super::*;
use crate::codex::request::{Context, Kind, prepare_body};
use crate::thinking::{apply_thinking, codex_usage_config, parse_suffix};

fn support(levels: &[&str]) -> ThinkingSupport {
    ThinkingSupport {
        levels: levels.iter().map(|level| (*level).to_owned()).collect(),
        ..ThinkingSupport::default()
    }
}

fn model(id: &str, model_type: &str, thinking: Option<ThinkingSupport>) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        model_type: model_type.to_owned(),
        thinking,
        ..ModelInfo::default()
    }
}

/// `getTestModels`, the ones the Codex and OpenAI cases use.
pub(crate) fn catalog() -> Arc<ModelRegistry> {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_client(
        "thinking-test",
        "test",
        &[
            model(
                "level-model",
                "openai",
                Some(support(&["minimal", "low", "medium", "high"])),
            ),
            model(
                "level-subset-model",
                "gemini",
                Some(support(&["low", "high"])),
            ),
            model("no-thinking-model", "openai", None),
            ModelInfo {
                user_defined: true,
                ..model("user-defined-model", "openai", None)
            },
        ],
    );
    registry
}

/// What a matrix case expects: a field with a value, no thinking setting,
/// or an error.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Want {
    Field(&'static str, &'static str),
    Nothing,
    Error,
}

/// A matrix case: its upstream name, the client's format, the model, the
/// client's request, and what is expected.
pub(crate) type Case = (&'static str, &'static str, &'static str, &'static str, Want);

/// `runThinkingTests` for the target `T`, named `to`: translates each
/// case's request to `to`, applies the thinking setting as `ApplyThinking`
/// does, and checks the result. `thinking` are the fields that count as a
/// thinking setting.
pub(crate) fn run_matrix<T: Target>(to: &str, thinking: &[&str], cases: &[Case]) {
    let catalog = catalog();
    for &(name, from, model, input, want) in cases {
        let name = format!("Case{name}_{from}->{to}_{model}");
        let input: Value = serde_json::from_str(input).unwrap();
        let mut body = Registry::global().translate_request(
            &Format::new(from.to_owned()),
            &Format::new(to.to_owned()),
            parse_suffix(model).0,
            input,
            true,
        );
        let route = Route {
            model,
            from,
            to,
            provider: to,
        };
        let result = apply_thinking::<T>(&mut body, route, |id| {
            lookup(Some(catalog.as_ref()), id, to)
        });
        match want {
            Want::Error => assert!(result.is_err(), "{name}: {body}"),
            Want::Nothing => {
                result.unwrap_or_else(|error| panic!("{name}: {}", error.message));
                for field in thinking {
                    assert!(!json::exists(&body, field), "{name}: {body}");
                }
            }
            Want::Field(field, value) => {
                result.unwrap_or_else(|error| panic!("{name}: {}", error.message));
                let found = json::get(&body, field);
                assert!(found.is_some(), "{name}: {body}");
                assert_eq!(json::str_of(found), value, "{name}: {body}");
            }
        }
    }
}

/// What a body without a Codex thinking setting lacks.
const NO_THINKING: [&str; 2] = ["reasoning.effort", "reasoning"];

// TestThinkingE2EMatrix_Suffix: the cases with a Codex target.
#[test]
fn suffix_matrix() {
    run_matrix::<Codex>(
        "codex",
        &NO_THINKING,
        &[
            (
                "1",
                "openai",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "2",
                "openai",
                "level-model(medium)",
                r#"{"model":"level-model(medium)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "3",
                "openai",
                "level-model(xhigh)",
                r#"{"model":"level-model(xhigh)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Error,
            ),
            (
                "4",
                "openai",
                "level-model(none)",
                r#"{"model":"level-model(none)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "minimal"),
            ),
            (
                "5",
                "openai",
                "level-model(auto)",
                r#"{"model":"level-model(auto)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "6",
                "gemini",
                "level-model",
                r#"{"model":"level-model","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "7",
                "gemini",
                "level-model(8192)",
                r#"{"model":"level-model(8192)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "8",
                "gemini",
                "level-model(64000)",
                r#"{"model":"level-model(64000)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning.effort", "high"),
            ),
            (
                "9",
                "gemini",
                "level-model(0)",
                r#"{"model":"level-model(0)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning.effort", "minimal"),
            ),
            (
                "10",
                "gemini",
                "level-model(-1)",
                r#"{"model":"level-model(-1)","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "17A",
                "openai",
                "level-subset-model(auto)",
                r#"{"model":"level-subset-model(auto)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "low"),
            ),
            (
                "71",
                "claude",
                "user-defined-model",
                r#"{"model":"user-defined-model","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "72",
                "claude",
                "user-defined-model(8192)",
                r#"{"model":"user-defined-model(8192)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "73",
                "claude",
                "user-defined-model(64000)",
                r#"{"model":"user-defined-model(64000)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "xhigh"),
            ),
            (
                "74",
                "claude",
                "user-defined-model(0)",
                r#"{"model":"user-defined-model(0)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "none"),
            ),
            (
                "75",
                "claude",
                "user-defined-model(-1)",
                r#"{"model":"user-defined-model(-1)","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "auto"),
            ),
            (
                "82",
                "openai-response",
                "level-model(high)",
                r#"{"model":"level-model(high)","input":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "high"),
            ),
            (
                "83",
                "openai-response",
                "level-model(xhigh)",
                r#"{"model":"level-model(xhigh)","input":[{"role":"user","content":"hi"}]}"#,
                Want::Error,
            ),
        ],
    );
}

// TestThinkingE2EMatrix_Body: the cases with a Codex target.
#[test]
fn body_matrix() {
    run_matrix::<Codex>(
        "codex",
        &NO_THINKING,
        &[
            (
                "1",
                "openai",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "2",
                "openai",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"medium"}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "3",
                "openai",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"xhigh"}"#,
                Want::Error,
            ),
            (
                "4",
                "openai",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"none"}"#,
                Want::Field("reasoning.effort", "minimal"),
            ),
            (
                "5",
                "openai",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"auto"}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "6",
                "gemini",
                "level-model",
                r#"{"model":"level-model","contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "7",
                "gemini",
                "level-model",
                r#"{"model":"level-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":8192}}}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "8",
                "gemini",
                "level-model",
                r#"{"model":"level-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":64000}}}"#,
                Want::Field("reasoning.effort", "high"),
            ),
            (
                "9",
                "gemini",
                "level-model",
                r#"{"model":"level-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":0}}}"#,
                Want::Field("reasoning.effort", "minimal"),
            ),
            (
                "10",
                "gemini",
                "level-model",
                r#"{"model":"level-model","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":-1}}}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "71",
                "claude",
                "user-defined-model",
                r#"{"model":"user-defined-model","messages":[{"role":"user","content":"hi"}]}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "72",
                "claude",
                "user-defined-model",
                r#"{"model":"user-defined-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":8192}}"#,
                Want::Field("reasoning.effort", "medium"),
            ),
            (
                "73",
                "claude",
                "user-defined-model",
                r#"{"model":"user-defined-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":64000}}"#,
                Want::Field("reasoning.effort", "xhigh"),
            ),
            (
                "74",
                "claude",
                "user-defined-model",
                r#"{"model":"user-defined-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":0}}"#,
                Want::Field("reasoning.effort", "none"),
            ),
            (
                "75",
                "claude",
                "user-defined-model",
                r#"{"model":"user-defined-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"enabled","budget_tokens":-1}}"#,
                Want::Field("reasoning.effort", "auto"),
            ),
            (
                "82",
                "openai-response",
                "level-model",
                r#"{"model":"level-model","input":[{"role":"user","content":"hi"}],"reasoning":{"effort":"high"}}"#,
                Want::Field("reasoning.effort", "high"),
            ),
            (
                "83",
                "openai-response",
                "level-model",
                r#"{"model":"level-model","input":[{"role":"user","content":"hi"}],"reasoning":{"effort":"xhigh"}}"#,
                Want::Error,
            ),
        ],
    );
}

// TestThinkingE2EClaudeAdaptive_Body: the cases with a Codex target.
#[test]
fn claude_adaptive_matrix() {
    run_matrix::<Codex>(
        "codex",
        &NO_THINKING,
        &[
            (
                "C14",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"minimal"}}"#,
                Want::Field("reasoning.effort", "minimal"),
            ),
            (
                "C15",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}}"#,
                Want::Field("reasoning.effort", "low"),
            ),
            (
                "C16",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"high"}}"#,
                Want::Field("reasoning.effort", "high"),
            ),
            (
                "C17",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"xhigh"}}"#,
                Want::Field("reasoning.effort", "high"),
            ),
            (
                "C18",
                "claude",
                "level-model",
                r#"{"model":"level-model","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive"},"output_config":{"effort":"max"}}"#,
                Want::Field("reasoning.effort", "high"),
            ),
        ],
    );
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn codex_route<'a>(model: &'a str, format: &'a str) -> Route<'a> {
    Route {
        model,
        from: format,
        to: format,
        provider: "codex",
    }
}

// TestExtractCodexReasoningEffortWithConfigurationUpdate: the effective
// effort, the last update's if any has one, else the top-level one.
#[test]
fn reads_the_effective_effort() {
    let cases = [
        (
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#,
            Config::level("low"),
        ),
        (
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"second turn"},{"type":"configuration_update","reasoning":{"effort":"medium"}}]}"#,
            Config::level("medium"),
        ),
        (
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","tools":[]}]}"#,
            Config::level("xhigh"),
        ),
        (
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"none"}}]}"#,
            Config::none(),
        ),
        (
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"auto"}}]}"#,
            Config::auto(),
        ),
        (
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","tools":[]}]}"#,
            Config::level("low"),
        ),
        (
            r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh"},"input":[{"role":"user","content":"hello"}]}"#,
            Config::level("xhigh"),
        ),
        (
            r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","tools":[]}]}"#,
            Config::level("xhigh"),
        ),
    ];
    for (body, want) in cases {
        assert_eq!(codex_usage_config(&parse(body)), want, "{body}");
    }
}

// TestExtractCodexReasoningEffortWithConfigurationUpdateTargetRouting: a
// suffix rewrites the top-level effort; a model that takes updates keeps
// them, so the last one still has the effect.
#[test]
fn a_suffix_keeps_updates_only_for_models_that_take_them() {
    let source = r#"{"reasoning":{"effort":"xhigh"},"input":[{"type":"configuration_update","reasoning":{"effort":"medium"}},{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":null}},{"role":"user","content":"ok"}]}"#;
    for supported in [true, false] {
        let info = Model {
            id: "opaque-route".into(),
            model_type: "codex".into(),
            support_configuration_update: supported,
            thinking: Some(support(&["low", "high", "xhigh"])),
            ..Model::default()
        };
        let mut body = parse(source);
        let route = Route {
            from: "openai-response",
            ..codex_route("opaque-route(high)", "codex")
        };
        shared::apply_with_model::<Codex>(&mut body, &Body::Json(parse(source)), route, Some(info))
            .unwrap();
        let want = if supported { "low" } else { "high" };
        assert_eq!(codex_usage_config(&body), Config::level(want), "{body}");
        assert_eq!(json::get(&body, "reasoning.effort"), Some(&json!("high")));
        assert_eq!(
            json::str_at(&body, "input.0.type") == "configuration_update",
            supported,
            "{body}"
        );
    }
}

// TestApplyConfigurationUpdateRouting.
#[test]
fn routes_configuration_updates() {
    struct Case {
        name: &'static str,
        body: &'static str,
        format: &'static str,
        suffix: &'static str,
        supported: bool,
        no_thinking: bool,
        want_effort: &'static str,
        want_input: &'static str,
        want_same: bool,
    }
    const BASE: Case = Case {
        name: "",
        body: "",
        format: "codex",
        suffix: "",
        supported: false,
        no_thinking: false,
        want_effort: "",
        want_input: "",
        want_same: false,
    };
    let updates = r#"[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]"#;
    let user_only = r#"[{"role":"user","content":"ok"}]"#;
    let cases = [
        Case {
            name: "supported native request preserves baseline and updates byte for byte",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            want_effort: "xhigh",
            want_input: updates,
            want_same: true,
            ..BASE
        },
        Case {
            name: "supported no-effort request remains unchanged",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]}"#,
            supported: true,
            want_input: r#"[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]"#,
            want_same: true,
            ..BASE
        },
        Case {
            name: "supported native request without thinking metadata preserves baseline summary and updates",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            no_thinking: true,
            want_effort: "xhigh",
            want_input: updates,
            want_same: true,
            ..BASE
        },
        Case {
            name: "supported native suffix still strips effort without thinking metadata",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            no_thinking: true,
            suffix: "high",
            want_input: updates,
            ..BASE
        },
        Case {
            name: "supported suffix only rewrites the top-level effort",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto","other":7},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            suffix: "high",
            want_effort: "high",
            want_input: updates,
            ..BASE
        },
        Case {
            name: "supported suffix keeps effective input effort for current turn",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            suffix: "high",
            want_effort: "high",
            want_input: updates,
            ..BASE
        },
        Case {
            name: "supported invalid suffix leaves native payload untouched",
            body: r#"{"reasoning":{"generate_summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            supported: true,
            suffix: "invalid",
            want_input: updates,
            want_same: true,
            ..BASE
        },
        Case {
            name: "unsupported latest nonempty update wins and other input order stays unchanged",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto","other":7},"input":[{"role":"user","content":"first"},{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":"  "}},{"role":"assistant","content":"reply"},{"type":"configuration_update","reasoning":{"effort":"medium"}},{"type":"configuration_update","tools":[]},{"role":"user","content":"last"}]}"#,
            want_effort: "medium",
            want_input: r#"[{"role":"user","content":"first"},{"role":"assistant","content":"reply"},{"role":"user","content":"last"}]"#,
            ..BASE
        },
        Case {
            name: "unsupported ignores empty and nonstring efforts",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"configuration_update","reasoning":{"effort":42}},{"type":"configuration_update","reasoning":{"effort":null}},{"type":"configuration_update","reasoning":{"effort":"  "}},{"role":"user","content":"ok"}]}"#,
            want_effort: "low",
            want_input: user_only,
            ..BASE
        },
        Case {
            name: "unsupported suffix takes precedence and still removes updates",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            suffix: "high",
            want_effort: "high",
            want_input: user_only,
            ..BASE
        },
        Case {
            name: "unsupported without top-level reasoning promotes the last update",
            body: r#"{"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            want_effort: "low",
            want_input: user_only,
            ..BASE
        },
        Case {
            name: "unsupported removes updates with no effort without inventing one",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]}"#,
            want_input: user_only,
            ..BASE
        },
        Case {
            name: "unsupported without thinking support still removes updates",
            body: r#"{"reasoning":{"summary":"auto"},"input":[{"type":"configuration_update","tools":[]},{"role":"user","content":"ok"}]}"#,
            no_thinking: true,
            want_input: user_only,
            ..BASE
        },
        Case {
            name: "unsupported without thinking support strips effort but keeps summary",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto","other":7},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            no_thinking: true,
            want_input: user_only,
            ..BASE
        },
        Case {
            name: "unsupported openai-response alias removes updates",
            body: r#"{"reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"ok"}]}"#,
            format: "openai-response",
            want_effort: "low",
            want_input: user_only,
            ..BASE
        },
        Case {
            name: "nonarray input is untouched",
            body: r#"{"reasoning":{"summary":"auto"},"input":{"type":"configuration_update","reasoning":{"effort":"low"}}}"#,
            want_input: r#"{"type":"configuration_update","reasoning":{"effort":"low"}}"#,
            want_same: true,
            ..BASE
        },
    ];
    for case in cases {
        let name = case.name;
        let info = Model {
            id: "configured-responses".into(),
            model_type: "codex".into(),
            support_configuration_update: case.supported,
            thinking: (!case.no_thinking).then(|| support(&["low", "medium", "high", "xhigh"])),
            ..Model::default()
        };
        let model = if case.suffix.is_empty() {
            "configured-responses".to_owned()
        } else {
            format!("configured-responses({})", case.suffix)
        };
        let source = parse(case.body);
        let mut body = source.clone();
        let route = codex_route(&model, case.format);
        shared::apply_with_model::<Codex>(
            &mut body,
            &Body::Json(source.clone()),
            route,
            Some(info),
        )
        .unwrap_or_else(|error| panic!("{name}: {}", error.message));
        if case.want_same {
            assert_eq!(body, source, "{name}");
        }
        assert_eq!(
            json::str_at(&body, "reasoning.effort"),
            case.want_effort,
            "{name}: {body}"
        );
        if !case.want_input.is_empty() {
            assert_eq!(
                body.get("input"),
                Some(&parse(case.want_input)),
                "{name}: {body}"
            );
        }
        if json::exists(&source, "reasoning.summary") {
            assert_eq!(
                json::str_at(&body, "reasoning.summary"),
                "auto",
                "{name}: {body}"
            );
        }
        if json::exists(&source, "reasoning.other") {
            assert_eq!(
                json::get(&body, "reasoning.other"),
                Some(&json!(7)),
                "{name}: {body}"
            );
        }
        if case.supported && !case.suffix.is_empty() {
            assert_eq!(
                codex_usage_config(&body),
                Config::level("low"),
                "{name}: {body}"
            );
        }
    }
}

// TestApplyThinkingLogsNativeResponsesEffectiveEffort, without its log
// checks: a native request to a model that takes updates goes on as it is,
// whether the model is looked up or bound to the request.
#[test]
fn native_requests_go_on_unchanged() {
    let cases = [
        (
            r#"{"model":"gpt-6-sol","reasoning":{"effort":"high","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"xhigh"}},{"role":"user","content":"ok"}]}"#,
            Config::level("xhigh"),
        ),
        (
            r#"{"model":"gpt-6-sol","reasoning":{"effort":"high"},"input":[{"type":"configuration_update","reasoning":{"effort":"xhigh"}},{"type":"configuration_update","reasoning":{"effort":"max"}}]}"#,
            Config::level("max"),
        ),
        (
            r#"{"model":"gpt-6-sol","reasoning":{"effort":"high"},"input":[{"type":"configuration_update","reasoning":{"effort":"xhigh"}},{"type":"configuration_update","reasoning":{"effort":null}},{"type":"configuration_update","reasoning":{"effort":42}},{"type":"configuration_update","reasoning":{"effort":"  "}}]}"#,
            Config::level("xhigh"),
        ),
        (
            r#"{"model":"gpt-6-sol","reasoning":{"effort":"high"},"input":[{"role":"user","content":"ok"}]}"#,
            Config::level("high"),
        ),
        (
            r#"{"model":"gpt-6-sol","input":[{"type":"configuration_update","reasoning":{"effort":"xhigh"}}]}"#,
            Config::level("xhigh"),
        ),
        (
            r#"{"model":"gpt-6-sol","reasoning":{"effort":"high"},"input":[{"type":"configuration_update","reasoning":{"effort":"none"}}]}"#,
            Config::none(),
        ),
    ];
    let info = lookup(None, "gpt-6-sol", "codex").unwrap();
    assert!(
        info.support_configuration_update,
        "gpt-6-sol must take updates"
    );
    for (text, want) in cases {
        let source = parse(text);
        let route = codex_route("gpt-6-sol", "codex");

        let mut body = source.clone();
        apply_thinking::<Codex>(&mut body, route, |id| lookup(None, id, "codex")).unwrap();
        assert_eq!(body, source);
        assert_eq!(codex_usage_config(&body), want, "{text}");

        let mut body = source.clone();
        shared::apply_with_model::<Codex>(
            &mut body,
            &Body::Json(source.clone()),
            route,
            Some(info.clone()),
        )
        .unwrap();
        assert_eq!(body, source);
        assert_eq!(codex_usage_config(&body), want, "{text}");
    }
}

// TestApplyThinkingPreservesCodexTopLevelReasoningEffortBaseline.
#[test]
fn keeps_the_top_level_baseline() {
    let mut body = parse(
        r#"{"model":"gpt-6-astra","reasoning":{"effort":"xhigh","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}}]}"#,
    );
    apply_thinking::<Codex>(&mut body, codex_route("gpt-6-astra", "codex"), |id| {
        lookup(None, id, "codex")
    })
    .unwrap();
    assert_eq!(json::str_at(&body, "reasoning.effort"), "xhigh");
    assert_eq!(json::str_at(&body, "reasoning.summary"), "auto");
    assert_eq!(json::str_at(&body, "input.0.reasoning.effort"), "low");
    assert_eq!(codex_usage_config(&body), Config::level("low"));
}

#[test]
fn writes_known_efforts() {
    let levels = support(&["low", "medium", "high"]);
    let off = |level: &str| Config {
        level: level.into(),
        ..Config::none()
    };
    assert_eq!(
        known_effort(&Config::level("high"), &levels).as_deref(),
        Some("high")
    );
    // Off goes out as the level validation chose, else the lowest.
    assert_eq!(known_effort(&off("low"), &levels).as_deref(), Some("low"));
    assert_eq!(
        known_effort(&Config::none(), &levels).as_deref(),
        Some("low")
    );
    // A model that can turn thinking off takes `none`.
    let zero = ThinkingSupport {
        zero_allowed: true,
        ..levels.clone()
    };
    assert_eq!(known_effort(&off("low"), &zero).as_deref(), Some("none"));
    let none = support(&["none", "low"]);
    assert_eq!(known_effort(&off("low"), &none).as_deref(), Some("none"));
    // Budgets and auto change nothing.
    assert_eq!(known_effort(&Config::budget(1024), &levels), None);
    assert_eq!(known_effort(&Config::auto(), &levels), None);
    assert_eq!(known_effort(&Config::none(), &support(&[])), None);
}

// applyCompatibleCodex.
#[test]
fn writes_compatible_efforts() {
    let cases = [
        (Config::level("turbo"), Some("turbo")),
        (Config::level(""), None),
        (Config::none(), Some("none")),
        (
            Config {
                level: "low".into(),
                ..Config::none()
            },
            Some("low"),
        ),
        (Config::auto(), Some("auto")),
        (Config::budget(8192), Some("medium")),
        (Config::budget(64000), Some("xhigh")),
    ];
    for (config, want) in cases {
        assert_eq!(compatible_effort(&config).as_deref(), want, "{config:?}");
    }
}

#[test]
fn strips_only_the_effort() {
    let mut body = json!({"reasoning": {"effort": "high", "summary": "auto"}, "input": "hi"});
    Codex::strip(&mut body);
    assert_eq!(
        body,
        json!({"reasoning": {"summary": "auto"}, "input": "hi"})
    );
    let mut body = json!({"reasoning": {"effort": "high"}, "input": "hi"});
    Codex::strip(&mut body);
    assert_eq!(body, json!({"input": "hi"}));
    let mut body = json!({"reasoning": {}, "input": "hi"});
    Codex::strip(&mut body);
    assert_eq!(body, json!({"reasoning": {}, "input": "hi"}));
}

// LookupModelInfo: the provider's registration first, then the catalog.
#[test]
fn looks_models_up_by_provider() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "codex-client",
        "codex",
        &[ModelInfo {
            support_configuration_update: true,
            ..model("gpt-5.5", "custom", Some(support(&["low"])))
        }],
    );
    let found = lookup(Some(&registry), " gpt-5.5 ", " Codex ").unwrap();
    assert_eq!(found.model_type, "custom");
    assert!(found.support_configuration_update);
    let fallback = lookup(None, "gpt-5.5", "codex").unwrap();
    assert_eq!(fallback.model_type, "openai");
    assert!(fallback.thinking.is_some());
    assert!(lookup(Some(&registry), "  ", "codex").is_none());
    assert!(lookup(None, "no-such-model", "codex").is_none());
}

/// A request for `model` with `body`.
fn request(model: &str, body: &str) -> Request {
    Request {
        model: model.into(),
        payload: body.to_owned().into(),
    }
}

// codex_executor_execute.go:50 and :233, codex_executor_stream.go:55 and
// codex_executor_tokens.go:26: every call applies the setting.
#[test]
fn every_call_applies_the_setting() {
    let responses = Options::new(Format::OPENAI_RESPONSE);
    let effort = r#"{"input":"hi","reasoning":{"effort":"high","summary":"auto"}}"#;
    for kind in [
        Kind::Execute,
        Kind::Stream,
        Kind::Compact,
        Kind::CountTokens,
    ] {
        let prepared = prepare_body(
            kind,
            Context::default(),
            &request("gpt-5.5(low)", effort),
            &responses,
        )
        .unwrap();
        assert_eq!(
            json::get(&prepared.body, "reasoning"),
            Some(&json!({"effort": "low", "summary": "auto"})),
            "{kind:?}"
        );

        let error = prepare_body(
            kind,
            Context::default(),
            &request("gpt-5.5(minimal)", effort),
            &responses,
        )
        .unwrap_err();
        assert_eq!(error.status, 400, "{kind:?}");
        assert_eq!(
            error.message,
            r#"level "minimal" not supported, valid levels: low, medium, high, xhigh"#
        );
    }

    // A Chat Completions client's effort is checked and carried over.
    let chat = Options::new(Format::OPENAI);
    let prepared = prepare_body(
        Kind::Execute,
        Context::default(),
        &request(
            "gpt-5.5",
            r#"{"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"xhigh"}"#,
        ),
        &chat,
    )
    .unwrap();
    assert_eq!(
        json::get(&prepared.body, "reasoning.effort"),
        Some(&json!("xhigh"))
    );
    let error = prepare_body(
        Kind::Execute,
        Context::default(),
        &request(
            "gpt-5.5",
            r#"{"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"max"}"#,
        ),
        &chat,
    )
    .unwrap_err();
    assert_eq!(error.status, 400);

    // The models the executor was given come first.
    let registry = ModelRegistry::new();
    registry.register_client("codex-client", "codex", &[model("gpt-5.5", "openai", None)]);
    let context = Context {
        models: Some(&registry),
        ..Context::default()
    };
    let prepared = prepare_body(
        Kind::Execute,
        context,
        &request("gpt-5.5(low)", effort),
        &responses,
    )
    .unwrap();
    assert_eq!(
        json::get(&prepared.body, "reasoning"),
        Some(&json!({"summary": "auto"})),
        "{}",
        prepared.body
    );
}
