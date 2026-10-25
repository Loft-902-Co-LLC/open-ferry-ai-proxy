use super::*;

#[test]
fn writer_r4_deep_root_partial_repair_keeps_its_repeat_stability() {
    let source = input("s3_root_deep_seq_col6");
    let expected_owners: Value =
        serde_json::from_str(include_str!("../testdata/writer/writer_r5_owners.json")).unwrap();
    for op in ["nested2", "raw2"] {
        let result = op_run(&source, op, false);
        assert!(
            !result["refused"].as_bool().unwrap_or(false),
            "deep root {op} refuses"
        );
        assert_eq!(result["files"].as_array().unwrap().len(), 2);
        for file in result["files"].as_array().unwrap() {
            let actual_owners: Vec<_> = inventory(file.as_str().unwrap())
                .into_iter()
                .map(|((path, slot, line), count)| json!([path, slot, line, count]))
                .collect();
            assert!(
                Value::Array(actual_owners) == expected_owners[op],
                "deep root {op} changed the frozen owner inventory"
            );
            let mut expected = Config::parse(&source).unwrap();
            if op == "nested2" {
                expected.auth_dir = "/x".into();
            }
            assert!(
                config_values(&Config::parse(file.as_str().unwrap()).unwrap())
                    == config_values(&expected),
                "deep root {op} changed a value"
            );
        }
        assert!(
            result["files"][0] == result["files"][1],
            "deep root {op} did not settle"
        );
    }
}

#[test]
fn writer_r4_source_repair_requires_eligible_emitted_item_feet() {
    let source = unmarshal(input("s3_head_col8").as_bytes()).unwrap();
    for compact in [false, true] {
        let mut source = source.clone();
        if compact {
            yaml_path_mut(&mut source.content[0], "plugins.configs.demo.items")
                .unwrap()
                .style |= crate::config::yaml3::Style::FLOW;
        }
        let emitted =
            String::from_utf8(crate::config::yaml3::encode::marshal_node(&source, 2).unwrap())
                .unwrap();
        let emitted = if compact {
            emitted
        } else {
            emitted
                .split_inclusive('\n')
                .filter(|line| line.trim() != "# f0")
                .collect::<String>()
        };
        let actual = super::super::super::super::tree::source_flow_item_heads(
            &source,
            emitted.as_bytes().to_vec(),
        );
        assert!(
            actual == emitted.as_bytes(),
            "unsupported emitted feet changed the previous bytes"
        );
    }
}

#[cfg(feature = "parity")]
#[test]
fn writer_r4_source_repair_respects_each_switch() {
    use parity::ablation::{Protection, with_disabled};
    let source = unmarshal(input("s3_head_col8").as_bytes()).unwrap();
    let emitted = crate::config::yaml3::encode::marshal_node(&source, 2).unwrap();
    for protection in [
        Protection::MultipleFlowItemFeet,
        Protection::EmptyMappingPendingComment,
    ] {
        let actual = with_disabled(&[protection], || {
            super::super::super::super::tree::source_flow_item_heads(&source, emitted.clone())
        });
        assert!(
            actual == emitted,
            "disabled {protection:?} source repair changed the previous bytes"
        );
    }
}

#[test]
fn writer_r4_source_repair_requires_the_exact_comment_split() {
    let source = unmarshal(input("s3_head_col8").as_bytes()).unwrap();
    let emitted =
        String::from_utf8(crate::config::yaml3::encode::marshal_node(&source, 2).unwrap()).unwrap();
    for (from, to) in [
        ("# r1", "# other foot"),
        ("# r2", "# other head"),
        (
            "      empty: {}",
            "      # additional head\n      empty: {}",
        ),
    ] {
        assert_eq!(emitted.matches(from).count(), 1);
        let emitted = emitted.replacen(from, to, 1);
        let actual = super::super::super::super::tree::source_flow_item_heads(
            &source,
            emitted.as_bytes().to_vec(),
        );
        assert!(
            actual == emitted.as_bytes(),
            "unmatched comment split changed the previous bytes"
        );
    }
}
