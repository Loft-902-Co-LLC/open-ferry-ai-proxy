//! Exact upstream v8.0.42 / Go 1.26.10 typed-save byte fixtures.

use super::{Config, save, testing::TempDir};

fn fixtures() -> serde_json::Value {
    serde_json::from_str(include_str!("testdata/flow-comment-save.json")).unwrap()
}

fn assert_matches_upstream(name: &str) {
    let fixtures = fixtures();
    let row = fixtures["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap();
    let dir = TempDir::new();
    let path = dir.write("config.yaml", row["source"].as_str().unwrap());
    let cfg = match row["config"].as_str() {
        Some(text) => Config::parse(text.as_bytes()).unwrap(),
        None => Config::load(&path).unwrap(),
    };
    save::save_preserving_comments(&path, &cfg, false).unwrap();
    assert_eq!(
        std::fs::read(path).unwrap(),
        row["go"].as_str().unwrap().as_bytes(),
        "{name}"
    );
}

#[test]
fn reported_flow_comment_loss_matches_upstream() {
    assert_matches_upstream("reported");
}
#[test]
fn outer_flow_mapping_comments_match_upstream() {
    assert_matches_upstream("outer_mapping");
}
#[test]
fn outer_flow_sequence_comments_match_upstream() {
    assert_matches_upstream("outer_sequence");
}
#[test]
fn nested_flow_mapping_comments_match_upstream() {
    assert_matches_upstream("nested_mapping");
}
#[test]
fn nested_flow_sequence_comments_match_upstream() {
    assert_matches_upstream("nested_sequence");
}
#[test]
fn changed_flow_mapping_comments_match_upstream() {
    assert_matches_upstream("changed_mapping");
}
#[test]
fn unchanged_flow_mapping_comments_match_upstream() {
    assert_matches_upstream("unchanged_mapping");
}
#[test]
fn changed_flow_sequence_comments_match_upstream() {
    assert_matches_upstream("changed_sequence");
}
#[test]
fn unchanged_flow_sequence_comments_match_upstream() {
    assert_matches_upstream("unchanged_sequence");
}
#[test]
fn non_flow_control_matches_upstream() {
    assert_matches_upstream("non_flow");
}

#[test]
fn two_saves_of_upstream_output_match_upstream_and_are_stable() {
    for row in fixtures()["cases"].as_array().unwrap() {
        let name = row["name"].as_str().unwrap();
        let dir = TempDir::new();
        // Go's initial v8 save can reorder once more. Start at its recorded
        // output and require the next two no-change saves to be byte-stable.
        let path = dir.write("config.yaml", row["go"].as_str().unwrap());
        let mut previous = None;
        for _ in 0..2 {
            let cfg = Config::load(&path).unwrap();
            save::save_preserving_comments(&path, &cfg, false).unwrap();
            let saved = std::fs::read(&path).unwrap();
            assert_eq!(
                saved,
                row["go_resave"].as_str().unwrap().as_bytes(),
                "{name}"
            );
            if let Some(previous) = previous {
                assert_eq!(saved, previous, "second save: {name}");
            }
            previous = Some(saved);
        }
    }
}
