use super::*;
use crate::config::testing::TempDir;
use serde_json::Value;

#[test]
fn saved_pending_empty_mapping_keeps_values_comments_and_reloads() {
    let fixture: Value =
        serde_json::from_str(include_str!("../testdata/pending-empty-map-save.json")).unwrap();
    let source = fixture["options"]["file"].as_str().unwrap();
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source).unwrap();
    let original =
        expand_config_aliases(&unmarshal(source.as_bytes()).unwrap().content[0]).unwrap();
    fn data(node: &Node) -> Value {
        serde_json::json!([
            format!("{:?}", node.kind),
            node.tag,
            node.value,
            node.content.iter().map(data).collect::<Vec<_>>()
        ])
    }
    for step in fixture["options"]["steps"].as_array().unwrap() {
        if step["op"] == "nested" {
            let keys: Vec<_> = step["keys"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            update_nested_scalar(&path, &keys, step["value"].as_str().unwrap()).unwrap();
        } else {
            let config = Config::parse(step["config"].as_str().unwrap()).unwrap();
            save_preserving_comments(&path, &config, step["migrate"].as_bool().unwrap_or(false))
                .unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        Config::load_bytes(&bytes).unwrap();
        let saved = unmarshal(&bytes).unwrap();
        for field in ["plugins.configs", "pprof"] {
            if let Some(before) = yaml_path(&original, field) {
                assert_eq!(
                    Some(data(before)),
                    yaml_path(&saved.content[0], field).map(data),
                    "{field}"
                );
            }
        }
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(text.matches("#   c43 with words").count(), 1);
    }
    let output = std::fs::read_to_string(&path).unwrap();
    let saved = unmarshal(output.as_bytes()).unwrap();
    let map = yaml_path(&saved.content[0], "oauth-request-scoped-errors").unwrap();
    assert_eq!(map.kind, Kind::Mapping);
    assert!(map.content.is_empty());
}

#[test]
fn unrelated_own_comment_on_empty_mapping_remains_a_safe_refusal() {
    let source = "port: 8317\noauth-excluded-models: # own\n  codex: [m]\n";
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source).unwrap();
    let cfg = Config::parse("port: 8317\n").unwrap();
    assert!(save_preserving_comments(&path, &cfg, false).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), source);
}

#[test]
fn deferred_empty_mapping_v8_saves_keep_plugin_values_and_comments() {
    let f: Value =
        serde_json::from_str(include_str!("../testdata/pending-empty-map-v8-save.json")).unwrap();
    let source = f["options"]["file"].as_str().unwrap();
    let original = super::super::AnyValue::parse_yaml(source).unwrap();
    fn configs(v: &super::super::AnyValue) -> Option<&super::super::AnyValue> {
        let super::super::AnyValue::Map(m) = v else {
            return None;
        };
        let super::super::AnyValue::Map(p) = m.get("plugins")? else {
            return None;
        };
        p.get("configs")
    }
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source).unwrap();
    for step in f["options"]["steps"].as_array().unwrap() {
        let current = std::fs::read_to_string(&path).unwrap();
        let cfg = Config::parse(step["config"].as_str().unwrap_or(&current)).unwrap();
        save_preserving_comments(&path, &cfg, step["migrate"].as_bool().unwrap_or(false)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        Config::load_bytes(text.as_bytes()).unwrap();
        let reloaded = super::super::AnyValue::parse_yaml(&text).unwrap();
        assert_eq!(configs(&original), configs(&reloaded));
        for comment in ["#c38", "# c39"] {
            assert_eq!(text.matches(comment).count(), 1);
        }
    }
}
