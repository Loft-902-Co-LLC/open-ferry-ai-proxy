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
fn review_r10_exact_unicode_sources_preserve_loaded_values_or_refuse() {
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
            if let Err(error) = result {
                assert_eq!(error.kind(), SaveErrorKind::Unwritable);
                assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
            }
            assert_eq!(
                Config::load(&path).unwrap().auth_dir,
                before,
                "{op} {line:?}"
            );
        }
    }
}
