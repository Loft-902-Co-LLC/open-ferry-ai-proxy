// Ported from CLIProxyAPI internal/config/claude_code_test.go,
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
