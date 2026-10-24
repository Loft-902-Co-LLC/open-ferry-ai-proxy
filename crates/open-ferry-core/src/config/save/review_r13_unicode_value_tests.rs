use super::*;
use crate::config::AnyValue;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8};

fn retained(config: &Config, sequence: bool) -> AnyValue {
    if sequence {
        AnyValue::Seq(config.api_keys.iter().cloned().map(AnyValue::Str).collect())
    } else {
        AnyValue::Str(config.auth_dir.clone())
    }
}

fn put(path: &Path, value: Option<&str>) {
    let (keys, body, yaml) = if let Some(value) = value {
        let body = serde_json::to_string(value)
            .unwrap()
            .replace('\u{0085}', "\\u0085")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
        (vec!["oauth".into(), "auth-dir".into()], body, true)
    } else {
        (vec!["server".into(), "port".into()], "1234".into(), false)
    };
    edit_v8(
        path,
        &V8Edit {
            method: V8Method::Put,
            path: keys,
            body: body.into_bytes(),
            yaml,
        },
    )
    .unwrap();
}

fn source_values(sources: &[(&str, &str)], sequence: bool) {
    for (id, source) in sources {
        for op in ["settings", "migrate", "nested", "put"] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, source).unwrap();
            let mut config = Config::load(&path).unwrap();
            let before = retained(&config, sequence);
            match op {
                "settings" | "migrate" => {
                    config.debug = !config.debug;
                    save_preserving_comments(&path, &config, op == "migrate").unwrap();
                }
                "nested" => update_nested_scalar(&path, &["proxy-url"], "/proof").unwrap(),
                "put" => put(&path, None),
                _ => unreachable!(),
            }
            assert!(
                retained(&Config::load(&path).unwrap(), sequence) == before,
                "{id} {op} changed the retained value"
            );
        }
    }
}

fn generated_values(values: &[&str], sequence: bool) {
    for value in values {
        for op in ["settings", "migrate", "nested", "put"] {
            if sequence && matches!(op, "nested" | "put") {
                continue;
            }
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, "auth-dir: /x\napi-keys: [k]\n").unwrap();
            match op {
                "settings" | "migrate" => {
                    let mut config = Config::load(&path).unwrap();
                    if sequence {
                        config.api_keys = vec![(*value).into()];
                    } else {
                        config.auth_dir = (*value).into();
                    }
                    save_preserving_comments(&path, &config, op == "migrate").unwrap();
                }
                "nested" => update_nested_scalar(&path, &["auth-dir"], value).unwrap(),
                "put" => put(&path, Some(value)),
                _ => unreachable!(),
            }
            let expected = if sequence {
                AnyValue::Seq(vec![AnyValue::Str((*value).into())])
            } else {
                AnyValue::Str((*value).into())
            };
            assert!(
                retained(&Config::load(&path).unwrap(), sequence) == expected,
                "{op} changed the generated value"
            );
        }
    }
}

#[test]
fn review_r13_leading_lf_values_survive_all_writes() {
    generated_values(&["\n", "\nx", "\n\n"], false);
    generated_values(&["\n", "\nx", "\n\n"], true);
    source_values(
        &[
            ("N4", "auth-dir: |-\n  \u{0085}\n  x\n"),
            ("N4 folded", "auth-dir: >-\n  \u{0085}\n  x\n"),
            (
                "NEL after blank lines",
                "auth-dir: |-\n\n\n  \u{0085}\n  x\n",
            ),
            ("S13", "auth-dir: |-\n\n\n  x\n"),
        ],
        false,
    );
}

#[test]
fn review_r13_tab_values_survive_generated_writes() {
    let values = [
        "\t\u{2028}\n",
        "\t\u{2028}x\ny",
        "\tx\n\u{2028}",
        "\t\u{2029}\n",
        "\tx\u{2028}\n",
        " \t\u{2028}\n",
        "\tx\ny",
        "\t\n",
    ];
    generated_values(&values, false);
    generated_values(&values, true);
}

#[test]
fn review_r13_tab_values_survive_source_writes() {
    source_values(
        &[
            ("TB1", "auth-dir: \"\\t\\L\\n\"\n"),
            ("TB2", "auth-dir: \"\\t\\Lx\\ny\"\n"),
            ("TB4", "auth-dir: \"\\tx\\ny\"\n"),
            ("TB literal", "auth-dir: |2-\n  \tx\n  y\n"),
        ],
        false,
    );
    source_values(&[("TB3", "api-keys:\n  - \" \\t\\L\\n\"\n")], true);
}

#[test]
fn review_r13_live_tab_protection_preserves_emitted_values() {
    for value in ["\t\u{2028}\n", "\tx\n\u{2028}", " \t\u{2028}\n"] {
        let mut node = Node::mapping();
        node.content.push(Node::scalar(STR_TAG, "auth-dir"));
        node.content.push(Node::scalar(STR_TAG, value));
        encode::quote_config_unicode(&mut node);
        let output = encode::faithful_node(&node, 2).unwrap();
        assert!(Config::parse(output).unwrap().auth_dir == value);
    }
}

#[test]
fn review_r13_retained_tab_block_matches_main_bytes() {
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    let source = "auth-dir: |2-\n  \tx\n  y\n";
    std::fs::write(&path, source).unwrap();
    update_nested_scalar(&path, &["proxy-url"], "socks5://example.invalid:1").unwrap();
    // Exact output from main 1919373e, including its indentation indicator.
    let expected = "auth-dir: |2-\n  \tx\n  y\nproxy-url: socks5://example.invalid:1\n";
    assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
    assert!(Config::load(&path).unwrap().auth_dir == "\tx\ny");
}

#[test]
fn review_r13_plugin_values_survive_generated_node_writes() {
    for source in [
        "plugins:\n  configs:\n    demo:\n      items:\n        - |-\n          \u{2028}\n          k\n",
        "plugins:\n  configs:\n    demo:\n      items:\n        - |2-\n          \tx\n          y\n",
        "plugins:\n  configs:\n    demo:\n      items:\n        - |-\n\n          x\n",
    ] {
        for migrate in [false, true] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, source).unwrap();
            let before = AnyValue::parse_yaml(source).unwrap();
            let config = Config::load(&path).unwrap();
            let result = save_preserving_comments(&path, &config, migrate);
            if migrate && source.contains('\t') {
                // Main 1919373e also refuses this retained plugin sequence
                // during migration; an unrelated settings save succeeds.
                let error = result.expect_err("main's sequence migration refusal is retained");
                assert_eq!(error.kind(), SaveErrorKind::Check);
                assert_eq!(
                    error.to_string(),
                    "failed to parse config file: yaml: line 6: wrongly indented line in block scalar"
                );
                assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
                continue;
            }
            result.unwrap();
            let output = std::fs::read_to_string(&path).unwrap();
            let AnyValue::Map(before) = before else {
                panic!("fixture mapping")
            };
            let AnyValue::Map(after) = AnyValue::parse_yaml(&output).unwrap() else {
                panic!("saved mapping")
            };
            assert!(
                before.get("plugins") == after.get("plugins"),
                "plugin value changed"
            );
            Config::load(&path).unwrap();
        }
    }
}

#[test]
fn review_r13_save_alias_text_budget_refuses_before_writing() {
    let source = format!(
        "big: &a {}\nx-unknown:\n{}",
        "x".repeat(256 << 10),
        "  - *a\n".repeat(300)
    );
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, &source).unwrap();
    let node = unmarshal(source.as_bytes()).unwrap();
    let result = expand_config_aliases(&node);
    assert!(
        matches!(result, Err(ref error) if error.to_string() == "yaml: document contains excessive aliasing"),
        "large alias expansion must be refused before emission"
    );
    let config = Config::load(&path).unwrap();
    for nested in [false, true] {
        let result = if nested {
            update_nested_scalar(&path, &["proxy-url"], "/proof")
        } else {
            save_preserving_comments(&path, &config, false)
        };
        let error = result.expect_err("large alias text expansion must be refused");
        assert_eq!(error.kind(), SaveErrorKind::Yaml);
        assert_eq!(
            error.to_string(),
            "yaml: document contains excessive aliasing"
        );
        assert!(std::fs::read(&path).unwrap() == source.as_bytes());
    }
}
