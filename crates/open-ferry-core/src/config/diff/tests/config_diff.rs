// Ported from CLIProxyAPI internal/watcher/diff/config_diff_test.go
// (TestBuildConfigChangeDetailsClientCodexEnableApplyPatch,
// TestBuildConfigChangeDetailsClientCodexOptimizeMultiAgentV2,
// TestBuildConfigChangeDetails, TestBuildConfigChangeDetails_NoChanges,
// TestBuildConfigChangeDetails_GeminiVertexHeaders,
// TestBuildConfigChangeDetails_ModelPrefixes,
// TestBuildConfigChangeDetails_CodexAlphaSearch,
// TestBuildConfigChangeDetails_CodexOrphanDelegationCompatibility,
// TestBuildConfigChangeDetails_SecretsAndCounts,
// TestBuildConfigChangeDetails_RedactsEndpointURLs,
// TestBuildConfigChangeDetails_FlagsAndKeys,
// TestBuildConfigChangeDetails_AllBranches, TestFormatProxyURL,
// TestBuildConfigChangeDetails_RemoteManagementSecretUpdated,
// TestBuildConfigChangeDetails_RemoteManagementBaseURL,
// TestBuildConfigChangeDetails_CountBranches, TestTrimStrings)
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The change details of the settings open-ferry types.
//!
//! Deviations from upstream:
//! - The expectations for settings open-ferry doesn't type are left out:
//!   `codex.disable-codex-cloaking`, `disable-image-generation`,
//!   `claude-code.disable-cloaking-model-list`, `antigravity.*` and
//!   `xai.inject-x-search`.
//! - Dropped: TestBuildConfigChangeDetails_CodexLiveMediaRelay,
//!   TestBuildConfigChangeDetails_CodexKey_DisableCodexCloaking,
//!   TestBuildConfigChangeDetails_XAIKeys and
//!   TestBuildConfigChangeDetails_XAIForceMappingOnly (live media relay,
//!   per-key cloaking and xAI keys aren't typed), and
//!   TestBuildConfigChangeDetails_NilSafe (the configs are references, never
//!   nil).
//! - TestTrimStrings checks the trimmed comparison of `api-keys` it serves,
//!   as there is no separate helper.

use std::collections::BTreeMap;

use super::{config_with, expect_contains, strings};
use crate::config::diff::{build_change_details, format_url};
use crate::config::{
    AnyValue, ClaudeKey, CodexKey, Config, GeminiKey, OpenAiCompatibility,
    OpenAiCompatibilityApiKey, OpenAiCompatibilityModel, PayloadFilterRule, PayloadModelRule,
    PayloadRule, VertexCompatKey, VertexCompatModel,
};

fn gemini(api_key: &str) -> GeminiKey {
    GeminiKey {
        api_key: api_key.to_owned(),
        ..GeminiKey::default()
    }
}

fn claude(api_key: &str) -> ClaudeKey {
    ClaudeKey {
        api_key: api_key.to_owned(),
        ..ClaudeKey::default()
    }
}

fn codex(api_key: &str) -> CodexKey {
    CodexKey {
        api_key: api_key.to_owned(),
        ..CodexKey::default()
    }
}

fn vertex(api_key: &str) -> VertexCompatKey {
    VertexCompatKey {
        api_key: api_key.to_owned(),
        ..VertexCompatKey::default()
    }
}

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn vertex_models(names: &[&str]) -> Vec<VertexCompatModel> {
    names
        .iter()
        .map(|name| VertexCompatModel {
            name: (*name).to_owned(),
            ..VertexCompatModel::default()
        })
        .collect()
}

fn compat(name: &str, keys: &[&str], models: &[&str]) -> OpenAiCompatibility {
    OpenAiCompatibility {
        name: name.to_owned(),
        api_key_entries: keys
            .iter()
            .map(|key| OpenAiCompatibilityApiKey {
                api_key: (*key).to_owned(),
                ..OpenAiCompatibilityApiKey::default()
            })
            .collect(),
        models: models
            .iter()
            .map(|name| OpenAiCompatibilityModel {
                name: (*name).to_owned(),
                ..OpenAiCompatibilityModel::default()
            })
            .collect(),
        ..OpenAiCompatibility::default()
    }
}

// Ports TestBuildConfigChangeDetailsClientCodexEnableApplyPatch.
#[test]
fn client_codex_enable_apply_patch() {
    let old = Config::default();
    let mut new = Config::default();
    new.client.codex.enable_apply_patch = true;
    for (from, to, want) in [
        (&old, &new, "client.codex.enable-apply-patch: false -> true"),
        (&new, &old, "client.codex.enable-apply-patch: true -> false"),
    ] {
        assert_eq!(build_change_details(from, to), [want]);
    }
    assert!(build_change_details(&new, &new).is_empty());
}

// Ports TestBuildConfigChangeDetailsClientCodexOptimizeMultiAgentV2.
#[test]
fn client_codex_optimize_multi_agent_v2() {
    let old = Config::default();
    let mut new = Config::default();
    new.client.codex.optimize_multi_agent_v2 = true;
    for (from, to, want) in [
        (
            &old,
            &new,
            "client.codex.optimize-multi-agent-v2: false -> true",
        ),
        (
            &new,
            &old,
            "client.codex.optimize-multi-agent-v2: true -> false",
        ),
    ] {
        assert_eq!(build_change_details(from, to), [want]);
    }
    assert!(build_change_details(&new, &new).is_empty());
}

// Ports TestBuildConfigChangeDetails.
#[test]
fn build_config_change_details() {
    let old = config_with(|old| {
        old.port = 8080;
        old.auth_dir = "/tmp/auth-old".to_owned();
        old.gemini_api_key = vec![GeminiKey {
            base_url: "http://old".to_owned(),
            excluded_models: strings(&["old-model"]),
            ..gemini("old")
        }];
        old.remote_management.secret_key = "old".to_owned();
        old.remote_management.panel_github_repository = "repo-old".to_owned();
        old.oauth_excluded_models = BTreeMap::from([("providerA".to_owned(), strings(&["m1"]))]);
        old.openai_compatibility = vec![compat("compat-a", &["k1"], &["m1"])];
    });

    let new = config_with(|new| {
        new.port = 9090;
        new.auth_dir = "/tmp/auth-new".to_owned();
        new.gemini_api_key = vec![GeminiKey {
            base_url: "http://old".to_owned(),
            excluded_models: strings(&["old-model", "extra"]),
            ..gemini("old")
        }];
        new.remote_management.allow_remote = true;
        new.remote_management.secret_key = "new".to_owned();
        new.remote_management.disable_control_panel = true;
        new.remote_management.disable_auto_update_panel = true;
        new.remote_management.panel_github_repository = "repo-new".to_owned();
        new.oauth_excluded_models = BTreeMap::from([
            ("providerA".to_owned(), strings(&["m1", "m2"])),
            ("providerB".to_owned(), strings(&["x"])),
        ]);
        new.openai_compatibility = vec![
            compat("compat-a", &["k1"], &["m1", "m2"]),
            compat("compat-b", &["k2"], &[]),
        ];
    });

    let details = build_change_details(&old, &new);
    expect_contains(&details, "port: 8080 -> 9090");
    expect_contains(&details, "auth-dir: /tmp/auth-old -> /tmp/auth-new");
    expect_contains(
        &details,
        "gemini[0].excluded-models: updated (1 -> 2 entries)",
    );
    expect_contains(&details, "remote-management.allow-remote: false -> true");
    expect_contains(
        &details,
        "remote-management.disable-auto-update-panel: false -> true",
    );
    expect_contains(&details, "remote-management.secret-key: updated");
    expect_contains(
        &details,
        "oauth-excluded-models[providera]: updated (1 -> 2 entries)",
    );
    expect_contains(
        &details,
        "oauth-excluded-models[providerb]: added (1 entries)",
    );
    expect_contains(&details, "openai-compatibility:");
    expect_contains(
        &details,
        "  provider added: compat-b (api-keys=1, models=0)",
    );
    expect_contains(&details, "  provider updated: compat-a (models 1 -> 2)");
}

// Ports TestBuildConfigChangeDetails_NoChanges.
#[test]
fn no_changes() {
    let config = config_with(|config| {
        config.port = 8080;
    });
    assert!(build_change_details(&config, &config).is_empty());
}

// Ports TestBuildConfigChangeDetails_GeminiVertexHeaders.
#[test]
fn gemini_vertex_headers() {
    let old = config_with(|old| {
        old.gemini_api_key = vec![GeminiKey {
            headers: headers(&[("H", "1")]),
            excluded_models: strings(&["a"]),
            ..gemini("g1")
        }];
        old.vertex_api_key = vec![VertexCompatKey {
            base_url: "http://v-old".to_owned(),
            models: vertex_models(&["m1"]),
            ..vertex("v1")
        }];
    });
    let new = config_with(|new| {
        new.gemini_api_key = vec![GeminiKey {
            headers: headers(&[("H", "2")]),
            excluded_models: strings(&["a", "b"]),
            ..gemini("g1")
        }];
        new.vertex_api_key = vec![VertexCompatKey {
            base_url: "http://v-new".to_owned(),
            models: vertex_models(&["m1", "m2"]),
            ..vertex("v1")
        }];
    });

    let details = build_change_details(&old, &new);
    expect_contains(&details, "gemini[0].headers: updated");
    expect_contains(
        &details,
        "gemini[0].excluded-models: updated (1 -> 2 entries)",
    );
}

// Ports TestBuildConfigChangeDetails_ModelPrefixes.
#[test]
fn model_prefixes() {
    let configs = ["old", "new"].map(|age| {
        config_with(|config| {
            config.gemini_api_key = vec![GeminiKey {
                prefix: format!("{age}-g"),
                base_url: "http://g".to_owned(),
                proxy_url: "http://gp".to_owned(),
                ..gemini("g1")
            }];
            config.claude_api_key = vec![ClaudeKey {
                prefix: format!("{age}-c"),
                base_url: "http://c".to_owned(),
                proxy_url: "http://cp".to_owned(),
                ..claude("c1")
            }];
            config.codex_api_key = vec![CodexKey {
                prefix: format!("{age}-x"),
                base_url: "http://x".to_owned(),
                proxy_url: "http://xp".to_owned(),
                ..codex("x1")
            }];
            config.vertex_api_key = vec![VertexCompatKey {
                prefix: format!("{age}-v"),
                base_url: "http://v".to_owned(),
                proxy_url: "http://vp".to_owned(),
                ..vertex("v1")
            }];
        })
    });

    let changes = build_change_details(&configs[0], &configs[1]);
    expect_contains(&changes, "gemini[0].prefix: old-g -> new-g");
    expect_contains(&changes, "claude[0].prefix: old-c -> new-c");
    expect_contains(&changes, "codex[0].prefix: old-x -> new-x");
    expect_contains(&changes, "vertex[0].prefix: old-v -> new-v");
}

// Ports TestBuildConfigChangeDetails_CodexAlphaSearch.
#[test]
fn codex_alpha_search() {
    let key = CodexKey {
        base_url: "https://codex.example.com".to_owned(),
        ..codex("key")
    };
    let old = config_with(|old| {
        old.codex_api_key = vec![key.clone()];
    });
    let new = config_with(|new| {
        new.codex_api_key = vec![CodexKey {
            alpha_search: true,
            ..key
        }];
    });

    let changes = build_change_details(&old, &new);
    expect_contains(&changes, "codex[0].alpha-search: false -> true");
}

// Ports TestBuildConfigChangeDetails_CodexOrphanDelegationCompatibility.
#[test]
fn codex_orphan_delegation_compatibility() {
    let old = Config::default();
    let mut new = Config::default();
    new.codex.orphan_delegation_compatibility = true;

    let changes = build_change_details(&old, &new);
    expect_contains(
        &changes,
        "codex.orphan-delegation-compatibility: false -> true",
    );
}

// Ports TestBuildConfigChangeDetails_SecretsAndCounts.
#[test]
fn secrets_and_counts() {
    let old = config_with(|old| {
        old.api_keys = strings(&["a"]);
    });
    let new = config_with(|new| {
        new.api_keys = strings(&["a", "b", "c"]);
        new.remote_management.secret_key = "new-secret".to_owned();
    });

    let details = build_change_details(&old, &new);
    expect_contains(&details, "api-keys count: 1 -> 3");
    expect_contains(&details, "remote-management.secret-key: created");
}

// Ports TestBuildConfigChangeDetails_RedactsEndpointURLs.
#[test]
fn redacts_endpoint_urls() {
    let configs = ["old", "new"].map(|age| {
        config_with(|config| {
            config.gemini_api_key = vec![GeminiKey {
                base_url: format!(
                    "https://{age}-user:{age}-pass@{age}.example/v1?token={age}-token"
                ),
                ..GeminiKey::default()
            }];
            config.remote_management.panel_github_repository = format!(
                "https://{age}-user:{age}-pass@{age}-panel.example/private?token={age}-token"
            );
            config.openai_compatibility = vec![OpenAiCompatibility {
                base_url: format!(
                    "https://{age}-user:{age}-pass@{age}-compat.example/v1?token={age}-token"
                ),
                ..OpenAiCompatibility::default()
            }];
        })
    });

    let details = build_change_details(&configs[0], &configs[1]);
    expect_contains(
        &details,
        "gemini[0].base-url: https://old.example -> https://new.example",
    );
    expect_contains(
        &details,
        "remote-management.panel-github-repository: https://old-panel.example -> https://new-panel.example",
    );
    let joined = details.join("\n");
    for sensitive in [
        "old-user",
        "new-user",
        "old-pass",
        "new-pass",
        "old-token",
        "new-token",
        "/private",
        "/v1",
    ] {
        assert!(
            !joined.contains(sensitive),
            "leaked {sensitive:?}: {joined}"
        );
    }
}

// Ports TestBuildConfigChangeDetails_FlagsAndKeys.
#[test]
fn flags_and_keys() {
    let old = config_with(|old| {
        old.port = 1000;
        old.auth_dir = "/old".to_owned();
        old.request_retry = 1;
        old.max_retry_credentials = 1;
        old.max_retry_interval = 1;
        old.ws_auth = false;
        old.claude_api_key = vec![claude("c1")];
        old.codex_api_key = vec![codex("x1")];
        old.remote_management.panel_github_repository = "old/repo".to_owned();
        old.remote_management.secret_key = "keep".to_owned();
        old.proxy_url = "http://old-proxy".to_owned();
        old.api_keys = strings(&["key-1"]);
    });

    let new = config_with(|new| {
        new.port = 2000;
        new.auth_dir = "/new".to_owned();
        new.debug = true;
        new.logging_to_file = true;
        new.usage_statistics_enabled = true;
        new.disable_cooling = true;
        new.save_cooldown_status = true;
        new.transient_error_cooldown_seconds = -1;
        new.request_retry = 2;
        new.max_retry_credentials = 3;
        new.max_retry_interval = 3;
        new.ws_auth = true;
        new.quota_exceeded.switch_project = true;
        new.quota_exceeded.switch_preview_model = true;
        new.quota_exceeded.antigravity_credits = true;
        new.claude_api_key = vec![
            ClaudeKey {
                base_url: "http://new".to_owned(),
                proxy_url: "http://p".to_owned(),
                headers: headers(&[("H", "1")]),
                excluded_models: strings(&["a"]),
                ..claude("c1")
            },
            claude("c2"),
        ];
        new.codex_api_key = vec![
            CodexKey {
                base_url: "http://x".to_owned(),
                proxy_url: "http://px".to_owned(),
                headers: headers(&[("H", "2")]),
                excluded_models: strings(&["b"]),
                ..codex("x1")
            },
            codex("x2"),
        ];
        new.remote_management.disable_control_panel = true;
        new.remote_management.disable_auto_update_panel = true;
        new.remote_management.panel_github_repository = "new/repo".to_owned();
        new.request_log = true;
        new.proxy_url = "http://new-proxy".to_owned();
        new.api_keys = strings(&[" key-1 ", "key-2"]);
        new.force_model_prefix = true;
        new.nonstream_keepalive_interval = 5;
    });

    let details = build_change_details(&old, &new);
    for want in [
        "debug: false -> true",
        "logging-to-file: false -> true",
        "usage-statistics-enabled: false -> true",
        "disable-cooling: false -> true",
        "save-cooldown-status: false -> true",
        "transient-error-cooldown-seconds: 0 -> -1",
        "request-log: false -> true",
        "request-retry: 1 -> 2",
        "max-retry-credentials: 1 -> 3",
        "max-retry-interval: 1 -> 3",
        "proxy-url: http://old-proxy -> http://new-proxy",
        "ws-auth: false -> true",
        "force-model-prefix: false -> true",
        "nonstream-keepalive-interval: 0 -> 5",
        "quota-exceeded.switch-project: false -> true",
        "quota-exceeded.switch-preview-model: false -> true",
        "quota-exceeded.antigravity-credits: false -> true",
        "api-keys count: 1 -> 2",
        "claude-api-key count: 1 -> 2",
        "codex-api-key count: 1 -> 2",
        "remote-management.disable-control-panel: false -> true",
        "remote-management.disable-auto-update-panel: false -> true",
        "remote-management.panel-github-repository: old -> new",
        "remote-management.secret-key: deleted",
    ] {
        expect_contains(&details, want);
    }
}

// Ports TestBuildConfigChangeDetails_AllBranches.
#[test]
fn all_branches() {
    let old = config_with(|old| {
        old.port = 1;
        old.auth_dir = "/a".to_owned();
        old.request_retry = 1;
        old.max_retry_credentials = 1;
        old.max_retry_interval = 1;
        old.ws_auth = false;
        old.gemini_api_key = vec![GeminiKey {
            base_url: "http://g-old".to_owned(),
            proxy_url: "http://gp-old".to_owned(),
            headers: headers(&[("A", "1")]),
            ..gemini("g-old")
        }];
        old.claude_api_key = vec![ClaudeKey {
            base_url: "http://c-old".to_owned(),
            proxy_url: "http://cp-old".to_owned(),
            headers: headers(&[("H", "1")]),
            excluded_models: strings(&["x"]),
            ..claude("c-old")
        }];
        old.codex_api_key = vec![CodexKey {
            base_url: "http://x-old".to_owned(),
            proxy_url: "http://xp-old".to_owned(),
            headers: headers(&[("H", "1")]),
            excluded_models: strings(&["x"]),
            ..codex("x-old")
        }];
        old.vertex_api_key = vec![VertexCompatKey {
            base_url: "http://v-old".to_owned(),
            proxy_url: "http://vp-old".to_owned(),
            headers: headers(&[("H", "1")]),
            models: vertex_models(&["m1"]),
            ..vertex("v-old")
        }];
        old.remote_management.panel_github_repository = "old/repo".to_owned();
        old.remote_management.secret_key = "old".to_owned();
        old.proxy_url = "http://old-proxy".to_owned();
        old.api_keys = strings(&[" keyA "]);
        old.oauth_excluded_models = BTreeMap::from([("p1".to_owned(), strings(&["a"]))]);
        old.openai_compatibility = vec![compat("prov-old", &["k1"], &["m1"])];
    });

    let new = config_with(|new| {
        new.port = 2;
        new.auth_dir = "/b".to_owned();
        new.debug = true;
        new.logging_to_file = true;
        new.usage_statistics_enabled = true;
        new.disable_cooling = true;
        new.save_cooldown_status = true;
        new.transient_error_cooldown_seconds = -1;
        new.request_retry = 2;
        new.max_retry_credentials = 3;
        new.max_retry_interval = 3;
        new.ws_auth = true;
        new.quota_exceeded.switch_project = true;
        new.quota_exceeded.switch_preview_model = true;
        new.quota_exceeded.antigravity_credits = true;
        new.gemini_api_key = vec![GeminiKey {
            base_url: "http://g-new".to_owned(),
            proxy_url: "http://gp-new".to_owned(),
            headers: headers(&[("A", "2")]),
            excluded_models: strings(&["x", "y"]),
            ..gemini("g-new")
        }];
        new.claude_api_key = vec![ClaudeKey {
            base_url: "http://c-new".to_owned(),
            proxy_url: "http://cp-new".to_owned(),
            headers: headers(&[("H", "2")]),
            excluded_models: strings(&["x", "y"]),
            ..claude("c-new")
        }];
        new.codex_api_key = vec![CodexKey {
            base_url: "http://x-new".to_owned(),
            proxy_url: "http://xp-new".to_owned(),
            headers: headers(&[("H", "2")]),
            excluded_models: strings(&["x", "y"]),
            ..codex("x-new")
        }];
        new.vertex_api_key = vec![VertexCompatKey {
            base_url: "http://v-new".to_owned(),
            proxy_url: "http://vp-new".to_owned(),
            headers: headers(&[("H", "2")]),
            models: vertex_models(&["m1", "m2"]),
            ..vertex("v-new")
        }];
        new.remote_management.allow_remote = true;
        new.remote_management.disable_control_panel = true;
        new.remote_management.disable_auto_update_panel = true;
        new.remote_management.panel_github_repository = "new/repo".to_owned();
        new.request_log = true;
        new.proxy_url = "http://new-proxy".to_owned();
        new.api_keys = strings(&["keyB"]);
        new.oauth_excluded_models = BTreeMap::from([
            ("p1".to_owned(), strings(&["b", "c"])),
            ("p2".to_owned(), strings(&["d"])),
        ]);
        new.openai_compatibility = vec![
            compat("prov-old", &["k1", "k2"], &["m1", "m2"]),
            compat("prov-new", &["k3"], &[]),
        ];
    });

    let changes = build_change_details(&old, &new);
    for want in [
        "port: 1 -> 2",
        "auth-dir: /a -> /b",
        "debug: false -> true",
        "logging-to-file: false -> true",
        "usage-statistics-enabled: false -> true",
        "disable-cooling: false -> true",
        "save-cooldown-status: false -> true",
        "transient-error-cooldown-seconds: 0 -> -1",
        "request-retry: 1 -> 2",
        "max-retry-credentials: 1 -> 3",
        "max-retry-interval: 1 -> 3",
        "proxy-url: http://old-proxy -> http://new-proxy",
        "ws-auth: false -> true",
        "quota-exceeded.switch-project: false -> true",
        "quota-exceeded.switch-preview-model: false -> true",
        "quota-exceeded.antigravity-credits: false -> true",
        "api-keys: values updated (count unchanged, redacted)",
        "gemini[0].base-url: http://g-old -> http://g-new",
        "gemini[0].proxy-url: http://gp-old -> http://gp-new",
        "gemini[0].api-key: updated",
        "gemini[0].headers: updated",
        "gemini[0].excluded-models: updated (0 -> 2 entries)",
        "claude[0].base-url: http://c-old -> http://c-new",
        "claude[0].proxy-url: http://cp-old -> http://cp-new",
        "claude[0].api-key: updated",
        "claude[0].headers: updated",
        "claude[0].excluded-models: updated (1 -> 2 entries)",
        "codex[0].base-url: http://x-old -> http://x-new",
        "codex[0].proxy-url: http://xp-old -> http://xp-new",
        "codex[0].api-key: updated",
        "codex[0].headers: updated",
        "codex[0].excluded-models: updated (1 -> 2 entries)",
        "vertex[0].base-url: http://v-old -> http://v-new",
        "vertex[0].proxy-url: http://vp-old -> http://vp-new",
        "vertex[0].api-key: updated",
        "vertex[0].models: updated (1 -> 2 entries)",
        "vertex[0].headers: updated",
        "oauth-excluded-models[p1]: updated (1 -> 2 entries)",
        "oauth-excluded-models[p2]: added (1 entries)",
        "remote-management.allow-remote: false -> true",
        "remote-management.disable-control-panel: false -> true",
        "remote-management.disable-auto-update-panel: false -> true",
        "remote-management.panel-github-repository: old -> new",
        "remote-management.secret-key: deleted",
        "openai-compatibility:",
    ] {
        expect_contains(&changes, want);
    }
}

// Ports TestFormatProxyURL.
#[test]
fn format_proxy_url() {
    for (name, input, want) in [
        ("empty", "", "<none>"),
        ("invalid", "http://[::1", "<redacted>"),
        (
            "fullURLRedactsUserinfoAndPath",
            "http://user:pass@example.com:8080/path?x=1#frag",
            "http://example.com:8080",
        ),
        (
            "socks5RedactsUserinfoAndPath",
            "socks5://user:pass@192.168.1.1:1080/path?x=1",
            "socks5://192.168.1.1:1080",
        ),
        (
            "socks5HostPort",
            "socks5://proxy.example.com:1080/",
            "socks5://proxy.example.com:1080",
        ),
        (
            "hostPortNoScheme",
            "example.com:1234/path?x=1",
            "example.com:1234",
        ),
        ("relativePathRedacted", "/just/path", "<redacted>"),
        (
            "schemeAndHost",
            "https://example.com",
            "https://example.com",
        ),
    ] {
        assert_eq!(format_url(input), want, "{name}");
    }
}

// Ports TestBuildConfigChangeDetails_RemoteManagementSecretUpdated.
#[test]
fn remote_management_secret_updated() {
    let mut old = Config::default();
    old.remote_management.secret_key = "old".to_owned();
    let mut new = Config::default();
    new.remote_management.secret_key = "new".to_owned();

    let changes = build_change_details(&old, &new);
    expect_contains(&changes, "remote-management.secret-key: updated");
}

// Ports TestBuildConfigChangeDetails_RemoteManagementBaseURL.
#[test]
fn remote_management_base_url() {
    let mut old = Config::default();
    old.remote_management.base_url = "https://old.example.com".to_owned();
    let mut new = Config::default();
    new.remote_management.base_url = "https://new.example.com".to_owned();

    let changes = build_change_details(&old, &new);
    expect_contains(
        &changes,
        "remote-management.base-url: https://old.example.com -> https://new.example.com",
    );
}

// Ports TestBuildConfigChangeDetails_CountBranches, without its xAI key.
#[test]
fn count_branches() {
    let old = Config::default();
    let new = config_with(|new| {
        new.gemini_api_key = vec![gemini("g")];
        new.claude_api_key = vec![claude("c")];
        new.codex_api_key = vec![codex("c")];
        new.vertex_api_key = vec![VertexCompatKey {
            base_url: "http://v".to_owned(),
            ..vertex("v")
        }];
    });

    let changes = build_change_details(&old, &new);
    expect_contains(&changes, "gemini-api-key count: 0 -> 1");
    expect_contains(&changes, "claude-api-key count: 0 -> 1");
    expect_contains(&changes, "codex-api-key count: 0 -> 1");
    expect_contains(&changes, "vertex-api-key count: 0 -> 1");
}

// Ports TestTrimStrings, through the comparison of `api-keys` it serves.
#[test]
fn trim_strings() {
    let old = config_with(|old| {
        old.api_keys = strings(&[" a ", "b", "  c"]);
    });
    let mut new = config_with(|new| {
        new.api_keys = strings(&["a", "b", "c"]);
    });
    assert!(build_change_details(&old, &new).is_empty());
    new.api_keys = strings(&["a", "b", "d"]);
    assert_eq!(
        build_change_details(&old, &new),
        ["api-keys: values updated (count unchanged, redacted)"]
    );
}

// Not upstream's: the payload sections count their rules, and a rule's
// params are compared as the map they are upstream, in any order.
#[test]
fn payload_sections() {
    let rule = |params: &[(&str, i64)]| PayloadRule {
        models: vec![PayloadModelRule {
            name: "gpt-*".to_owned(),
            ..PayloadModelRule::default()
        }],
        params: params
            .iter()
            .map(|(path, value)| ((*path).to_owned(), AnyValue::Int(*value)))
            .collect(),
    };
    let mut old = Config::default();
    old.payload.default = vec![rule(&[("a", 1), ("b", 2)])];
    old.payload.override_raw = vec![rule(&[("a", 1)])];
    let mut new = old.clone();
    new.payload.default = vec![rule(&[("b", 2), ("a", 1)])];
    assert!(build_change_details(&old, &new).is_empty());

    new.payload.default = vec![rule(&[("b", 2), ("a", 3)])];
    new.payload.default_raw = vec![rule(&[("c", 1)])];
    new.payload.r#override = vec![rule(&[]), rule(&[])];
    new.payload.override_raw = Vec::new();
    new.payload.filter = vec![PayloadFilterRule {
        params: strings(&["x"]),
        ..PayloadFilterRule::default()
    }];
    assert_eq!(
        build_change_details(&old, &new),
        [
            "payload.default: updated (1 -> 1 rules)",
            "payload.default-raw: updated (0 -> 1 rules)",
            "payload.override: updated (0 -> 2 rules)",
            "payload.override-raw: updated (1 -> 0 rules)",
            "payload.filter: updated (0 -> 1 rules)",
        ]
    );
}

// Not upstream's: no line shows an API key, the management key, header
// values or a URL's user information, path or query.
#[test]
fn secrets_never_show() {
    let configs = ["old", "new"].map(|age| {
        config_with(|config| {
            config.api_keys = vec![format!("sk-{age}-client")];
            config.proxy_url = format!("socks5://{age}-user:{age}-pass@proxy.example:1080");
            config.remote_management.secret_key = format!("{age}-management");
            config.gemini_api_key = vec![GeminiKey {
                proxy_url: format!("http://{age}-user:{age}-pass@gp.example/{age}-path"),
                headers: headers(&[("Authorization", &format!("Bearer {age}-header"))]),
                ..gemini(&format!("sk-{age}-gemini"))
            }];
            config.openai_compatibility = vec![OpenAiCompatibility {
                headers: headers(&[("X-Key", &format!("{age}-header"))]),
                ..compat("compat", &[&format!("sk-{age}-compat")], &[])
            }];
        })
    });

    let joined = build_change_details(&configs[0], &configs[1]).join("\n");
    assert!(joined.contains("api-keys: values updated"), "{joined}");
    for age in ["old", "new"] {
        for secret in [
            "-client",
            "-gemini",
            "-compat",
            "-management",
            "-header",
            "-user",
            "-pass",
            "-path",
        ] {
            let secret = format!("{age}{secret}");
            assert!(!joined.contains(&secret), "leaked {secret:?}: {joined}");
        }
    }
}
