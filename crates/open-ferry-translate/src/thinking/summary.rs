// Ported from CLIProxyAPI internal/thinking/summary.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Whether a client asked to see reasoning summaries, and asking the
//! upstream for the same.
//!
//! Formats are named as the translator registry names them: `openai` (Chat
//! Completions), `openai-response`, `codex`, `claude`, `gemini`,
//! `antigravity` and `interactions`.
//!
//! Deviations from upstream:
//! - Bodies are parsed JSON, so the checks that a body is valid JSON fall
//!   away. Upstream reads nothing from a body that isn't, and changes nothing
//!   in it.
//! - Not yet ported: `stripInferredClaudeSummaryActivation`, which only
//!   upstream's `ApplyThinking` uses. It comes with that.

use serde_json::Value;

use crate::go;
use crate::json::{delete_path, int_of, path, set_path, str_of};
use crate::models::{ModelCatalog, ModelInfo};
use crate::thinking::base_model_name;

/// Whether to show reasoning summaries (upstream's `SummaryConfig`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Summary {
    /// The request doesn't say.
    #[default]
    Unspecified,
    /// Summaries are hidden.
    Hidden,
    /// Summaries are shown, in this much detail.
    Shown(Detail),
}

/// How much a shown summary says, where the format can tell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Detail {
    /// The upstream decides.
    #[default]
    Auto,
    /// Short summaries.
    Concise,
    /// Long summaries.
    Detailed,
}

impl Detail {
    fn as_str(self) -> &'static str {
        match self {
            Detail::Auto => "auto",
            Detail::Concise => "concise",
            Detail::Detailed => "detailed",
        }
    }
}

/// Chat Completions fields that say whether to show summaries, as booleans.
/// The first one present decides. Google's documented extension comes first,
/// then aliases other clients send.
const OPENAI_BOOL_PATHS: [&str; 16] = [
    "extra_body.google.thinking_config.include_thoughts",
    "extra_body.google.thinking_config.includeThoughts",
    "extra_body.google.thinkingConfig.include_thoughts",
    "extra_body.google.thinkingConfig.includeThoughts",
    "extra_body.extra_body.google.thinking_config.include_thoughts",
    "extra_body.extra_body.google.thinking_config.includeThoughts",
    "google.thinking_config.include_thoughts",
    "google.thinking_config.includeThoughts",
    "thinking.includeThoughts",
    "thinking.include_thoughts",
    "reasoning.includeThoughts",
    "reasoning.include_thoughts",
    "generationConfig.thinkingConfig.includeThoughts",
    "generationConfig.thinkingConfig.include_thoughts",
    "generation_config.thinking_config.include_thoughts",
    "generation_config.thinking_config.includeThoughts",
];

/// Gemini's switch, then the spellings it also reads. Setting it removes the
/// others.
const GEMINI_PATHS: [&str; 4] = [
    "generationConfig.thinkingConfig.includeThoughts",
    "generationConfig.thinkingConfig.include_thoughts",
    "generation_config.thinking_config.include_thoughts",
    "generation_config.thinking_config.includeThoughts",
];

/// Antigravity wraps a Gemini request in `request`.
const ANTIGRAVITY_PATHS: [&str; 4] = [
    "request.generationConfig.thinkingConfig.includeThoughts",
    "request.generationConfig.thinkingConfig.include_thoughts",
    "request.generationConfig.thinking_config.includeThoughts",
    "request.generationConfig.thinking_config.include_thoughts",
];

/// Ways an Interactions request says whether to include thoughts, after its
/// `thinking_summaries` setting.
const INTERACTIONS_BOOL_PATHS: [&str; 4] = [
    "generation_config.thinking_config.include_thoughts",
    "generation_config.thinking_config.includeThoughts",
    "generation_config.thinkingConfig.include_thoughts",
    "generation_config.thinkingConfig.includeThoughts",
];

/// `ExtractSummaryConfig`: whether a request in `format` asks to show
/// summaries.
///
/// Only Chat Completions treats a reasoning effort as asking for them: it
/// has no summary field, and clients sending `reasoning_effort` have always
/// got summaries. The other formats have a summary field, so effort alone
/// says nothing.
pub fn extract(body: &Value, format: &str) -> Summary {
    let bools = |paths: &[&str]| paths.iter().find_map(|key| bool_summary(path(body, key)));
    let summary = match normalize(format).as_str() {
        "openai" => openai_explicit(body).or_else(|| match body.get("reasoning_effort") {
            Some(Value::String(effort)) => match go::to_lower(effort.trim()).as_str() {
                "" => None,
                "none" => Some(Summary::Hidden),
                _ => Some(Summary::Shown(Detail::Auto)),
            },
            _ => None,
        }),
        "openai-response" | "codex" => responses_summary(path(body, "reasoning.summary"))
            .or_else(|| responses_summary(path(body, "reasoning.generate_summary"))),
        // Claude takes `display` only alongside active thinking.
        "claude" if claude_thinking_accepts_display(body) => match path(body, "thinking.display") {
            Some(Value::String(display)) => match go::to_lower(display.trim()).as_str() {
                "summarized" => Some(Summary::Shown(Detail::Auto)),
                "omitted" => Some(Summary::Hidden),
                _ => None,
            },
            _ => None,
        },
        "gemini" => bools(&GEMINI_PATHS),
        "antigravity" => bools(&ANTIGRAVITY_PATHS),
        // The official `generation_config` setting comes first; the
        // OpenAI-style `reasoning` object is read for compatibility.
        "interactions" => interactions_summary(path(body, "generation_config.thinking_summaries"))
            .or_else(|| interactions_summary(path(body, "generation_config.thinkingSummaries")))
            .or_else(|| interactions_summary(path(body, "reasoning.summary")))
            .or_else(|| bools(&INTERACTIONS_BOOL_PATHS)),
        _ => None,
    };
    summary.unwrap_or_default()
}

/// `ExtractExplicitSummaryConfig`: [`extract`], except that a Chat
/// Completions `reasoning_effort` doesn't count.
pub fn extract_explicit(body: &Value, format: &str) -> Summary {
    if normalize(format) != "openai" {
        return extract(body, format);
    }
    openai_explicit(body).unwrap_or_default()
}

/// `ExtractTranslatedSummaryConfig`: whether a request in `source` format,
/// being translated to `target`, asks to show summaries. A Chat Completions
/// `reasoning_effort` sets how hard Claude thinks, not what it shows, so it
/// doesn't count when translating Chat Completions to Claude.
pub fn extract_translated(body: &Value, source: &str, target: &str) -> Summary {
    if normalize(target) == "claude" && normalize(source) == "openai" {
        return extract_explicit(body, source);
    }
    extract(body, source)
}

/// `ApplyTranslatedSummaryToClaude`: copies a choice the `source` request
/// makes explicitly onto the Claude request `out` translated from it.
pub(crate) fn apply_translated_to_claude(
    out: &mut Value,
    source: &Value,
    source_format: &str,
    model: &str,
    models: &ModelCatalog,
) {
    let summary = extract_translated(source, source_format, "claude");
    if summary != Summary::Unspecified {
        apply_for_model(out, "claude", model, summary, models);
    }
}

/// `ApplySummaryConfigForModel`: asks a request in `format` to show or hide
/// summaries, as `summary` says. `model`'s settings in `models` decide how to
/// turn Claude's thinking on, where summaries need it.
pub fn apply_for_model(
    body: &mut Value,
    format: &str,
    model: &str,
    summary: Summary,
    models: &ModelCatalog,
) {
    apply_for_provider(body, format, model, "", None, summary, models);
}

/// `applySummaryConfigForProvider`: [`apply_for_model`] for a request going
/// to `provider`, whose Chat Completions dialect may have its own switch.
/// `model_info` is the model as resolved for the request, if it was;
/// otherwise `model` is looked up.
fn apply_for_provider(
    body: &mut Value,
    format: &str,
    model: &str,
    provider: &str,
    model_info: Option<&ModelInfo>,
    summary: Summary,
    models: &ModelCatalog,
) {
    let (show, detail) = match summary {
        Summary::Unspecified => return,
        Summary::Hidden => (false, Detail::Auto),
        Summary::Shown(detail) => (true, detail),
    };
    match normalize(format).as_str() {
        "openai" => apply_to_openai_chat(body, provider, show),
        "claude" => {
            // Claude rejects `display` unless thinking is on. A missing
            // `thinking` keeps the model's own default, which for newer models
            // is to think, so only a request to show summaries turns thinking
            // on; hiding them only adds `omitted` to thinking that is already
            // on.
            if show && path(body, "thinking.type").is_none() {
                enable_claude_thinking(body, model, model_info, models);
            }
            if claude_thinking_accepts_display(body) {
                let display = if show { "summarized" } else { "omitted" };
                set_path(body, "thinking.display", display.into());
            }
        }
        "gemini" => set_bool(body, &GEMINI_PATHS, show),
        "antigravity" => set_bool(body, &ANTIGRAVITY_PATHS, show),
        "interactions" => {
            // Interactions takes only `auto` or `none`.
            let value = if show { "auto" } else { "none" };
            set_path(body, "generation_config.thinking_summaries", value.into());
            delete_path(body, "generation_config.thinkingSummaries");
        }
        "openai-response" | "codex" => {
            if show {
                set_path(body, "reasoning.summary", detail.as_str().into());
                delete_path(body, "reasoning.generate_summary");
                return;
            }
            // Leaving the field out is the documented way to hide summaries;
            // not every Responses-compatible backend takes `null`.
            delete_path(body, "reasoning.summary");
            delete_path(body, "reasoning.generate_summary");
            if matches!(body.get("reasoning"), Some(Value::Object(reasoning)) if reasoning.is_empty())
            {
                delete_path(body, "reasoning");
            }
        }
        _ => {}
    }
}

/// Sets the first of `paths` to `show` and removes the others.
fn set_bool(body: &mut Value, paths: &[&str], show: bool) {
    set_path(body, paths[0], show.into());
    for key in &paths[1..] {
        delete_path(body, key);
    }
}

/// `strings.ToLower(strings.TrimSpace(format))`.
fn normalize(format: &str) -> String {
    go::to_lower(format.trim())
}

/// `extractOpenAIExplicitSummaryConfig`: a Chat Completions request's
/// explicit choice, if it makes one.
fn openai_explicit(body: &Value) -> Option<Summary> {
    let flag = |key: &str| path(body, key).and_then(Value::as_bool);
    let shown = |show: bool| {
        if show {
            Summary::Shown(Detail::Auto)
        } else {
            Summary::Hidden
        }
    };
    OPENAI_BOOL_PATHS
        .into_iter()
        .find_map(|key| bool_summary(path(body, key)))
        .or_else(|| responses_summary(path(body, "reasoning.summary")))
        .or_else(|| responses_summary(path(body, "reasoning.generate_summary")))
        // OpenRouter's "reason but hide" switch, its older inverse alias, and
        // its switch for reasoning with nothing hidden.
        .or_else(|| flag("reasoning.exclude").map(|exclude| shown(!exclude)))
        .or_else(|| flag("include_reasoning").map(shown))
        .or_else(|| flag("reasoning.enabled").map(shown))
}

/// `summaryBoolConfig`: a JSON boolean switch.
fn bool_summary(value: Option<&Value>) -> Option<Summary> {
    match value? {
        Value::Bool(true) => Some(Summary::Shown(Detail::Auto)),
        Value::Bool(false) => Some(Summary::Hidden),
        _ => None,
    }
}

/// `responsesSummaryConfig`: a Responses-style summary setting. `auto`,
/// `concise` or `detailed` shows summaries; `none` or `null` hides them.
fn responses_summary(value: Option<&Value>) -> Option<Summary> {
    match value? {
        Value::Null => Some(Summary::Hidden),
        Value::String(summary) => match go::to_lower(summary.trim()).as_str() {
            "auto" => Some(Summary::Shown(Detail::Auto)),
            "concise" => Some(Summary::Shown(Detail::Concise)),
            "detailed" => Some(Summary::Shown(Detail::Detailed)),
            "none" => Some(Summary::Hidden),
            _ => None,
        },
        _ => None,
    }
}

/// `interactionsSummaryConfig`: `auto` or `none`.
fn interactions_summary(value: Option<&Value>) -> Option<Summary> {
    match value? {
        Value::String(summary) => match go::to_lower(summary.trim()).as_str() {
            "auto" => Some(Summary::Shown(Detail::Auto)),
            "none" => Some(Summary::Hidden),
            _ => None,
        },
        _ => None,
    }
}

/// `applyOpenAIChatSummaryConfig`: Chat Completions has no switch for
/// showing reasoning, and neither do DeepSeek's or Kimi's dialects, so this
/// never touches the reasoning effort. OpenRouter's `reasoning.exclude` is
/// its documented "reason but hide" switch and `include_reasoning` its older
/// inverse; for other providers they are only updated if already there.
fn apply_to_openai_chat(body: &mut Value, provider: &str, show: bool) {
    if is_openrouter(provider) || path(body, "reasoning.exclude").is_some_and(Value::is_boolean) {
        set_path(body, "reasoning.exclude", (!show).into());
    }
    if body.get("include_reasoning").is_some_and(Value::is_boolean) {
        set_path(body, "include_reasoning", show.into());
    }
}

/// `isOpenRouterProvider`: `openrouter`, or a name with `openrouter` as one
/// of its `-`, `_`, `/`, `.` or `:` separated parts.
fn is_openrouter(provider: &str) -> bool {
    normalize(provider)
        .split(['-', '_', '/', '.', ':'])
        .any(|part| part == "openrouter")
}

/// `enableClaudeThinkingForSummary`: turns on thinking so summaries can come
/// back: adaptive for a model with effort levels, otherwise the model's
/// smallest budget, if `max_tokens` leaves room for it.
fn enable_claude_thinking(
    body: &mut Value,
    model: &str,
    model_info: Option<&ModelInfo>,
    models: &ModelCatalog,
) {
    let looked_up;
    let model_info = match model_info {
        Some(model_info) => model_info,
        None => {
            let mut base = base_model_name(model).to_owned();
            if base.is_empty() {
                base = base_model_name(&str_of(body.get("model"))).to_owned();
            }
            looked_up = models.lookup(&base);
            let Some(model_info) = looked_up else {
                return;
            };
            model_info
        }
    };
    let Some(support) = &model_info.thinking else {
        return;
    };
    if !support.levels.is_empty() {
        set_path(body, "thinking.type", "adaptive".into());
        delete_path(body, "thinking.budget_tokens");
        return;
    }
    let budget = support.min;
    if budget <= 0
        || body
            .get("max_tokens")
            .is_some_and(|max| int_of(max) <= budget)
    {
        return;
    }
    set_path(body, "thinking.type", "enabled".into());
    set_path(body, "thinking.budget_tokens", budget.into());
}

/// `claudeThinkingAcceptsDisplay`: whether the request's thinking is on, so
/// it can take `display`. A budget of -1 means the model decides, so it
/// counts as on, and so does a missing budget, which is filled in later.
fn claude_thinking_accepts_display(body: &Value) -> bool {
    match go::to_lower(str_of(path(body, "thinking.type")).trim()).as_str() {
        "adaptive" => true,
        "enabled" => match path(body, "thinking.budget_tokens") {
            Some(budget @ Value::Number(_)) => {
                let budget = int_of(budget);
                budget == -1 || budget > 0
            }
            _ => true,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests;
