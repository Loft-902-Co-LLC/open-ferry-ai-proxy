// Ported from CLIProxyAPI internal/thinking/apply.go, validate.go, suffix.go,
// configuration_update.go, and
// internal/runtime/executor/helps/model_capabilities.go and thinking.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings on a request, whatever provider it goes to.
//!
//! A suffix on the model name, such as `gemini-2.5-pro(8192)` or
//! `claude-opus-4-6(high)`, overrides what the request says. The setting is
//! checked against the model's limits, then a [`Target`] writes it the way
//! its provider takes it. Whether reasoning summaries are shown is kept from
//! the client's request.
//!
//! A request going to a Responses target (Codex, or OpenAI Responses for a
//! compact call) may carry `configuration_update` input items, which change
//! the effort mid-conversation. A model that can't take them has them
//! removed, and its setting is read from the last of them. A Responses
//! client's request to a model that can take them goes on as it is, unless
//! a suffix overrides it.
//!
//! [`crate::claude::thinking`], [`crate::gemini::thinking`],
//! [`crate::codex::thinking`] (`reasoning.effort`) and
//! [`crate::openai_compat::thinking`] (`reasoning_effort`) are the targets.
//!
//! Deviations from upstream:
//! - The caller looks the model up (see each target); upstream asks its
//!   model registry, falling back to the built-in catalog, or takes the
//!   model the credential manager resolved for an API key
//!   (`ResolvedModelInfo`), which isn't ported, so the executors never
//!   bind a model to a request. [`apply_with_model`] is upstream's
//!   `ApplyThinkingWithModelInfo` for such a model, with its mapping of an
//!   `xhigh` or `max` level onto the model's levels
//!   (`mapConfiguredHighIntent`); only the parity harness calls it, for the
//!   Codex and OpenAI targets. It reads a Responses client's request that
//!   isn't valid JSON as having no effort, where upstream's
//!   `extractCodexConfig` reads what gjson finds in it. A Claude model bound
//!   to a request would also need `stripInferredClaudeSummaryActivation`,
//!   which isn't ported.
//! - The xAI, Kimi and Antigravity targets aren't ported.
//! - A plugin's request normalizer (`normalizedUpdatesChanged`) isn't
//!   ported, so the client's Responses request is always read for its
//!   effort updates.

use open_ferry_core::exec::ExecError;
use open_ferry_translate::go;
use open_ferry_translate::models::{ModelCatalog, ThinkingSupport};
use open_ferry_translate::registry::{Format, Registry};
use open_ferry_translate::thinking::summary::{self, Summary};
use open_ferry_translate::thinking::{budget_to_level, level_to_budget};
use serde_json::Value;
use tracing::{debug, warn};

use crate::json::{self, Body};

/// Levels from least to most thinking.
const LEVEL_ORDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Budget,
    Level,
    None,
    Auto,
}

/// Upstream's `ThinkingConfig`. Its zero value, a budget of 0 with no level,
/// means the request says nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) mode: Mode,
    pub(crate) budget: i64,
    pub(crate) level: String,
}

impl Config {
    pub(crate) fn unset() -> Self {
        Self::budget(0)
    }

    pub(crate) fn none() -> Self {
        Self {
            mode: Mode::None,
            budget: 0,
            level: String::new(),
        }
    }

    pub(crate) fn auto() -> Self {
        Self {
            mode: Mode::Auto,
            budget: -1,
            level: String::new(),
        }
    }

    pub(crate) fn level(level: impl Into<String>) -> Self {
        Self {
            mode: Mode::Level,
            budget: 0,
            level: level.into(),
        }
    }

    pub(crate) fn budget(budget: i64) -> Self {
        Self {
            mode: Mode::Budget,
            budget,
            level: String::new(),
        }
    }

    /// `hasThinkingConfig`.
    pub(crate) fn is_set(&self) -> bool {
        self.mode != Mode::Budget || self.budget != 0 || !self.level.is_empty()
    }

    /// `thinkingIsFullyDisabled`.
    pub(crate) fn fully_disabled(&self) -> bool {
        self.mode == Mode::None && self.budget == 0 && self.level.is_empty()
    }
}

/// What the thinking code needs of a model (upstream's `ModelInfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Model {
    /// The model's ID.
    pub(crate) id: String,
    /// Its provider family, such as `gemini`, or empty when unknown.
    pub(crate) model_type: String,
    /// Its thinking settings, if it thinks.
    pub(crate) thinking: Option<ThinkingSupport>,
    /// Configured by the user rather than taken from the catalog: its
    /// setting goes upstream unchecked.
    pub(crate) user_defined: bool,
    /// The most output tokens it gives, or 0 if unknown.
    pub(crate) max_completion_tokens: i64,
    /// Takes a Responses request's `configuration_update` input items.
    pub(crate) support_configuration_update: bool,
}

/// A provider format that thinking settings are written for (upstream's
/// `ProviderApplier`).
pub(crate) trait Target {
    /// The format, as the translator registry names it.
    const NAME: &'static str;

    /// `StripThinkingConfig`: removes the setting from a request for a model
    /// that doesn't think.
    fn strip(body: &mut Value);

    /// `Apply` for a model the catalog knows, with a validated setting.
    fn apply_known(body: &mut Value, config: Config, model: &Model, support: &ThinkingSupport);

    /// `Apply` for a model the catalog doesn't know or the user defined: the
    /// setting as it is.
    fn apply_compatible(body: &mut Value, config: &Config);

    /// `applySummaryConfigForProvider`: shows or hides summaries as `summary`
    /// says, for a request for `model` going to `provider` (upstream's
    /// provider key). Only a Chat Completions provider's name matters.
    fn apply_summary(body: &mut Value, model: &str, _provider: &str, summary: Summary) {
        summary::apply_for_model(body, Self::NAME, model, summary, ModelCatalog::embedded());
    }
}

/// Where a request goes, as upstream's `ApplyThinking` is told.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Route<'a> {
    /// The model as the client named it, with any suffix.
    pub(crate) model: &'a str,
    /// The client's format.
    pub(crate) from: &'a str,
    /// The format the body was translated to: the target's, or
    /// `openai-response` for the Codex target on a compact call.
    pub(crate) to: &'a str,
    /// The executor's provider, which upstream looks models up by.
    pub(crate) provider: &'a str,
}

/// `ApplyRequestThinking` for a request translated from `from` into the
/// `T` body `body`: applies the thinking setting that `model`'s suffix or
/// the request asks for. `payload` and `original_request` are the client's
/// request as the executor got it and as the client first sent it.
/// `lookup` finds a model by its name without a suffix.
///
/// A setting the model can't take is a 400 error.
pub(crate) fn apply_request<T: Target>(
    body: &mut Value,
    model: &str,
    from: &str,
    payload: &Body,
    original_request: &Body,
    lookup: impl FnOnce(&str) -> Option<Model>,
) -> Result<(), ExecError> {
    let route = Route {
        model,
        from,
        to: T::NAME,
        provider: T::NAME,
    };
    apply_request_to::<T>(body, route, payload, original_request, lookup)
}

/// [`apply_request`] for a body translated to `route.to`, which may name the
/// target differently, for an executor of `route.provider`.
pub(crate) fn apply_request_to<T: Target>(
    body: &mut Value,
    route: Route<'_>,
    payload: &Body,
    original_request: &Body,
    lookup: impl FnOnce(&str) -> Option<Model>,
) -> Result<(), ExecError> {
    let original_source = if original_request.is_empty() {
        payload
    } else {
        original_request
    };
    let source = if payload.is_empty() {
        original_request
    } else {
        payload
    };
    let summary = translated_summary(body, payload.json(), original_source.json(), &route);
    apply::<T>(body, source, route, summary, lookup, false)
}

/// `ApplyThinkingWithModelInfo`: applies the thinking setting to a `T`
/// body for `model`, the model bound to the request, or for an unknown
/// model if `None`. `source` is the client's request; summaries are shown
/// as it asks, or as the body does if it is empty.
///
/// A setting the model can't take is an error, with the body changed only
/// by removing the effort updates the model can't take.
pub(crate) fn apply_with_model<T: Target>(
    body: &mut Value,
    source: &Body,
    route: Route<'_>,
    model: Option<Model>,
) -> Result<(), ExecError> {
    let summary = match source {
        Body::Empty => summary::extract(body, route.to),
        Body::Json(source) => summary::extract(source, route.from),
        Body::Invalid => Summary::Unspecified,
    };
    apply::<T>(body, source, route, summary, |_| model, true)
}

/// `ApplyThinking`, which upstream's conversion matrix calls: the body alone
/// says whether summaries are shown, and there is no client request to read.
#[cfg(test)]
pub(crate) fn apply_thinking<T: Target>(
    body: &mut Value,
    route: Route<'_>,
    lookup: impl FnOnce(&str) -> Option<Model>,
) -> Result<(), ExecError> {
    let summary = summary::extract(body, route.to);
    apply::<T>(body, &Body::Empty, route, summary, lookup, false)
}

/// `translatedRequestSummaryConfig`: whether to show summaries, from the
/// translated body or else from the client's request.
fn translated_summary(
    body: &Value,
    current: Option<&Value>,
    original: Option<&Value>,
    route: &Route<'_>,
) -> Summary {
    let from = json::lower_trim(route.from);
    let to = json::lower_trim(route.to);
    let target = if from == to {
        summary::extract(body, &to)
    } else {
        summary::extract_explicit(body, &to)
    };
    if target != Summary::Unspecified {
        return target;
    }

    let translated = |source: Option<&Value>| {
        source.map_or(Summary::Unspecified, |source| {
            summary::extract_translated(source, &from, &to)
        })
    };
    let current = translated(current);
    let original = translated(original);
    if current == Summary::Unspecified {
        return original;
    }
    if !Registry::global().has_request_transformer(&Format::new(from), &Format::new(to.clone())) {
        return Summary::Unspecified;
    }
    // If the translated body could say it but doesn't, the translation
    // dropped it on purpose.
    let mut candidate = body.clone();
    summary::apply_for_model(
        &mut candidate,
        &to,
        route.model,
        current,
        ModelCatalog::embedded(),
    );
    if summary::extract_explicit(&candidate, &to) != Summary::Unspecified {
        return Summary::Unspecified;
    }
    current
}

/// What `applyThinking` settles about a request before applying it.
struct Plan<'a> {
    /// The model without its suffix.
    base: &'a str,
    /// What is inside the model's suffix, if it has one.
    suffix: Option<&'a str>,
    /// The client's format, in lower case.
    from: String,
    /// The executor's provider, in lower case.
    provider: String,
    /// Whether to show summaries.
    summary: Summary,
    /// A Responses client's request to a Responses model that takes its
    /// effort updates, whose summary settings are left alone.
    native_responses: bool,
}

/// `applyThinking` with `T` as the target. `resolved` says the model was
/// bound to the request rather than looked up.
fn apply<T: Target>(
    body: &mut Value,
    source: &Body,
    route: Route<'_>,
    summary: Summary,
    lookup: impl FnOnce(&str) -> Option<Model>,
    resolved: bool,
) -> Result<(), ExecError> {
    let mut from = json::lower_trim(route.from);
    if from.is_empty() {
        T::NAME.clone_into(&mut from);
    }
    let mut provider = json::lower_trim(route.provider);
    if provider.is_empty() {
        T::NAME.clone_into(&mut provider);
    }
    let (base, suffix) = parse_suffix(route.model);
    let info = lookup(base);

    // A Responses request may change its effort mid-conversation; the
    // client's request says so before updates are removed from the body.
    let responses_source = is_responses_format(&from);
    let source_config = if responses_source {
        let request = if source.is_empty() {
            Some(&*body)
        } else {
            source.json()
        };
        request.map_or_else(Config::unset, codex_usage_config)
    } else {
        Config::unset()
    };
    let response_target = matches!(T::NAME, "codex" | "xai");
    let supports_updates = info
        .as_ref()
        .is_some_and(|info| info.support_configuration_update);
    if response_target && !supports_updates {
        strip_configuration_updates(body);
    }
    let native_responses = response_target && responses_source && supports_updates;
    if native_responses && suffix.is_none() {
        // The request's own effort and updates go on as they are.
        if let Some(info) = &info {
            log_native_effort(body, info, T::NAME);
        }
        return Ok(());
    }

    let plan = Plan {
        base,
        suffix,
        from,
        provider,
        summary,
        native_responses,
    };
    let info = match info {
        Some(info) if !info.user_defined => info,
        info => {
            apply_user_defined::<T>(body, info.as_ref(), &plan, source_config);
            return Ok(());
        }
    };
    let Some(support) = info.thinking.as_ref() else {
        if extract_config(body, T::NAME).is_set() || summary != Summary::Unspecified {
            debug!(
                model = base,
                provider = T::NAME,
                "thinking: model does not support thinking, stripping config"
            );
            T::strip(body);
        }
        return Ok(());
    };

    let mut config = match suffix {
        Some(raw) => suffix_config(raw),
        None => {
            let mut config = source_config;
            if !config.is_set() && resolved {
                // `extractSourceThinkingConfig`.
                if let Some(source) = source.json() {
                    config = if plan.from == "openai-response" {
                        codex_config(source)
                    } else {
                        extract_config(source, &plan.from)
                    };
                }
            }
            if !config.is_set() {
                config = extract_config(body, T::NAME);
            }
            config
        }
    };
    if !config.is_set() {
        if !native_responses {
            T::apply_summary(body, base, &plan.provider, summary);
        }
        return Ok(());
    }
    if resolved && config.mode == Mode::Level && maps_high_intent(&plan.from, T::NAME, &info) {
        config.level = map_high_intent(&config.level, support);
    }
    let config = validate(config, &info, support, &plan.from, T::NAME, suffix.is_some())
        .inspect_err(|error| {
            warn!(model = %info.id, provider = T::NAME, error = %error.message, "thinking: validation failed");
        })?;
    debug!(
        model = %info.id,
        provider = T::NAME,
        mode = ?config.mode,
        budget = config.budget,
        level = %config.level,
        "thinking: processed config to apply"
    );
    T::apply_known(body, config.clone(), &info, support);
    // A setting that turns thinking off wins over showing summaries.
    if !config.fully_disabled() && !native_responses {
        T::apply_summary(body, base, &plan.provider, summary);
    }
    Ok(())
}

/// Logs the effort a native Responses request goes on with, which isn't
/// checked or rewritten.
fn log_native_effort(body: &Value, info: &Model, provider: &str) {
    if !tracing::enabled!(tracing::Level::DEBUG) || (info.thinking.is_none() && !info.user_defined)
    {
        return;
    }
    let config = codex_usage_config(body);
    if !config.is_set() {
        return;
    }
    let baseline = codex_config(body);
    let baseline_level = (baseline.mode == Mode::Level).then_some(baseline.level.as_str());
    for message in [
        "thinking: original config from request",
        "thinking: processed config to apply",
    ] {
        debug!(
            provider,
            model = %info.id,
            mode = ?config.mode,
            budget = config.budget,
            level = %config.level,
            baseline_level,
            "{message}"
        );
    }
}

/// `shouldMapConfiguredHighIntent`: whether a level from another format, or
/// for a model of another family, is mapped onto the model's own levels.
fn maps_high_intent(from: &str, to: &str, model: &Model) -> bool {
    if from != to {
        return true;
    }
    let model_type = json::lower_trim(&model.model_type);
    !model_type.is_empty() && !same_family(to, &model_type)
}

/// `mapConfiguredHighIntent`: `xhigh` or `max` becomes the first of itself,
/// the other and `high` that the model has. Other levels are lowercased.
fn map_high_intent(level: &str, support: &ThinkingSupport) -> String {
    if support.levels.is_empty() {
        return level.to_owned();
    }
    let level = json::lower_trim(level);
    let candidates: [&str; 3] = match level.as_str() {
        "xhigh" => ["xhigh", "max", "high"],
        "max" => ["max", "xhigh", "high"],
        _ => return level,
    };
    candidates
        .into_iter()
        .find(|candidate| level_supported(candidate, &support.levels))
        .map_or(level, str::to_owned)
}

/// `applyUserDefinedModel`: a model the catalog doesn't know, or one the
/// user defined, gets the setting unchecked, and the provider judges it.
fn apply_user_defined<T: Target>(
    body: &mut Value,
    info: Option<&Model>,
    plan: &Plan<'_>,
    source_config: Config,
) {
    let model = info.map_or(plan.base, |info| info.id.as_str());
    let config = match plan.suffix {
        Some(raw) => suffix_config(raw),
        None => {
            let mut config = source_config;
            if !config.is_set() {
                config = extract_config(body, &plan.from);
            }
            if !config.is_set() && plan.from != T::NAME {
                config = extract_config(body, T::NAME);
            }
            config
        }
    };
    if !config.is_set() {
        T::apply_summary(body, model, &plan.provider, plan.summary);
        return;
    }
    let config = normalize_user_defined(config, T::NAME);
    T::apply_compatible(body, &config);
    if !config.fully_disabled() && !plan.native_responses {
        T::apply_summary(body, model, &plan.provider, plan.summary);
    }
}

/// `normalizeUserDefinedConfig`: providers that take budgets get a level as
/// a budget, except Claude, which takes levels as adaptive efforts.
fn normalize_user_defined(config: Config, target: &str) -> Config {
    if config.mode != Mode::Level || target == "claude" || !budget_capable(target) {
        return config;
    }
    match level_to_budget(&config.level) {
        Some(budget) => Config::budget(budget),
        None => config,
    }
}

/// `isBudgetCapableProvider`.
fn budget_capable(provider: &str) -> bool {
    matches!(provider, "gemini" | "antigravity" | "claude")
}

/// `ParseSuffix`: the model name and what is inside a trailing `(...)`.
pub(crate) fn parse_suffix(model: &str) -> (&str, Option<&str>) {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => (
            model.get(..open).unwrap_or(model),
            model.get(open + 1..model.len() - 1),
        ),
        _ => (model, None),
    }
}

/// `parseSuffixToConfig`: `none`, `auto` or `-1`, a level name, or a
/// non-negative budget. Anything else says nothing.
pub(crate) fn suffix_config(raw: &str) -> Config {
    if raw.is_empty() {
        return Config::unset();
    }
    let lower = go::to_lower(raw);
    match lower.as_str() {
        "none" => return Config::none(),
        "auto" | "-1" => return Config::auto(),
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => return Config::level(lower),
        _ => {}
    }
    match raw.parse::<i64>() {
        Ok(0) => Config::none(),
        Ok(budget) if budget > 0 => Config::budget(budget),
        _ => {
            debug!(
                raw_suffix = raw,
                "thinking: unknown suffix format, treating as no config"
            );
            Config::unset()
        }
    }
}

/// `extractThinkingConfig`: the setting in a request in `provider`'s format.
pub(crate) fn extract_config(body: &Value, provider: &str) -> Config {
    match provider {
        "claude" => claude_config(body),
        "gemini" => gemini_config(body, "generationConfig.thinkingConfig"),
        "antigravity" => gemini_config(body, "request.generationConfig.thinkingConfig"),
        "interactions" => interactions_config(body),
        "openai" => openai_config(body),
        "codex" | "xai" => codex_config(body),
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => kimi_config(body),
        _ => Config::unset(),
    }
}

/// A level from an effort field: `none`, `auto`, or a level name.
fn effort_config(value: String) -> Config {
    match value.as_str() {
        "none" => Config::none(),
        "auto" => Config::auto(),
        _ => Config::level(value),
    }
}

/// A budget field: 0 is off, -1 is auto.
fn budget_config(value: i64) -> Config {
    match value {
        0 => Config::none(),
        -1 => Config::auto(),
        _ => Config::budget(value),
    }
}

/// `extractClaudeConfig`. A disabled type wins over a budget, and a budget
/// over an effort. Adaptive thinking counts only with an effort; enabled
/// thinking with neither means auto.
pub(crate) fn claude_config(body: &Value) -> Config {
    let kind = json::str_at(body, "thinking.type");
    let effort = || {
        json::string_at(body, "output_config.effort")
            .map(json::lower_trim)
            .filter(|effort| !effort.is_empty())
    };
    if kind == "disabled" {
        return Config::none();
    }
    if kind == "adaptive" || kind == "auto" {
        return effort().map_or_else(Config::unset, effort_config);
    }
    if let Some(budget) = json::get(body, "thinking.budget_tokens") {
        return budget_config(json::int_of(Some(budget)));
    }
    if kind == "enabled" {
        return effort().map_or_else(Config::auto, effort_config);
    }
    Config::unset()
}

/// `extractGeminiConfig`: the level first, then the budget, each in camel or
/// snake case.
pub(crate) fn gemini_config(body: &Value, prefix: &str) -> Config {
    let field = |camel: &str, snake: &str| {
        json::get(body, &format!("{prefix}.{camel}"))
            .or_else(|| json::get(body, &format!("{prefix}.{snake}")))
    };
    if let Some(level) = field("thinkingLevel", "thinking_level") {
        return effort_config(json::str_of(Some(level)));
    }
    if let Some(budget) = field("thinkingBudget", "thinking_budget") {
        return budget_config(json::int_of(Some(budget)));
    }
    Config::unset()
}

/// `extractInteractionsConfig`.
fn interactions_config(body: &Value) -> Config {
    const LEVELS: [&str; 6] = [
        "generation_config.thinking_level",
        "generation_config.thinkingLevel",
        "generation_config.thinking_config.thinking_level",
        "generation_config.thinking_config.thinkingLevel",
        "generation_config.thinkingConfig.thinking_level",
        "generation_config.thinkingConfig.thinkingLevel",
    ];
    const BUDGETS: [&str; 6] = [
        "generation_config.thinking_budget",
        "generation_config.thinkingBudget",
        "generation_config.thinking_config.thinking_budget",
        "generation_config.thinking_config.thinkingBudget",
        "generation_config.thinkingConfig.thinking_budget",
        "generation_config.thinkingConfig.thinkingBudget",
    ];
    if let Some(level) = LEVELS.iter().find_map(|path| json::get(body, path)) {
        return effort_config(json::lower_trim(&json::str_of(Some(level))));
    }
    if let Some(budget) = BUDGETS.iter().find_map(|path| json::get(body, path)) {
        return budget_config(json::int_of(Some(budget)));
    }
    Config::unset()
}

/// `extractOpenAIConfig`: `reasoning_effort`, as written.
fn openai_config(body: &Value) -> Config {
    level_field(body, "reasoning_effort")
}

/// `extractCodexConfig`: `reasoning.effort`, as written.
fn codex_config(body: &Value) -> Config {
    level_field(body, "reasoning.effort")
}

fn level_field(body: &Value, path: &str) -> Config {
    match json::get(body, path) {
        Some(effort) => {
            let value = json::str_of(Some(effort));
            if value == "none" {
                Config::none()
            } else {
                Config::level(value)
            }
        }
        None => Config::unset(),
    }
}

/// `extractKimiConfig`: Kimi's own `thinking` object, else
/// `reasoning_effort`.
fn kimi_config(body: &Value) -> Config {
    let kind = json::get(body, "thinking.type");
    let effort = json::get(body, "thinking.effort");
    if let Some(kind) = kind {
        match json::lower_trim(&json::str_of(Some(kind))).as_str() {
            "disabled" => return Config::none(),
            "enabled" if effort.is_none() => return Config::unset(),
            _ => {}
        }
    }
    if let Some(effort) = effort {
        let value = json::lower_trim(&json::str_of(Some(effort)));
        if value.is_empty() {
            return Config::unset();
        }
        return effort_config(value);
    }
    if kind.is_some() {
        return Config::unset();
    }
    openai_config(body)
}

/// `extractCodexUsageConfig`: the last effort a Responses request changes
/// to, else its top-level effort.
pub(crate) fn codex_usage_config(body: &Value) -> Config {
    let update = configuration_update_config(body);
    if update.is_set() {
        return update;
    }
    codex_config(body)
}

/// `isResponsesFormat`.
fn is_responses_format(format: &str) -> bool {
    matches!(format, "codex" | "openai-response")
}

/// Whether an input item is a `configuration_update`.
fn is_configuration_update(item: &Value) -> bool {
    json::str_at(item, "type") == "configuration_update"
}

/// `extractConfigurationUpdateConfig`: the last non-empty effort among the
/// `configuration_update` input items.
fn configuration_update_config(body: &Value) -> Config {
    let Some(Value::Array(input)) = body.get("input") else {
        return Config::unset();
    };
    let effort = input
        .iter()
        .filter(|item| is_configuration_update(item))
        .filter_map(|item| json::string_at(item, "reasoning.effort"))
        .map(json::lower_trim)
        .rfind(|effort| !effort.is_empty());
    effort.map_or_else(Config::unset, effort_config)
}

/// `stripConfigurationUpdates`: removes the `configuration_update` input
/// items, leaving the others as they are.
pub(crate) fn strip_configuration_updates(body: &mut Value) {
    if let Some(Value::Array(input)) = body.get_mut("input") {
        input.retain(|item| !is_configuration_update(item));
    }
}

/// `stripResponsesEffort`: removes `reasoning.effort`, and `reasoning` if
/// nothing else is left in it, keeping the summary settings.
pub(crate) fn strip_responses_effort(body: &mut Value) {
    if !json::delete(body, "reasoning.effort") {
        return;
    }
    if body
        .get("reasoning")
        .is_some_and(|reasoning| reasoning.as_object().is_some_and(serde_json::Map::is_empty))
    {
        json::delete(body, "reasoning");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Capability {
    None,
    BudgetOnly,
    LevelOnly,
    Hybrid,
}

/// `detectModelCapability`.
fn capability(support: &ThinkingSupport) -> Capability {
    let budget = support.min > 0 || support.max > 0;
    let levels = !support.levels.is_empty();
    match (budget, levels) {
        (true, true) => Capability::Hybrid,
        (true, false) => Capability::BudgetOnly,
        (false, true) => Capability::LevelOnly,
        (false, false) => Capability::None,
    }
}

/// `isSameProviderFamily`.
fn same_family(from: &str, to: &str) -> bool {
    let gemini = |format: &str| matches!(format, "gemini" | "antigravity");
    let openai = |format: &str| matches!(format, "openai" | "openai-response" | "codex");
    from == to || (gemini(from) && gemini(to)) || (openai(from) && openai(to))
}

fn thinking_error(message: String) -> ExecError {
    ExecError::upstream(400, message)
}

/// `ValidateConfig` for a model with thinking support: converts between
/// budgets and levels as the model needs, clamps to its limits, and rejects
/// what it can't take. `from` and `to` are lower-case formats.
fn validate(
    mut config: Config,
    model: &Model,
    support: &ThinkingSupport,
    from: &str,
    to: &str,
    from_suffix: bool,
) -> Result<Config, ExecError> {
    let capability = capability(support);
    let has_levels = matches!(capability, Capability::LevelOnly | Capability::Hybrid);
    // A model served over another provider's protocol may clamp levels its
    // own family wouldn't.
    let model_type = json::lower_trim(&model.model_type);
    let family_mismatch = !model_type.is_empty()
        && ((!from.is_empty() && !same_family(from, &model_type))
            || (!to.is_empty() && !same_family(to, &model_type)));
    // A level from another provider's request may be clamped to the nearest
    // one the model has; a request's own level must fit.
    let allow_clamp = has_levels && (!same_family(from, to) || family_mismatch);
    let strict_budget =
        !from_suffix && !from.is_empty() && same_family(from, to) && !family_mismatch;
    let mut derived_budget = false;

    match capability {
        Capability::BudgetOnly if config.mode == Mode::Level && config.level != "auto" => {
            let budget = level_to_budget(&config.level)
                .ok_or_else(|| thinking_error(format!("unknown level: {}", config.level)))?;
            config = Config::budget(budget);
            derived_budget = true;
        }
        Capability::LevelOnly if config.mode == Mode::Budget => {
            let level = budget_to_level(config.budget).ok_or_else(|| {
                thinking_error(format!(
                    "budget {} cannot be converted to a valid level",
                    config.budget
                ))
            })?;
            config = Config::level(clamp_level(level, support));
        }
        _ => {}
    }

    if config.mode == Mode::Level && config.level == "none" {
        config = Config::none();
    }
    if config.mode == Mode::Level && config.level == "auto" {
        config = Config::auto();
    }
    if config.mode == Mode::Budget && config.budget == 0 {
        config.mode = Mode::None;
        config.level.clear();
    }

    if !support.levels.is_empty()
        && config.mode == Mode::Level
        && !level_supported(&config.level, &support.levels)
    {
        if allow_clamp {
            config.level = clamp_level(&config.level, support);
        }
        if !level_supported(&config.level, &support.levels) {
            let valid: Vec<String> = support
                .levels
                .iter()
                .map(|level| json::lower_trim(level))
                .collect();
            return Err(thinking_error(format!(
                "level {} not supported, valid levels: {}",
                go::quote(&go::to_lower(&config.level)),
                valid.join(", ")
            )));
        }
    }

    if strict_budget
        && config.mode == Mode::Budget
        && !derived_budget
        && (support.min != 0 || support.max != 0)
        && (config.budget < support.min
            || config.budget > support.max
            || (config.budget == 0 && !support.zero_allowed))
    {
        return Err(thinking_error(format!(
            "budget {} out of range [{},{}]",
            config.budget, support.min, support.max
        )));
    }

    if config.mode == Mode::Auto && !support.dynamic_allowed {
        config = auto_to_mid_range(config, support);
        if config.mode == Mode::Level
            && !support.levels.is_empty()
            && !level_supported(&config.level, &support.levels)
        {
            config.level = clamp_level(&config.level, support);
        }
    }

    if config.mode == Mode::None && to == "claude" {
        // Claude turns thinking off outright, without a budget.
        config.budget = 0;
        config.level.clear();
    } else {
        if matches!(config.mode, Mode::Budget | Mode::Auto | Mode::None) {
            config.budget = clamp_budget(config.budget, support);
        }
        // Off on a model that can't turn thinking off falls back to its
        // lowest level.
        let cannot_disable = !support.zero_allowed && !level_supported("none", &support.levels);
        if config.mode == Mode::None
            && !support.levels.is_empty()
            && (config.budget > 0 || cannot_disable)
            && let Some(lowest) = support.levels.first()
        {
            lowest.clone_into(&mut config.level);
        }
    }
    Ok(config)
}

/// `convertAutoToMidRange`: auto on a model that can't decide for itself
/// becomes `medium`, or the middle of its budget range.
fn auto_to_mid_range(config: Config, support: &ThinkingSupport) -> Config {
    if !support.levels.is_empty() && support.min == 0 && support.max == 0 {
        return Config::level("medium");
    }
    let mid = support.min.wrapping_add(support.max) / 2;
    let (mode, budget) = if mid <= 0 && support.zero_allowed {
        (Mode::None, 0)
    } else if mid <= 0 {
        (Mode::Budget, support.min)
    } else {
        (Mode::Budget, mid)
    };
    Config {
        mode,
        budget,
        ..config
    }
}

/// `clampLevel`: the supported level nearest to `level`, the lower one on a
/// tie. A level outside the standard order stays as it is.
pub(crate) fn clamp_level(level: &str, support: &ThinkingSupport) -> String {
    if support.levels.is_empty() || level_supported(level, &support.levels) {
        return level.to_owned();
    }
    let Some(position) = level_index(level) else {
        return level.to_owned();
    };
    let mut best: Option<(usize, usize)> = None;
    for supported in &support.levels {
        let Some(index) = level_index(supported.trim()) else {
            continue;
        };
        let distance = position.abs_diff(index);
        if best.is_none_or(|(best_index, best_distance)| {
            distance < best_distance || (distance == best_distance && index < best_index)
        }) {
            best = Some((index, distance));
        }
    }
    match best.and_then(|(index, _)| LEVEL_ORDER.get(index)) {
        Some(clamped) => {
            debug!(original = level, clamped, "thinking: level clamped");
            (*clamped).to_owned()
        }
        None => level.to_owned(),
    }
}

/// `clampBudget`: -1 passes; 0 becomes the minimum unless the model allows
/// it; anything else is held to the model's range.
pub(crate) fn clamp_budget(value: i64, support: &ThinkingSupport) -> i64 {
    if value == -1 {
        return value;
    }
    let (min, max) = (support.min, support.max);
    if value == 0 && !support.zero_allowed {
        warn!(clamped_to = min, "thinking: budget zero not allowed");
        return min;
    }
    if min == 0 && max == 0 {
        return value;
    }
    if value < min {
        if value == 0 && support.zero_allowed {
            return 0;
        }
        return min;
    }
    value.min(max)
}

/// `isLevelSupported` (and `HasLevel`): whether `supported` has `level`,
/// whatever their case and the spaces around a supported one.
pub(crate) fn level_supported(level: &str, supported: &[String]) -> bool {
    supported
        .iter()
        .any(|candidate| json::eq_fold(level, candidate.trim()))
}

fn level_index(level: &str) -> Option<usize> {
    LEVEL_ORDER
        .iter()
        .position(|candidate| json::eq_fold(level, candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_suffixes() {
        assert_eq!(parse_suffix("m(high)"), ("m", Some("high")));
        assert_eq!(parse_suffix("m(a)(8192)"), ("m(a)", Some("8192")));
        assert_eq!(parse_suffix("m(high"), ("m(high", None));
        assert_eq!(suffix_config("NONE"), Config::none());
        assert_eq!(suffix_config("-1"), Config::auto());
        assert_eq!(suffix_config("XHigh"), Config::level("xhigh"));
        assert_eq!(suffix_config("0"), Config::none());
        assert_eq!(suffix_config("+16384"), Config::budget(16384));
        assert_eq!(suffix_config("-5"), Config::unset());
        assert_eq!(suffix_config("lots"), Config::unset());
    }

    // extractGeminiConfig, as apply_test.go's Gemini cases read it.
    #[test]
    fn reads_gemini_settings() {
        let cases = [
            (
                json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "high", "thinkingBudget": 8192}}}),
                Config::level("high"),
            ),
            (
                json!({"generationConfig": {"thinkingConfig": {"thinking_level": "none"}}}),
                Config::none(),
            ),
            (
                json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "auto"}}}),
                Config::auto(),
            ),
            (
                json!({"generationConfig": {"thinkingConfig": {"thinking_budget": 0}}}),
                Config::none(),
            ),
            (
                json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": -1}}}),
                Config::auto(),
            ),
            (
                json!({"generationConfig": {"thinkingConfig": {"thinkingBudget": 1024}}}),
                Config::budget(1024),
            ),
            (json!({"generationConfig": {}}), Config::unset()),
        ];
        for (body, want) in cases {
            assert_eq!(extract_config(&body, "gemini"), want, "{body}");
        }
        let wrapped =
            json!({"request": {"generationConfig": {"thinkingConfig": {"thinkingBudget": 64}}}});
        assert_eq!(extract_config(&wrapped, "antigravity"), Config::budget(64));
    }

    fn gemini_model(support: ThinkingSupport) -> Model {
        Model {
            id: "m".into(),
            model_type: "gemini".into(),
            thinking: Some(support),
            ..Model::default()
        }
    }

    // validate_test.go: the paths a Gemini target takes, which the Claude
    // target doesn't.
    #[test]
    fn validates_for_a_gemini_target() {
        let levels = ThinkingSupport {
            levels: vec!["low".into(), "high".into()],
            ..ThinkingSupport::default()
        };
        let model = gemini_model(levels.clone());
        // Off on a level model that can't turn thinking off falls back to its
        // lowest level.
        let config = validate(Config::none(), &model, &levels, "gemini", "gemini", false).unwrap();
        assert_eq!(
            config,
            Config {
                level: "low".into(),
                ..Config::none()
            }
        );

        // A budget becomes the nearest level the model has.
        let config = validate(
            Config::budget(8192),
            &model,
            &levels,
            "gemini",
            "gemini",
            false,
        )
        .unwrap();
        assert_eq!(config, Config::level("low"));

        // Off with zero allowed stays off.
        let budget = ThinkingSupport {
            min: 128,
            max: 32768,
            zero_allowed: true,
            ..ThinkingSupport::default()
        };
        let model = gemini_model(budget.clone());
        let config = validate(Config::none(), &model, &budget, "gemini", "gemini", false).unwrap();
        assert_eq!(config, Config::none());

        // A Gemini request's budget must be in range; another format's is
        // clamped.
        let error = validate(
            Config::budget(64),
            &model,
            &budget,
            "gemini",
            "gemini",
            false,
        )
        .unwrap_err();
        assert_eq!(error.message, "budget 64 out of range [128,32768]");
        let config = validate(
            Config::budget(64),
            &model,
            &budget,
            "openai",
            "gemini",
            false,
        )
        .unwrap();
        assert_eq!(config, Config::budget(128));

        // Off without zero allowed takes the minimum budget.
        let strict = ThinkingSupport {
            zero_allowed: false,
            ..budget
        };
        let model = gemini_model(strict.clone());
        let config = validate(Config::none(), &model, &strict, "openai", "gemini", false).unwrap();
        assert_eq!(
            config,
            Config {
                budget: 128,
                ..Config::none()
            }
        );
    }

    // validate_test.go: a model served over another family's protocol
    // clamps a level rather than rejecting it.
    #[test]
    fn a_family_mismatch_clamps_levels() {
        let support = ThinkingSupport {
            levels: vec!["low".into(), "high".into()],
            ..ThinkingSupport::default()
        };
        let model = Model {
            model_type: "kimi".into(),
            ..gemini_model(support.clone())
        };
        let config = validate(
            Config::level("max"),
            &model,
            &support,
            "claude",
            "claude",
            false,
        )
        .unwrap();
        assert_eq!(config, Config::level("high"));
        let same = gemini_model(support.clone());
        let error = validate(
            Config::level("max"),
            &same,
            &support,
            "gemini",
            "gemini",
            false,
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            r#"level "max" not supported, valid levels: low, high"#
        );
    }

    #[test]
    fn user_defined_levels_become_budgets_for_budget_providers() {
        assert_eq!(
            normalize_user_defined(Config::level("high"), "gemini"),
            Config::budget(24576)
        );
        assert_eq!(
            normalize_user_defined(Config::level("high"), "claude"),
            Config::level("high")
        );
        assert_eq!(
            normalize_user_defined(Config::level("turbo"), "gemini"),
            Config::level("turbo")
        );
        assert_eq!(
            normalize_user_defined(Config::level("high"), "openai"),
            Config::level("high")
        );
    }

    #[test]
    fn clamps_levels_to_the_nearest() {
        let support = ThinkingSupport {
            levels: vec!["low".into(), " HIGH ".into()],
            ..ThinkingSupport::default()
        };
        assert_eq!(clamp_level("medium", &support), "low");
        assert_eq!(clamp_level("max", &support), "high");
        assert_eq!(clamp_level("High", &support), "High");
        assert_eq!(clamp_level("turbo", &support), "turbo");
    }

    // configuration_update.go: only the update items go, the rest keep
    // their order, and input that isn't an array is left alone.
    #[test]
    fn strips_configuration_updates() {
        let mut body = json!({"input": [
            {"role": "user", "content": "a"},
            {"type": "configuration_update", "reasoning": {"effort": "low"}},
            {"type": "message", "role": "user", "content": "b"},
            {"type": "configuration_update"}
        ]});
        strip_configuration_updates(&mut body);
        assert_eq!(
            body,
            json!({"input": [
                {"role": "user", "content": "a"},
                {"type": "message", "role": "user", "content": "b"}
            ]})
        );
        let mut body = json!({"input": {"type": "configuration_update"}});
        strip_configuration_updates(&mut body);
        assert_eq!(body, json!({"input": {"type": "configuration_update"}}));
    }

    #[test]
    fn strips_a_responses_effort() {
        let mut body = json!({"reasoning": {"effort": "high", "summary": "auto"}});
        strip_responses_effort(&mut body);
        assert_eq!(body, json!({"reasoning": {"summary": "auto"}}));
        let mut body = json!({"reasoning": {"effort": "high"}, "n": 1});
        strip_responses_effort(&mut body);
        assert_eq!(body, json!({"n": 1}));
        // Without an effort, nothing changes.
        let mut body = json!({"reasoning": {}});
        strip_responses_effort(&mut body);
        assert_eq!(body, json!({"reasoning": {}}));
    }

    // mapConfiguredHighIntent.
    #[test]
    fn maps_high_levels_onto_the_models() {
        let support = |levels: &[&str]| ThinkingSupport {
            levels: levels.iter().map(|level| (*level).to_owned()).collect(),
            ..ThinkingSupport::default()
        };
        assert_eq!(map_high_intent("xhigh", &support(&["low", "max"])), "max");
        assert_eq!(map_high_intent(" MAX ", &support(&["xhigh"])), "xhigh");
        assert_eq!(map_high_intent("max", &support(&["low", "high"])), "high");
        assert_eq!(map_high_intent("max", &support(&["low"])), "max");
        assert_eq!(map_high_intent("Medium", &support(&["low"])), "medium");
        assert_eq!(map_high_intent("Max", &support(&[])), "Max");

        let model = |model_type: &str| Model {
            model_type: model_type.into(),
            ..Model::default()
        };
        assert!(maps_high_intent("claude", "codex", &model("")));
        assert!(!maps_high_intent("codex", "codex", &model("openai")));
        assert!(maps_high_intent("codex", "codex", &model("claude")));
        assert!(!maps_high_intent("openai", "openai", &model("")));
    }

    // helps/thinking_test.go
    // (TestApplyThinkingWithSourcePayloadPreservesOriginalOnlySummary): a
    // summary setting only the client's original request has still counts.
    // The test that a plugin's normalizer can remove the summary, and the
    // Antigravity one, are dropped: neither is ported.
    #[test]
    fn keeps_a_summary_only_the_original_request_has() {
        let current = Body::Json(json!({"model": "gemini-3.6-flash", "input": "hi"}));
        let original = Body::Json(
            json!({"model": "gemini-3.6-flash", "reasoning": {"summary": null}, "input": "hi"}),
        );
        let mut body = json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "high"}}});
        crate::gemini::thinking::apply_request(
            &mut body,
            "gemini-3.6-flash",
            "openai-response",
            &current,
            &original,
            None,
            "gemini",
        )
        .unwrap();
        assert_eq!(
            json::get(&body, "generationConfig.thinkingConfig.includeThoughts"),
            Some(&Value::Bool(false)),
            "{body}"
        );
    }

    #[test]
    fn clamps_budgets() {
        let support = ThinkingSupport {
            min: 1024,
            max: 128_000,
            ..ThinkingSupport::default()
        };
        assert_eq!(clamp_budget(0, &support), 1024);
        assert_eq!(clamp_budget(-1, &support), -1);
        assert_eq!(clamp_budget(500, &support), 1024);
        assert_eq!(clamp_budget(200_000, &support), 128_000);
        let zero = ThinkingSupport {
            zero_allowed: true,
            ..support
        };
        assert_eq!(clamp_budget(0, &zero), 0);
    }
}
