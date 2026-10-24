use super::*;
use crate::config::AnyValue;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8};

fn sequence_values(op: &str) {
    for value in [
        " x\n\u{2028}",
        "  x\n\u{2028}",
        " \t\u{2028}\n",
        " x\n",
        "  x\n",
    ] {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, "api-keys: [k]\n").unwrap();
        if op == "put" {
            let body = serde_json::to_string(&[value])
                .unwrap()
                .replace('\u{2028}', "\\u2028");
            edit_v8(
                &path,
                &V8Edit {
                    method: V8Method::Put,
                    path: vec!["access".into(), "api-keys".into()],
                    body: body.into_bytes(),
                    yaml: true,
                },
            )
            .unwrap();
        } else {
            let mut config = Config::load(&path).unwrap();
            config.api_keys = vec![value.into()];
            save_preserving_comments(&path, &config, op == "migrate").unwrap();
        }
        assert!(
            Config::load(&path).unwrap().api_keys == [value],
            "{op} changed the item"
        );
    }
}

#[test]
fn review_r14_sequence_items_survive_settings() {
    sequence_values("settings");
}

#[test]
fn review_r14_sequence_items_survive_migration() {
    sequence_values("migrate");
}

#[test]
fn review_r14_sequence_items_survive_put() {
    sequence_values("put");
}

#[test]
fn review_r14_mapping_tab_after_spaces_matches_main_bytes() {
    let value = " \tx\n";
    for settings in [false, true] {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, "auth-dir: /x\n").unwrap();
        if settings {
            let mut config = Config::load(&path).unwrap();
            config.auth_dir = value.into();
            save_preserving_comments(&path, &config, false).unwrap();
        } else {
            update_nested_scalar(&path, &["auth-dir"], value).unwrap();
        }
        let output = std::fs::read(&path).unwrap();
        // Baselines captured from main 1919373e; Go preserves this block.
        if settings {
            assert_eq!(
                sha256_hex(&output),
                "d3173c9e136e256d7fb58266d69fc83c334eae4577e21a556ade7339057df160"
            );
        } else {
            assert_eq!(output, b"auth-dir: |2\n   \tx\n");
        }
        assert!(Config::load(&path).unwrap().auth_dir == value);
    }
}

#[test]
fn review_r14_mapping_children_clear_sequence_context() {
    use super::super::yaml3::encode::{Field, Value};
    let value = " \tx\n";
    let mut mapping = Node::mapping();
    mapping.content.push(Node::scalar(STR_TAG, "text"));
    mapping.content.push(Node::scalar(STR_TAG, value));
    let input = Value::Struct(vec![
        Field::new("items", Value::Seq(vec![Value::str(" x\n\u{2028}")])),
        Field::new(
            "mapping",
            Value::Map(vec![(Value::str("text"), Value::str(value))]),
        ),
        Field::new("node", Value::Node(mapping)),
        Field::new("after", Value::str(value)),
    ]);
    let output = encode::marshal_with_comment_ids(&input).unwrap().0;
    let text = std::str::from_utf8(&output).unwrap();
    assert!(text.contains("after: |4\n     \tx\n"));
    let actual = AnyValue::parse_yaml(text).unwrap();
    let item = AnyValue::Str(value.into());
    let nested = AnyValue::Map(std::collections::BTreeMap::from([(
        "text".into(),
        item.clone(),
    )]));
    let expected = AnyValue::Map(std::collections::BTreeMap::from([
        (
            "items".into(),
            AnyValue::Seq(vec![AnyValue::Str(" x\n\u{2028}".into())]),
        ),
        ("mapping".into(), nested.clone()),
        ("node".into(), nested),
        ("after".into(), item),
    ]));
    assert!(actual == expected);
}

#[test]
fn review_r14_generated_node_sequences_use_item_context() {
    let values = [" x\n\u{2028}", "  x\n\u{2028}", " \t\u{2028}\n"];
    let mut sequence = Node::sequence();
    sequence.content = values
        .iter()
        .map(|value| Node::scalar(STR_TAG, value))
        .collect();
    let output = encode::marshal_with_comment_ids(&encode::Value::Node(sequence))
        .unwrap()
        .0;
    let actual = AnyValue::parse_yaml(std::str::from_utf8(&output).unwrap()).unwrap();
    assert!(
        actual
            == AnyValue::Seq(
                values
                    .into_iter()
                    .map(|value| AnyValue::Str(value.into()))
                    .collect()
            )
    );
}
