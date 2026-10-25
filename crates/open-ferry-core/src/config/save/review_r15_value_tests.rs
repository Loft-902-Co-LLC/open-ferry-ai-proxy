use super::*;
use crate::config::AnyValue;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8};

fn named<'a>(value: &'a AnyValue, key: &str) -> Option<&'a AnyValue> {
    match value {
        AnyValue::Map(map) => map
            .get(key)
            .filter(|value| matches!(value, AnyValue::Str(_)))
            .or_else(|| map.values().find_map(|value| named(value, key))),
        AnyValue::Seq(items) => items.iter().find_map(|value| named(value, key)),
        _ => None,
    }
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value)
        .unwrap()
        .replace('\u{2028}', "\\L")
        .replace('\u{2029}', "\\P")
}

fn save_operation(path: &Path, op: &str) -> Result<(), String> {
    if op == "put" {
        edit_v8(
            path,
            &V8Edit {
                method: V8Method::Put,
                path: vec!["server".into(), "port".into()],
                body: b"1234".to_vec(),
                yaml: false,
            },
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
    } else {
        let mut config = Config::load(path).map_err(|error| error.to_string())?;
        config.debug = !config.debug;
        save_preserving_comments(path, &config, op == "migrate").map_err(|error| error.to_string())
    }
}

fn mapping_values(op: &str) {
    let values = [
        "      x\n\u{2028}",
        "  x\n\u{2028}",
        " x\n\u{2028}",
        "   x\n\u{2028}",
        "      x\n\u{2029}",
        "      x\u{2028}y\n",
        "      x\n",
        "  x\n",
        " x\n",
        "   x\n",
        "  \t  x\n\u{2028}",
        " \t x\n\u{2028}",
    ];
    for value in values {
        let quoted = quoted(value);
        for (label, source, key) in [
            (
                "claude",
                format!("claude-api-key:\n  - api-key: {quoted}\n"),
                "api-key",
            ),
            (
                "claude later key",
                format!("claude-api-key:\n  - prefix: p\n    api-key: {quoted}\n"),
                "api-key",
            ),
            (
                "payload",
                format!(
                    "payload: {{default: [{{models: [{{name: \"*\"}}], params: {{probe: [{{text: {quoted}}}]}}}}]}}\n"
                ),
                "text",
            ),
            (
                "payload later key",
                format!(
                    "payload: {{default: [{{models: [{{name: \"*\"}}], params: {{probe: [{{a: z, text: {quoted}}}]}}}}]}}\n"
                ),
                "text",
            ),
            (
                "openai entry",
                format!(
                    "openai-compatibility:\n  - name: n\n    base-url: https://example.invalid\n    api-key-entries:\n      - api-key: {quoted}\n"
                ),
                "api-key",
            ),
        ] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, source).unwrap();
            save_operation(&path, op).unwrap_or_else(|_| panic!("{label} {op} refused"));
            let output = std::fs::read_to_string(&path).unwrap();
            let actual = AnyValue::parse_yaml(&output).unwrap();
            assert!(
                named(&actual, key) == Some(&AnyValue::Str(value.into())),
                "{label} {op} changed the value"
            );
            Config::load(&path).unwrap();
        }
    }
}

#[test]
fn review_r15_mapping_values_survive_settings() {
    mapping_values("settings");
}

#[test]
fn review_r15_mapping_values_survive_migrate() {
    mapping_values("migrate");
}

#[test]
fn review_r15_mapping_values_survive_put() {
    mapping_values("put");
}

#[test]
fn review_r15_replaced_blocks_survive_migrate() {
    for style in ["|-", ">-"] {
        for value in ["      x\n\u{2028}", " x\n\u{2028}", "   x\n"] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, format!("api-keys:\n  - {style}\n    x\n")).unwrap();
            let mut config = Config::load(&path).unwrap();
            config.api_keys = vec![value.into()];
            save_preserving_comments(&path, &config, true).unwrap();
            assert!(Config::load(&path).unwrap().api_keys == [value]);
        }
    }
}

#[test]
fn review_r15_replaced_mapping_tab_scalar_survives() {
    for style in ["|-", ">-"] {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, format!("auth-dir: {style}\n  x\n")).unwrap();
        let mut config = Config::load(&path).unwrap();
        config.auth_dir = "\tx".into();
        save_preserving_comments(&path, &config, false).unwrap();
        assert!(Config::load(&path).unwrap().auth_dir == "\tx");
    }
}

#[test]
fn review_r15_v8_replaced_blocks_survive_repeated_settings() {
    for style in ["|-", ">-"] {
        for value in ["      x\n\u{2028}", "      x\n"] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(
                &path,
                format!("config-version: 8\naccess:\n  api-keys:\n    - {style}\n      x\n"),
            )
            .unwrap();
            for _ in 0..3 {
                let mut config = Config::load(&path).unwrap();
                config.api_keys = vec![value.into()];
                save_preserving_comments(&path, &config, false).unwrap();
                assert!(Config::load(&path).unwrap().api_keys == [value]);
            }
        }
    }
}

#[test]
fn review_r15_existing_blocks_keep_their_values_or_stay_unchanged() {
    for source in [
        "api-keys:\n  - |2-\n          x\n    \u{2028}\n",
        "config-version: 8\naccess:\n  api-keys:\n    - |2-\n            x\n      \u{2028}\n",
    ] {
        for op in ["settings", "migrate", "put"] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, source).unwrap();
            let before = Config::load(&path).unwrap().api_keys;
            if save_operation(&path, op).is_ok() {
                assert!(Config::load(&path).unwrap().api_keys == before);
            } else {
                assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
            }
        }
    }
}

#[test]
fn review_r15_duplicate_key_errors_have_load_prefixes() {
    for source in ["port: 1\nport: 2\n", "# \u{2028}\nport: 1\nport: 2\n"] {
        let error = Config::parse(source).unwrap_err();
        assert_eq!(error.kind(), crate::config::ConfigErrorKind::Decode);
        assert!(
            error
                .to_string()
                .starts_with("parse config payload: yaml: ")
        );
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        let error = Config::load(&path).unwrap_err();
        assert_eq!(error.kind(), crate::config::ConfigErrorKind::Decode);
        assert!(
            error
                .to_string()
                .starts_with("failed to parse config file: yaml: ")
        );
    }
}

#[test]
fn review_r15_quoted_column_zero_continuations_match_go() {
    for (source, key) in [
        ("auth-dir: 'a\n\nz'\n", "auth-dir"),
        (
            "plugins:\n  configs:\n    demo:\n      text: 'a\n\nz'\n",
            "text",
        ),
    ] {
        // Actual yaml.v3 v3.0.1 and ParseConfigBytes both give a LF z.
        let (actual, extras) = AnyValue::parse_yaml_with_extras(source).unwrap();
        assert!(named(&actual, key) == Some(&AnyValue::Str("a\nz".into())));
        assert!(!extras.more_documents);
        Config::parse(source).unwrap();
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        Config::load(&path).unwrap();
        let (_, extras) =
            AnyValue::parse_yaml_with_extras(&format!("{source}---\nport: 3\n")).unwrap();
        assert!(extras.more_documents);
    }
}

#[test]
fn review_r15_quoted_continuation_fallback_keeps_refusals_and_limits() {
    for source in ["auth-dir: 'a\n\nz\n", "auth-dir: 'a\n\nz'\n#\0"] {
        assert!(Config::parse(source).is_err());
    }
    let source = "auth-dir: 'a\n\nz'\nnull: &a [*a]\n";
    assert!(Config::parse(source).is_err());
    let source = format!(
        "auth-dir: 'a\n\nz'\nx: {}x{}\n",
        "[".repeat(257),
        "]".repeat(257)
    );
    assert!(Config::parse(&source).is_err());
    let source = format!(
        "auth-dir: 'a\n\nz'\nbig: &a {}\nrefs: [{}]\n",
        "x".repeat(128 << 10),
        vec!["*a"; 520].join(", ")
    );
    // Unknown values share the anchor's text and aren't decoded into strings.
    assert!(Config::parse(&source).is_ok());
    // Typed values must still charge every owned alias string to the budget.
    assert!(Config::parse(&source.replace("refs:", "api-keys:")).is_err());
}

#[test]
fn review_r15_quoted_fallback_aliases_share_anchor_text() {
    let source = format!(
        "auth-dir: 'a\n\nz'\nshared: &a {}\nrefs: [{}]\n",
        "x".repeat(256 << 10),
        vec!["*a"; 1000].join(", ")
    );
    let root = crate::config::yaml::parse_document(&source)
        .unwrap()
        .unwrap();
    let value = |key: &str| {
        root.content
            .chunks_exact(2)
            .find(|pair| pair[0].value == key)
            .map(|pair| &pair[1])
            .unwrap()
    };
    let anchor = value("shared");
    assert!(
        value("refs")
            .content
            .iter()
            .all(|node| node.value.shares(&anchor.value))
    );
}

#[test]
fn review_r15_value_check_refuses_changed_typed_values_before_backup() {
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    let source = "auth-dir: /x\n";
    std::fs::write(&path, source).unwrap();
    let mut config = Config::load(&path).unwrap();
    config.auth_dir = "synthetic-intended-text".into();
    let intended = IntendedValues::settings(source.as_bytes(), &config, false).unwrap();
    let lock = WriteLock::acquire(&path).unwrap();
    let error = write::commit_locked_values(
        &lock,
        &path,
        b"auth-dir: synthetic-other-text\n",
        None,
        &intended,
    )
    .expect_err("a changed value must be refused");
    assert_eq!(error.kind(), SaveErrorKind::Check);
    assert_eq!(
        error.to_string(),
        "the file would read back differently at $.auth-dir"
    );
    assert!(!error.to_string().contains("synthetic-intended-text"));
    assert!(!error.to_string().contains("synthetic-other-text"));
    assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
    assert!(!backup_path(&path).exists());
    assert!(recorded_sha256(&path).is_none());
}

#[test]
fn review_r15_value_check_covers_plugin_and_non_string_mapping_values() {
    for (source, changed) in [
        (
            "plugins: {configs: {demo: {text: x}}}\n",
            "plugins: {configs: {demo: {text: y}}}\n",
        ),
        (
            "payload: {default: [{models: [{name: '*'}], params: {probe: {1: x}}}]}\n",
            "payload: {default: [{models: [{name: '*'}], params: {probe: {1: y}}}]}\n",
        ),
    ] {
        let intended = IntendedValues::bytes(source.as_bytes()).unwrap();
        check_values(source.as_bytes(), &intended).unwrap();
        let error = check_values(changed.as_bytes(), &intended)
            .expect_err("opaque values must also be compared");
        assert_eq!(error.kind(), SaveErrorKind::Check);
        assert!(
            error
                .to_string()
                .starts_with("the file would read back differently at $.")
        );
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, "port: 1\n").unwrap();
        write_file(&path, source.as_bytes()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
    }
}

#[test]
fn review_r15_value_check_accepts_loader_normalization() {
    let source = "openai-compatibility:\n  - name: '  n  '\n    base-url: ' https://example.invalid '\n    api-key-entries: [{api-key: k}]\nclaude-api-key:\n  - api-key: k\n    headers: {X-A: '  v  '}\noauth-excluded-models: {'  ChAnNeL  ': ['  M  ']}\n";
    for op in ["settings", "migrate", "put"] {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        save_operation(&path, op).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.openai_compatibility[0].name, "n");
        assert!(loaded.oauth_excluded_models.contains_key("channel"));
    }
}

#[test]
fn review_r15_safety_net_preserves_values_or_refuses_without_writing() {
    for mapping in [true, false] {
        let source = if mapping {
            "claude-api-key:\n  - api-key: \"      x\\n\\L\"\n"
        } else {
            "api-keys:\n  - |-\n    x\n"
        };
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        let mut config = Config::load(&path).unwrap();
        let wanted = "      x\n\u{2028}";
        if !mapping {
            config.api_keys = vec![wanted.into()];
        }
        let result = save_preserving_comments(&path, &config, !mapping);
        match result {
            Ok(()) => {
                let text = std::fs::read_to_string(&path).unwrap();
                let actual = AnyValue::parse_yaml(&text).unwrap();
                if mapping {
                    assert!(named(&actual, "api-key") == Some(&AnyValue::Str(wanted.into())));
                } else {
                    assert!(Config::load(&path).unwrap().api_keys == [wanted]);
                }
            }
            Err(error) => {
                assert_eq!(error.kind(), SaveErrorKind::Check);
                assert!(
                    error
                        .to_string()
                        .starts_with("the file would read back differently at $.")
                );
                assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
                assert!(!backup_path(&path).exists());
                eprintln!("{}; original file and backup state verified", error);
            }
        }
    }
}
