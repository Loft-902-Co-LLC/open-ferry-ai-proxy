use super::*;
use crate::config::testing::TempDir;

#[test]
fn review_r10_plain_source_unicode_values_reload_after_each_write_path() {
    for separator in ['\u{2028}', '\u{2029}'] {
        let value = format!("/a{separator}b");
        for op in ["save", "migrate", "nested"] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, "auth-dir: /old\n").unwrap();
            if op == "nested" {
                update_nested_scalar(&path, &["auth-dir"], &value).unwrap();
            } else {
                let mut config = Config::load(&path).unwrap();
                config.auth_dir.clone_from(&value);
                save_preserving_comments(&path, &config, op == "migrate").unwrap();
            }
            assert_eq!(Config::load(&path).unwrap().auth_dir, value, "{op}");
        }
    }
}

#[test]
fn review_r10_exact_unicode_sources_preserve_go_values_after_writing() {
    for line in [
        "    auth-dir: '/a\u{2028}        b'",
        "    auth-dir: '/a\u{2028}b'",
        "  auth-dir: '/a\u{2028}  b'",
        "    auth-dir: '/a\u{2028}    b'",
        "    auth-dir: /a\u{2028}        b",
        "    auth-dir: \"/a\u{2028}        b\"",
    ] {
        let source = format!("config-version: 8\noauth:\n{line}\n");
        let before = Config::load_bytes(source.as_bytes()).unwrap().auth_dir;
        for op in ["raw", "normalize"] {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            std::fs::write(&path, &source).unwrap();
            let result = if op == "raw" {
                write_file(&path, source.as_bytes())
            } else {
                normalize_config_layout(source.as_bytes(), true).map(|(out, _)| {
                    std::fs::write(&path, out).unwrap();
                })
            };
            result.unwrap();
            assert_eq!(
                Config::load(&path).unwrap().auth_dir,
                before,
                "{op} {line:?}"
            );
        }
    }
}

#[test]
fn review_r10_live_unicode_emission_matches_the_faithful_go_encoder() {
    for separator in ['\u{0085}', '\u{2028}', '\u{2029}'] {
        for tail in ["b", "        b", "\nb", ""] {
            let value = format!("/a{separator}{tail}");
            let scalar = Node::scalar(STR_TAG, &value);
            assert_eq!(
                marshal_config(&scalar).unwrap(),
                encode::faithful_node(&scalar, 2).unwrap()
            );
            let generated = encode::Value::Str(value);
            assert_eq!(
                encode::marshal_with_comment_ids(&generated).unwrap().0,
                encode::marshal(&generated).unwrap()
            );
        }
    }
}
