use super::*;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8};
use serde_json::Value;

fn raw_then_v8_edit(name: &str) {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/owner_round7.json")).unwrap();
    let row = rows.iter().find(|row| row["case"] == name).unwrap();
    let source = row["source"].as_str().unwrap();
    for method in [V8Method::Put, V8Method::Patch, V8Method::Delete] {
        let dir = crate::config::testing::TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        write_file(&path, source.as_bytes()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let edit = V8Edit {
            method,
            path: if method == V8Method::Patch {
                vec!["oauth".into()]
            } else {
                vec!["oauth".into(), "auth-dir".into()]
            },
            body: match method {
                V8Method::Put => br#""/changed""#.to_vec(),
                V8Method::Patch => br#"{"auth-dir":"/changed"}"#.to_vec(),
                V8Method::Delete => Vec::new(),
            },
            yaml: false,
        };
        edit_v8(&path, &edit).unwrap();
        let output = std::fs::read(&path).unwrap();
        assert_eq!(
            tree::physical_comment_counts(&output)
                .unwrap()
                .get(b"# c1".as_slice()),
            Some(&1),
            "{name}: {method:?} lost the comment kept by main"
        );
        Config::load_bytes(&output).unwrap();
        assert_eq!(raw, row["main_raw"].as_str().unwrap(), "{name}");
    }
}

#[test]
fn owner_round7_raw_then_edit_flow_excluded_models() {
    raw_then_v8_edit("p1");
}

#[test]
fn owner_round7_raw_then_edit_block_excluded_models() {
    raw_then_v8_edit("p2");
}

#[test]
fn owner_round7_raw_then_edit_disable_cooling() {
    raw_then_v8_edit("p3");
}

#[test]
fn owner_round7_raw_then_edit_headers() {
    raw_then_v8_edit("p4");
}

#[test]
fn owner_round7_raw_then_edit_claude_key() {
    raw_then_v8_edit("p5");
}

#[test]
fn owner_round7_raw_over_budget_keeps_writer_bytes() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/owner_round6.json")).unwrap();
    let mut source = rows.iter().find(|row| row["case"] == "fff").unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned();
    source.extend(std::iter::repeat_n(
        '\n',
        RAW_COMMENT_OWNER_BUDGET + 1 - source.len(),
    ));
    assert_eq!(source.len(), RAW_COMMENT_OWNER_BUDGET + 1);
    let writer = tree::without_owner_protection(|| render_write_file(source.as_bytes())).unwrap();
    let actual = render_write_file(source.as_bytes()).unwrap();
    assert_eq!(actual, writer);
}

#[test]
fn owner_round7_raw_output_crossing_budget_keeps_writer_bytes() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/owner_round6.json")).unwrap();
    let mut source = rows.iter().find(|row| row["case"] == "fff").unwrap()["source"]
        .as_str()
        .unwrap()
        .to_owned();
    source.push_str("plugins:\n  configs:\n    padding:\n      values: [");
    source.push_str(&"x,".repeat(32_000));
    source.push_str("x]\n");
    assert!(source.len() < RAW_COMMENT_OWNER_BUDGET);
    let writer = tree::without_owner_protection(|| render_write_file(source.as_bytes())).unwrap();
    assert!(writer.len() > RAW_COMMENT_OWNER_BUDGET);
    let actual = render_write_file(source.as_bytes()).unwrap();
    assert!(actual == writer, "raw repair would cross the repeat budget");
    let main_repeat = tree::without_owner_protection(|| render_write_file(&writer)).unwrap();
    let repeated = render_write_file(&actual).unwrap();
    assert!(
        repeated == main_repeat,
        "over-budget repeat changed the writer's bytes"
    );
}

#[test]
fn owner_round7_preview_reuse_keeps_legacy_saves_distinct() {
    let source = b"auth-dir: /x # setting\n";
    let previews = raw_feet_survive_following_saves(source, true).unwrap();
    assert!(previews.outputs[0] != previews.outputs[1]);
    for (index, output) in previews.outputs.iter().enumerate() {
        assert!(
            *output == render_preserving_comments(source, &previews.config, index == 1).unwrap()
        );
    }
    let source = b"config-version: 8\noauth:\n  auth-dir: /x # setting\n";
    let previews = raw_feet_survive_following_saves(source, false).unwrap();
    for (index, output) in previews.outputs.iter().enumerate() {
        assert!(
            *output == render_preserving_comments(source, &previews.config, index == 1).unwrap()
        );
    }
}

#[test]
fn owner_round7_preview_only_needs_writer_for_a_source_deficit() {
    use std::cell::Cell;
    let source = b"auth-dir: /x # same\n";
    let counts = tree::physical_comment_counts(source).unwrap();
    let writer_called = Cell::new(false);
    assert!(raw_preview_keeps_comment_counts(&counts, source, || {
        writer_called.set(true);
        Ok(Vec::new())
    }));
    assert!(!writer_called.get());
    assert!(!raw_preview_keeps_comment_counts(
        &counts,
        b"auth-dir: /x\n",
        || {
            writer_called.set(true);
            Ok(source.to_vec())
        },
    ));
    assert!(writer_called.get());
}
