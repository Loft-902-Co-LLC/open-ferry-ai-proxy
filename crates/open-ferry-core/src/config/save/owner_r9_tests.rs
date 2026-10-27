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

fn refusals() -> &'static [Value] {
    static ROWS: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| {
        let rows: Vec<Value> = serde_json::from_str(include_str!(
            "../testdata/writer/owner_round9_refusals.json"
        ))
        .unwrap();
        assert_eq!(rows.len(), 158);
        for (group, count) in [("r2", 124), ("r4", 32), ("legacy", 2)] {
            assert_eq!(
                rows.iter().filter(|row| row["group"] == group).count(),
                count
            );
        }
        assert!(rows.iter().all(|row| {
            row["recipe"]["disable_empty"].as_bool().unwrap_or(false)
                ^ row["recipe"]["disable_flow_feet"]
                    .as_bool()
                    .unwrap_or(false)
        }));
        rows
    })
}

pub(in crate::config::save) fn recorded_refusal(
    group: &str,
    case: &str,
    op: &str,
    disabled: &str,
    pass: usize,
) -> bool {
    refusals().iter().any(|row| {
        row["group"] == group
            && row["case"] == case
            && (group == "legacy" || row["op"] == op && row["disable"] == disabled)
            && row["passes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_u64() == Some(pass as u64))
    })
}

#[cfg(feature = "parity")]
#[test]
fn owner_round9_diagnostic_refusals_keep_main_and_pin_unsafe_previews() {
    use std::collections::BTreeMap;
    fn counts(data: &[u8]) -> Option<BTreeMap<String, usize>> {
        fn hash_free_data(node: &Node) -> bool {
            !node.value.contains('#')
                && !node.tag.contains('#')
                && node.content.iter().all(hash_free_data)
        }
        // These fixed fixtures have no hashes in data. Count their physical
        // tokens independently: a parser can remember a pending comment even
        // when the next writer no longer emits its line.
        if !hash_free_data(&unmarshal(data).unwrap()) {
            return None;
        }
        let mut counts = BTreeMap::new();
        for line in std::str::from_utf8(data).unwrap().lines() {
            if let Some(at) = line.find('#') {
                let text = &line[at..];
                if text.contains(['\u{85}', '\u{2028}', '\u{2029}', '\r']) {
                    return None;
                }
                *counts.entry(text.to_owned()).or_default() += 1;
            }
        }
        Some(counts)
    }
    fn native_values(node: &Node) -> Value {
        serde_json::json!([
            format!("{:?}", node.kind),
            node.tag,
            format!("{:?}", node.style),
            node.value,
            node.content.iter().map(native_values).collect::<Vec<_>>()
        ])
    }
    fn result_hash(result: &Result<Vec<u8>, SaveError>) -> Value {
        match result {
            Ok(bytes) => serde_json::json!({"Ok":write::sha256_hex(bytes)}),
            Err(error) => {
                serde_json::json!({"Err":write::sha256_hex(error.to_string().as_bytes())})
            }
        }
    }
    let mut r2_sources: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_r2_cases.json")).unwrap();
    r2_sources.extend(
        serde_json::from_str::<Vec<Value>>(include_str!("../testdata/writer/writer_cases.json"))
            .unwrap(),
    );
    let r4_sources: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_r4_cases.json")).unwrap();
    let legacy_sources = owner_legacy_ops::resolve_sources(
        serde_json::from_str(include_str!("../testdata/writer/owner_legacy.json")).unwrap(),
    );
    for row in refusals() {
        let original = match row["group"].as_str().unwrap() {
            "r2" => &r2_sources
                .iter()
                .find(|r| r["case"] == row["case"])
                .unwrap()["source"],
            "r4" => &r4_sources
                .iter()
                .find(|r| r["case"] == row["case"])
                .unwrap()["source"],
            "legacy" => &legacy_sources
                .iter()
                .find(|r| r["recipe"]["id"] == row["case"])
                .unwrap()["recipe"]["source"],
            _ => unreachable!(),
        };
        assert_eq!(
            row["recipe"]["source"], *original,
            "{}: original fixture",
            row["id"]
        );
        owner_legacy_ops::old_scope(&row["recipe"], || {
            let on = owner_legacy_ops::history(&row["recipe"]);
            let off = tree::without_owner_protection(|| owner_legacy_ops::history(&row["recipe"]));
            assert_eq!(on, off, "{}: frozen writer fallback", row["id"]);
            assert_eq!(
                owner_matrix_tests::receipt(&on),
                row["on"],
                "{}: receipt",
                row["id"]
            );
            let source = row["recipe"]["source"].as_str().unwrap().as_bytes();
            let proposal = row["proposal"].as_str().unwrap().as_bytes();
            let writer = row["writer"].as_str().unwrap().as_bytes();
            let expected_owners = owner_legacy_tests::independent_canonical_owners(source);
            let proposal_owners = owner_legacy_tests::independent_canonical_owners(proposal);
            assert!(
                expected_owners
                    .iter()
                    .all(|(owner, count)| proposal_owners.get(owner) == Some(count)),
                "{}: the prior proposal did not restore source owners",
                row["id"]
            );
            assert_eq!(
                native_values(&unmarshal(proposal).unwrap()),
                native_values(&unmarshal(writer).unwrap())
            );
            let config = Config::load_bytes(proposal).unwrap();
            let source_counts = counts(proposal);
            let mut unsafe_preview = false;
            for frozen in row["previews"].as_array().unwrap() {
                let migrate = frozen["migrate"].as_bool().unwrap();
                let next = render_preserving_comments(proposal, &config, migrate);
                let main = render_preserving_comments(writer, &config, migrate);
                assert_eq!(result_hash(&next), frozen["proposal"]);
                assert_eq!(result_hash(&main), frozen["writer"]);
                match (&next, &main) {
                    (Ok(next), Ok(main))
                        if Config::load_bytes(next).is_ok() && Config::load_bytes(main).is_ok() =>
                    {
                        match (&source_counts, counts(next), counts(main)) {
                            (Some(source), Some(actual), Some(baseline)) => {
                                unsafe_preview |= source.iter().any(|(text, count)| {
                                    actual.get(text).copied().unwrap_or(0)
                                        < (*count).min(baseline.get(text).copied().unwrap_or(0))
                                });
                            }
                            _ => {
                                // Ambiguous raw hashes cannot prove a complete
                                // copy set. Pin the scanner's refusal separately
                                // from the frozen writer outcome above.
                                unsafe_preview |= [proposal, next, main].into_iter().any(|data| {
                                    super::super::super::yaml3::parser::physical_comment_lines(data)
                                        .is_none()
                                });
                            }
                        }
                    }
                    _ => unsafe_preview = true,
                }
            }
            assert!(
                unsafe_preview,
                "{}: no independently unsafe preview",
                row["id"]
            );
        });
    }
}
