// Ported from CLIProxyAPI internal/translator/gemini/common/safety.go (DefaultSafetySettings
// and AttachDefaultSafetySettings) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers shared by the translators to a Gemini upstream.
//!
//! Deviations from upstream: none.

use serde_json::Value;

use crate::json::{object, path, set_path};

/// The safety settings upstream adds to every Gemini request that has none:
/// each category and its threshold.
const DEFAULT_SAFETY_SETTINGS: [(&str, &str); 5] = [
    ("HARM_CATEGORY_HARASSMENT", "OFF"),
    ("HARM_CATEGORY_HATE_SPEECH", "OFF"),
    ("HARM_CATEGORY_SEXUALLY_EXPLICIT", "OFF"),
    ("HARM_CATEGORY_DANGEROUS_CONTENT", "OFF"),
    ("HARM_CATEGORY_CIVIC_INTEGRITY", "BLOCK_NONE"),
];

/// The default safety settings, as a Gemini `safetySettings` array.
fn default_safety_settings() -> Value {
    Value::Array(
        DEFAULT_SAFETY_SETTINGS
            .iter()
            .map(|&(category, threshold)| {
                object([
                    ("category", Value::from(category)),
                    ("threshold", Value::from(threshold)),
                ])
            })
            .collect(),
    )
}

/// Sets the default safety settings at `at`, a dotted path such as
/// `safetySettings` or `request.safetySettings`, unless something is there
/// already, even `null`.
pub(crate) fn attach_default_safety_settings(request: &mut Value, at: &str) {
    if path(request, at).is_none() {
        set_path(request, at, default_safety_settings());
    }
}

#[cfg(test)]
mod tests {
    //! Upstream has no tests of safety.go.

    use serde_json::json;

    use super::*;

    #[test]
    fn attaches_defaults_when_missing() {
        let mut request = json!({"contents":[]});
        attach_default_safety_settings(&mut request, "safetySettings");
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            concat!(
                r#"{"contents":[],"safetySettings":["#,
                r#"{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},"#,
                r#"{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},"#,
                r#"{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},"#,
                r#"{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},"#,
                r#"{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]}"#
            )
        );
    }

    #[test]
    fn keeps_existing_settings_even_null() {
        for existing in [json!(null), json!([]), json!("x")] {
            let mut request = json!({"safetySettings": existing.clone()});
            attach_default_safety_settings(&mut request, "safetySettings");
            assert_eq!(request, json!({"safetySettings": existing}));
        }
    }

    #[test]
    fn follows_dotted_paths() {
        let mut request = json!({"request":{}});
        attach_default_safety_settings(&mut request, "request.safetySettings");
        assert_eq!(
            request["request"]["safetySettings"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
    }
}
