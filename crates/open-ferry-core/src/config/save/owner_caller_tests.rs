use super::*;
use serde_json::Value;
#[test]
fn owner_both_edit_callers_match_main_when_disabled_and_keep_their_controls() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../testdata/writer/owner_callers.json")).unwrap();
    let rows = owner_legacy_ops::resolve_sources(rows);
    assert_eq!(rows.len(), 116);
    for row in rows {
        let recipe = &row["recipe"];
        let id = recipe["id"].as_str().unwrap();
        let disabled = tree::without_owner_protection(|| owner_legacy_ops::caller_history(recipe));
        assert!(
            super::owner_matrix_tests::receipt(&disabled) == row["main"],
            "{id}: both callers' frozen main bytes/errors: {} vs {}",
            super::owner_matrix_tests::receipt(&disabled),
            row["main"]
        );
        let enabled = owner_legacy_ops::caller_history(recipe);
        assert!(
            super::owner_matrix_tests::receipt(&enabled) == row["main"],
            "{id}: faithful controls or main fallback"
        );
    }
}
