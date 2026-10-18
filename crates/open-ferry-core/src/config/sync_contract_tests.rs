// Ported from CLIProxyAPI internal/config/claude_code_test.go,
// claude_header_defaults_test.go, tls_handshake_timeout_test.go,
// internal/watcher/diff/config_diff_test.go and the configuration contracts
// in internal/config/config_types.go, sdk_config.go and vertex_compat.go
// (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::testing::TempDir;
use super::*;

const LEGACY: &str = r#"
max-request-body-bytes: 1048576
routing:
  session-affinity-rebind-on: quota
streaming:
  keepalive-seconds: 15
  send-keepalive-before-ft: true
claude:
  tool-search: true
claude-api-key:
  - api-key: test-claude
    base-url: https://gateway.example.com
    native-tool-search: true
vertex-api-key:
  - api-key: test-vertex
    interactions: true
"#;

const V8: &str = r#"
config-version: 8
server:
  max-request-body-bytes: 1048576
routing:
  session-affinity-rebind-on: quota
requests:
  streaming:
    keepalive-seconds: 15
    send-keepalive-before-ft: true
upstream:
  claude:
    tool-search: true
api-keys:
  claude:
    - base-url: https://gateway.example.com
      keys:
        - api-key: test-claude
          native-tool-search: true
  vertex:
    - keys:
        - api-key: test-vertex
          interactions: true
"#;

fn assert_round_trip(path: &std::path::Path, original: &Config) {
    let mut loaded = Config::load(path).expect("reload");
    let mut original = original.clone();
    // Saving migrates ws-auth into oauth.providers, which recomputes this
    // pre-existing layout metadata without changing the config values.
    loaded.oauth_only_fields.clear();
    original.oauth_only_fields.clear();
    assert_eq!(loaded, original);
}

#[test]
fn new_keys_parse_in_both_layouts_and_round_trip_on_save() {
    for raw in [LEGACY, V8] {
        let config = Config::parse(raw.as_bytes()).expect("parse");
        assert_eq!(config.max_request_body_bytes, 1048576);
        assert_eq!(config.routing.session_affinity_rebind_on, "quota");
        assert_eq!(config.routing.session_affinity_rebind_on(), "quota");
        assert!(config.streaming.send_keepalive_before_ft);
        assert!(config.claude.tool_search);
        assert!(config.claude_api_key[0].native_tool_search);
        assert_eq!(config.vertex_api_key[0].interactions, Some(true));
        for migrate in [false, true] {
            let dir = TempDir::new();
            let path = dir.write("config.yaml", raw);
            save::save_preserving_comments(&path, &config, migrate).expect("save");
            assert_round_trip(&path, &config);
            save::save_preserving_comments(&path, &config, migrate).expect("save again");
            assert_round_trip(&path, &config);
        }
    }
}

#[test]
fn new_keys_default_to_disabled_and_rebind_policy_defaults_to_any() {
    let config =
        Config::parse(b"claude-api-key: [{api-key: test}]\nvertex-api-key: [{api-key: test}]\n")
            .expect("parse");
    assert_eq!(config.max_request_body_bytes, 0);
    assert_eq!(config.routing.session_affinity_rebind_on(), "any");
    // Upstream's string is unset in the typed config; its runtime default is any.
    assert!(config.routing.session_affinity_rebind_on.is_empty());
    assert!(!config.streaming.send_keepalive_before_ft);
    assert!(!config.claude.tool_search);
    assert!(!config.claude_api_key[0].native_tool_search);
    assert_eq!(config.vertex_api_key[0].interactions, None);
    for raw in ["", "any", "unknown", " QuOtA "] {
        let yaml = format!("routing: {{session-affinity-rebind-on: '{raw}'}}\n");
        let config = Config::parse(yaml.as_bytes()).expect("parse");
        assert_eq!(config.routing.session_affinity_rebind_on, raw);
        assert_eq!(
            config.routing.session_affinity_rebind_on(),
            if raw == " QuOtA " { "quota" } else { "any" }
        );
    }
}

#[test]
fn body_limit_accepts_int64_values_and_reports_its_upstream_type() {
    for value in [i64::MIN, i64::MAX] {
        let yaml = format!("max-request-body-bytes: {value}\n");
        assert_eq!(
            Config::parse(yaml.as_bytes())
                .expect("int64")
                .max_request_body_bytes,
            value
        );
    }
    let error =
        Config::parse(b"max-request-body-bytes: {}\n").expect_err("mapping is not an int64");
    assert!(
        error
            .to_string()
            .contains("cannot unmarshal !!map into int64"),
        "{error}"
    );
}

#[test]
fn explicit_false_and_negative_body_limit_survive_saves() {
    let raw = LEGACY
        .replace("true", "false")
        .replace("1048576", "-1")
        .replace("quota", "unknown");
    let config = Config::parse(raw.as_bytes()).expect("parse");
    assert_eq!(config.max_request_body_bytes, -1);
    assert_eq!(config.vertex_api_key[0].interactions, Some(false));
    assert_eq!(config.routing.session_affinity_rebind_on(), "any");
    for migrate in [false, true] {
        let dir = TempDir::new();
        let path = dir.write("config.yaml", &raw);
        save::save_preserving_comments(&path, &config, migrate).expect("save");
        assert_round_trip(&path, &config);
    }
}

#[test]
fn saves_insert_new_keys_including_an_explicit_false_vertex_switch() {
    let raw = "claude-api-key: [{api-key: test}]\nvertex-api-key: [{api-key: test}]\n";
    let mut config = Config::parse(raw.as_bytes()).expect("parse");
    config.max_request_body_bytes = 1048576;
    config.routing.session_affinity_rebind_on = "quota".into();
    config.streaming.send_keepalive_before_ft = true;
    config.claude.tool_search = true;
    config.claude_api_key[0].native_tool_search = true;
    config.vertex_api_key[0].interactions = Some(false);
    for migrate in [false, true] {
        let dir = TempDir::new();
        let path = dir.write("config.yaml", raw);
        save::save_preserving_comments(&path, &config, migrate).expect("save");
        assert_round_trip(&path, &config);
    }
}

// Includes upstream's TestParseConfigBytesClaudeToolSearch cases.
#[test]
fn claude_tool_search_loads_in_legacy_and_v8_layouts() {
    for (raw, want) in [
        ("port: 8317\n", false),
        ("claude:\n  tool-search: true\n", true),
        (
            "config-version: 8\nupstream:\n  claude:\n    tool-search: true\n",
            true,
        ),
        (
            "config-version: 8\nupstream:\n  claude:\n    tool-search: false\n",
            false,
        ),
    ] {
        assert_eq!(
            Config::parse(raw.as_bytes())
                .expect("parse")
                .claude
                .tool_search,
            want
        );
    }
}

// Includes upstream's TestBuildConfigChangeDetails_ClaudeNativeToolSearch.
#[test]
fn reload_details_detect_the_new_config_keys() {
    let before = Config::parse(
        LEGACY
            .replace("true", "false")
            .replace("1048576", "0")
            .replace("quota", "any")
            .as_bytes(),
    )
    .expect("before");
    let after = Config::parse(LEGACY.as_bytes()).expect("after");
    let changes = diff::build_change_details(&before, &after);
    for want in [
        "max-request-body-bytes: 0 -> 1048576",
        "routing.session-affinity-rebind-on: any -> quota",
        "streaming.send-keepalive-before-ft: false -> true",
        "claude.tool-search: false -> true",
        "claude[0].native-tool-search: false -> true",
        "vertex[0].interactions: false -> true",
    ] {
        assert!(
            changes.iter().any(|change| change == want),
            "{want}: {changes:?}"
        );
    }
}

#[test]
fn v8_edits_accept_every_new_key_and_list_it_in_the_schema() {
    let edit = v8_edit::V8Edit {
        method: v8_edit::V8Method::Put,
        path: Vec::new(),
        yaml: true,
        body: V8.as_bytes().to_vec(),
    };
    let out = v8_edit::preview_v8(b"config-version: 8\n", &edit).expect("edit");
    assert_eq!(
        Config::parse(&out).expect("edited"),
        Config::parse(V8.as_bytes()).expect("original")
    );
    let known = v8_edit::known_v8_paths();
    for path in [
        "server.max-request-body-bytes",
        "routing.session-affinity-rebind-on",
        "requests.streaming.send-keepalive-before-ft",
        "upstream.claude.tool-search",
    ] {
        assert!(known.iter().any(|entry| entry.path == path), "{path}");
    }
    for (path, body) in [
        ("server/max-request-body-bytes", "-1"),
        ("routing/session-affinity-rebind-on", "\"unknown\""),
        ("requests/streaming/send-keepalive-before-ft", "false"),
        ("upstream/claude/tool-search", "false"),
        (
            "api-keys/claude",
            r#"[{"keys":[{"api-key":"test","native-tool-search":false}]}]"#,
        ),
        (
            "api-keys/vertex",
            r#"[{"keys":[{"api-key":"test","interactions":false}]}]"#,
        ),
    ] {
        let edit = v8_edit::V8Edit {
            method: v8_edit::V8Method::Put,
            path: path.split('/').map(str::to_owned).collect(),
            yaml: false,
            body: body.as_bytes().to_vec(),
        };
        v8_edit::preview_v8(V8.as_bytes(), &edit).expect(path);
    }
}

// Ported from TestParseConfigBytes_ClaudeHeaderDefaultsOperatorOverrides.
#[test]
fn claude_header_operator_overrides_normalize() {
    for prefix in [
        "claude-header-defaults:\n",
        "upstream:\n  claude:\n    header-defaults:\n",
    ] {
        let indent = if prefix.starts_with("upstream") {
            "      "
        } else {
            "  "
        };
        let yaml = format!(
            "{prefix}{indent}anthropic-beta-strip: [' beta-a ', '', beta-a]\n{indent}anthropic-beta-append: [beta-b, ' beta-b ', beta-c]\n{indent}inject-diagnostics: false\n{indent}client-request-id: ' PassThrough '\n"
        );
        let cfg = Config::parse(yaml.as_bytes()).expect("parse");
        let hd = &cfg.claude_header_defaults;
        assert_eq!(hd.anthropic_beta_strip, ["beta-a"]);
        assert_eq!(hd.anthropic_beta_append, ["beta-b", "beta-c"]);
        assert_eq!(hd.inject_diagnostics, Some(false));
        assert_eq!(hd.client_request_id, "passthrough");
        for migrate in [false, true] {
            let out =
                save::render_preserving_comments(yaml.as_bytes(), &cfg, migrate).expect("save");
            let loaded = Config::parse(&out).expect("reload");
            assert!(loaded.claude_header_defaults == *hd);
        }
    }
    for (raw, want) in [("sometimes", ""), (" ON ", "on"), ("Off", "off")] {
        let yaml = format!("claude-header-defaults: {{client-request-id: '{raw}'}}\n");
        assert_eq!(
            Config::parse(yaml.as_bytes())
                .unwrap()
                .claude_header_defaults
                .client_request_id,
            want
        );
    }
}

// Ported from TestParseConfigBytesClaudeSessionDatePinDays and
// TestSaveConfigPreserveComments_SessionDatePinDaysExplicitZeroWhenAbsent /
// TestSaveConfigPreserveComments_SessionDatePinDaysDefaultNotPersisted.
#[test]
fn claude_session_date_pin_defaults_and_saves() {
    for (yaml, want) in [
        ("port: 8317\n", 7),
        ("claude:\n  session-date-pin-days: 14\n", 14),
        ("upstream:\n  claude:\n    session-date-pin-days: 7\n", 7),
        ("upstream:\n  claude:\n    session-date-pin-days: 0\n", 0),
        ("claude:\n  session-date-pin-days: 0\n", 0),
    ] {
        assert_eq!(
            Config::parse(yaml.as_bytes())
                .unwrap()
                .claude
                .session_date_pin_days,
            Some(want)
        );
    }
    for migrate in [false, true] {
        let yaml = b"port: 8317\n";
        let mut cfg = Config::parse(yaml).unwrap();
        let out = save::render_preserving_comments(yaml, &cfg, migrate).unwrap();
        assert!(!String::from_utf8_lossy(&out).contains("session-date-pin-days"));
        cfg.claude.session_date_pin_days = Some(0);
        cfg.claude_header_defaults.inject_diagnostics = Some(false);
        let out = save::render_preserving_comments(yaml, &cfg, migrate).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("session-date-pin-days: 0"));
        let loaded = Config::parse(&out).unwrap();
        assert_eq!(loaded.claude.session_date_pin_days, Some(0));
        assert_eq!(
            loaded.claude_header_defaults.inject_diagnostics,
            Some(false)
        );
    }
}

// Ported from TestParseConfigBytesRequestsTLSHandshakeTimeout,
// TestParseConfigBytesRequestsTLSHandshakeTimeoutLegacyLayout,
// TestParseConfigBytesRequestsTLSHandshakeTimeoutPrecedence and
// TestParseConfigBytesUpstreamClaudeTLSHandshakeTimeoutIgnored.
#[test]
fn tls_handshake_timeout_layouts_and_precedence() {
    for (yaml, want) in [
        ("requests:\n  tls-handshake-timeout-seconds: 12\n", 12),
        ("tls-handshake-timeout-seconds: 7\n", 7),
        (
            "tls-handshake-timeout-seconds: 7\nrequests:\n  tls-handshake-timeout-seconds: 12\n",
            12,
        ),
        (
            "tls-handshake-timeout-seconds: 7\nrequests:\n  tls-handshake-timeout-seconds: 0\n",
            0,
        ),
        (
            "upstream:\n  claude:\n    tls-handshake-timeout-seconds: 99\n",
            0,
        ),
        ("tls-handshake-timeout-seconds: -5\n", -5),
        (
            "tls-handshake-timeout-seconds: 9223372036854775807\n",
            i64::MAX,
        ),
    ] {
        let cfg = Config::parse(yaml.as_bytes()).unwrap();
        assert_eq!(cfg.tls_handshake_timeout_seconds, want);
        for migrate in [false, true] {
            let out = save::render_preserving_comments(yaml.as_bytes(), &cfg, migrate).unwrap();
            assert_eq!(
                Config::parse(&out).unwrap().tls_handshake_timeout_seconds,
                want
            );
        }
    }
}

// Not upstream's: carried header overrides must never leak values in reload logs.
#[test]
fn v32_carried_settings_have_redacted_diff_lines() {
    let old = Config::default();
    let mut new = old.clone();
    new.tls_handshake_timeout_seconds = 12;
    new.claude.session_date_pin_days = Some(0);
    new.claude_header_defaults
        .anthropic_beta_append
        .push("private-beta-name".into());
    let lines = diff::build_change_details(&old, &new).join("\n");
    assert!(lines.contains("tls-handshake-timeout-seconds: 0 -> 12"));
    assert!(lines.contains("claude.session-date-pin-days:"));
    assert!(lines.contains("claude-header-defaults: updated"));
    assert!(!lines.contains("private-beta-name"));
    assert!(!format!("{new:?}").contains("private-beta-name"));
}
