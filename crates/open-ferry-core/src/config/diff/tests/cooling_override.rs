// Ported from CLIProxyAPI internal/watcher/diff/cooling_override_test.go
// (TestBuildConfigChangeDetailsIncludesAllCoolingOverrides) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The per-key `disable-cooling` overrides.
//!
//! Deviations from upstream: none.

use super::config_with;
use crate::config::diff::build_change_details;
use crate::config::{ClaudeKey, CodexKey, Config, GeminiKey, OpenAiCompatibility, VertexCompatKey};

fn gemini(disable_cooling: Option<bool>) -> Config {
    config_with(|config| {
        config.gemini_api_key = vec![GeminiKey {
            api_key: "gemini-key".to_owned(),
            disable_cooling,
            ..GeminiKey::default()
        }];
    })
}

fn interactions(disable_cooling: Option<bool>) -> Config {
    config_with(|config| {
        config.interactions_api_key = vec![GeminiKey {
            api_key: "interactions-key".to_owned(),
            disable_cooling,
            ..GeminiKey::default()
        }];
    })
}

fn claude(disable_cooling: Option<bool>) -> Config {
    config_with(|config| {
        config.claude_api_key = vec![ClaudeKey {
            api_key: "claude-key".to_owned(),
            disable_cooling,
            ..ClaudeKey::default()
        }];
    })
}

fn codex(disable_cooling: Option<bool>) -> Config {
    config_with(|config| {
        config.codex_api_key = vec![CodexKey {
            api_key: "codex-key".to_owned(),
            disable_cooling,
            ..CodexKey::default()
        }];
    })
}

fn xai(disable_cooling: Option<bool>) -> Config {
    config_with(|config| {
        config.xai_api_key = vec![CodexKey {
            api_key: "xai-key".to_owned(),
            disable_cooling,
            ..CodexKey::default()
        }];
    })
}

fn compat(disable_cooling: Option<bool>) -> Config {
    config_with(|config| {
        config.openai_compatibility = vec![OpenAiCompatibility {
            name: "compat".to_owned(),
            base_url: "https://compat.example.com".to_owned(),
            disable_cooling,
            ..OpenAiCompatibility::default()
        }];
    })
}

fn vertex(disable_cooling: Option<bool>) -> Config {
    config_with(|config| {
        config.vertex_api_key = vec![VertexCompatKey {
            api_key: "vertex-key".to_owned(),
            disable_cooling,
            ..VertexCompatKey::default()
        }];
    })
}

// Ports TestBuildConfigChangeDetailsIncludesAllCoolingOverrides.
#[test]
fn includes_all_cooling_overrides() {
    for (name, old, new, want) in [
        (
            "gemini inherit to false",
            gemini(None),
            gemini(Some(false)),
            "gemini[0].disable-cooling: inherit -> false",
        ),
        (
            "interactions false to true",
            interactions(Some(false)),
            interactions(Some(true)),
            "interactions[0].disable-cooling: false -> true",
        ),
        (
            "claude false to true",
            claude(Some(false)),
            claude(Some(true)),
            "claude[0].disable-cooling: false -> true",
        ),
        (
            "codex true to inherit",
            codex(Some(true)),
            codex(None),
            "codex[0].disable-cooling: true -> inherit",
        ),
        (
            "xai inherit to true",
            xai(None),
            xai(Some(true)),
            "xai[0].disable-cooling: inherit -> true",
        ),
        (
            "openai compatibility false to inherit",
            compat(Some(false)),
            compat(None),
            "disable-cooling false -> inherit",
        ),
        (
            "vertex inherit to false",
            vertex(None),
            vertex(Some(false)),
            "vertex[0].disable-cooling: inherit -> false",
        ),
    ] {
        let changes = build_change_details(&old, &new).join("\n");
        assert!(
            changes.contains(want),
            "{name}: changes missing {want:?}:\n{changes}"
        );
    }
}
