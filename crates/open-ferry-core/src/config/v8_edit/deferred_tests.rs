use super::*;
use crate::config::testing::TempDir;
#[test]
fn layout_conversion_keeps_go_lossless_deferred_comment_bytes() {
    let f: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/deferred-key-layout-go.json")).unwrap();
    let source = f["source"].as_str().unwrap();
    let dir = TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, source).unwrap();
    let edit = V8Edit {
        method: V8Method::Put,
        path: vec!["oauth".into(), "auth-dir".into()],
        body: br#""/x""#.to_vec(),
        yaml: false,
    };
    let preview = preview_v8(source.as_bytes(), &edit).unwrap();
    edit_v8(&path, &edit).unwrap();
    let actual = std::fs::read(&path).unwrap();
    assert_eq!(preview, actual);
    assert_eq!(
        std::str::from_utf8(&actual).unwrap(),
        f["file"].as_str().unwrap()
    );
}

#[test]
fn layout_and_edits_match_lossless_go_and_keep_deleted_recipients_comments() {
    let rows: serde_json::Value = serde_json::from_str(include_str!(
        "../testdata/deferred-key-layout-controls.json"
    ))
    .unwrap();
    let mut equal = 0;
    let mut protected = 0;
    for f in rows.as_array().unwrap() {
        let source = f["source"].as_str().unwrap();
        let op = f["op"].as_str().unwrap();
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        match op {
            "normalize" => {
                std::fs::write(
                    &path,
                    normalize_config_layout(source.as_bytes(), true).unwrap().0,
                )
                .unwrap();
            }
            "raw" => save::write_file(&path, source.as_bytes()).unwrap(),
            _ => {
                let edit = V8Edit {
                    method: match op {
                        "put" => V8Method::Put,
                        "patch" => V8Method::Patch,
                        _ => V8Method::Delete,
                    },
                    path: vec!["observability".into(), "logs".into(), "debug".into()],
                    body: b"true".to_vec(),
                    yaml: false,
                };
                let preview = preview_v8(source.as_bytes(), &edit).unwrap();
                edit_v8(&path, &edit).unwrap();
                assert_eq!(preview, std::fs::read(&path).unwrap());
            }
        }
        let text = std::fs::read_to_string(&path).unwrap();
        Config::load_bytes(text.as_bytes()).unwrap();
        for c in ["# c1", "# c2"] {
            assert_eq!(text.matches(c).count(), 1, "{} {op}: {c}", f["name"]);
        }
        let before = AnyValue::parse_yaml(source).unwrap();
        let after = AnyValue::parse_yaml(&text).unwrap();
        fn plugins(v: &AnyValue) -> Option<&AnyValue> {
            let AnyValue::Map(m) = v else {
                return None;
            };
            let AnyValue::Map(p) = m.get("plugins")? else {
                return None;
            };
            p.get("configs")
        }
        assert_eq!(plugins(&before), plugins(&after));
        let go = f["file"].as_str().unwrap();
        if go.contains("# c1") && go.contains("# c2") {
            equal += 1;
            assert_eq!(text, go, "{} {op}", f["name"]);
        } else {
            protected += 1;
            assert_eq!(op, "delete");
        }
    }
    assert_eq!(equal + protected, 40);
    assert!(equal > 0 && protected > 0);
}

#[test]
fn canonical_config_version_keeps_its_comments() {
    let source = "config-version: 8 # version note\nobservability:\n  logs:\n    debug: false\n";
    let normalized = normalize_config_layout(source.as_bytes(), true).unwrap().0;
    assert_eq!(
        String::from_utf8_lossy(&normalized)
            .matches("# version note")
            .count(),
        1
    );
    assert_eq!(
        normalize_config_layout(&normalized, true).unwrap().0,
        normalized
    );
    for method in [V8Method::Put, V8Method::Patch, V8Method::Delete] {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        let edit = V8Edit {
            method,
            path: vec!["observability".into(), "logs".into(), "debug".into()],
            body: b"true".to_vec(),
            yaml: false,
        };
        let preview = preview_v8(source.as_bytes(), &edit).unwrap();
        edit_v8(&path, &edit).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(preview, bytes);
        Config::load_bytes(&bytes).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&bytes)
                .matches("# version note")
                .count(),
            1
        );
    }
}
