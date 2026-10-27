use super::*;
use serde_json::Value;

fn native_values(node: &Node) -> Value {
    serde_json::json!([
        format!("{:?}", node.kind),
        node.tag,
        node.value,
        format!("{:?}", node.style),
        node.content.iter().map(native_values).collect::<Vec<_>>()
    ])
}

fn fixture(name: &str) -> Value {
    serde_json::from_str::<Vec<Value>>(include_str!("../testdata/writer/owner_round10.json"))
        .unwrap()
        .into_iter()
        .find(|row| row["case"] == name)
        .unwrap()
}

fn layout_history(name: &str) {
    let row = fixture(name);
    let source = row["source"].as_str().unwrap().as_bytes();
    let first = || match row["op"].as_str().unwrap() {
        "nested" => render_nested_scalar(source, &["auth-dir"], "/probe").unwrap(),
        "save" => {
            render_preserving_comments(source, &Config::parse(source).unwrap(), false).unwrap()
        }
        _ => unreachable!(),
    };
    let main = tree::without_owner_protection(first);
    assert_eq!(main, row["main_first"].as_str().unwrap().as_bytes());
    let output = first();
    assert_eq!(
        output, main,
        "unsafe layout repair must keep writer placement"
    );
    let (layout, _) = normalize_config_layout(&output, true).unwrap();
    let (main_layout, _) =
        tree::without_owner_protection(|| normalize_config_layout(&main, true)).unwrap();
    assert_eq!(main_layout, row["main_layout"].as_str().unwrap().as_bytes());
    let source_counts = tree::physical_comment_counts(&output).unwrap();
    let main_counts = tree::physical_comment_counts(&main_layout).unwrap();
    let counts = tree::physical_comment_counts(&layout).unwrap();
    assert!(source_counts.iter().all(|(text, n)| {
        counts.get(text).copied().unwrap_or(0)
            >= (*n).min(main_counts.get(text).copied().unwrap_or(0))
    }));
    assert_eq!(
        native_values(&unmarshal(&layout).unwrap()),
        native_values(&unmarshal(&main_layout).unwrap())
    );
    Config::load_bytes(&layout).unwrap();
}

#[test]
fn owner_round10_nested_repair_previews_v8_layout() {
    layout_history("random-30-28");
}

#[test]
fn owner_round10_save_repair_previews_v8_layout() {
    layout_history("random-30-274");
}

#[test]
fn owner_round10_raw_preview_counts_expanded_alias_copies() {
    let row = refusals()
        .iter()
        .find(|row| row["group"] == "legacy")
        .unwrap();
    let source = row["input"].as_str().unwrap().as_bytes();
    let proposal = row["proposal"].as_str().unwrap().as_bytes();
    let writer = row["writer"].as_str().unwrap().as_bytes();
    let original = tree::physical_comment_counts(source).unwrap();
    let expanded = tree::physical_comment_counts(proposal).unwrap();
    assert!(
        expanded
            .iter()
            .any(|(text, n)| { *n == 2 && original.get(text).copied().unwrap_or(0) == 1 })
    );
    assert_eq!(
        native_values(&unmarshal(proposal).unwrap()),
        native_values(&unmarshal(writer).unwrap())
    );
    // Real following saves keep both copies; the layout step keeps only one
    // at the repaired placement, while the writer placement retains both.
    // A cap of one from the unexpanded input would excuse that lost copy.
    assert!(!raw_comments_survive_following_saves(
        source, writer, proposal, true, None,
    ));
}

fn patch() -> crate::config::v8_edit::V8Edit {
    crate::config::v8_edit::V8Edit {
        method: crate::config::v8_edit::V8Method::Patch,
        path: Vec::new(),
        body: br#"{"server":{"port":8319}}"#.to_vec(),
        yaml: false,
    }
}

fn unsafe_following_save(row: &Value) {
    let writer = row["writer"].as_str().unwrap().as_bytes();
    let proposal = row["proposal"].as_str().unwrap().as_bytes();
    assert_ne!(writer, proposal);
    assert_eq!(
        native_values(&unmarshal(writer).unwrap()),
        native_values(&unmarshal(proposal).unwrap())
    );
    let config = Config::parse(proposal).unwrap();
    let counts = tree::physical_comment_counts(proposal).unwrap();
    let mut unsafe_preview = false;
    for migrate in [false, true] {
        let proposed = candidate_preserving_comments(proposal, &config, migrate);
        let baseline = candidate_preserving_comments(writer, &config, migrate);
        match (proposed, baseline) {
            (Ok((next, _)), Ok((main, _))) => {
                let actual = tree::physical_comment_counts(&next.bytes).unwrap();
                let retained = tree::physical_comment_counts(&main.bytes).unwrap();
                unsafe_preview |= counts.iter().any(|(text, n)| {
                    actual.get(text).copied().unwrap_or(0)
                        < (*n).min(retained.get(text).copied().unwrap_or(0))
                });
            }
            _ => unsafe_preview = true,
        }
    }
    assert!(
        unsafe_preview,
        "fixture must independently expose an unsafe following save"
    );
}

#[test]
fn owner_round10_layout_repair_checks_its_following_saves() {
    let row = fixture("layout-random-30-1186");
    unsafe_following_save(&row);
    let source = row["source"].as_str().unwrap().as_bytes();
    let writer = row["writer"].as_str().unwrap().as_bytes();
    let output = normalize_config_layout(source, true).unwrap().0;
    assert_eq!(output, writer);
    assert_eq!(
        tree::without_owner_protection(|| normalize_config_layout(source, true))
            .unwrap()
            .0,
        writer
    );
    Config::load_bytes(&output).unwrap();
}

#[test]
fn owner_round10_prepared_edit_checks_its_following_saves() {
    let row = fixture("prepared-random-30-116");
    unsafe_following_save(&row);
    let source = row["source"].as_str().unwrap().as_bytes();
    let writer = row["writer"].as_str().unwrap().as_bytes();
    let output = crate::config::v8_edit::preview_v8(source, &patch()).unwrap();
    assert_eq!(output, writer);
    assert_eq!(
        tree::without_owner_protection(|| crate::config::v8_edit::preview_v8(source, &patch()))
            .unwrap(),
        writer
    );
    Config::load_bytes(&output).unwrap();
}

#[test]
fn owner_round10_migration_preview_is_independent_of_no_change_save() {
    let row = fixture("migration-random-30-224");
    let writer = row["writer"].as_str().unwrap().as_bytes();
    let proposal = row["proposal"].as_str().unwrap().as_bytes();
    let document = unmarshal(proposal).unwrap();
    assert!(yaml_path(document.content.first().unwrap(), "config-version").is_none());
    assert_eq!(
        native_values(&document),
        native_values(&unmarshal(writer).unwrap())
    );
    let config = Config::parse(proposal).unwrap();
    let counts = tree::physical_comment_counts(proposal).unwrap();
    for (index, migrate) in [false, true].into_iter().enumerate() {
        let next = candidate_preserving_comments(proposal, &config, migrate)
            .unwrap()
            .0
            .bytes;
        let main = candidate_preserving_comments(writer, &config, migrate)
            .unwrap()
            .0
            .bytes;
        assert_eq!(next, row["previews"][index].as_str().unwrap().as_bytes());
        assert_eq!(
            main,
            row["writer_previews"][index].as_str().unwrap().as_bytes()
        );
        let actual = tree::physical_comment_counts(&next).unwrap();
        let retained = tree::physical_comment_counts(&main).unwrap();
        let lost = counts.iter().any(|(text, n)| {
            actual.get(text).copied().unwrap_or(0)
                < (*n).min(retained.get(text).copied().unwrap_or(0))
        });
        assert_eq!(lost, migrate, "only migration loses a retained copy");
    }
    let layout = v8::normalize_config_layout_writer(proposal, true)
        .unwrap()
        .0;
    let main_layout = v8::normalize_config_layout_writer(writer, true).unwrap().0;
    let actual = tree::physical_comment_counts(&layout).unwrap();
    let retained = tree::physical_comment_counts(&main_layout).unwrap();
    assert!(
        counts.iter().all(|(text, n)| {
            actual.get(text).copied().unwrap_or(0)
                >= (*n).min(retained.get(text).copied().unwrap_or(0))
        }),
        "the layout preview must not mask the missing migration check"
    );
    assert!(!following_saves_keep_comments(
        writer, proposal, &counts, &config, true
    ));
    assert!(!repair_survives_following_saves(writer, proposal));
}

fn refusals() -> &'static [Value] {
    static ROWS: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../testdata/writer/owner_round10_refusals.json"
        ))
        .unwrap()
    })
}

pub(in crate::config::save) fn recorded_refusal(
    group: &str,
    case: &str,
    op: &str,
    disable: &str,
    pass: usize,
) -> bool {
    refusals().iter().any(|row| {
        row["group"] == group
            && row["case"] == case
            && (group == "legacy" || row["op"] == op && row["disable"] == disable)
            && row["passes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_u64() == Some(pass as u64))
    })
}

#[test]
#[cfg(feature = "parity")]
fn owner_round10_new_refusals_pin_their_unsafe_layout_previews() {
    let rows = refusals();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "r2/random-30-1826/nested/empty",
            "r2/random-30-1826/nested/feet",
            "legacy/aliases/merge-outside-plugin"
        ]
    );
    for row in rows {
        let input = row["input"].as_str().unwrap().as_bytes();
        let writer = row["writer"].as_str().unwrap().as_bytes();
        let proposal = row["proposal"].as_str().unwrap().as_bytes();
        assert_eq!(input, row["recipe"]["source"].as_str().unwrap().as_bytes());
        if row["group"] == "legacy" {
            let catalog = owner_legacy_ops::resolve_sources(
                serde_json::from_str(include_str!("../testdata/writer/owner_legacy.json")).unwrap(),
            );
            let original = catalog
                .iter()
                .find(|r| r["recipe"]["id"] == row["case"])
                .unwrap();
            assert_eq!(original["recipe"], row["recipe"]);
        } else {
            let mut catalog: Vec<Value> =
                serde_json::from_str(include_str!("../testdata/writer/writer_r2_cases.json"))
                    .unwrap();
            catalog.extend(
                serde_json::from_str::<Vec<Value>>(include_str!(
                    "../testdata/writer/writer_cases.json"
                ))
                .unwrap(),
            );
            let original = catalog.iter().find(|r| r["case"] == row["case"]).unwrap();
            assert_eq!(original["source"], row["recipe"]["source"]);
            assert_eq!(row["recipe"]["op"], "nested");
            assert_eq!(row["recipe"]["disable_empty"], row["disable"] == "empty");
            assert_eq!(row["recipe"]["disable_flow_feet"], row["disable"] == "feet");
        }
        assert!(owner_legacy_tests::independent_source_owners_held(
            input, proposal
        ));
        assert_eq!(
            native_values(&unmarshal(writer).unwrap()),
            native_values(&unmarshal(proposal).unwrap())
        );
        owner_legacy_ops::old_scope(&row["recipe"], || {
            let op = owner_legacy_ops::operation(input, &row["recipe"], 0).unwrap();
            assert_eq!(op.run(input).unwrap(), writer);
            assert_eq!(
                tree::without_owner_protection(|| op.run(input)).unwrap(),
                writer
            );
            let counts = tree::physical_comment_counts(proposal).unwrap();
            let config = Config::parse(proposal).unwrap();
            for (index, migrate) in [false, true].into_iter().enumerate() {
                let next = candidate_preserving_comments(proposal, &config, migrate)
                    .unwrap()
                    .0
                    .bytes;
                let main = candidate_preserving_comments(writer, &config, migrate)
                    .unwrap()
                    .0
                    .bytes;
                assert_eq!(
                    write::sha256_hex(&next),
                    row["bare"][index]["proposal"]["Ok"]
                );
                assert_eq!(write::sha256_hex(&main), row["bare"][index]["writer"]["Ok"]);
                let actual = tree::physical_comment_counts(&next).unwrap();
                let retained = tree::physical_comment_counts(&main).unwrap();
                assert!(counts.iter().all(|(text, n)| {
                    actual.get(text).copied().unwrap_or(0)
                        >= (*n).min(retained.get(text).copied().unwrap_or(0))
                }));
            }
            let next = v8::normalize_config_layout_writer(proposal, true)
                .unwrap()
                .0;
            let main = v8::normalize_config_layout_writer(writer, true).unwrap().0;
            assert_eq!(write::sha256_hex(&next), row["layout"]["proposal"]["Ok"]);
            assert_eq!(write::sha256_hex(&main), row["layout"]["writer"]["Ok"]);
            let actual = tree::physical_comment_counts(&next).unwrap();
            let retained = tree::physical_comment_counts(&main).unwrap();
            assert!(counts.iter().any(|(text, n)| {
                actual.get(text).copied().unwrap_or(0)
                    < (*n).min(retained.get(text).copied().unwrap_or(0))
            }));
            Config::load_bytes(writer).unwrap();
        });
    }
}
