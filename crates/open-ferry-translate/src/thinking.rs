// Ported from CLIProxyAPI internal/thinking/convert.go, suffix.go, text.go and types.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings: model-name suffixes, mapping between token budgets and
//! named reasoning levels, and whether reasoning summaries are shown.

pub mod summary;

use serde_json::Value;

use crate::go;

pub(crate) const LEVEL_XHIGH: &str = "xhigh";

const THRESHOLD_MINIMAL: i64 = 512;
const THRESHOLD_LOW: i64 = 1024;
const THRESHOLD_MEDIUM: i64 = 8192;
const THRESHOLD_HIGH: i64 = 24576;

/// Converts a thinking token budget into a reasoning level. `-1` means "auto"
/// and `0` means "none"; anything below `-1` is invalid.
pub fn budget_to_level(budget: i64) -> Option<&'static str> {
    Some(match budget {
        ..-1 => return None,
        -1 => "auto",
        0 => "none",
        b if b <= THRESHOLD_MINIMAL => "minimal",
        b if b <= THRESHOLD_LOW => "low",
        b if b <= THRESHOLD_MEDIUM => "medium",
        b if b <= THRESHOLD_HIGH => "high",
        _ => LEVEL_XHIGH,
    })
}

/// `GetThinkingText`: the text of a thinking block. A string `text` comes
/// first; else `thinking`, a string or an object with a string `text` or
/// `thinking` inside. Empty if there is none.
pub(crate) fn thinking_text(part: &Value) -> &str {
    if let Some(Value::String(text)) = part.get("text") {
        return text;
    }
    match part.get("thinking") {
        Some(Value::String(text)) => text,
        Some(inner @ Value::Object(_)) => ["text", "thinking"]
            .into_iter()
            .find_map(|key| inner.get(key).and_then(Value::as_str))
            .unwrap_or_default(),
        _ => "",
    }
}

/// Converts a reasoning level, in any case, into a thinking token budget.
pub fn level_to_budget(level: &str) -> Option<i64> {
    Some(match go::to_lower(level).as_str() {
        "none" => 0,
        "auto" => -1,
        "minimal" => 512,
        "low" => 1024,
        "medium" => 8192,
        "high" => 24576,
        "xhigh" => 32768,
        // Claude's adaptive "max" effort, for models that only take a budget.
        "max" => 128_000,
        _ => return None,
    })
}

/// Reports whether `levels` holds `target`, ignoring case as Go's
/// `strings.EqualFold` does, and surrounding whitespace.
pub fn has_level(levels: &[String], target: &str) -> bool {
    levels
        .iter()
        .any(|level| go::equal_fold(level.trim(), target))
}

/// Maps a reasoning level onto a Claude adaptive thinking effort: `low`,
/// `medium`, `high` or, when the model supports it, `max`.
pub(crate) fn claude_effort(level: &str, supports_max: bool) -> Option<&'static str> {
    Some(match go::to_lower(level.trim()).as_str() {
        "minimal" | "low" => "low",
        "medium" => "medium",
        "high" | "auto" => "high",
        "xhigh" | "max" if supports_max => "max",
        "xhigh" | "max" => "high",
        _ => return None,
    })
}

/// The model name without a thinking suffix such as `(high)` or `(8192)`, as
/// upstream's `ParseSuffix` returns it.
pub fn base_model_name(model: &str) -> &str {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => &model[..open],
        _ => model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_model_name_drops_the_last_suffix() {
        assert_eq!(base_model_name("grok-4(high)"), "grok-4");
        assert_eq!(base_model_name("a(b)(8192)"), "a(b)");
        assert_eq!(base_model_name("grok-4(high"), "grok-4(high");
        assert_eq!(base_model_name("grok-4)"), "grok-4)");
        assert_eq!(base_model_name("()"), "");
    }

    #[test]
    fn levels_map_to_budgets_in_any_case() {
        assert_eq!(level_to_budget("none"), Some(0));
        assert_eq!(level_to_budget("AUTO"), Some(-1));
        assert_eq!(level_to_budget("High"), Some(24576));
        assert_eq!(level_to_budget("max"), Some(128_000));
        assert_eq!(level_to_budget(" high"), None);
        assert_eq!(level_to_budget("banana"), None);
    }

    #[test]
    fn levels_map_to_claude_efforts() {
        assert_eq!(claude_effort(" Minimal ", false), Some("low"));
        assert_eq!(claude_effort("medium", false), Some("medium"));
        assert_eq!(claude_effort("auto", false), Some("high"));
        assert_eq!(claude_effort("xhigh", false), Some("high"));
        assert_eq!(claude_effort("xhigh", true), Some("max"));
        assert_eq!(claude_effort("max", true), Some("max"));
        assert_eq!(claude_effort("none", true), None);
        assert_eq!(claude_effort("", true), None);
        assert!(has_level(&[" MAX ".to_owned()], "max"));
        assert!(!has_level(&["xhigh".to_owned()], "max"));
    }

    #[test]
    fn budgets_map_to_levels_at_threshold_boundaries() {
        assert_eq!(budget_to_level(-2), None);
        assert_eq!(budget_to_level(-1), Some("auto"));
        assert_eq!(budget_to_level(0), Some("none"));
        assert_eq!(budget_to_level(512), Some("minimal"));
        assert_eq!(budget_to_level(513), Some("low"));
        assert_eq!(budget_to_level(1024), Some("low"));
        assert_eq!(budget_to_level(8192), Some("medium"));
        assert_eq!(budget_to_level(24576), Some("high"));
        assert_eq!(budget_to_level(24577), Some("xhigh"));
    }
}
