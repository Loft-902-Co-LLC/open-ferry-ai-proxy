// Ported from CLIProxyAPI internal/thinking/convert.go, suffix.go and types.go
// (v8.0.10, MIT). https://github.com/router-for-me/CLIProxyAPI

//! Thinking settings: model-name suffixes, and mapping token budgets to named
//! reasoning levels.

pub(crate) const LEVEL_XHIGH: &str = "xhigh";

const THRESHOLD_MINIMAL: i64 = 512;
const THRESHOLD_LOW: i64 = 1024;
const THRESHOLD_MEDIUM: i64 = 8192;
const THRESHOLD_HIGH: i64 = 24576;

/// Converts a thinking token budget into a reasoning level. `-1` means "auto"
/// and `0` means "none"; anything below `-1` is invalid.
pub(crate) fn budget_to_level(budget: i64) -> Option<&'static str> {
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

/// The model name without a thinking suffix such as `(high)` or `(8192)`, as
/// upstream's `ParseSuffix` returns it.
pub(crate) fn base_model_name(model: &str) -> &str {
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
