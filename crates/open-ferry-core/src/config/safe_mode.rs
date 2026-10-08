// Ported from CLIProxyAPI internal/safemode/example_api_keys.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Safe mode: refusing service while `api-keys` still holds the values from
//! upstream's config template.
//!
//! This module only detects the template keys and renders the warning page.
//! Turning proxy endpoints off is the server's job.
//!
//! Deviations from upstream:
//! - None in this module.

use std::fmt::Write as _;

use super::types::Config;

/// The client keys upstream's `config.example.yaml` ships with.
const EXAMPLE_API_KEYS: [&str; 3] = ["your-api-key-1", "your-api-key-2", "your-api-key-3"];

impl Config {
    /// The template keys left in `api-keys`, trimmed, each once, in the
    /// order they appear.
    pub fn example_api_keys(&self) -> Vec<String> {
        let mut matches: Vec<String> = Vec::new();
        for key in &self.api_keys {
            let trimmed = key.trim();
            if EXAMPLE_API_KEYS.contains(&trimmed) && !matches.iter().any(|seen| seen == trimmed) {
                matches.push(trimmed.to_owned());
            }
        }
        matches
    }

    /// Whether `api-keys` still holds a template key.
    pub fn has_example_api_keys(&self) -> bool {
        !self.example_api_keys().is_empty()
    }
}

/// The page shown instead of the proxy while template keys are configured.
/// `keys` are listed; a non-blank `management_path` adds a link to it.
pub fn example_api_key_warning_page(keys: &[String], management_path: &str) -> String {
    let mut page = String::from(concat!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">"#,
        r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#,
        r#"<title>Example API key detected</title><style>"#,
        r#"body{margin:0;font-family:Arial,sans-serif;background:#f6f8fa;color:#1f2328}"#,
        r#".wrap{max-width:760px;margin:12vh auto;padding:0 24px}"#,
        r#".panel{background:#fff;border:1px solid #d0d7de;border-radius:8px;padding:28px;"#,
        r#"box-shadow:0 8px 24px rgba(140,149,159,.2)}"#,
        r#"h1{margin:0 0 12px;font-size:28px;line-height:1.25}"#,
        r#"p{font-size:16px;line-height:1.55}"#,
        r#"code{background:#f6f8fa;border:1px solid #d0d7de;border-radius:4px;padding:2px 5px}"#,
        r#".keys{margin:16px 0;padding-left:22px}.actions{margin-top:24px}"#,
        r#".button{display:inline-block;border-radius:6px;background:#0969da;color:#fff;"#,
        r#"text-decoration:none;font-weight:600;padding:10px 16px}"#,
        r#".button:hover{background:#0759b8}</style></head><body><main class="wrap">"#,
        r#"<section class="panel"><h1>Example API key detected</h1>"#,
        r#"<p>Proxy API endpoints are disabled because the top-level <code>api-keys</code> "#,
        r#"configuration still contains template values.</p>"#,
    ));
    if !keys.is_empty() {
        page.push_str(r#"<p>Replace these values before using the proxy:</p><ul class="keys">"#);
        for key in keys {
            let _ = write!(page, "<li><code>{}</code></li>", escape_html(key));
        }
        page.push_str("</ul>");
    }
    page.push_str("<p>Set strong random API keys, then retry the proxy endpoint.</p>");
    let trimmed = management_path.trim();
    if !trimmed.is_empty() {
        let _ = write!(
            page,
            r#"<div class="actions"><a class="button" href="{}">Open Management</a></div>"#,
            escape_html(trimmed)
        );
    }
    page.push_str("</section></main></body></html>");
    page
}

/// Go's `html.EscapeString`.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(keys: &[&str]) -> Config {
        Config {
            api_keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn example_api_keys_detects_only_template_values() {
        let config = config(&[
            " real-key ",
            " your-api-key-1 ",
            "your-api-key",
            "change-me",
            "your-api-key-2",
            "your-api-key-2",
            "your-api-key-3",
        ]);
        assert_eq!(
            config.example_api_keys(),
            ["your-api-key-1", "your-api-key-2", "your-api-key-3"]
        );
        assert!(config.has_example_api_keys());
    }

    #[test]
    fn example_api_keys_ignores_similar_values() {
        let config = config(&[
            "your-api-key",
            "change-me",
            "changeme",
            "your-api-key-4",
            "my-your-api-key-1",
        ]);
        assert!(config.example_api_keys().is_empty());
        assert!(!config.has_example_api_keys());
    }

    #[test]
    fn warning_page_includes_management_button() {
        let body = example_api_key_warning_page(
            &["your-api-key-1".to_owned()],
            "/management.html?safe-mode=configure",
        );
        for want in [
            "Example API key detected",
            "your-api-key-1",
            "Open Management",
            r#"href="/management.html?safe-mode=configure""#,
            "Proxy API endpoints are disabled",
        ] {
            assert!(body.contains(want), "missing {want}");
        }
        assert!(!body.contains(r#"class="path""#));
        let plain = example_api_key_warning_page(&[], " ");
        assert!(!plain.contains("Open Management"));
        assert!(!plain.contains("<ul"));
        let escaped = example_api_key_warning_page(&["<a&'\">".to_owned()], "");
        assert!(escaped.contains("<code>&lt;a&amp;&#39;&#34;&gt;</code>"));
    }
}
