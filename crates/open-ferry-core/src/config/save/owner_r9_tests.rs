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
    let counts = tree::physical_comment_counts(source).unwrap();
    for snapshot in history(&row) {
        let actual = tree::physical_comment_counts(&snapshot).unwrap();
        // The reduced sources contain only surviving owners. Include every
        // source text, rather than recognizing the named regression comment.
        assert!(
            counts
                .iter()
                .all(|(text, count)| actual.get(text).unwrap_or(&0) >= count),
            "{name}: a surviving owner's comment was lost"
        );
    }
}

#[test]
fn owner_round9_flow_collection_comment_survives_later_saves() {
    retained_through_history("minimal-random-38-909");
}

#[test]
fn owner_round9_explicit_key_comment_survives_migration() {
    retained_through_history("minimal-random-30-991");
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
