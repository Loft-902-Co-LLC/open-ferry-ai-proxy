// Ported from CLIProxyAPI internal/config/vertex_openapi_test.go and
// internal/watcher/diff/vertex_openapi_test.go (v8.0.38, MIT).
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
fn openapi_load_save_edit_and_clear_in_both_layouts() {
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
fn group_inheritance_and_key_overrides() {
    let raw = b"config-version: 8\napi-keys:\n  vertex:\n    - openapi: true\n      openapi-location: global\n      models: [{name: partner/model, openapi: false}]\n      keys:\n        - api-key: dummy-one\n        - api-key: dummy-two\n          openapi: false\n          openapi-location: europe-west4\n";
    let cfg = Config::parse(raw).unwrap();
    assert_eq!(cfg.vertex_api_key[0].openapi, Some(true));
    assert_eq!(cfg.vertex_api_key[0].openapi_location, "global");
    assert_eq!(cfg.vertex_api_key[1].openapi, Some(false));
    assert_eq!(cfg.vertex_api_key[1].openapi_location, "europe-west4");
    for key in &cfg.vertex_api_key {
        assert_eq!(key.models[0].openapi, Some(false));
    }
    let dir = TempDir::new();
    let path = dir.write("config.yaml", std::str::from_utf8(raw).unwrap());
    save::save_preserving_comments(&path, &cfg, false).unwrap();
    assert_eq!(
        Config::load(&path).unwrap().vertex_api_key,
        cfg.vertex_api_key
    );
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
