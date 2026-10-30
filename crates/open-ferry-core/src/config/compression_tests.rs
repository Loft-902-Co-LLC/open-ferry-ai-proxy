// Native configuration cases from upstream 064e008b's
// codex_compression_test.go and response_compression_test.go, plus save guards.

use super::testing::TempDir;
use super::*;

fn parse(raw: &str) -> Config {
    Config::parse(raw.as_bytes()).expect("compression config")
}

#[test]
fn compression_defaults_and_thresholds() {
    let cfg = Config::default();
    assert!(cfg.effective_response_compression().is_enabled());
    assert_eq!(
        cfg.effective_response_compression().effective_min_bytes(),
        1024
    );
    assert!(!cfg.codex.request_compression_enabled());
    assert!(cfg.codex.websocket_compression_enabled());
    assert_eq!(cfg.codex.request_compression_min_bytes(), 1024);
    for (raw, want) in [(0, 1024), (-1, 1024), (2048, 2048), (i64::MAX, i64::MAX)] {
        let cfg = parse(&format!(
            "response-compression: {{min-bytes: {raw}}}\ncodex: {{upstream-request-compression-min-bytes: {raw}}}\n"
        ));
        assert_eq!(cfg.response_compression.min_bytes, raw);
        assert_eq!(cfg.response_compression.effective_min_bytes(), want);
        assert_eq!(cfg.codex.upstream_request_compression_min_bytes, raw);
        assert_eq!(cfg.codex.request_compression_min_bytes(), want);
    }
}

#[test]
fn compression_legacy_and_v8_mappings() {
    for prefix in [
        "codex: ",
        "oauth: {providers: {codex: ",
        "upstream: {codex: ",
    ] {
        let suffix = match prefix {
            "codex: " => "",
            "upstream: {codex: " => "}",
            _ => "}}",
        };
        for enabled in [false, true] {
            let cfg = parse(&format!(
                "{prefix}{{websocket-compression: {enabled}, upstream-request-compression: {enabled}, upstream-request-compression-min-bytes: 4096}}{suffix}\n"
            ));
            assert_eq!(cfg.codex.websocket_compression, Some(enabled));
            assert_eq!(cfg.codex.websocket_compression_enabled(), enabled);
            assert_eq!(cfg.codex.request_compression_enabled(), enabled);
            assert_eq!(cfg.codex.request_compression_min_bytes(), 4096);
        }
    }
    // The three upstream response-compression cases, in both layouts.
    for (body, enabled, min) in [
        ("{}", true, 1024),
        ("{enabled: false, min-bytes: 64}", false, 64),
        ("{enabled: true, min-bytes: -1}", true, 1024),
    ] {
        for (prefix, suffix) in [
            ("response-compression: ", ""),
            ("server: {response-compression: ", "}"),
        ] {
            let cfg = parse(&format!("{prefix}{body}{suffix}\n"));
            assert_eq!(cfg.response_compression.is_enabled(), enabled);
            assert_eq!(cfg.response_compression.effective_min_bytes(), min);
        }
    }
}

#[test]
fn compression_partial_layout_presence_wins() {
    let cfg = parse(
        "response-compression: {enabled: true, min-bytes: 4096}\nserver: {response-compression: {enabled: false, min-bytes: 0}}\ncodex: {websocket-compression: true, upstream-request-compression: true, upstream-request-compression-min-bytes: 4096}\noauth: {providers: {codex: {websocket-compression: true, upstream-request-compression: true, upstream-request-compression-min-bytes: 2048}}}\nupstream: {codex: {websocket-compression: false, upstream-request-compression: false, upstream-request-compression-min-bytes: 0}}\n",
    );
    assert_eq!(cfg.response_compression.enabled, Some(false));
    assert_eq!(cfg.response_compression.min_bytes, 0);
    assert_eq!(cfg.codex.websocket_compression, Some(false));
    assert!(!cfg.codex.request_compression_enabled());
    assert_eq!(cfg.codex.upstream_request_compression_min_bytes, 0);
    assert_eq!(cfg.codex.request_compression_min_bytes(), 1024);
    let empty =
        parse("response-compression: {enabled: false}\nserver: {response-compression: {}}\n");
    assert_eq!(empty.response_compression.enabled, Some(false));
    assert!(!empty.response_compression.is_enabled());
}

#[test]
fn compression_partial_response_fields_merge_on_load() {
    for raw in [
        "response-compression: {enabled: false}\nserver: {response-compression: {min-bytes: 64}}\n",
        "response-compression: {min-bytes: 64}\nserver: {response-compression: {enabled: false}}\n",
    ] {
        let cfg = parse(raw);
        assert_eq!(cfg.response_compression.enabled, Some(false));
        assert_eq!(cfg.response_compression.min_bytes, 64);
    }
}

#[test]
fn compression_partial_response_fields_survive_save() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("compression_save_go_r3.json")).unwrap();
    for case in cases
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["Name"].as_str().unwrap().starts_with("partial-"))
    {
        assert_go_save_history(case);
    }
}

#[test]
fn compression_canonical_null_section_is_rejected() {
    let raw = b"server: {response-compression: null}\n";
    let error = Config::parse(raw).expect_err("canonical section must be a mapping");
    assert_eq!(
        error.message,
        "server.response-compression must be a mapping"
    );
    assert!(V8Document::migrate(raw).is_err());
    assert!(save::normalize_config_layout(raw, true).is_err());
}

#[test]
fn compression_legacy_null_migrates_to_empty_mapping() {
    let raw = b"# keep\nresponse-compression: null # defaults\n";
    let document = V8Document::migrate(raw).unwrap();
    assert_eq!(
        document
            .value(&["server", "response-compression"])
            .transpose()
            .unwrap(),
        Some(AnyValue::Map(Default::default()))
    );
    let (bytes, changed) = save::normalize_config_layout(raw, true).unwrap();
    assert!(changed);
    let doc = save::unmarshal(&bytes).unwrap();
    let root = doc.content.first().unwrap();
    let section = save::yaml_path(root, "server.response-compression").unwrap();
    assert!(section.content.is_empty());
    assert_eq!(section.kind, yaml3::Kind::Mapping);
    assert_eq!(
        parse(std::str::from_utf8(&bytes).unwrap())
            .response_compression
            .enabled,
        None
    );
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("# keep"));
    assert!(text.contains("# defaults"));
    let dir = TempDir::new();
    let path = dir.write("config.yaml", raw);
    for _ in 0..2 {
        let cfg = Config::load(&path).unwrap();
        save::save_preserving_comments(&path, &cfg, true).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let doc = save::unmarshal(&bytes).unwrap();
        let root = doc.content.first().unwrap();
        let section = save::yaml_path(root, "server.response-compression").unwrap();
        assert_eq!(section.kind, yaml3::Kind::Mapping);
        assert_eq!(
            Config::load(&path).unwrap().response_compression,
            cfg.response_compression
        );
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("# keep"));
        assert!(text.contains("# defaults"));
    }
}

#[test]
fn compression_unknown_response_children_are_archived_on_migration() {
    for raw in [
        "# keep\nresponse-compression: {enabled: false, junk: 1} # legacy\n",
        "# keep\nserver: {response-compression: {enabled: false, junk: 1}} # canonical\n",
    ] {
        let document = V8Document::migrate(raw.as_bytes()).unwrap();
        assert!(
            document
                .value(&["server", "response-compression", "junk"])
                .is_none()
        );
        let dir = TempDir::new();
        let path = dir.write("config.yaml", raw);
        for _ in 0..2 {
            let cfg = Config::load(&path).unwrap();
            save::save_preserving_comments(&path, &cfg, true).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let doc = save::unmarshal(&bytes).unwrap();
            let root = doc.content.first().unwrap();
            assert!(save::yaml_path(root, "response-compression").is_none());
            assert!(save::yaml_path(root, "server.response-compression.junk").is_none());
            assert_eq!(
                Config::load(&path).unwrap().response_compression.enabled,
                Some(false)
            );
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.contains("# keep"));
            assert!(text.contains("# legacy") || text.contains("# canonical"));
            assert!(
                text.lines().any(|line| line.starts_with('#')
                    && line.contains("junk")
                    && line.contains('1'))
            );
        }
    }
}

#[test]
fn compression_codex_migration_uses_upstream_field_order() {
    let raw = b"codex: {response-steering: true, disable-codex-cloaking: true, upstream-request-compression-min-bytes: 64, upstream-request-compression: true, websocket-compression: false}\n";
    for bytes in [
        save::normalize_config_layout(raw, true).unwrap().0,
        save::render_preserving_comments(raw, &Config::parse(raw).unwrap(), true).unwrap(),
    ] {
        let doc = save::unmarshal(&bytes).unwrap();
        let root = doc.content.first().unwrap();
        let codex = save::yaml_path(root, "upstream.codex").unwrap();
        let keys: Vec<&str> = codex
            .content
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[key, _]| key.value.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "websocket-compression",
                "upstream-request-compression",
                "upstream-request-compression-min-bytes",
                "disable-codex-cloaking",
                "response-steering"
            ]
        );
    }
}

#[test]
fn compression_codex_existing_keys_follow_upstream_order() {
    let raw = b"codex: {disable-codex-cloaking: true, upstream-request-compression-min-bytes: 64, upstream-request-compression: true}\nupstream: {codex: {response-steering: true, websocket-compression: false}}\n";
    for (index, bytes) in [
        save::normalize_config_layout(raw, true).unwrap().0,
        save::render_preserving_comments(raw, &Config::parse(raw).unwrap(), true).unwrap(),
    ]
    .into_iter()
    .enumerate()
    {
        let doc = save::unmarshal(&bytes).unwrap();
        let codex = save::yaml_path(doc.content.first().unwrap(), "upstream.codex").unwrap();
        let keys: Vec<&str> = codex
            .content
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[key, _]| key.value.as_str())
            .collect();
        let expected = if index == 0 {
            [
                "response-steering",
                "websocket-compression",
                "upstream-request-compression",
                "upstream-request-compression-min-bytes",
                "disable-codex-cloaking",
            ]
        } else {
            [
                "websocket-compression",
                "response-steering",
                "upstream-request-compression",
                "upstream-request-compression-min-bytes",
                "disable-codex-cloaking",
            ]
        };
        assert_eq!(keys, expected);
    }
}

#[test]
fn compression_explicit_false_survives_new_parent_on_save() {
    for raw in [
        "# keep\nport: 8317\n",
        "config-version: 8\nserver: {port: 8317}\n",
        "oauth: {providers: {codex: {response-steering: true}}}\n",
    ] {
        for migrate in [false, true] {
            let dir = TempDir::new();
            let path = dir.write("config.yaml", raw);
            let mut cfg = Config::load(&path).unwrap();
            cfg.response_compression.enabled = Some(false);
            cfg.codex.websocket_compression = Some(false);
            for _ in 0..3 {
                save::save_preserving_comments(&path, &cfg, migrate).unwrap();
                cfg = Config::load(&path).unwrap();
                assert_eq!(cfg.response_compression.enabled, Some(false));
                assert_eq!(cfg.codex.websocket_compression, Some(false));
                assert!(!cfg.response_compression.is_enabled());
                assert!(!cfg.codex.websocket_compression_enabled());
            }
        }
    }
}

#[test]
fn compression_unset_stays_unset_on_save() {
    for raw in [
        "# keep\nport: 8317\n",
        "config-version: 8\nserver: {port: 8317}\n",
        "codex: {response-steering: true}\n",
    ] {
        for migrate in [false, true] {
            let dir = TempDir::new();
            let path = dir.write("config.yaml", raw);
            for _ in 0..3 {
                let cfg = Config::load(&path).unwrap();
                save::save_preserving_comments(&path, &cfg, migrate).unwrap();
                let text = std::fs::read_to_string(&path).unwrap();
                for key in [
                    "response-compression",
                    "upstream-request-compression",
                    "websocket-compression",
                ] {
                    assert!(!text.contains(key), "default populated {key}");
                }
            }
        }
    }
}

#[test]
fn compression_values_survive_saves_migration_and_reset_to_zero() {
    for raw in [
        "response-compression: {enabled: false, min-bytes: -1}\ncodex: {websocket-compression: false, upstream-request-compression: true, upstream-request-compression-min-bytes: 4096}\n",
        "server: {response-compression: {enabled: false, min-bytes: -1}}\nupstream: {codex: {websocket-compression: false, upstream-request-compression: true, upstream-request-compression-min-bytes: 4096}}\n",
        "oauth: {providers: {codex: {websocket-compression: false, upstream-request-compression: true, upstream-request-compression-min-bytes: 4096}}}\nresponse-compression: {enabled: false, min-bytes: -1}\n",
    ] {
        let dir = TempDir::new();
        let path = dir.write("config.yaml", raw);
        for migrate in [false, true, false, true] {
            let cfg = Config::load(&path).unwrap();
            save::save_preserving_comments(&path, &cfg, migrate).unwrap();
            let loaded = Config::load(&path).unwrap();
            assert_eq!(loaded.response_compression, cfg.response_compression);
            assert_eq!(loaded.codex, cfg.codex);
        }
        let mut cfg = Config::load(&path).unwrap();
        cfg.codex.upstream_request_compression_min_bytes = 0;
        cfg.response_compression.min_bytes = 0;
        save::save_preserving_comments(&path, &cfg, false).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.codex.upstream_request_compression_min_bytes, 0);
        assert_eq!(cfg.codex.request_compression_min_bytes(), 1024);
        assert_eq!(cfg.response_compression.min_bytes, 0);
    }
}

#[test]
fn compression_reload_uses_fresh_values_without_populating_defaults() {
    let dir = TempDir::new();
    let path = dir.write("config.yaml", "server: {response-compression: {enabled: false, min-bytes: 64}}\nupstream: {codex: {websocket-compression: false, upstream-request-compression: true, upstream-request-compression-min-bytes: 4096}}\n");
    let old = Config::load(&path).unwrap();
    std::fs::write(&path, "server: {response-compression: {min-bytes: -1}}\nupstream: {codex: {upstream-request-compression-min-bytes: -1}}\n").unwrap();
    let new = Config::load(&path).unwrap();
    assert!(!old.response_compression.is_enabled());
    assert!(new.response_compression.is_enabled());
    assert_eq!(new.response_compression.enabled, None);
    assert_eq!(new.response_compression.effective_min_bytes(), 1024);
    assert!(new.codex.websocket_compression_enabled());
    assert!(!new.codex.request_compression_enabled());
    assert_eq!(new.codex.request_compression_min_bytes(), 1024);
    assert_eq!(
        Config::load_bytes_quietly(&std::fs::read(path).unwrap()).unwrap(),
        new
    );
}

#[test]
fn compression_known_paths_and_strict_validation() {
    let known = v8_edit::known_v8_paths();
    for path in [
        "server.response-compression.enabled",
        "server.response-compression.min-bytes",
        "upstream.codex.websocket-compression",
        "upstream.codex.upstream-request-compression",
        "upstream.codex.upstream-request-compression-min-bytes",
    ] {
        assert!(v8_edit::is_known_v8_path(
            &known,
            &path.split('.').collect::<Vec<_>>()
        ));
    }
    v8_edit::validate_v8_config(b"config-version: 8\nserver: {response-compression: {enabled: false, min-bytes: -1}}\nupstream: {codex: {websocket-compression: false, upstream-request-compression: true, upstream-request-compression-min-bytes: 4096}}\n").unwrap();
    for raw in [
        "response-compression: {enabled: []}",
        "response-compression: {min-bytes: {}}",
        "codex: {websocket-compression: []}",
        "codex: {upstream-request-compression: []}",
        "codex: {upstream-request-compression-min-bytes: {}}",
    ] {
        assert!(Config::parse(raw).is_err());
    }
}

#[test]
fn compression_diff_reports_all_stored_settings() {
    let old = Config::default();
    let new = parse(
        "response-compression: {enabled: false, min-bytes: 64}\ncodex: {websocket-compression: false, upstream-request-compression: true, upstream-request-compression-min-bytes: 4096}\n",
    );
    let changes = diff::build_change_details(&old, &new);
    for key in [
        "response-compression.enabled",
        "response-compression.min-bytes",
        "codex.websocket-compression",
        "codex.upstream-request-compression",
        "codex.upstream-request-compression-min-bytes",
    ] {
        assert!(
            changes
                .iter()
                .any(|line| line.starts_with(&format!("{key}: ")))
        );
    }
}

#[test]
fn discovery_advertise_management_remains_opaque_on_load_and_save() {
    for advertise in ["true", "false", "opaque-value"] {
        for (prefix, indent) in [("discovery:\n", "  "), ("server:\n  discovery:\n", "    ")] {
            let raw = format!(
                "# discovery head\n{prefix}{indent}advertise-management: {advertise} # discovery line\n{indent}service-name: 'Local gateway'\nplugins:\n  configs:\n    example:\n      # plugin head\n      custom: {{retained: [one, two]}}\n"
            );
            let dir = TempDir::new();
            let path = dir.write("config.yaml", &raw);
            // An intentionally untyped discovery value must never break a load.
            let cfg = Config::load(&path).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
            let before = V8Document::migrate(raw.as_bytes()).unwrap();
            let discovery = before
                .value(&["server", "discovery", "advertise-management"])
                .unwrap()
                .unwrap();
            let plugin = before
                .value(&["plugins", "configs", "example"])
                .unwrap()
                .unwrap();
            for migrate in [false, true, false] {
                save::save_preserving_comments(&path, &cfg, migrate).unwrap();
                let bytes = std::fs::read(&path).unwrap();
                let after = V8Document::migrate(&bytes).unwrap();
                assert_eq!(
                    after
                        .value(&["server", "discovery", "advertise-management"])
                        .unwrap()
                        .unwrap(),
                    discovery
                );
                assert_eq!(
                    after
                        .value(&["plugins", "configs", "example"])
                        .unwrap()
                        .unwrap(),
                    plugin
                );
                let text = String::from_utf8(bytes).unwrap();
                for comment in ["# discovery head", "# discovery line", "# plugin head"] {
                    assert!(
                        text.contains(comment),
                        "missing {comment}; prefix={prefix}, migrate={migrate}, advertise={advertise}"
                    );
                }
            }
        }
    }
}

// Captured from v8.0.47 ParseConfigBytes + SaveConfigPreserveComments.
fn assert_go_save_history(case: &serde_json::Value) {
    let dir = TempDir::new();
    let path = dir.write("config.yaml", case["Raw"].as_str().unwrap());
    let migrate = case["Migrate"].as_bool().unwrap();
    for field in ["Output", "Second", "Third"] {
        let cfg = Config::load(&path).unwrap();
        save::save_preserving_comments(&path, &cfg, migrate).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            case[field].as_str().unwrap().as_bytes(),
            "case={}, save={field}",
            case["Name"]
        );
    }
    assert_eq!(case["Second"], case["Third"], "Go stabilizes on save two");
}

#[test]
fn compression_go_exact_save_histories() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("compression_save_go_r3.json")).unwrap();
    for case in cases
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| !case["Name"].as_str().unwrap().starts_with("partial-"))
    {
        assert_go_save_history(case);
    }
}

#[test]
fn compression_go_exact_flow_patch() {
    let raw = b"config-version: 8\nserver: {port: 8317, response-compression: {enabled: false}} # main\nrouting: {strategy: fill-first}\n";
    let edit = v8_edit::V8Edit {
        method: v8_edit::V8Method::Patch,
        path: Vec::new(),
        body: b"{}".to_vec(),
        yaml: false,
    };
    let expected = b"config-version: 8\nserver:\n    port: 8317\n    response-compression:\n        enabled: false\nrouting: # main\n    strategy: fill-first\n";
    assert_eq!(v8_edit::preview_v8(raw, &edit).unwrap(), expected);
}
