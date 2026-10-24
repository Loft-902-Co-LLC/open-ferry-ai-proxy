use super::*;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8, preview_v8};

#[test]
fn review_r10_comment_separators_follow_go_after_each_write() {
    let mut observations = Vec::new();
    for sep in ['\u{2028}', '\u{2029}', '\u{0085}'] {
        for (shape, source) in [
            (
                "debug-legacy",
                format!("debug: # c{sep}  true\nauth-dir: /old\n"),
            ),
            (
                "debug-v8",
                format!("config-version: 8\ndebug: # c{sep}  true\noauth:\n  auth-dir: /old\n"),
            ),
            (
                "plugin",
                format!("plugins:\n  configs:\n    demo:\n      text: # c{sep}        x\n"),
            ),
            ("sequence", format!("x:\n  - # c{sep}    y\n")),
        ] {
            for op in ["raw", "normalize", "nested", "save", "migrate", "put"] {
                let dir = TempDir::new();
                let path = dir.join("config.yaml");
                std::fs::write(&path, &source).unwrap();
                let result: Result<(), String> = match op {
                    "raw" => write_file(&path, source.as_bytes()).map_err(|e| e.to_string()),
                    "normalize" => normalize_config_layout(source.as_bytes(), true)
                        .map(|(out, _)| std::fs::write(&path, out).unwrap())
                        .map_err(|e| e.to_string()),
                    "nested" => {
                        update_nested_scalar(&path, &["auth-dir"], "/x").map_err(|e| e.to_string())
                    }
                    "save" | "migrate" => save_preserving_comments(
                        &path,
                        &Config::load(&path).unwrap(),
                        op == "migrate",
                    )
                    .map_err(|e| e.to_string()),
                    _ => {
                        let edit = V8Edit {
                            method: V8Method::Put,
                            path: vec!["oauth".into(), "auth-dir".into()],
                            body: br#""/x""#.to_vec(),
                            yaml: false,
                        };
                        edit_v8(&path, &edit).map(|_| ()).map_err(|e| e.to_string())
                    }
                };
                let out = std::fs::read_to_string(&path).unwrap();
                let writable = result.is_ok() && Config::load(&path).is_ok();
                observations.push(serde_json::json!({"shape":shape,"separator":format!("U+{:04X}",sep as u32),"op":op,"writable":writable}));
                assert!(result.is_ok(), "{shape} {op}");
                if shape.starts_with("debug") {
                    assert_eq!(
                        Config::load_bytes(source.as_bytes()).unwrap().debug,
                        Config::load_bytes(out.as_bytes()).unwrap().debug
                    );
                    continue;
                }
                let keep = match shape {
                    "plugin" => "plugins.configs.demo.text",
                    "sequence" => "x",
                    _ => "debug",
                };
                let source_root = loader_root(source.as_bytes()).unwrap().unwrap();
                let mut output_root = loader_root(out.as_bytes()).unwrap().unwrap();
                if shape == "sequence" && loader_yaml::yaml_path(&output_root, keep).is_none() {
                    // Go archives unknown top-level fields during migration.
                    // Read that archive back to check the same sequence value.
                    let archived = out[out.find("# x:").unwrap()..]
                        .lines()
                        .take_while(|line| line.starts_with('#'))
                        .map(|line| line.strip_prefix("# ").unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join("\n");
                    output_root = loader_root(archived.as_bytes()).unwrap().unwrap();
                }
                assert_eq!(
                    unicode_values(loader_yaml::yaml_path(&source_root, keep).unwrap()),
                    unicode_values(loader_yaml::yaml_path(&output_root, keep).unwrap())
                );
            }
        }
    }
    if let Some(path) = std::env::var_os("R10_STEER_CAPTURE") {
        std::fs::write(path, serde_json::to_vec_pretty(&observations).unwrap()).unwrap();
    }
    let failures: Vec<_> = observations
        .iter()
        .filter(|r| r["writable"] != true)
        .map(|r| (&r["shape"], &r["separator"], &r["op"]))
        .collect();
    assert!(failures.is_empty(), "{failures:?}");
}

fn edit(source: &str, path: Vec<String>, body: Vec<u8>) -> Result<String, String> {
    let dir = TempDir::new();
    let file = dir.join("config.yaml");
    std::fs::write(&file, source).unwrap();
    let edit = V8Edit {
        method: V8Method::Put,
        path,
        body,
        yaml: false,
    };
    let preview = preview_v8(source.as_bytes(), &edit).map_err(|e| e.to_string())?;
    edit_v8(&file, &edit).map_err(|e| e.to_string())?;
    assert_eq!(preview, std::fs::read(&file).unwrap());
    let output = std::fs::read_to_string(&file).unwrap();
    Config::load(&file).unwrap();
    Ok(output)
}
#[test]
fn review_r10_generated_escaped_unicode_key_is_writable() {
    for source in [
        "auth-dir: /old\n",
        "config-version: 8\noauth:\n  auth-dir: /old\n",
    ] {
        let out = edit(
            source,
            vec!["plugins".into(), "configs".into(), "demo".into()],
            br#"{"k\u2028ey":1}"#.to_vec(),
        )
        .unwrap();
        let doc = compose::unmarshal(out.as_bytes()).unwrap();
        let plugin = yaml_path(&doc.content[0], "plugins.configs.demo").unwrap();
        let i = find_map_key_index(plugin, "k\u{2028}ey").unwrap();
        assert_eq!(plugin.content[i + 1].value, "1");
        assert!(out.contains("k\u{2028}"));
        assert!(!out.contains("\\L"));
    }
}
#[test]
fn review_r10_literal_unicode_request_bodies_follow_go_reader() {
    for (body, expected) in [
        ("\"/a\u{2028}  b\"", "/a\u{2028}b"),
        ("\"/a\u{0085}b\"", "/a b"),
    ] {
        for source in [
            "auth-dir: /old\n",
            "config-version: 8\noauth:\n  auth-dir: /old\n",
        ] {
            let out = edit(
                source,
                vec!["oauth".into(), "auth-dir".into()],
                body.as_bytes().to_vec(),
            )
            .unwrap();
            assert_eq!(
                Config::load_bytes(out.as_bytes()).unwrap().auth_dir,
                expected
            );
        }
    }
}
#[test]
fn review_r10_agreeing_literal_unicode_sources_remain_writable() {
    for source in [
        "# c\u{0085}\nauth-dir: /old\n",
        "x: 'a\u{2028}b'\nauth-dir: /old\n",
    ] {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        std::fs::write(&path, source).unwrap();
        write_file(&path, source.as_bytes()).unwrap();
        update_nested_scalar(&path, &["auth-dir"], "/x").unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        let mut before = crate::config::AnyValue::parse_yaml(source).unwrap();
        let mut after = crate::config::AnyValue::parse_yaml(&out).unwrap();
        if let (crate::config::AnyValue::Map(a), crate::config::AnyValue::Map(b)) =
            (&mut before, &mut after)
        {
            a.remove("auth-dir");
            b.remove("auth-dir");
        }
        assert_eq!(before, after);
    }
}

fn unicode_values(node: &loader_yaml::Node) -> serde_json::Value {
    serde_json::json!([
        format!("{:?}", node.kind),
        node.tag.as_str(),
        node.value.as_str(),
        node.content.iter().map(unicode_values).collect::<Vec<_>>()
    ])
}
