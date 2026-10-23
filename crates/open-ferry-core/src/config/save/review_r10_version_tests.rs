use super::review_r10_tests::outputs;
use super::*;
use crate::config::testing::TempDir;

#[test]
fn review_r10_lossless_version_paragraphs_keep_go_bytes_on_every_operation() {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../testdata/config-save-r10-version-go.json")).unwrap();
    for row in rows {
        let out = outputs(row["source"].as_str().unwrap(), row["op"].as_str().unwrap())
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            out,
            row["go"].as_str().unwrap(),
            "{} {}",
            row["case"],
            row["op"]
        );
    }
}
#[test]
fn review_r10_version_paragraph_repairs_keep_each_owner() {
    for source in [
        "models: {}\nplugins: {}\n\n# k1\n\n# k2\nconfig-version: 8\n",
        "models: {}\nplugins: {}\n\n# k1\n\n# k2\nconfig-version:\n  # v1\n  8\n",
        "request-retry: 3\n\n# k1\n\n# k2\nconfig-version: 8\nmodels: {}\n",
    ] {
        for op in [
            "raw",
            "raw2",
            "normalize",
            "normalize2",
            "save",
            "save2",
            "migrate",
            "migrate2",
            "put",
            "patch",
        ] {
            for out in outputs(source, op).unwrap() {
                let doc = unmarshal(out.as_bytes()).unwrap();
                let root = &doc.content[0];
                let at = find_map_key_index(root, "config-version").unwrap();
                for comment in ["# k1", "# k2", "# v1"]
                    .into_iter()
                    .filter(|c| source.contains(c))
                {
                    assert!(
                        root.content[at]
                            .head_comment
                            .lines()
                            .any(|line| line == comment),
                        "{op}: {out}"
                    );
                    assert_eq!(parity::comment_occurrences(&out, comment, false), Some(1));
                }
            }
        }
    }
}
#[test]
fn review_r10_version_paragraph_separator_is_stable_after_two_saves() {
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(
        &path,
        "models:\n  a: 1\n  # mfoot\nconfig-version:\n  # v1\n\n  # v2\n  8\n",
    )
    .unwrap();
    let mut config = Config::load(&path).unwrap();
    config.auth_dir = "/x".into();
    save_preserving_comments(&path, &config, false).unwrap();
    let first = std::fs::read_to_string(&path).unwrap();
    save_preserving_comments(&path, &Config::load(&path).unwrap(), false).unwrap();
    let second = std::fs::read_to_string(&path).unwrap();
    assert_eq!(first, second);
    assert!(!first.contains("\n\n\n"), "{first}");
    let parsed = unmarshal(first.as_bytes()).unwrap();
    let root = &parsed.content[0];
    let i = find_map_key_index(root, "config-version").unwrap();
    assert!(
        root.content[i]
            .head_comment
            .trim()
            .ends_with("# v1\n\n# v2")
    );
    for comment in ["# mfoot", "# v1", "# v2"] {
        assert_eq!(parity::comment_occurrences(&first, comment, false), Some(1));
    }
}
