use super::*;
use crate::config::AnyValue;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8};

fn retained_value(source: &str) -> AnyValue {
    let AnyValue::Map(mut root) = AnyValue::parse_yaml(source).unwrap() else {
        panic!("expected fixture mapping")
    };
    if let Some(value) = root.remove("auth-dir") {
        return value;
    }
    if let Some(AnyValue::Map(mut oauth)) = root.remove("oauth")
        && let Some(value) = oauth.remove("auth-dir")
    {
        return value;
    }
    if let Some(value @ AnyValue::Seq(_)) = root.remove("api-keys") {
        return value;
    }
    if let Some(AnyValue::Map(mut access)) = root.remove("access")
        && let Some(value) = access.remove("api-keys")
    {
        return value;
    }
    panic!("fixture value is missing")
}

fn save_source(source: &str, op: &str) -> Vec<u8> {
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source).unwrap();
    match op {
        "settings" | "migrate" => {
            let mut config = Config::load(&path).unwrap();
            config.debug = !config.debug;
            save_preserving_comments(&path, &config, op == "migrate").unwrap();
        }
        "put" => {
            edit_v8(
                &path,
                &V8Edit {
                    method: V8Method::Put,
                    path: vec!["server".into(), "port".into()],
                    body: b"1234".to_vec(),
                    yaml: false,
                },
            )
            .unwrap();
        }
        _ => panic!("unknown fixture operation"),
    }
    Config::load(&path).unwrap();
    std::fs::read(&path).unwrap()
}

fn generated_values(op: &str) {
    for value in [
        "\u{2028}\n",
        "\n\u{2028}",
        "\u{2029}\n",
        "\n\u{2029}",
        "\u{2028}\nx",
        "\n\u{2028}\nx",
    ] {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, "auth-dir: /x\n").unwrap();
        if op == "nested" {
            update_nested_scalar(&path, &["auth-dir"], value).unwrap();
        } else {
            let mut config = Config::load(&path).unwrap();
            config.auth_dir = value.into();
            save_preserving_comments(&path, &config, false).unwrap();
        }
        let after = Config::load(&path).unwrap();
        assert!(after.auth_dir == value, "{op} changed the generated value");
    }
}

#[test]
fn review_r12_generated_leading_unicode_breaks_survive_nested() {
    generated_values("nested");
}

#[test]
fn review_r12_generated_leading_unicode_breaks_survive_settings() {
    generated_values("settings");
}

fn source_values(op: &str) {
    for (id, source) in [
        ("S01", "auth-dir: |-\n  \u{2028}\n  x\n"),
        ("S04", "auth-dir: \"\\L\\n\"\n"),
        ("F06", "api-keys:\n  - |-\n    \u{2028}\n    k\n"),
    ] {
        let before = retained_value(source);
        let output = save_source(source, op);
        let after = retained_value(std::str::from_utf8(&output).unwrap());
        assert!(after == before, "{id} {op} changed the retained value");
    }
}

#[test]
fn review_r12_source_values_survive_settings() {
    source_values("settings");
}

#[test]
fn review_r12_source_values_survive_migrate() {
    source_values("migrate");
}

#[test]
fn review_r12_source_values_survive_put() {
    source_values("put");
}

#[test]
fn review_r12_escaped_source_uses_its_decoded_value() {
    let source = "auth-dir: \"\\L\\n\"\n";
    assert!(!source.contains(['\u{0085}', '\u{2028}', '\u{2029}']));
    let before = retained_value(source);
    for op in ["settings", "migrate"] {
        let output = save_source(source, op);
        assert!(
            retained_value(std::str::from_utf8(&output).unwrap()) == before,
            "{op} changed the escaped value"
        );
    }
}

#[test]
fn review_r12_normalized_document_preserves_leading_unicode_block_value() {
    let source = "auth-dir: |-\n  \u{2028}\n  x\n";
    let (output, changed) = normalize_config_layout(source.as_bytes(), true).unwrap();
    assert!(changed);
    assert!(retained_value(std::str::from_utf8(&output).unwrap()) == retained_value(source));
}

#[test]
fn review_r12_v8_render_preserves_yaml_body_block_value() {
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, "config-version: 8\noauth: {auth-dir: /x}\n").unwrap();
    let edit = V8Edit {
        method: V8Method::Put,
        path: vec!["oauth".into(), "auth-dir".into()],
        body: "|-\n  \u{2028}\n  x\n".as_bytes().to_vec(),
        yaml: true,
    };
    edit_v8(&path, &edit).unwrap();
    assert!(Config::load(&path).unwrap().auth_dir == "\u{2028}\nx");
}
