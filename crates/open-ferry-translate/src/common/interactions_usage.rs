// Ported from CLIProxyAPI internal/translator/common/interactions_usage.go
// (InteractionsUsage) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Finding the usage in a Gemini Interactions response or stream event, for
//! the translators that turn Interactions answers into other formats.
//!
//! Deviations from upstream: none.

use serde_json::Value;

use crate::json::path;

/// Where an Interactions response or event may keep its usage, in the order
/// they are tried.
const USAGE_PATHS: &[&str] = &[
    "interaction.usage",
    "usage",
    "metadata.total_usage",
    "metadata.usage",
    "interaction.metadata.total_usage",
    "interaction.metadata.usage",
];

/// `InteractionsUsage`: the first of [`USAGE_PATHS`] that `root` has, whatever
/// its value, even `null`; `None` if it has none of them.
pub(crate) fn interactions_usage(root: &Value) -> Option<&Value> {
    USAGE_PATHS.iter().find_map(|usage| path(root, usage))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Not upstream's: upstream has no test of InteractionsUsage of its own.
    #[test]
    fn the_first_path_found_wins() {
        let usage = json!({ "total_tokens": 3 });
        for root in [
            json!({ "interaction": { "usage": usage }, "usage": 1 }),
            json!({ "usage": usage, "metadata": { "total_usage": 1 } }),
            json!({ "metadata": { "total_usage": usage, "usage": 1 } }),
            json!({ "metadata": { "usage": usage }, "interaction": { "metadata": { "usage": 1 } } }),
            json!({ "interaction": { "metadata": { "total_usage": usage, "usage": 1 } } }),
            json!({ "interaction": { "metadata": { "usage": usage } } }),
        ] {
            assert_eq!(interactions_usage(&root), Some(&usage), "{root}");
        }
    }

    // Not upstream's: gjson's Exists is true for a `null` value, and false
    // for a path through something that isn't an object (checked with Go).
    #[test]
    fn null_counts_and_non_objects_do_not() {
        assert_eq!(
            interactions_usage(&json!({ "interaction": { "usage": null }, "usage": 1 })),
            Some(&Value::Null)
        );
        for root in [
            json!({}),
            json!({ "interaction": [{ "usage": 1 }], "metadata": "usage" }),
            json!([{ "usage": 1 }]),
            json!("usage"),
        ] {
            assert_eq!(interactions_usage(&root), None, "{root}");
        }
    }
}
