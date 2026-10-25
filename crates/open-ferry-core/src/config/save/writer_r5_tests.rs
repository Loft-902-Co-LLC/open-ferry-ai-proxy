use super::*;

#[test]
fn writer_r5_duplicate_foot_text_keeps_the_following_head() {
    for case in [
        "w_dup_moved_eq_head",
        "w_dup_first_eq_head",
        "w_head_eq_written",
    ] {
        let source = input(case);
        let doc = unmarshal(source.as_bytes()).unwrap();
        assert!(
            yaml_path(&doc.content[0], "config-version").is_none()
                && !is_v8_config_layout(&doc.content[0]).unwrap(),
            "{case} must exercise a legacy file"
        );
        for op in ["nested", "nested2", "raw", "raw2"] {
            let result = op_run(&source, op, true);
            check_owners_values_and_repeat(case, op, &result);
        }
    }
}
