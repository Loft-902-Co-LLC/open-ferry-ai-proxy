use super::*;
use crate::config::testing::TempDir;
use serde_json::{Value, json};
fn data(node: &Node) -> Value {
    json!([
        format!("{:?}", node.kind),
        node.tag,
        if node.tag == NULL_TAG {
            ""
        } else {
            &node.value
        },
        node.content.iter().map(data).collect::<Vec<_>>()
    ])
}
#[test]
fn null_plugin_full_saves_keep_values_comments_and_reload() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../testdata/null-plugin-full-saves.json")).unwrap();
    assert_eq!(fixtures.as_array().unwrap().len(), 16);
    for case in fixtures.as_array().unwrap() {
        let source = case["source"].as_str().unwrap();
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        let mut cfg = Config::parse(if case["separate"] == true {
            "debug: false\n"
        } else {
            source
        })
        .unwrap();
        cfg.auth_dir = "/x".into();
        save_preserving_comments(&path, &cfg, case["migrate"].as_bool().unwrap()).unwrap();
        let actual = std::fs::read_to_string(&path).unwrap();
        assert_eq!(actual, case["file"].as_str().unwrap(), "{case}");
        let mut reloaded = Config::load_bytes(actual.as_bytes()).unwrap();
        reloaded.oauth_only_fields.clear();
        cfg.oauth_only_fields.clear();
        assert_eq!(reloaded, cfg);
        let original = unmarshal(source.as_bytes()).unwrap();
        let saved = unmarshal(actual.as_bytes()).unwrap();
        assert_eq!(
            data(yaml_path(&original.content[0], "plugins.configs").unwrap()),
            data(yaml_path(&saved.content[0], "plugins.configs").unwrap())
        );
        for comment in ["# moved", "# on", "# null comment"] {
            assert_eq!(actual.matches(comment).count(), 1, "{comment}: {case}")
        }
    }
}
