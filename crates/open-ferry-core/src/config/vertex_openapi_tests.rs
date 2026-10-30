// Ported from CLIProxyAPI internal/config/vertex_openapi_test.go and
// internal/watcher/diff/vertex_openapi_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI
use super::testing::TempDir;
use super::*;

fn yaml(v8: bool, flag: &str) -> String {
    let credential = format!(
        "api-key: dummy\nopenapi: {flag}\nopenapi-location: ' global '\nmodels:\n  - name: partner/model\n    alias: native\n    openapi: false\n  - name: gemini-2.5-pro\n    alias: compat\n    openapi: true\n  - name: automatic\n"
    );
    let (prefix, indent) = if v8 {
        (
            "config-version: 8\napi-keys:\n  vertex:\n    - keys:\n        - ",
            "          ",
        )
    } else {
        ("vertex-api-key:\n  - ", "    ")
    };
    let mut lines = credential.lines();
    let mut out = format!("{prefix}{}\n", lines.next().unwrap());
    for line in lines {
        out.push_str(&format!("{indent}{line}\n"));
    }
    out.push_str(if v8 { "oauth:\n  model-alias:\n    vertex: [{name: gemini-2.5-pro, alias: public, openapi: true}]\n  settings:\n    vertex: [{name: partner/model, openapi: false}]\n" } else { "oauth-model-alias:\n  vertex: [{name: gemini-2.5-pro, alias: public, openapi: true}]\noauth-settings:\n  vertex: [{name: partner/model, openapi: false}]\n" });
    out
}

#[test]
fn openapi_load_save_and_clear_in_both_layouts() {
    for v8 in [false, true] {
        for enabled in [false, true] {
            let raw = yaml(v8, if enabled { "true" } else { "false" });
            let mut cfg = Config::parse(raw.as_bytes()).unwrap();
            let key = &cfg.vertex_api_key[0];
            assert_eq!(key.openapi, Some(enabled));
            assert_eq!(key.openapi_location, "global");
            assert_eq!(
                key.models.iter().map(|m| m.openapi).collect::<Vec<_>>(),
                [Some(false), Some(true), None]
            );
            assert_eq!(cfg.oauth_model_alias["vertex"][0].openapi, Some(true));
            assert_eq!(cfg.oauth_settings["vertex"][0].openapi, Some(false));
            for migrate in [false, true] {
                let dir = TempDir::new();
                let path = dir.write("config.yaml", &raw);
                save::save_preserving_comments(&path, &cfg, migrate).unwrap();
                if v8 || migrate {
                    assert_v8_key_settings_stay_on_keys(&path);
                }
                let loaded = Config::load(&path).unwrap();
                assert_eq!(loaded.vertex_api_key, cfg.vertex_api_key);
                assert_eq!(loaded.oauth_model_alias, cfg.oauth_model_alias);
                assert_eq!(loaded.oauth_settings, cfg.oauth_settings);
            }
            cfg.vertex_api_key[0].openapi = None;
            cfg.vertex_api_key[0].openapi_location.clear();
            cfg.vertex_api_key[0].models[0].openapi = None;
            cfg.oauth_model_alias.get_mut("vertex").unwrap()[0].openapi = None;
            cfg.oauth_settings.get_mut("vertex").unwrap()[0].openapi = None;
            let dir = TempDir::new();
            let path = dir.write("config.yaml", &raw);
            save::save_preserving_comments(&path, &cfg, false).unwrap();
            let loaded = Config::load(&path).unwrap();
            assert_eq!(loaded.vertex_api_key, cfg.vertex_api_key);
            assert_eq!(loaded.oauth_model_alias, cfg.oauth_model_alias);
            assert_eq!(loaded.oauth_settings, cfg.oauth_settings);
        }
    }
}

#[test]
fn changes_and_hashes_distinguish_every_optional_state() {
    use crate::auth::synthesizer::vertex::compute_vertex_models_hash;
    let mut hashes = std::collections::BTreeSet::new();
    let mut previous = Config::parse(yaml(false, "false").as_bytes()).unwrap();
    for enabled in [None, Some(true), Some(false)] {
        let mut cfg = previous.clone();
        cfg.vertex_api_key[0].openapi = enabled;
        cfg.vertex_api_key[0].openapi_location = "us-central1".into();
        cfg.vertex_api_key[0].models[0].openapi = enabled;
        cfg.oauth_model_alias.get_mut("vertex").unwrap()[0].openapi = enabled;
        cfg.oauth_settings.get_mut("vertex").unwrap()[0].openapi = enabled;
        hashes.insert(compute_vertex_models_hash(&cfg.vertex_api_key[0].models));
        let changes = diff::build_change_details(&previous, &cfg).join("\n");
        for field in [
            "vertex[0].openapi:",
            "vertex[0].models:",
            "oauth-model-alias[vertex]:",
            "oauth-settings[vertex]:",
        ] {
            assert!(changes.contains(field), "{field}: {changes}");
        }
        if previous.vertex_api_key[0].openapi_location != cfg.vertex_api_key[0].openapi_location {
            assert!(changes.contains("vertex[0].openapi-location:"));
        }
        previous = cfg;
    }
    assert_eq!(hashes.len(), 3);
}

#[test]
fn group_openapi_fields_are_refused_by_both_flatteners() {
    for (field, value) in [("openapi", "true"), ("openapi-location", "global")] {
        let raw = format!(
            "config-version: 8\napi-keys:\n  vertex:\n    - {field}: {value}\n      keys: [{{api-key: dummy}}]\n"
        );
        let expected = format!("api-keys.vertex: unsupported group field {field}");
        assert_eq!(Config::parse(&raw).unwrap_err().to_string(), expected);
        let doc = save::unmarshal(raw.as_bytes()).unwrap();
        assert_eq!(
            save::flatten_v8(&doc.content[0]).unwrap_err().to_string(),
            expected
        );
    }
}

#[test]
fn group_openapi_edits_are_refused_without_writing() {
    use v8_edit::{V8Edit, V8EditError, V8Method, edit_v8};
    for field in ["openapi", "openapi-location"] {
        for method in [V8Method::Put, V8Method::Patch] {
            let dir = TempDir::new();
            let initial = "config-version: 8\napi-keys: {vertex: [{keys: [{api-key: dummy}]}]}\n";
            let path = dir.write("config.yaml", initial);
            let value = if field == "openapi" {
                serde_json::json!(true)
            } else {
                serde_json::json!("global")
            };
            let body = serde_json::json!([{(field): value, "keys": [{"api-key": "dummy"}]}]);
            let error = edit_v8(
                &path,
                &V8Edit {
                    method,
                    path: vec!["api-keys".into(), "vertex".into()],
                    body: serde_json::to_vec(&body).unwrap(),
                    yaml: false,
                },
            )
            .unwrap_err();
            assert_eq!(
                error,
                V8EditError::Unprocessable(format!(
                    "api-keys.vertex: unsupported group field {field}"
                ))
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
        }
    }
}

fn without_openapi(v8: bool) -> &'static str {
    if v8 {
        "config-version: 8\napi-keys: {vertex: [{keys: [{api-key: dummy, models: [{name: custom, alias: public}]}]}]}\noauth:\n  model-alias: {vertex: [{name: custom, alias: public}], codex: [{name: custom, alias: public}]}\n  settings: {vertex: [{name: custom}], codex: [{name: custom}]}\n"
    } else {
        "vertex-api-key: [{api-key: dummy, models: [{name: custom, alias: public}]}]\noauth-model-alias: {vertex: [{name: custom, alias: public}], codex: [{name: custom, alias: public}]}\noauth-settings: {vertex: [{name: custom}], codex: [{name: custom}]}\n"
    }
}

fn assert_inserted_false(path: &std::path::Path) {
    let written = std::fs::read_to_string(path).unwrap();
    assert_eq!(written.matches("openapi: false").count(), 6, "{written}");
    let cfg = Config::load(path).unwrap();
    assert_eq!(cfg.vertex_api_key[0].openapi, Some(false));
    assert_eq!(cfg.vertex_api_key[0].models[0].openapi, Some(false));
    for channel in ["vertex", "codex"] {
        assert_eq!(cfg.oauth_model_alias[channel][0].openapi, Some(false));
        assert_eq!(cfg.oauth_settings[channel][0].openapi, Some(false));
    }
}

fn insert_false_with_save(v8: bool) {
    for migrate in [false, true] {
        let dir = TempDir::new();
        let path = dir.write("config.yaml", without_openapi(v8));
        let mut cfg = Config::load(&path).unwrap();
        cfg.vertex_api_key[0].openapi = Some(false);
        cfg.vertex_api_key[0].models[0].openapi = Some(false);
        for rules in cfg.oauth_model_alias.values_mut() {
            rules[0].openapi = Some(false);
        }
        for rules in cfg.oauth_settings.values_mut() {
            rules[0].openapi = Some(false);
        }
        save::save_preserving_comments(&path, &cfg, migrate).unwrap();
        assert_inserted_false(&path);
    }
}

#[test]
fn insert_false_in_legacy_save() {
    insert_false_with_save(false);
}
#[test]
fn insert_false_in_v8_save() {
    insert_false_with_save(true);
}

fn insert_false_with_edit(v8: bool) {
    use v8_edit::{V8Edit, V8Method, edit_v8};
    for method in [V8Method::Put, V8Method::Patch] {
        let dir = TempDir::new();
        let path = dir.write("config.yaml", without_openapi(v8));
        let body = serde_json::json!({
            "api-keys": {"vertex": [{"keys": [{"api-key": "dummy", "openapi": false, "models": [{"name": "custom", "alias": "public", "openapi": false}]}]}]},
            "oauth": {
                "model-alias": {"vertex": [{"name": "custom", "alias": "public", "openapi": false}], "codex": [{"name": "custom", "alias": "public", "openapi": false}]},
                "settings": {"vertex": [{"name": "custom", "openapi": false}], "codex": [{"name": "custom", "openapi": false}]}
            }
        });
        edit_v8(
            &path,
            &V8Edit {
                method,
                path: vec![],
                body: serde_json::to_vec(&body).unwrap(),
                yaml: false,
            },
        )
        .unwrap();
        assert_inserted_false(&path);
    }
}

#[test]
fn insert_false_with_v8_edit_from_legacy() {
    insert_false_with_edit(false);
}
#[test]
fn insert_false_with_v8_edit_from_v8() {
    insert_false_with_edit(true);
}

fn assert_v8_key_settings_stay_on_keys(path: &std::path::Path) {
    let doc = save::unmarshal(&std::fs::read(path).unwrap()).unwrap();
    let groups = save::yaml_path(&doc.content[0], "api-keys.vertex").unwrap();
    for group in &groups.content {
        assert!(save::yaml_path(group, "openapi").is_none());
        assert!(save::yaml_path(group, "openapi-location").is_none());
        for key in &save::yaml_path(group, "keys").unwrap().content {
            assert!(save::yaml_path(key, "openapi").is_some());
            assert!(save::yaml_path(key, "openapi-location").is_some());
        }
    }
}
