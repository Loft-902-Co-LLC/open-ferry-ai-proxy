use super::*;

#[test]
fn writer_r2_other_yaml_breaks_do_not_enable_foot_indentation() {
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/writer/writer_cases.json")).unwrap();
    let base = cases.iter().find(|c| c["case"] == "flow_two_feet").unwrap()["source"]
        .as_str()
        .unwrap();
    for separator in ["\u{85}", "\u{2028}", "\u{2029}", "\r"] {
        let source = format!("{base}# tail{separator}");
        let expected = source
            .replace("      # f0", "# f0")
            .replace("      # f1", "# f1");
        assert_eq!(
            super::super::sha256_hex(&normalize_comment_indentation(source.as_bytes())),
            super::super::sha256_hex(expected.as_bytes())
        );
    }
}
