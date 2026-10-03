// Ported from CLIProxyAPI internal/thinking/apply.go, validate.go, suffix.go,
// strip.go, configuration_update.go and provider/claude/apply.go, and
// internal/runtime/executor/helps/model_capabilities.go and thinking.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings on a request going to Claude.
//!
//! A suffix on the model name, such as `claude-sonnet-4-5(16384)` or
//! `claude-opus-4-6(high)`, overrides what the request says. The setting is
//! checked against the model's limits and written the way the model takes
//! it: a token budget, or an adaptive effort. Whether reasoning summaries
//! are shown is kept from the client's request.
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
//! - Only the Claude target is ported.

use open_ferry_core::exec::ExecError;
use open_ferry_translate::go;
use open_ferry_translate::models::{ModelCatalog, ModelInfo, ThinkingSupport};
use open_ferry_translate::registry::{Format, Registry};
use open_ferry_translate::thinking::summary::{self, Summary};
use open_ferry_translate::thinking::{budget_to_level, level_to_budget};
use serde_json::Value;
use tracing::{debug, warn};

use super::json::{self, Body};

const TARGET: &str = "claude";

/// Levels from least to most thinking.
const LEVEL_ORDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Budget,
    Level,
    None,
    Auto,
}

/// Upstream's `ThinkingConfig`. Its zero value, a budget of 0 with no level,
/// means the request says nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Config {
    mode: Mode,
    budget: i64,
    level: String,
}

impl Config {
    fn unset() -> Self {
        Self::budget(0)
    }

    fn none() -> Self {
        Self {
            mode: Mode::None,
            budget: 0,
            level: String::new(),
        }
    }

    fn auto() -> Self {
        Self {
            mode: Mode::Auto,
            budget: -1,
            level: String::new(),
        }
    }

    fn level(level: impl Into<String>) -> Self {
        Self {
            mode: Mode::Level,
            budget: 0,
            level: level.into(),
        }
    }

    fn budget(budget: i64) -> Self {
        Self {
            mode: Mode::Budget,
            budget,
            level: String::new(),
        }
    }

    /// `hasThinkingConfig`.
    fn is_set(&self) -> bool {
        self.mode != Mode::Budget || self.budget != 0 || !self.level.is_empty()
    }

    /// `thinkingIsFullyDisabled`.
    fn fully_disabled(&self) -> bool {
        self.mode == Mode::None && self.budget == 0 && self.level.is_empty()
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
    let summary = translated_summary(body, payload.json(), original_source.json(), model, from);
    apply(body, source, model, from, summary)
}

/// `translatedRequestSummaryConfig`: whether to show summaries, from the
/// translated body or else from the client's request.
fn translated_summary(
    body: &Value,
    current: Option<&Value>,
    original: Option<&Value>,
    model: &str,
    from: &str,
) -> Summary {
    let from = json::lower_trim(from);
    let target = if from == TARGET {
        summary::extract(body, TARGET)
    } else {
        summary::extract_explicit(body, TARGET)
    };
    if target != Summary::Unspecified {
        return target;
    }

    let translated = |source: Option<&Value>| {
        source.map_or(Summary::Unspecified, |source| {
            summary::extract_translated(source, &from, TARGET)
        })
    };
    let current = translated(current);
    let original = translated(original);
    if current == Summary::Unspecified {
        return original;
    }
    if !Registry::global().has_request_transformer(&Format::new(from), &Format::CLAUDE) {
        return Summary::Unspecified;
    }
    // If the translated body could say it but doesn't, the translation
    // dropped it on purpose.
    let mut candidate = body.clone();
    summary::apply_for_model(
        &mut candidate,
        TARGET,
        model,
        current,
        ModelCatalog::embedded(),
    );
    if summary::extract_explicit(&candidate, TARGET) != Summary::Unspecified {
        return Summary::Unspecified;
    }
    current
}

/// `applyThinking` with Claude as the target.
fn apply(
    body: &mut Value,
    source: &Body,
    model: &str,
    from: &str,
    summary: Summary,
) -> Result<(), ExecError> {
    let mut from = json::lower_trim(from);
    if from.is_empty() {
        TARGET.clone_into(&mut from);
    }
    let (base, suffix) = parse_suffix(model);
    let info = ModelCatalog::embedded().lookup(base);

    // A Responses request may change its effort mid-conversation.
    let source_config = if from == "codex" || from == "openai-response" {
        let request = if source.is_empty() {
            Some(&*body)
        } else {
            source.json()
        };
        request.map_or_else(Config::unset, codex_usage_config)
    } else {
        Config::unset()
    };

    let Some(info) = info else {
        apply_user_defined(body, base, &from, suffix, source_config, summary);
        return Ok(());
    };
    let Some(support) = info.thinking.as_ref() else {
        if extract_config(body, TARGET).is_set() || summary != Summary::Unspecified {
            debug!(
                model = base,
                "thinking: model does not support thinking, stripping config"
            );
            strip(body);
        }
        return Ok(());
    };

    let config = match suffix {
        Some(raw) => suffix_config(raw),
        None if source_config.is_set() => source_config,
        None => claude_config(body),
    };
    if !config.is_set() {
        apply_summary(body, base, summary);
        return Ok(());
    }
    let config = validate(config, support, &from, suffix.is_some()).inspect_err(|error| {
        warn!(model = %info.id, error = %error.message, "thinking: validation failed");
    })?;
    debug!(
        model = %info.id,
        mode = ?config.mode,
        budget = config.budget,
        level = %config.level,
        "thinking: processed config to apply"
    );
    apply_known(body, config.clone(), info, support);
    // A setting that turns thinking off wins over showing summaries.
    if !config.fully_disabled() {
        apply_summary(body, base, summary);
    }
    Ok(())
}

/// `applyUserDefinedModel`: a model the catalog doesn't know gets the setting
/// unchecked, and Claude judges it.
fn apply_user_defined(
    body: &mut Value,
    model: &str,
    from: &str,
    suffix: Option<&str>,
    source_config: Config,
    summary: Summary,
) {
    let config = match suffix {
        Some(raw) => suffix_config(raw),
        None => {
            let mut config = source_config;
            if !config.is_set() {
                config = extract_config(body, from);
            }
            if !config.is_set() && from != TARGET {
                config = extract_config(body, TARGET);
            }
            config
        }
    };
    if !config.is_set() {
        apply_summary(body, model, summary);
        return;
    }
    apply_compatible(body, &config);
    if !config.fully_disabled() {
        apply_summary(body, model, summary);
    }
}

fn apply_summary(body: &mut Value, model: &str, summary: Summary) {
    summary::apply_for_model(body, TARGET, model, summary, ModelCatalog::embedded());
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
fn suffix_config(raw: &str) -> Config {
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
fn extract_config(body: &Value, provider: &str) -> Config {
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
fn claude_config(body: &Value) -> Config {
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
fn gemini_config(body: &Value, prefix: &str) -> Config {
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
fn codex_usage_config(body: &Value) -> Config {
    let update = configuration_update_config(body);
    if update.is_set() {
        return update;
    }
    codex_config(body)
}

/// `extractConfigurationUpdateConfig`: the last non-empty effort among the
/// `configuration_update` input items.
fn configuration_update_config(body: &Value) -> Config {
    let Some(Value::Array(input)) = body.get("input") else {
        return Config::unset();
    };
    let effort = input
        .iter()
        .filter(|item| json::str_at(item, "type") == "configuration_update")
        .filter_map(|item| json::string_at(item, "reasoning.effort"))
        .map(json::lower_trim)
        .rfind(|effort| !effort.is_empty());
    effort.map_or_else(Config::unset, effort_config)
}

/// `StripThinkingConfig` for Claude.
fn strip(body: &mut Value) {
    json::delete(body, "thinking");
    json::delete(body, "output_config.effort");
    drop_empty_output_config(body);
}

fn drop_empty_output_config(body: &mut Value) {
    if json::get(body, "output_config")
        .is_some_and(|config| config.as_object().is_some_and(serde_json::Map::is_empty))
    {
        json::delete(body, "output_config");
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

/// `ValidateConfig` for a Claude model with thinking support: converts
/// between budgets and levels as the model needs, clamps to its limits, and
/// rejects what it can't take. `from` is lower-case.
fn validate(
    mut config: Config,
    support: &ThinkingSupport,
    from: &str,
    from_suffix: bool,
) -> Result<Config, ExecError> {
    let capability = capability(support);
    let has_levels = matches!(capability, Capability::LevelOnly | Capability::Hybrid);
    // A level from another provider's request may be clamped to the nearest
    // one the model has; a Claude request's own level must fit.
    let allow_clamp = has_levels && !same_family(from, TARGET);
    let strict_budget = !from_suffix && !from.is_empty() && same_family(from, TARGET);
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

    if config.mode == Mode::None {
        config.budget = 0;
        config.level.clear();
    } else if matches!(config.mode, Mode::Budget | Mode::Auto) {
        config.budget = clamp_budget(config.budget, support);
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
fn clamp_level(level: &str, support: &ThinkingSupport) -> String {
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
fn clamp_budget(value: i64, support: &ThinkingSupport) -> i64 {
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

fn level_supported(level: &str, supported: &[String]) -> bool {
    supported
        .iter()
        .any(|candidate| json::eq_fold(level, candidate.trim()))
}

fn level_index(level: &str) -> Option<usize> {
    LEVEL_ORDER
        .iter()
        .position(|candidate| json::eq_fold(level, candidate))
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
fn apply_known(body: &mut Value, mut config: Config, info: &ModelInfo, support: &ThinkingSupport) {
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
fn normalize_budget(body: &mut Value, budget: i64, info: &ModelInfo, support: &ThinkingSupport) {
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
