use super::*;
use serde_json::Value;

fn fixture(name: &str) -> Value {
    serde_json::from_str::<Vec<Value>>(include_str!("../testdata/writer/owner_round9.json"))
        .unwrap()
        .into_iter()
        .find(|row| row["case"] == name)
        .unwrap()
}

fn history(row: &Value) -> Vec<Vec<u8>> {
    let options = &row["options"];
    let mut data = options["file"].as_str().unwrap().as_bytes().to_vec();
    let mut snapshots = Vec::new();
    for step in options["steps"].as_array().unwrap() {
        data = match step["op"].as_str().unwrap() {
            "save" => {
                let cfg = Config::parse(
                    step["config"]
                        .as_str()
                        .unwrap_or(std::str::from_utf8(&data).unwrap()),
                )
                .unwrap();
                render_preserving_comments(&data, &cfg, step["migrate"].as_bool().unwrap_or(false))
                    .unwrap()
            }
            "nested" => render_nested_scalar(
                &data,
                &step["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect::<Vec<_>>(),
                step["value"].as_str().unwrap(),
            )
            .unwrap(),
            _ => unreachable!(),
        };
        Config::load_bytes(&data).unwrap();
        snapshots.push(data.clone());
    }
    snapshots
}

fn retained_through_history(name: &str) {
    let row = fixture(name);
    let source = row["options"]["file"].as_str().unwrap().as_bytes();
    let mut input = source.to_vec();
    for (step, snapshot) in history(&row).into_iter().enumerate() {
        let counts = tree::physical_comment_counts(&input).unwrap();
        let actual = tree::physical_comment_counts(&snapshot).unwrap();
        assert!(
            counts.iter().all(|(text, count)| {
                let go = row["go_comment_counts"][step]
                    .get(std::str::from_utf8(text).unwrap())
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or_else(|| {
                        if row["go_comment_counts"].is_null() {
                            *count
                        } else {
                            0
                        }
                    });
                actual.get(text).copied().unwrap_or(0) >= (*count).min(go)
            }),
            "{name}: a surviving owner's comment was lost"
        );
        input = snapshot;
    }
}

pub(super) fn unsafe_nested_repair_is_refused(data: &[u8]) {
    let (candidate, source) = candidate_nested_scalar(data, &["auth-dir"], "/x").unwrap();
    let writer = candidate.bytes.clone();
    let proposal = tree::finish_comment_owners(&source, candidate, |data| {
        let (candidate, source) = candidate_nested_scalar(data, &["auth-dir"], "/x").ok()?;
        Some(tree::finish_comment_owners_replay(&source, candidate))
    });
    assert!(proposal != writer, "fixture did not exercise a repair");
    assert!(!repair_survives_following_saves(&writer, &proposal));
    assert_eq!(
        render_nested_scalar(data, &["auth-dir"], "/x").unwrap(),
        writer
    );
    assert_eq!(
        writer,
        tree::without_owner_protection(|| render_nested_scalar(data, &["auth-dir"], "/x")).unwrap()
    );
}

#[test]
fn owner_round9_flow_collection_comment_survives_later_saves() {
    retained_through_history("minimal-random-38-909");
}

#[test]
fn owner_round9_explicit_key_comment_survives_migration() {
    retained_through_history("random-30-991");
}

#[test]
fn owner_round9_only_required_foot_separators_are_emitted() {
    let row = fixture("isolated-1216");
    let result = history(&row);
    let output = result.last().unwrap();
    assert_eq!(
        write::sha256_hex(output),
        row["expected_sha256"].as_str().unwrap()
    );
    let lines: Vec<_> = std::str::from_utf8(output).unwrap().lines().collect();
    assert_eq!(lines[51], "");
    assert_eq!(lines[80], "");
    assert_eq!(lines[99].trim_start(), "model-alias:");
}
