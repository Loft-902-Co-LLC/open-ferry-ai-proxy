use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8, preview_v8};

#[test]
fn review_r9_unicode_sources_preserve_loader_values_or_refuse_without_writing() {
    for separator in ['\u{2028}', '\u{2029}', '\u{0085}'] {
        for style in ["single", "double", "plain"] {
            for tail in ["        b", "", "\n        b"] {
                for nested in [false, true] {
                    let raw = format!("/a{separator}{tail}");
                    let value = match style {
                        "single" => format!("'{raw}'"),
                        "double" => format!("\"{raw}\""),
                        _ => raw,
                    };
                    let source = if nested {
                        format!("config-version: 8\noauth:\n    auth-dir: {value}\n")
                    } else {
                        format!("auth-dir: {value}\n")
                    };
                    let before = Config::load_bytes(source.as_bytes()).unwrap();
                    for op in ["raw", "save", "migrate", "put"] {
                        let dir = TempDir::new();
                        let path = dir.join("config.yaml");
                        std::fs::write(&path, &source).unwrap();
                        let result = match op {
                            "raw" => write_file(&path, source.as_bytes())
                                .map_err(|error| error.to_string()),
                            "save" | "migrate" => {
                                let mut changed = before.clone();
                                changed.debug = !changed.debug;
                                save_preserving_comments(&path, &changed, op == "migrate")
                                    .map_err(|error| error.to_string())
                            }
                            "put" => {
                                let edit = V8Edit {
                                    method: V8Method::Put,
                                    path: vec!["server".into(), "port".into()],
                                    body: b"1234".to_vec(),
                                    yaml: false,
                                };
                                let preview = preview_v8(source.as_bytes(), &edit);
                                let result = edit_v8(&path, &edit).map(|_| ());
                                assert_eq!(preview.is_ok(), result.is_ok());
                                if let Ok(preview) = preview {
                                    assert_eq!(preview, std::fs::read(&path).unwrap());
                                }
                                result.map_err(|error| error.to_string())
                            }
                            _ => unreachable!(),
                        };
                        let after = std::fs::read(&path).unwrap();
                        if let Err(error) = result {
                            assert!(
                                error.contains("write refused, file unchanged"),
                                "{op}: {error}"
                            );
                            assert_eq!(after, source.as_bytes());
                        }
                        assert_eq!(
                            Config::load_bytes(&after).unwrap().auth_dir,
                            before.auth_dir,
                            "{separator:?} {style} {tail:?} {nested} {op}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn review_r9_generated_unicode_and_explicit_escapes_remain_writable() {
    for separator in ['\u{2028}', '\u{2029}', '\u{0085}'] {
        for tail in ["b", "        b", "\nb"] {
            for migrate in [false, true] {
                let dir = TempDir::new();
                let path = dir.join("config.yaml");
                std::fs::write(&path, "debug: false\n").unwrap();
                let mut cfg = Config::parse("debug: false\n").unwrap();
                cfg.auth_dir = format!("/a{separator}{tail}");
                save_preserving_comments(&path, &cfg, migrate).unwrap();
                assert_eq!(Config::load(&path).unwrap().auth_dir, cfg.auth_dir);
            }
        }
    }
    let escaped = b"config-version: 8\noauth:\n  auth-dir: \"/a\\u2028        b\"\n";
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    write_file(&path, escaped).unwrap();
    assert_eq!(Config::load(&path).unwrap().auth_dir, "/a\u{2028}        b");
}

#[test]
fn review_r9_unicode_disagreement_in_untyped_values_also_refuses() {
    for source in [
        "plugins:\n  configs:\n    demo:\n      text: 'a\u{2028}        b'\n",
        "plugins:\n  configs:\n    demo:\n      text: |\n        a\u{2029}        b\n",
    ] {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        let error = write_file(&path, source.as_bytes()).unwrap_err();
        assert_eq!(error.kind(), SaveErrorKind::Unwritable);
        assert_eq!(std::fs::read(&path).unwrap(), source.as_bytes());
    }
}
