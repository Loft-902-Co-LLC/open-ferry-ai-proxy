// Ported from CLIProxyAPI sdk/cliproxy/service_codex_models_test.go,
// service_excluded_models_test.go, service_oauth_model_alias_test.go,
// service_oauth_settings_test.go, config_model_display_name_test.go,
// config_model_max_context_length_test.go,
// service_models_config_index_test.go, openai_compat_config_models_test.go,
// sdk/cliproxy/auth/classification_test.go
// and oauth_model_alias_test.go, internal/config/oauth_model_alias_test.go
// and oauth_settings_test.go, internal/modelconfig/model_info_test.go,
// internal/auth/codex/jwt_parser_test.go and
// internal/watcher/synthesizer/file_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of registration.
//!
//! Upstream's tests that use the global registry use a fresh `ModelRegistry`
//! here. Those for Gemini, Vertex, Devin and plugin providers run against
//! Claude or Codex where the rule tested is provider-neutral
//! (`UsesPreMergedExcludedModelsAttribute`), and as written where it is (the
//! alias and channel tests). `MetaOAuthAliasAndExcludedModels` runs as
//! written with no provider held back as unported, as Meta's executor isn't
//! ported yet. `RegisterConfigAPIKeyAuthsCodexModelModes` builds the
//! credential the config loader would.
//! `OpenAICompatibilityRegistrationCacheUsesConfigIndex` checks the
//! registration the cache feeds, as the cache isn't ported.
//!
//! Dropped:
//! - `AntigravityFetchesWebSearchCapability` and `DevinSWE16SlowIncluded`:
//!   Antigravity and Devin aren't ported.
//! - `ApplyOAuthSettings_CodexCatalogPipeline`: it needs the Codex client
//!   models builder, which is deferred.
//! - The native capability part of `ApplyModelPrefixes_PreservesMetadataModelID`:
//!   native capabilities aren't ported.
//! - `ResolveOAuthUpstreamModel_*`, `ApplyOAuthModelAlias_*SuffixPreservation`,
//!   `*ForceMapping*`, `PerAuthOverridesGlobalAlias`, `PerAuthAliasSkipsAPIKey`,
//!   `ApplyOAuthModelAlias_Devin` and the `ApplyOAuthModelAliasWithResult_*`
//!   tests in sdk/cliproxy/auth: they resolve a request's model when routing,
//!   which is the credential manager's.
//! - `AccountInfoUsesAuthKind`: account info isn't part of the registry.
//! - `ParseConfigOAuthMetaChannel` and the channel-key part of
//!   `SanitizeOAuthModelAlias_PreservesOptionalFields`: parsing the config
//!   isn't part of the registry.
//! - The parts of the model config tests that check the returned model's ID,
//!   `UserDefined` flag and that it doesn't share storage with the config:
//!   here only the thinking settings are resolved, and they are owned.

use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, json};

use super::*;

const IMAGE_MODELS: [&str; 5] = [
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
];

fn auth(id: &str, provider: &str, attributes: &[(&str, &str)]) -> Auth {
    Auth {
        id: id.to_owned(),
        provider: provider.to_owned(),
        attributes: attributes
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        ..Auth::default()
    }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        ..ModelInfo::default()
    }
}

fn configured(name: &str, alias: &str) -> ConfiguredModel {
    ConfiguredModel {
        name: name.to_owned(),
        alias: alias.to_owned(),
        ..ConfiguredModel::default()
    }
}

fn api_key_entry(api_key: &str, models: Vec<ConfiguredModel>) -> ApiKeyEntry {
    ApiKeyEntry {
        api_key: api_key.to_owned(),
        models,
        ..ApiKeyEntry::default()
    }
}

fn alias(name: &str, alias: &str) -> ModelAlias {
    ModelAlias {
        name: name.to_owned(),
        alias: alias.to_owned(),
        ..ModelAlias::default()
    }
}

fn setting(name: &str, alias: &str, max_context_length: u64) -> ModelSetting {
    ModelSetting {
        name: name.to_owned(),
        alias: alias.to_owned(),
        max_context_length,
    }
}

fn channel<T>(channel: &str, entries: Vec<T>) -> BTreeMap<String, Vec<T>> {
    BTreeMap::from([(channel.to_owned(), entries)])
}

fn alias_rules(channel_name: &str, aliases: Vec<ModelAlias>) -> RegistrationRules {
    RegistrationRules {
        oauth_model_alias: channel(channel_name, aliases),
        ..RegistrationRules::default()
    }
}

fn settings_rules(settings: Vec<ModelSetting>) -> RegistrationRules {
    RegistrationRules {
        oauth_settings: channel("codex", settings),
        ..RegistrationRules::default()
    }
}

fn id_set(models: &[ModelInfo]) -> BTreeSet<String> {
    models
        .iter()
        .filter(|model| !model.id.is_empty())
        .map(|model| model.id.clone())
        .collect()
}

fn ids(models: &[ModelInfo]) -> Vec<&str> {
    models.iter().map(|model| model.id.as_str()).collect()
}

fn codex_pro_models() -> Vec<ModelInfo> {
    StaticCatalog::embedded().codex_models(CodexPlan::Pro)
}

/// The models a fresh registry holds for `auth` after registering it.
fn registered(auth: &Auth, rules: &RegistrationRules) -> Vec<ModelInfo> {
    let registry = ModelRegistry::new();
    registry.register_auth(auth, rules);
    registry.models_for_client(&auth.id)
}

/// An unsigned ID token whose OpenAI claims carry `plan_type`, if any.
fn codex_id_token(plan_type: Option<&str>) -> String {
    let mut auth_info = json!({"chatgpt_account_id": "acc-123"});
    if let (Some(plan_type), Value::Object(info)) = (plan_type, &mut auth_info) {
        info.insert("chatgpt_plan_type".to_owned(), json!(plan_type));
    }
    let claims = json!({
        "email": "user@example.com",
        "https://api.openai.com/auth": auth_info,
    });
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{header}.{claims}.")
}

#[test]
fn register_models_for_auth_codex_configuration_update() {
    let capable_id = codex_pro_models()
        .into_iter()
        .find(|model| model.support_configuration_update)
        .map(|model| model.id)
        .expect("an OAuth Codex model supporting configuration_update");

    let cases = [
        (
            "defaults do not inherit OAuth capability",
            Vec::new(),
            vec![(capable_id.as_str(), false)],
        ),
        (
            "explicit per-model capability overrides OAuth",
            vec![
                configured(&capable_id, &capable_id),
                ConfiguredModel {
                    support_configuration_update: true,
                    ..configured(&capable_id, "configured-enabled")
                },
            ],
            vec![(capable_id.as_str(), false), ("configured-enabled", true)],
        ),
    ];
    for (name, models, want) in cases {
        let rules = RegistrationRules {
            codex_keys: vec![api_key_entry("config-update-key", models)],
            ..RegistrationRules::default()
        };
        let registry = ModelRegistry::new();
        let oauth = auth(
            "codex-config-update-oauth",
            "codex",
            &[("plan_type", "pro")],
        );
        let api_key = auth(
            "codex-config-update-apikey",
            "codex",
            &[
                ("api_key", "config-update-key"),
                ("config_index", "0"),
                ("source", "config:codex:test"),
            ],
        );
        registry.register_auth(&oauth, &rules);
        registry.register_auth(&api_key, &rules);

        let oauth_models = registry.models_for_client(&oauth.id);
        let oauth_model = oauth_models
            .iter()
            .find(|model| model.id == capable_id)
            .unwrap_or_else(|| panic!("{name}: OAuth model {capable_id} not registered"));
        assert!(oauth_model.support_configuration_update, "{name}");

        let api_key_models = registry.models_for_client(&api_key.id);
        for (id, want) in want {
            let model = api_key_models
                .iter()
                .find(|model| model.id == id)
                .unwrap_or_else(|| panic!("{name}: API-key model {id} not registered"));
            assert_eq!(model.support_configuration_update, want, "{name}: {id}");
        }
        for model in registry.available_model_maps("openai") {
            assert!(
                !model.contains_key("support_configuration_update"),
                "{name}: the public list exposed configuration_update: {model:?}"
            );
        }
    }
}

#[test]
fn register_models_for_auth_codex_api_key_models() {
    let defaults = codex_pro_models();
    let excluded_id = defaults
        .first()
        .map(|model| model.id.clone())
        .expect("Codex Pro default models");
    /// A name, the entry, the models wanted, and models that must be
    /// present or absent.
    type Case<'a> = (
        &'a str,
        ApiKeyEntry,
        BTreeSet<String>,
        &'a [&'a str],
        &'a [&'a str],
    );
    let cases: [Case<'_>; 3] = [
        (
            "defaults without explicit models",
            api_key_entry("default-key", Vec::new()),
            id_set(&defaults),
            &IMAGE_MODELS,
            &[],
        ),
        (
            "only explicitly configured models",
            api_key_entry(
                "configured-key",
                vec![configured("upstream-codex", "configured-codex")],
            ),
            BTreeSet::from(["configured-codex".to_owned()]),
            &[],
            &IMAGE_MODELS,
        ),
        (
            "exclusions apply to defaults",
            ApiKeyEntry {
                excluded_models: vec![excluded_id],
                ..api_key_entry("excluded-key", Vec::new())
            },
            id_set(defaults.get(1..).unwrap_or_default()),
            &[],
            &[],
        ),
    ];
    for (index, (name, entry, want, present, absent)) in cases.into_iter().enumerate() {
        let api_key = entry.api_key.clone();
        let rules = RegistrationRules {
            codex_keys: vec![entry],
            ..RegistrationRules::default()
        };
        let auth = auth(
            &format!("codex-api-key-models-{index}"),
            "codex",
            &[
                ("api_key", api_key.as_str()),
                ("config_index", "0"),
                ("source", "config:codex:test"),
            ],
        );
        let got = id_set(&registered(&auth, &rules));
        assert_eq!(got, want, "{name}");
        for id in present {
            assert!(got.contains(*id), "{name}: {id} is missing");
        }
        for id in absent {
            assert!(!got.contains(*id), "{name}: {id} is registered");
        }
    }
}

#[test]
fn register_models_for_auth_codex_api_key_default_requires_config_match() {
    let defaults = id_set(&codex_pro_models());
    /// A name, the entries, the credential's attributes and the models wanted.
    type Case<'a> = (
        &'a str,
        Vec<ApiKeyEntry>,
        &'a [(&'a str, &'a str)],
        BTreeSet<String>,
    );
    let cases: [Case<'_>; 4] = [
        (
            "valid index with unmatched API key",
            vec![api_key_entry("configured-key", Vec::new())],
            &[
                ("api_key", "stale-key"),
                ("config_index", "0"),
                ("source", "config:codex:stale"),
            ],
            BTreeSet::new(),
        ),
        (
            "valid index with unmatched base URL",
            vec![ApiKeyEntry {
                base_url: "https://new.example.com".to_owned(),
                ..api_key_entry("configured-key", Vec::new())
            }],
            &[
                ("api_key", "configured-key"),
                ("config_index", "0"),
                ("source", "config:codex:stale"),
                ("base_url", "https://old.example.com"),
            ],
            BTreeSet::new(),
        ),
        (
            "stale index falls back to matching credentials",
            vec![
                api_key_entry("wrong-key", vec![configured("wrong-model", "")]),
                api_key_entry("configured-key", Vec::new()),
            ],
            &[
                ("api_key", "configured-key"),
                ("config_index", "0"),
                ("source", "config:codex:stale"),
            ],
            defaults.clone(),
        ),
        (
            "API key ignores OAuth plan type",
            vec![api_key_entry("configured-key", Vec::new())],
            &[
                ("api_key", "configured-key"),
                ("config_index", "0"),
                ("source", "config:codex:test"),
                ("plan_type", "free"),
            ],
            defaults.clone(),
        ),
    ];
    for (index, (name, codex_keys, attributes, want)) in cases.into_iter().enumerate() {
        let rules = RegistrationRules {
            codex_keys,
            ..RegistrationRules::default()
        };
        let auth = auth(
            &format!("codex-api-key-config-match-{index}"),
            "codex",
            attributes,
        );
        let registry = ModelRegistry::new();
        registry.register_client(&auth.id, "codex", &[model("stale-model")]);
        registry.register_auth(&auth, &rules);
        assert_eq!(
            id_set(&registry.models_for_client(&auth.id)),
            want,
            "{name}"
        );
    }
}

#[test]
fn register_config_api_key_auths_codex_model_modes() {
    let cases = [
        (
            "empty models uses defaults with images",
            Vec::new(),
            id_set(&codex_pro_models()),
            true,
        ),
        (
            "configured models replace defaults",
            vec![configured("runtime-upstream", "runtime-configured")],
            BTreeSet::from(["runtime-configured".to_owned()]),
            false,
        ),
    ];
    for (index, (name, models, want, want_images)) in cases.into_iter().enumerate() {
        let api_key = format!("runtime-key-{index}");
        let rules = RegistrationRules {
            codex_keys: vec![api_key_entry(&api_key, models)],
            ..RegistrationRules::default()
        };
        // The credential the config loader makes for the entry.
        let auth = auth(
            &format!("codex:apikey:{index}"),
            "codex",
            &[
                ("auth_kind", "apikey"),
                ("api_key", api_key.as_str()),
                ("config_index", "0"),
                ("source", "config:codex[runtime]"),
            ],
        );
        let registry = ModelRegistry::new();
        registry.register_auth(&auth, &rules);

        let got = id_set(&registry.models_for_client(&auth.id));
        assert_eq!(got, want, "{name}");
        let listed: BTreeSet<String> = registry
            .available_model_maps("openai")
            .iter()
            .filter_map(|model| model.get("id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        for id in IMAGE_MODELS {
            assert_eq!(got.contains(id), want_images, "{name}: {id}");
            if want_images {
                assert!(listed.contains(id), "{name}: /v1/models is missing {id}");
            }
        }
    }
}

#[test]
fn register_models_for_auth_uses_pre_merged_excluded_models_attribute() {
    let rules = RegistrationRules {
        oauth_excluded_models: channel("claude", vec!["claude-opus-4-6".to_owned()]),
        ..RegistrationRules::default()
    };
    let auth = auth(
        "auth-claude",
        "claude",
        &[
            ("auth_kind", "oauth"),
            ("excluded_models", "claude-sonnet-4-6"),
        ],
    );
    let registry = ModelRegistry::new();
    registry.register_auth(&auth, &rules);

    let models = registry.available_models_by_provider("claude");
    assert!(!models.is_empty());
    let got = id_set(&models);
    assert!(
        !got.contains("claude-sonnet-4-6"),
        "the attribute's exclusion was ignored"
    );
    assert!(
        got.contains("claude-opus-4-6"),
        "the global exclusion applied though the attribute replaces it"
    );
}

#[test]
fn register_models_for_auth_oauth_alias_and_excluded_models() {
    let rules = RegistrationRules {
        oauth_excluded_models: channel("codex", vec!["gpt-5.5".to_owned()]),
        oauth_model_alias: channel("codex", vec![alias("gpt-6-luna", "luna-latest")]),
        ..RegistrationRules::default()
    };
    // An explicit kind wins over an API key.
    let auth = auth(
        "auth-codex-oauth",
        "codex",
        &[("auth_kind", "oauth"), ("api_key", "minted")],
    );
    let got = id_set(&registered(&auth, &rules));
    assert!(!got.is_empty());
    assert!(
        !got.contains("gpt-5.5"),
        "oauth-excluded-models was ignored"
    );
    assert!(
        !got.contains("gpt-6-luna"),
        "oauth-model-alias didn't rename"
    );
    assert!(
        got.contains("luna-latest"),
        "oauth-model-alias didn't add the alias"
    );
}

/// The models `auth` serves under `rules` with no provider held back as
/// unported, or none.
fn ported(auth: &Auth, rules: &RegistrationRules) -> Vec<ModelInfo> {
    match auth_models_gated(auth, rules, StaticCatalog::embedded(), &[]) {
        AuthModels::Register { models, .. } => models,
        AuthModels::Ignore | AuthModels::Unregister => Vec::new(),
    }
}

// TestRegisterModelsForAuth_MetaOAuthAliasAndExcludedModels
#[test]
fn register_models_for_auth_meta_oauth_alias_and_excluded_models() {
    let rules = RegistrationRules {
        oauth_excluded_models: channel("meta", vec!["muse-spark-1.1".to_owned()]),
        oauth_model_alias: channel("meta", vec![alias("muse-spark-1.3", "muse-latest")]),
        ..RegistrationRules::default()
    };
    let auth = auth(
        "auth-meta-oauth",
        "meta",
        &[("auth_kind", "oauth"), ("api_key", "LLM|minted")],
    );
    let got = id_set(&ported(&auth, &rules));
    assert!(!got.is_empty(), "expected meta models to be registered");
    assert!(
        !got.contains("muse-spark-1.1"),
        "oauth-excluded-models was ignored"
    );
    assert!(
        !got.contains("muse-spark-1.3"),
        "oauth-model-alias didn't rename"
    );
    assert!(
        got.contains("muse-latest"),
        "oauth-model-alias didn't add the alias"
    );
}

// Not upstream's: the xAI credentials get no models while their provider is
// unported; the interactions and Meta ones get them, as their executors are
// ported.
#[test]
fn xai_waits_for_its_executor() {
    assert!(UNPORTED_PROVIDERS.contains(&"xai"));
    let credential = auth("xai-key", "xai", &[("api_key", "k")]);
    assert_eq!(
        auth_models(&credential, &RegistrationRules::default()),
        AuthModels::Unregister
    );
    assert!(!ported(&credential, &RegistrationRules::default()).is_empty());
}

// service_models.go: the gemini-interactions, xai and meta cases of
// registerModelsForAuth, resolveConfigInteractionsKey and
// resolveConfigCodexStyleKey without the index check (no upstream test).
#[test]
fn interactions_xai_and_meta_keys_use_their_entry() {
    let catalog = StaticCatalog::embedded();
    let rules = RegistrationRules {
        oauth_excluded_models: channel("xai", vec!["grok-4.5".to_owned()]),
        interactions_keys: vec![
            ApiKeyEntry {
                excluded_models: vec!["gemini-2.5-flash*".to_owned()],
                ..api_key_entry("interactions-key", Vec::new())
            },
            api_key_entry(
                "listed-key",
                vec![configured("gemini-2.5-pro", "native-pro")],
            ),
        ],
        xai_keys: vec![
            ApiKeyEntry {
                excluded_models: vec!["grok-3-*".to_owned()],
                ..api_key_entry("xai-key", Vec::new())
            },
            api_key_entry("xai-listed", vec![configured("grok-4.5", "grok-latest")]),
        ],
        meta_keys: vec![api_key_entry(
            "meta-key",
            vec![configured("muse-spark-1.3", "muse")],
        )],
        ..RegistrationRules::default()
    };

    // The Gemini catalog without the entry's exclusions, or its own models.
    let interactions = auth(
        "interactions-catalog",
        "gemini-interactions",
        &[("api_key", "interactions-key"), ("auth_kind", "apikey")],
    );
    let got = id_set(&ported(&interactions, &rules));
    assert!(got.contains("gemini-2.5-pro"));
    assert!(!got.iter().any(|id| id.starts_with("gemini-2.5-flash")));
    assert!(got.is_subset(&id_set(&catalog.gemini_models())));
    let listed = auth(
        "interactions-listed",
        "gemini-interactions",
        &[("api_key", "listed-key")],
    );
    let models = ported(&listed, &rules);
    assert_eq!(ids(&models), ["native-pro"]);
    assert_eq!(
        (models[0].owned_by.as_str(), models[0].model_type.as_str()),
        ("google", "gemini")
    );
    match auth_models_gated(&listed, &rules, catalog, &[]) {
        AuthModels::Register { provider, .. } => assert_eq!(provider, "gemini-interactions"),
        other => panic!("{other:?}"),
    }

    // The xAI catalog without the entry's exclusions; the global OAuth
    // exclusions don't apply to API keys.
    let xai = auth(
        "xai-catalog",
        "xai",
        &[("api_key", "xai-key"), ("auth_kind", "apikey")],
    );
    let got = id_set(&ported(&xai, &rules));
    assert!(got.contains("grok-4.5"));
    assert!(!got.iter().any(|id| id.starts_with("grok-3-")));
    assert!(got.is_subset(&id_set(&catalog.xai_models())));
    // By config index, whatever the key, unlike a Codex key.
    let indexed = auth(
        "xai-indexed",
        "xai",
        &[
            ("api_key", "stale-key"),
            ("config_index", "1"),
            ("source", "config:xai[token]"),
        ],
    );
    let models = ported(&indexed, &rules);
    assert_eq!(ids(&models), ["grok-latest"]);
    assert_eq!(
        (models[0].owned_by.as_str(), models[0].model_type.as_str()),
        ("xai", "xai")
    );
    // An OAuth credential gets the catalog under the global exclusions.
    let oauth = auth("xai-oauth", "xai", &[]);
    let got = id_set(&ported(&oauth, &rules));
    assert!(!got.is_empty() && !got.contains("grok-4.5"));

    let meta = auth("meta-listed", "meta", &[("api_key", "meta-key")]);
    let models = ported(&meta, &rules);
    assert_eq!(ids(&models), ["muse"]);
    assert_eq!(
        (models[0].owned_by.as_str(), models[0].model_type.as_str()),
        ("meta", "meta")
    );
    let unknown = auth("meta-other", "meta", &[("api_key", "other")]);
    assert_eq!(
        id_set(&ported(&unknown, &rules)),
        id_set(&catalog.meta_models())
    );
}

// Not upstream's: resolveConfigCodexStyleKey's index check.
#[test]
fn codex_style_keys_check_the_index_only_for_codex() {
    let entries = [
        api_key_entry("first", Vec::new()),
        api_key_entry("second", Vec::new()),
    ];
    let credential = auth(
        "",
        "xai",
        &[
            ("api_key", "second"),
            ("config_index", "0"),
            ("source", "config:xai[token]"),
        ],
    );
    let checked = resolve_config_codex_style_key(&credential, &entries, true);
    assert_eq!(checked.map(|entry| entry.api_key.as_str()), Some("second"));
    let unchecked = resolve_config_codex_style_key(&credential, &entries, false);
    assert_eq!(unchecked.map(|entry| entry.api_key.as_str()), Some("first"));
}

#[test]
fn apply_oauth_model_alias_rename() {
    let rules = alias_rules(
        "codex",
        vec![ModelAlias {
            display_name: "Configured GPT Five".to_owned(),
            ..alias("gpt-5", "g5")
        }],
    );
    let models = vec![ModelInfo {
        name: "models/gpt-5".to_owned(),
        display_name: "Upstream GPT Five".to_owned(),
        ..model("gpt-5")
    }];
    let out = apply_model_aliases(&rules, "codex", "oauth", &Auth::default(), models);
    assert_eq!(ids(&out), ["g5"]);
    assert_eq!(out[0].name, "models/g5");
    assert_eq!(out[0].display_name, "Configured GPT Five");
}

#[test]
fn apply_oauth_model_alias_fork_adds_alias() {
    let rules = alias_rules(
        "codex",
        vec![ModelAlias {
            fork: true,
            display_name: "Configured GPT Five".to_owned(),
            ..alias("gpt-5", "g5")
        }],
    );
    let models = vec![ModelInfo {
        name: "models/gpt-5".to_owned(),
        display_name: "Upstream GPT Five".to_owned(),
        ..model("gpt-5")
    }];
    let out = apply_model_aliases(&rules, "codex", "oauth", &Auth::default(), models);
    assert_eq!(ids(&out), ["gpt-5", "g5"]);
    assert_eq!(out[1].name, "models/g5");
    assert_eq!(out[0].display_name, "Upstream GPT Five");
    assert_eq!(out[1].display_name, "Configured GPT Five");
}

#[test]
fn apply_oauth_model_alias_preserves_upstream_display_name_by_default() {
    let rules = alias_rules("codex", vec![alias("gpt-5", "g5")]);
    let models = vec![ModelInfo {
        display_name: "Upstream GPT Five".to_owned(),
        ..model("gpt-5")
    }];
    let out = apply_model_aliases(&rules, "codex", "oauth", &Auth::default(), models);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].display_name, "Upstream GPT Five");
}

#[test]
fn apply_oauth_model_alias_fork_adds_multiple_aliases() {
    let fork = |name, to| ModelAlias {
        fork: true,
        ..alias(name, to)
    };
    let rules = alias_rules("codex", vec![fork("gpt-5", "g5"), fork("gpt-5", "g5-2")]);
    let models = vec![ModelInfo {
        name: "models/gpt-5".to_owned(),
        ..model("gpt-5")
    }];
    let out = apply_model_aliases(&rules, "codex", "oauth", &Auth::default(), models);
    assert_eq!(ids(&out), ["gpt-5", "g5", "g5-2"]);
    assert_eq!(out[1].name, "models/g5");
    assert_eq!(out[2].name, "models/g5-2");
}

#[test]
fn apply_oauth_model_alias_meta() {
    let rules = alias_rules(
        "meta",
        vec![ModelAlias {
            display_name: "Muse Latest".to_owned(),
            ..alias("muse-spark-1.3", "muse-latest")
        }],
    );
    let models = vec![ModelInfo {
        name: "models/muse-spark-1.3".to_owned(),
        display_name: "Muse Spark 1.3".to_owned(),
        ..model("muse-spark-1.3")
    }];
    let out = apply_model_aliases(&rules, "meta", "oauth", &Auth::default(), models.clone());
    assert_eq!(ids(&out), ["muse-latest"]);
    assert_eq!(out[0].name, "models/muse-latest");
    assert_eq!(out[0].display_name, "Muse Latest");

    let api_key_out = apply_model_aliases(&rules, "meta", "apikey", &Auth::default(), models);
    assert_eq!(ids(&api_key_out), ["muse-spark-1.3"]);
}

#[test]
fn apply_oauth_model_alias_plugin_provider() {
    let rules = alias_rules(
        "sample-provider",
        vec![alias("sample-model-latest", "sample-latest")],
    );
    let models = vec![ModelInfo {
        name: "models/sample-model-latest".to_owned(),
        ..model("sample-model-latest")
    }];
    let out = apply_model_aliases(
        &rules,
        "sample-provider",
        "oauth",
        &Auth::default(),
        models.clone(),
    );
    assert_eq!(ids(&out), ["sample-latest"]);
    assert_eq!(out[0].name, "models/sample-latest");

    // An API key's models keep their names.
    let out = apply_model_aliases(
        &rules,
        "sample-provider",
        "api_key",
        &Auth::default(),
        models,
    );
    assert_eq!(ids(&out), ["sample-model-latest"]);
}

#[test]
fn apply_oauth_model_alias_per_auth_alias() {
    let models = vec![ModelInfo {
        name: "models/gpt-5.3-codex-spark".to_owned(),
        ..model("gpt-5.3-codex-spark")
    }];
    let auth = auth(
        "",
        "codex",
        &[(
            "model_aliases",
            r#"[{"name":"gpt-5.3-codex-spark","alias":"gpt-5.5","display-name":"Configured GPT Five"}]"#,
        )],
    );
    let out = apply_model_aliases(
        &RegistrationRules::default(),
        "codex",
        "oauth",
        &auth,
        models,
    );
    assert_eq!(ids(&out), ["gpt-5.5"]);
    assert_eq!(out[0].name, "models/gpt-5.5");
    assert_eq!(out[0].display_name, "Configured GPT Five");
    assert_eq!(out[0].metadata_model_id, "gpt-5.3-codex-spark");
}

#[test]
fn apply_oauth_model_alias_preserves_metadata_model_id() {
    let rules = alias_rules(
        "codex",
        vec![
            ModelAlias {
                fork: true,
                ..alias("gpt-6-astra", "codex-main")
            },
            alias("gpt-5.6-luna", "codex-luna"),
        ],
    );
    let models = vec![
        ModelInfo {
            name: "models/gpt-6-astra".to_owned(),
            ..model("gpt-6-astra")
        },
        ModelInfo {
            name: "models/gpt-5.6-luna".to_owned(),
            ..model("gpt-5.6-luna")
        },
    ];
    let out = apply_model_aliases(&rules, "codex", "oauth", &Auth::default(), models);
    assert_eq!(ids(&out), ["gpt-6-astra", "codex-main", "codex-luna"]);
    assert_eq!(out[1].metadata_model_id, "gpt-6-astra");
    assert_eq!(out[2].metadata_model_id, "gpt-5.6-luna");
}

#[test]
fn apply_model_prefixes_preserves_metadata_model_id() {
    let models = vec![
        model("gpt-6-astra"),
        ModelInfo {
            metadata_model_id: "gpt-6-astra".to_owned(),
            ..model("codex-main")
        },
    ];
    let out = apply_model_prefixes(models, "1", false);
    assert_eq!(
        ids(&out),
        ["gpt-6-astra", "1/gpt-6-astra", "codex-main", "1/codex-main"]
    );
    assert_eq!(out[1].metadata_model_id, "gpt-6-astra");
    assert_eq!(out[3].metadata_model_id, "gpt-6-astra");
}

#[test]
fn forced_prefixes_list_models_only_under_the_prefix() {
    let out = apply_model_prefixes(vec![model("gpt-5"), model("team")], " team ", true);
    assert_eq!(ids(&out), ["team/gpt-5", "team", "team/team"]);
    let unchanged = apply_model_prefixes(vec![model("gpt-5")], " ", true);
    assert_eq!(ids(&unchanged), ["gpt-5"]);
}

#[test]
fn oauth_model_alias_channel() {
    assert_eq!(alias_channel("gemini", "oauth"), "");
    for provider in ["kimi", "kimi-ai", "kimi.ai", "kimi.com"] {
        assert_eq!(alias_channel(provider, "oauth"), provider);
    }
    assert_eq!(alias_channel("meta", "oauth"), "meta");
    assert_eq!(alias_channel("meta", "api_key"), "");
    assert_eq!(
        alias_channel(" Sample-Provider ", "oauth"),
        "sample-provider"
    );
    assert_eq!(alias_channel("sample-provider", "api_key"), "");
}

#[test]
fn sanitize_oauth_model_alias_preserves_optional_fields() {
    let aliases = sanitize_aliases(vec![
        ModelAlias {
            fork: true,
            display_name: " GPT Five ".to_owned(),
            ..alias(" gpt-5 ", " g5 ")
        },
        alias("gpt-6", "g6"),
    ]);
    assert_eq!(
        aliases,
        [
            ModelAlias {
                fork: true,
                display_name: "GPT Five".to_owned(),
                ..alias("gpt-5", "g5")
            },
            alias("gpt-6", "g6"),
        ]
    );
}

#[test]
fn sanitize_oauth_model_alias_allows_multiple_aliases_for_same_name() {
    let name = "gemini-claude-opus-4-5-thinking";
    let fork = |to| ModelAlias {
        fork: true,
        ..alias(name, to)
    };
    let aliases = vec![
        fork("claude-opus-4-5-20251101"),
        fork("claude-opus-4-5-20251101-thinking"),
        fork("claude-opus-4-5"),
    ];
    assert_eq!(sanitize_aliases(aliases.clone()), aliases);
}

#[test]
fn sanitize_drops_incomplete_self_and_repeated_aliases() {
    let aliases = sanitize_aliases(vec![
        alias("", "a"),
        alias("b", " "),
        alias("Same", "same"),
        alias("one", "X"),
        alias("two", " x "),
    ]);
    assert_eq!(aliases, [alias("one", "X")]);
}

#[test]
fn apply_oauth_settings_max_context_length() {
    let rules = settings_rules(vec![
        setting("gpt-6-sol", "", 524_288),
        setting("deepseek-v4-flash", "", 1_048_576),
    ]);
    let models = ["gpt-6-sol", "deepseek-v4-flash", "gpt-5-codex"]
        .map(|id| ModelInfo {
            context_length: 272_000,
            ..model(id)
        })
        .to_vec();
    let out = apply_model_settings(&rules, "codex", "oauth", models);
    let limits: Vec<(u64, u64)> = out
        .iter()
        .map(|model| (model.max_context_length, model.context_length))
        .collect();
    assert_eq!(
        limits,
        [(524_288, 524_288), (1_048_576, 1_048_576), (0, 272_000)]
    );
}

#[test]
fn apply_oauth_settings_aliased_model() {
    let rules = settings_rules(vec![setting("gpt-6-sol", "", 524_288)]);
    let models = vec![ModelInfo {
        metadata_model_id: "gpt-6-sol".to_owned(),
        context_length: 272_000,
        ..model("custom-sol")
    }];
    let out = apply_model_settings(&rules, "codex", "oauth", models);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].max_context_length, 524_288);
}

#[test]
fn apply_oauth_settings_matched_by_alias() {
    let rules = settings_rules(vec![setting("gpt-6-sol", "sol-preview", 524_288)]);
    let models = ["sol-preview", "other-model"]
        .map(|id| ModelInfo {
            context_length: 272_000,
            ..model(id)
        })
        .to_vec();
    let out = apply_model_settings(&rules, "codex", "oauth", models);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].max_context_length, 524_288);
    assert_eq!(out[1].max_context_length, 0);
}

#[test]
fn apply_oauth_settings_skips_api_key() {
    let rules = settings_rules(vec![setting("gpt-6-sol", "", 524_288)]);
    let models = vec![ModelInfo {
        context_length: 1_048_576,
        max_context_length: 1_048_576,
        ..model("gpt-6-sol")
    }];
    let out = apply_model_settings(&rules, "codex", "api_key", models);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].max_context_length, 1_048_576);
    assert_eq!(out[0].context_length, 1_048_576);
}

#[test]
fn resolve_oauth_model_setting_priority() {
    let settings = [
        setting("upstream", "", 524_288),
        setting("upstream", "public", 1_048_576),
    ];
    let limit = |id, metadata_id| {
        resolve_model_setting(&settings, id, metadata_id, "")
            .map(|setting| setting.max_context_length)
    };
    assert_eq!(limit("upstream", "upstream"), Some(524_288));
    assert_eq!(limit("public", "upstream"), Some(1_048_576));
    assert_eq!(limit("unknown", ""), None);
}

#[test]
fn build_config_models_display_name() {
    let claude = build_config_models(
        &[ConfiguredModel {
            display_name: "Claude Catalog Name".to_owned(),
            ..configured("claude-upstream", "claude-catalog")
        }],
        "anthropic",
        "claude",
    );
    assert_eq!(claude[0].display_name, "Claude Catalog Name");

    let codex = build_codex_config_models(
        &api_key_entry(
            "",
            vec![ConfiguredModel {
                display_name: "Codex Catalog Name".to_owned(),
                ..configured("gpt-5.5", "gpt-5.5")
            }],
        ),
        StaticCatalog::embedded(),
    );
    assert_eq!(codex[0].display_name, "Codex Catalog Name");

    let gemini = build_config_models(
        &[ConfiguredModel {
            display_name: "Gemini Catalog Name".to_owned(),
            ..configured("gemini-upstream", "gemini-catalog")
        }],
        "google",
        "gemini",
    );
    assert_eq!(gemini[0].display_name, "Gemini Catalog Name");
    let vertex = build_config_models(
        &[ConfiguredModel {
            display_name: "Vertex Catalog Name".to_owned(),
            ..configured("vertex-upstream", "vertex-catalog")
        }],
        "google",
        "vertex",
    );
    assert_eq!(vertex[0].display_name, "Vertex Catalog Name");
    assert_eq!(
        (vertex[0].owned_by.as_str(), vertex[0].model_type.as_str()),
        ("google", "vertex")
    );
    let xai = build_config_models(
        &[ConfiguredModel {
            display_name: "xAI Catalog Name".to_owned(),
            ..configured("grok-4.5", "grok-latest")
        }],
        "xai",
        "xai",
    );
    assert_eq!(xai[0].display_name, "xAI Catalog Name");
}

#[test]
fn build_codex_config_models_selects_defaults_or_configured_models() {
    let configured = build_codex_config_models(
        &api_key_entry("", vec![configured("upstream-codex", "configured-codex")]),
        StaticCatalog::embedded(),
    );
    assert_eq!(ids(&configured), ["configured-codex"]);

    let defaults = build_codex_config_models(&ApiKeyEntry::default(), StaticCatalog::embedded());
    assert_eq!(defaults.len(), codex_pro_models().len());
    let default_ids = id_set(&defaults);
    for id in IMAGE_MODELS {
        assert!(default_ids.contains(id), "{id} is missing");
    }
}

#[test]
fn build_config_models_display_name_fallback() {
    let models = build_config_models(
        &[configured("claude-upstream", "claude-catalog")],
        "anthropic",
        "claude",
    );
    assert_eq!(models[0].display_name, "claude-upstream");
}

#[test]
fn build_config_models_propagates_max_context_length() {
    const WANT: u64 = 1_048_576;
    let codex = build_codex_config_models(
        &api_key_entry(
            "",
            vec![ConfiguredModel {
                max_context_length: WANT,
                ..configured("codex-upstream", "codex-alias")
            }],
        ),
        StaticCatalog::embedded(),
    );
    let claude = build_config_models(
        &[ConfiguredModel {
            max_context_length: WANT,
            ..configured("claude-upstream", "claude-alias")
        }],
        "anthropic",
        "claude",
    );
    let compat = build_openai_compat_models(&OpenAiCompatEntry {
        name: "compat".to_owned(),
        models: vec![CompatModel {
            name: "compat-upstream".to_owned(),
            alias: "compat-alias".to_owned(),
            max_context_length: WANT,
            ..CompatModel::default()
        }],
        ..OpenAiCompatEntry::default()
    });
    let gemini = build_config_models(
        &[ConfiguredModel {
            max_context_length: WANT,
            ..configured("gemini-upstream", "gemini-alias")
        }],
        "google",
        "gemini",
    );
    let interactions = build_config_models(
        &[ConfiguredModel {
            max_context_length: WANT,
            ..configured("interactions-upstream", "interactions-alias")
        }],
        "google",
        "gemini",
    );
    let xai = build_config_models(
        &[ConfiguredModel {
            max_context_length: WANT,
            ..configured("xai-upstream", "xai-alias")
        }],
        "xai",
        "xai",
    );
    for (name, model) in [
        ("codex", &codex[0]),
        ("claude", &claude[0]),
        ("gemini", &gemini[0]),
        ("interactions", &interactions[0]),
        ("xai", &xai[0]),
        ("openai-compatibility", &compat[0]),
    ] {
        assert_eq!(model.context_length, WANT, "{name}");
        assert_eq!(model.max_context_length, WANT, "{name}");
    }
}

#[test]
fn config_models_keep_each_alias_once_with_their_details() {
    let models = build_config_models(
        &[
            ConfiguredModel {
                is_compat: true,
                ..configured(" claude-opus-4-6 ", " opus ")
            },
            configured("claude-sonnet-4-6", "OPUS"),
            configured("", ""),
            ConfiguredModel {
                thinking: Some(ThinkingSupport {
                    levels: vec![" High ".to_owned()],
                    ..ThinkingSupport::default()
                }),
                ..configured("custom-model", "")
            },
        ],
        "anthropic",
        "claude",
    );
    assert_eq!(ids(&models), ["opus", "custom-model"]);
    let opus = &models[0];
    assert_eq!(opus.metadata_model_id, "claude-opus-4-6");
    assert_eq!(opus.display_name, "claude-opus-4-6");
    assert_eq!(opus.object, "model");
    assert_eq!(opus.owned_by, "anthropic");
    assert_eq!(opus.model_type, "claude");
    assert!(opus.user_defined && opus.is_compat);
    assert!(!opus.explicit_thinking);
    assert_eq!(
        opus.thinking.as_ref(),
        open_ferry_translate::models::ModelCatalog::embedded().thinking("claude-opus-4-6")
    );
    let custom = &models[1];
    assert!(custom.explicit_thinking);
    assert_eq!(
        custom
            .thinking
            .as_ref()
            .map(|thinking| thinking.levels.clone()),
        Some(vec!["high".to_owned()])
    );
}

#[test]
fn resolve_config_claude_key_uses_config_index() {
    let entries = [
        api_key_entry("shared-key", vec![configured("first", "")]),
        api_key_entry("shared-key", vec![configured("second", "")]),
    ];
    let auth = auth(
        "",
        "claude",
        &[
            ("api_key", "shared-key"),
            ("source", "config:claude[token-1]"),
            ("config_index", "1"),
        ],
    );
    let entry = resolve_config_claude_key(&auth, &entries).expect("an entry");
    assert_eq!(entry.models, [configured("second", "")]);
}

#[test]
fn claude_api_keys_use_their_entry() {
    let rules = RegistrationRules {
        oauth_excluded_models: channel("claude", vec!["claude-opus-4-6".to_owned()]),
        claude_keys: vec![
            ApiKeyEntry {
                base_url: "https://claude.example.com".to_owned(),
                excluded_models: vec!["claude-sonnet-*".to_owned()],
                ..api_key_entry("claude-key", Vec::new())
            },
            api_key_entry("listed-key", vec![configured("claude-opus-4-6", "opus")]),
        ],
        ..RegistrationRules::default()
    };

    // The catalog's models, without the entry's exclusions; the global
    // OAuth exclusions don't apply to API keys.
    let catalog_key = auth(
        "claude-catalog-key",
        "claude",
        &[
            ("api_key", "claude-key"),
            ("base_url", "https://claude.example.com"),
        ],
    );
    let got = id_set(&registered(&catalog_key, &rules));
    assert!(got.contains("claude-opus-4-6"));
    assert!(got.contains("claude-haiku-4-5-20251001"));
    assert!(!got.iter().any(|id| id.starts_with("claude-sonnet-")));

    let listed_key = auth("claude-listed-key", "claude", &[("api_key", "listed-key")]);
    assert_eq!(
        id_set(&registered(&listed_key, &rules)),
        BTreeSet::from(["opus".to_owned()])
    );
}

// service_models.go: the gemini and vertex cases of registerModelsForAuth,
// resolveConfigGeminiKey and resolveConfigVertexCompatKey (no upstream
// test).
#[test]
fn gemini_and_vertex_keys_use_their_entry() {
    let rules = RegistrationRules {
        oauth_excluded_models: channel("gemini", vec!["gemini-2.5-pro".to_owned()]),
        gemini_keys: vec![
            ApiKeyEntry {
                excluded_models: vec!["gemini-2.5-flash*".to_owned()],
                ..api_key_entry("gemini-key", Vec::new())
            },
            api_key_entry("listed-key", vec![configured("gemini-2.5-pro", "pro")]),
        ],
        vertex_keys: vec![api_key_entry(
            "vertex-key",
            vec![configured("gemini-2.5-pro", "vertex-pro")],
        )],
        ..RegistrationRules::default()
    };

    // The catalog's models, without the entry's exclusions; the global
    // OAuth exclusions don't apply to API keys.
    let catalog_key = auth(
        "gemini-catalog-key",
        "gemini",
        &[("api_key", "gemini-key"), ("auth_kind", "apikey")],
    );
    let got = id_set(&registered(&catalog_key, &rules));
    assert!(got.contains("gemini-2.5-pro"));
    assert!(!got.iter().any(|id| id.starts_with("gemini-2.5-flash")));
    let catalog_ids = id_set(&StaticCatalog::embedded().gemini_models());
    assert!(got.is_subset(&catalog_ids));

    let listed_key = auth("gemini-listed-key", "gemini", &[("api_key", "listed-key")]);
    let models = registered(&listed_key, &rules);
    assert_eq!(ids(&models), ["pro"]);
    assert_eq!(
        (models[0].owned_by.as_str(), models[0].model_type.as_str()),
        ("google", "gemini")
    );
    assert!(models[0].user_defined);
    assert!(models[0].thinking.is_some(), "the catalog's thinking");

    // A key without an entry gets the whole catalog; any other credential
    // gets it under the global exclusions.
    let unknown = auth("gemini-other", "gemini", &[("api_key", "other")]);
    assert_eq!(id_set(&registered(&unknown, &rules)), catalog_ids);
    let oauth = auth("gemini-oauth", "gemini", &[]);
    let got = id_set(&registered(&oauth, &rules));
    assert!(!got.is_empty() && !got.contains("gemini-2.5-pro"));

    let vertex = auth("vertex-key", "vertex", &[("api_key", "vertex-key")]);
    match auth_models(&vertex, &rules) {
        AuthModels::Register { provider, models } => {
            assert_eq!(provider, "vertex");
            assert_eq!(ids(&models), ["vertex-pro"]);
            assert_eq!(models[0].model_type, "vertex");
        }
        other => panic!("{other:?}"),
    }
    let service_account = auth("vertex.json", "vertex", &[]);
    assert_eq!(
        id_set(&registered(&service_account, &rules)),
        id_set(&StaticCatalog::embedded().vertex_models())
    );
}

#[test]
fn resolve_config_gemini_and_vertex_keys() {
    let entries = [
        ApiKeyEntry {
            base_url: "https://a.example.com".to_owned(),
            ..api_key_entry("shared", vec![configured("a", "")])
        },
        ApiKeyEntry {
            base_url: "https://b.example.com".to_owned(),
            ..api_key_entry("shared", vec![configured("b", "")])
        },
        ApiKeyEntry {
            base_url: "https://keyless.example.com".to_owned(),
            ..api_key_entry("", vec![configured("keyless", "")])
        },
    ];
    let first = |auth: &Auth| {
        resolve_config_gemini_key(auth, &entries).map(|entry| entry.models[0].name.clone())
    };
    let with = |attrs: &[(&str, &str)]| auth("x", "gemini", attrs);
    assert_eq!(
        first(&with(&[
            ("api_key", "SHARED"),
            ("base_url", "https://b.example.com")
        ])),
        Some("b".to_owned())
    );
    assert_eq!(
        first(&with(&[("base_url", "https://keyless.example.com")])),
        Some("keyless".to_owned())
    );
    // The key matches, but no base URL does: Gemini finds nothing, Vertex
    // falls back to the first with the key.
    let stray = with(&[("api_key", "shared"), ("base_url", "https://c.example.com")]);
    assert_eq!(first(&stray), None);
    assert_eq!(
        resolve_config_vertex_key(&stray, &entries).map(|entry| entry.models[0].name.as_str()),
        Some("a")
    );
    // A config credential goes by its index.
    let indexed = with(&[
        ("api_key", "shared"),
        ("source", "config:gemini[token]"),
        ("config_index", "1"),
    ]);
    assert_eq!(first(&indexed), Some("b".to_owned()));
}

#[test]
fn resolve_model_info_uses_suffix_free_static_capabilities() {
    assert!(resolve_thinking("claude-opus-4-6(high)", None).is_some());
}

#[test]
fn resolve_model_info_explicit_thinking_overrides() {
    let support = ThinkingSupport {
        levels: [" XHIGH ", "xhigh", " High "].map(str::to_owned).to_vec(),
        ..ThinkingSupport::default()
    };
    let thinking = resolve_thinking("custom-model", Some(&support)).expect("thinking settings");
    assert_eq!(thinking.levels, ["xhigh", "high"]);
}

#[test]
fn normalize_thinking_support_derives_special_level_flags() {
    let support = normalize_thinking(&ThinkingSupport {
        levels: ["low", "none", "auto"].map(str::to_owned).to_vec(),
        ..ThinkingSupport::default()
    });
    assert!(support.zero_allowed);
    assert!(support.dynamic_allowed);
}

#[test]
fn resolve_model_info_unknown_model_keeps_missing_capability() {
    assert_eq!(resolve_thinking("unknown-configured-model", None), None);
}

#[test]
fn parse_suffix_strips_a_trailing_parenthesis() {
    assert_eq!(parse_suffix("claude-opus-4-6(high)"), "claude-opus-4-6");
    assert_eq!(parse_suffix("a(b)(c)"), "a(b)");
    assert_eq!(parse_suffix("a(b)c"), "a(b)c");
    assert_eq!(parse_suffix("plain"), "plain");
}

#[test]
fn match_wildcard_matches_any_text_for_a_star() {
    for (pattern, value, want) in [
        ("gpt-5", "gpt-5", true),
        ("gpt-5", "gpt-5.5", false),
        ("gpt-*", "gpt-5.5", true),
        ("*-mini", "gpt-5-mini", true),
        ("*-mini", "gpt-5", false),
        ("gpt-*-mini", "gpt-5-mini", true),
        ("gpt-*-mini", "gpt-mini", false),
        ("a*b*c", "a-x-b-y-c", true),
        ("a*b*c", "a-x-c", false),
        ("*", "", true),
        ("", "", false),
    ] {
        assert_eq!(match_wildcard(pattern, value), want, "{pattern} {value}");
    }
}

#[test]
fn excluded_models_ignore_case_and_whitespace() {
    let models = ["GPT-5", "gpt-5-mini", "o3"].map(model).to_vec();
    let excluded = [" gpt-5* ", " "].map(str::to_owned);
    assert_eq!(ids(&apply_excluded_models(models, &excluded)), ["o3"]);
}

#[test]
fn rewrite_model_info_name_swaps_the_id() {
    assert_eq!(rewrite_model_info_name("gpt-5", "gpt-5", "g5"), "g5");
    assert_eq!(
        rewrite_model_info_name("models/gpt-5", "gpt-5", "g5"),
        "models/g5"
    );
    assert_eq!(rewrite_model_info_name("other", "gpt-5", "g5"), "other");
    assert_eq!(rewrite_model_info_name("gpt-5", "gpt-5", "GPT-5"), "gpt-5");
}

#[test]
fn parse_jwt_token_missing_plan_type_defaults_to_free() {
    assert_eq!(jwt_plan_type(&codex_id_token(None)), "free");
    assert_eq!(jwt_plan_type(&codex_id_token(Some("team"))), "team");
    assert_eq!(jwt_plan_type(&codex_id_token(Some("   "))), "free");
    assert_eq!(jwt_plan_type("not-a-token"), "free");
    assert_eq!(jwt_plan_type("a.!!!.c"), "free");
}

#[test]
fn synthesize_auth_file_codex_plan_type() {
    let cases = [
        (
            "explicit plan_type in metadata",
            json!({"type": "codex", "plan_type": "pro"}),
            "pro",
        ),
        (
            "id_token with plan_type",
            json!({"type": "codex", "id_token": codex_id_token(Some("team"))}),
            "team",
        ),
        (
            "id_token without plan_type defaults to free",
            json!({"type": "codex", "id_token": codex_id_token(None)}),
            "free",
        ),
        ("no plan type", json!({"type": "codex"}), ""),
    ];
    for (name, metadata, want) in cases {
        let auth = Auth {
            metadata: object(metadata),
            ..auth("codex.json", "codex", &[])
        };
        assert_eq!(codex_plan_type(&auth), want, "{name}");
    }

    // The loader's attribute wins.
    let auth = Auth {
        metadata: object(json!({"plan_type": "pro"})),
        ..auth("codex.json", "codex", &[("plan_type", " team ")])
    };
    assert_eq!(codex_plan_type(&auth), "team");
}

#[test]
fn codex_accounts_get_their_plans_models() {
    let catalog = StaticCatalog::embedded();
    let cases = [
        (json!({"id_token": codex_id_token(None)}), CodexPlan::Free),
        (
            json!({"id_token": codex_id_token(Some("plus"))}),
            CodexPlan::Plus,
        ),
        (json!({"plan_type": "business"}), CodexPlan::Team),
        (json!({"email": "user@example.com"}), CodexPlan::Pro),
    ];
    for (metadata, plan) in cases {
        let auth = Auth {
            metadata: object(metadata),
            ..auth("codex.json", "codex", &[])
        };
        assert_eq!(
            id_set(&registered(&auth, &RegistrationRules::default())),
            id_set(&catalog.codex_models(plan)),
            "{plan:?}"
        );
    }
}

#[test]
fn file_synthesizer_oauth_excluded_models_merged() {
    let rules = RegistrationRules {
        oauth_excluded_models: channel("claude", vec!["shared".to_owned(), "model-b".to_owned()]),
        ..RegistrationRules::default()
    };
    let auth = Auth {
        metadata: object(json!({
            "type": "claude",
            "excluded_models": ["custom-model", "MODEL-B"],
        })),
        ..auth("auth.json", "claude", &[])
    };
    let excluded = credential_excluded_models(&auth, &rules, "claude").expect("a list");
    let merged: BTreeSet<String> = excluded
        .iter()
        .map(|item| go::to_lower(item.trim()))
        .collect();
    assert_eq!(
        merged.into_iter().collect::<Vec<_>>().join(","),
        "custom-model,model-b,shared"
    );
}

#[test]
fn per_account_exclusions_add_to_the_global_ones() {
    let rules = RegistrationRules {
        oauth_excluded_models: channel("claude", vec!["claude-haiku-4-5-20251001".to_owned()]),
        ..RegistrationRules::default()
    };
    let file_auth = Auth {
        metadata: object(json!({
            "access_token": "fictional",
            "excluded-models": ["claude-opus-4-6", " CLAUDE-SONNET-4-6 ", 5],
        })),
        ..auth("claude.json", "claude", &[])
    };
    let got = id_set(&registered(&file_auth, &rules));
    for id in [
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-haiku-4-5-20251001",
    ] {
        assert!(!got.contains(id), "{id} is registered");
    }
    assert!(got.contains("claude-sonnet-5"));

    // An empty attribute leaves the global list.
    let attribute_auth = auth("claude-2.json", "claude", &[("excluded_models", " ")]);
    let got = id_set(&registered(&attribute_auth, &rules));
    assert!(!got.contains("claude-haiku-4-5-20251001"));
    assert!(got.contains("claude-opus-4-6"));
}

#[test]
fn file_synthesizer_oauth_model_aliases() {
    let auth = Auth {
        metadata: object(json!({
            "type": "codex",
            "email": "codex@example.com",
            "model_aliases": [
                {"name": " gpt-5.3-codex-spark ", "alias": " gpt-5.5 "},
                {"name": "gpt-5.3-codex-spark", "alias": "gpt-5.4", "fork": true},
                {"name": "gpt-5.3-codex-spark", "alias": "gpt-5.5"},
                {"name": "", "alias": "ignored"},
            ],
        })),
        ..auth("codex-auth.json", "codex", &[])
    };
    assert_eq!(
        per_auth_aliases(&auth),
        [
            alias("gpt-5.3-codex-spark", "gpt-5.5"),
            ModelAlias {
                fork: true,
                ..alias("gpt-5.3-codex-spark", "gpt-5.4")
            },
        ]
    );
}

#[test]
fn per_auth_aliases_decode_as_go_decodes_them() {
    let from_metadata = |metadata: Value| {
        per_auth_aliases(&Auth {
            metadata: object(metadata),
            ..Auth::default()
        })
    };
    // The legacy key, keys matched ignoring case, and unknown keys skipped.
    assert_eq!(
        from_metadata(json!({"model-aliases": [{"NAME": "a", "Alias": "b", "x": 1}]})),
        [alias("a", "b")]
    );
    // A null canonical key hides the legacy one.
    assert!(
        from_metadata(
            json!({"model_aliases": null, "model-aliases": [{"name": "a", "alias": "b"}]})
        )
        .is_empty()
    );
    // A list that doesn't decode counts as none.
    assert!(
        from_metadata(json!({"model_aliases": [{"name": "a", "alias": "b"}, {"fork": "yes"}]}))
            .is_empty()
    );
    assert!(from_metadata(json!({"model_aliases": {"name": "a"}})).is_empty());

    // The attribute wins over the metadata, even when it doesn't decode.
    let auth = Auth {
        metadata: object(json!({"model_aliases": [{"name": "a", "alias": "b"}]})),
        ..auth("", "codex", &[("model_aliases", "not json")])
    };
    assert!(per_auth_aliases(&auth).is_empty());
}

#[test]
fn per_auth_aliases_come_before_the_channels() {
    let rules = alias_rules(
        "codex",
        vec![
            alias("gpt-5-global", "gpt-5.5"),
            alias("gpt-6-luna", "luna"),
        ],
    );
    let aliases = aliases_for_auth(
        &rules,
        "codex",
        vec![alias("gpt-5.3-codex-spark", "GPT-5.5")],
    );
    assert_eq!(
        aliases,
        [
            alias("gpt-5.3-codex-spark", "GPT-5.5"),
            alias("gpt-6-luna", "luna")
        ]
    );
    assert_eq!(
        aliases_for_auth(&rules, "claude", vec![alias("a", "b")]),
        [alias("a", "b")]
    );
}

#[test]
fn registration_skips_and_unregisters() {
    let rules = RegistrationRules::default();
    assert_eq!(
        auth_models(&auth("", "claude", &[]), &rules),
        AuthModels::Ignore
    );
    let disabled = Auth {
        disabled: true,
        ..auth("claude.json", "claude", &[])
    };
    assert_eq!(auth_models(&disabled, &rules), AuthModels::Unregister);
    assert_eq!(
        auth_models(&auth("aistudio.json", "aistudio", &[]), &rules),
        AuthModels::Unregister
    );
    // A Codex API key without a matching entry serves nothing.
    assert_eq!(
        auth_models(&auth("codex-key", "codex", &[("api_key", "k")]), &rules),
        AuthModels::Unregister
    );
    match auth_models(&auth("claude.json", " Claude ", &[]), &rules) {
        AuthModels::Register { provider, models } => {
            assert_eq!(provider, "claude");
            assert_eq!(
                id_set(&models),
                id_set(&StaticCatalog::embedded().claude_models())
            );
        }
        other => panic!("{other:?}"),
    }

    let registry = ModelRegistry::new();
    registry.register_client("claude.json", "claude", &[model("stale")]);
    registry.register_auth(&disabled, &rules);
    assert!(registry.models_for_client("claude.json").is_empty());
}

#[test]
fn registration_applies_aliases_settings_then_prefixes() {
    let rules = RegistrationRules {
        force_model_prefix: true,
        oauth_model_alias: channel("codex", vec![alias("gpt-6-luna", "luna")]),
        oauth_settings: channel("codex", vec![setting("gpt-6-luna", "", 99)]),
        ..RegistrationRules::default()
    };
    let auth = Auth {
        prefix: "team-a".to_owned(),
        ..auth("codex.json", "codex", &[("plan_type", "free")])
    };
    let models = registered(&auth, &rules);
    let luna = models
        .iter()
        .find(|model| model.id == "team-a/luna")
        .expect("team-a/luna");
    assert_eq!(luna.metadata_model_id, "gpt-6-luna");
    assert_eq!(luna.max_context_length, 99);
    assert!(models.iter().all(|model| model.id.starts_with("team-a/")));
}

#[test]
fn api_key_entries_hide_the_key_when_debugged() {
    let mut entry = api_key_entry("sk-fictional-test-key", Vec::new());
    entry.base_url = "https://gateway.example/?key=sk-fictional-test-key".into();
    let shown = format!("{entry:?}");
    assert!(!shown.contains("sk-fictional-test-key"), "{shown}");
    assert!(
        shown.contains(r#""https://gateway.example/?<redacted>""#),
        "{shown}"
    );
}

#[test]
fn rules_come_from_the_config() {
    let config = Config::parse(concat!(
        "force-model-prefix: true\n",
        "oauth-excluded-models:\n  codex: [gpt-5-mini]\n",
        "oauth-model-alias:\n  claude:\n    - name: claude-opus\n      alias: opus\n      fork: true\n",
        "oauth-settings:\n  claude:\n    - name: claude-opus\n      max-context-length: -5\n",
        "codex-api-key:\n  - api-key: k\n    base-url: https://example.test\n",
        "    excluded-models: [x]\n    models:\n      - name: gpt-5\n        alias: g5\n",
        "        max-context-length: 1000\n        support-configuration-update: true\n",
    ))
    .unwrap();
    let rules = RegistrationRules::from(&config);
    assert!(rules.force_model_prefix);
    assert_eq!(rules.oauth_excluded_models["codex"], ["gpt-5-mini"]);
    let alias = &rules.oauth_model_alias["claude"][0];
    assert_eq!(
        (alias.name.as_str(), alias.alias.as_str(), alias.fork),
        ("claude-opus", "opus", true)
    );
    assert_eq!(rules.oauth_settings["claude"][0].max_context_length, 0);
    assert!(rules.claude_keys.is_empty());
    let key = &rules.codex_keys[0];
    assert_eq!(
        (key.api_key.as_str(), key.base_url.as_str()),
        ("k", "https://example.test")
    );
    assert_eq!(key.excluded_models, ["x"]);
    let model = &key.models[0];
    assert_eq!(
        (
            model.name.as_str(),
            model.alias.as_str(),
            model.max_context_length
        ),
        ("gpt-5", "g5", 1000)
    );
    assert!(model.support_configuration_update);

    let config = Config::parse(concat!(
        "gemini-api-key:\n  - api-key: g\n    excluded-models: [y]\n    models:\n",
        "      - name: gemini-2.5-pro\n        alias: pro\n        max-context-length: 7\n",
        "        is-compat: true\n",
        "vertex-api-key:\n  - api-key: v\n    base-url: https://vertex.example.test\n",
        "    models:\n      - name: gemini-2.5-flash\n        alias: flash\n",
        "        display-name: Flash\n",
    ))
    .unwrap();
    let rules = RegistrationRules::from(&config);
    let gemini = &rules.gemini_keys[0];
    assert_eq!(
        (gemini.api_key.as_str(), gemini.excluded_models.as_slice()),
        ("g", ["y".to_owned()].as_slice())
    );
    assert_eq!(
        gemini.models,
        [ConfiguredModel {
            max_context_length: 7,
            is_compat: true,
            ..configured("gemini-2.5-pro", "pro")
        }]
    );
    let vertex = &rules.vertex_keys[0];
    assert_eq!(vertex.base_url, "https://vertex.example.test");
    assert_eq!(
        vertex.models,
        [ConfiguredModel {
            display_name: "Flash".to_owned(),
            ..configured("gemini-2.5-flash", "flash")
        }]
    );

    let config = Config::parse(concat!(
        "interactions-api-key:\n  - api-key: i\n    excluded-models: [z]\n",
        "    models:\n      - name: gemini-2.5-pro\n        alias: pro\n",
        "xai-api-key:\n  - api-key: x\n    base-url: https://api.x.ai/v1\n",
        "    models:\n      - name: grok-4.5\n",
        "        alias: grok\n        max-context-length: 9\n",
        "meta-api-key:\n  - api-key: m\n    excluded-models: [muse-spark-1.1]\n",
    ))
    .unwrap();
    let rules = RegistrationRules::from(&config);
    assert_eq!(rules.interactions_keys[0].api_key, "i");
    assert_eq!(rules.interactions_keys[0].excluded_models, ["z"]);
    assert_eq!(
        rules.interactions_keys[0].models,
        [configured("gemini-2.5-pro", "pro")]
    );
    assert_eq!(
        rules.xai_keys[0].models,
        [ConfiguredModel {
            max_context_length: 9,
            ..configured("grok-4.5", "grok")
        }]
    );
    let meta = &rules.meta_keys[0];
    assert_eq!(meta.base_url, "https://api.meta.ai/v1");
    assert_eq!(meta.excluded_models, ["muse-spark-1.1"]);
    assert!(rules.gemini_keys.is_empty() && rules.codex_keys.is_empty());
}

// OpenAI-compatible providers: openai_compat_config_models_test.go,
// TestRegisterModelsForAuth_OpenAICompatibilityImageModelType and
// _OpenAICompatibilityInputModalities in service_excluded_models_test.go,
// TestOpenAICompatibilityRegistrationCacheUsesConfigIndex, and the
// OpenAI-compatible case of TestBuildConfigModelsPropagateMaxContextLength.

fn compat_model(name: &str, alias: &str) -> CompatModel {
    CompatModel {
        name: name.to_owned(),
        alias: alias.to_owned(),
        ..CompatModel::default()
    }
}

fn compat_entry(name: &str, models: Vec<CompatModel>) -> OpenAiCompatEntry {
    OpenAiCompatEntry {
        name: name.to_owned(),
        models,
        ..OpenAiCompatEntry::default()
    }
}

fn compat_rules(entries: Vec<OpenAiCompatEntry>) -> RegistrationRules {
    RegistrationRules {
        openai_compatibility: entries,
        ..RegistrationRules::default()
    }
}

/// The provider and model IDs `auth` registers under `rules`, or `None`
/// when it registers nothing.
fn registration(auth: &Auth, rules: &RegistrationRules) -> Option<(String, Vec<String>)> {
    match auth_models(auth, rules) {
        AuthModels::Register { provider, models } => {
            Some((provider, models.into_iter().map(|model| model.id).collect()))
        }
        _ => None,
    }
}

fn model_named<'a>(models: &'a [ModelInfo], id: &str) -> &'a ModelInfo {
    models
        .iter()
        .find(|model| model.id == id)
        .unwrap_or_else(|| panic!("{id} is missing"))
}

#[test]
fn build_openai_compatibility_config_models_input_modalities() {
    let models = build_openai_compat_models(&compat_entry(
        "mimo",
        vec![
            CompatModel {
                display_name: "Mimo Vision".to_owned(),
                input_modalities: vec!["TEXT".into(), "image".into(), "image".into()],
                ..compat_model("upstream-vision", "mimo-v2.5-pro")
            },
            CompatModel {
                image: true,
                ..compat_model("upstream-image", "compat-image")
            },
        ],
    ));
    assert_eq!(models.len(), 2);
    let vision = model_named(&models, "mimo-v2.5-pro");
    assert_eq!(vision.display_name, "Mimo Vision");
    assert_eq!(vision.supported_input_modalities, ["text", "image"]);
    let image = model_named(&models, "compat-image");
    assert_eq!(image.display_name, "compat-image");
    assert_eq!(image.model_type, OPENAI_IMAGE_MODEL_TYPE);
    assert!(image.supported_input_modalities.is_empty());
}

#[test]
fn build_openai_compatibility_config_models_details() {
    let models = build_openai_compat_models(&compat_entry(
        "Kimi",
        vec![
            CompatModel {
                max_context_length: 1_048_576,
                is_compat: true,
                output_modalities: vec![" Text ".into(), "".into()],
                ..compat_model(" kimi-k2 ", "")
            },
            compat_model("", ""),
            CompatModel {
                thinking: Some(ThinkingSupport {
                    levels: vec![" None ".into(), "HIGH".into(), "high".into()],
                    ..ThinkingSupport::default()
                }),
                ..compat_model("kimi-k2", "k2")
            },
            compat_model("kimi-k2", "K2"),
        ],
    ));
    assert_eq!(
        ids(&models),
        ["kimi-k2", "k2", "K2"],
        "nameless models are skipped, and an alias may repeat"
    );
    let first = &models[0];
    assert_eq!(
        (first.context_length, first.max_context_length),
        (1_048_576, 1_048_576)
    );
    assert_eq!(first.display_name, "kimi-k2", "no alias: the name");
    assert_eq!(first.metadata_model_id, "kimi-k2");
    assert_eq!(first.owned_by, "Kimi");
    assert_eq!(first.model_type, "openai-compatibility");
    assert_eq!(first.object, "model");
    assert!(first.is_compat && !first.user_defined && !first.explicit_thinking);
    assert_eq!(first.supported_output_modalities, ["text"]);
    assert!(first.supported_input_modalities.is_empty());
    assert_eq!(
        first.thinking,
        Some(ThinkingSupport {
            levels: vec!["low".into(), "medium".into(), "high".into()],
            ..ThinkingSupport::default()
        })
    );
    let explicit = &models[1];
    assert_eq!(explicit.display_name, "k2");
    assert!(explicit.explicit_thinking);
    assert_eq!(
        explicit.thinking,
        Some(ThinkingSupport {
            levels: vec!["none".into(), "high".into()],
            zero_allowed: true,
            ..ThinkingSupport::default()
        })
    );
}

#[test]
fn register_models_for_auth_openai_compatibility_image_model_type() {
    let rules = compat_rules(vec![compat_entry(
        "images",
        vec![
            CompatModel {
                image: true,
                ..compat_model("upstream-image", "compat-image")
            },
            compat_model("upstream-chat", "compat-chat"),
        ],
    )]);
    let auth = auth(
        "auth-openai-compat-image",
        "openai-compatibility",
        &[
            ("auth_kind", "api_key"),
            ("compat_name", "images"),
            ("provider_key", "images"),
        ],
    );
    let models = registered(&auth, &rules);
    let image = model_named(&models, "compat-image");
    assert_eq!(image.model_type, OPENAI_IMAGE_MODEL_TYPE);
    assert_eq!(image.thinking, None);
    let chat = model_named(&models, "compat-chat");
    assert_eq!(chat.model_type, "openai-compatibility");
    assert!(chat.thinking.is_some());
}

#[test]
fn register_models_for_auth_openai_compatibility_input_modalities() {
    let rules = compat_rules(vec![compat_entry(
        "mimo",
        vec![
            CompatModel {
                input_modalities: vec!["text".into(), "image".into()],
                output_modalities: vec!["text".into()],
                ..compat_model("mimo-v2.5-pro", "mimo-v2.5-pro")
            },
            CompatModel {
                image: true,
                ..compat_model("upstream-image", "compat-image")
            },
        ],
    )]);
    let auth = auth(
        "auth-openai-compat-modalities",
        "openai-compatibility",
        &[
            ("auth_kind", "api_key"),
            ("compat_name", "mimo"),
            ("provider_key", "mimo"),
        ],
    );
    let models = registered(&auth, &rules);
    let vision = model_named(&models, "mimo-v2.5-pro");
    assert_eq!(vision.model_type, "openai-compatibility");
    assert_eq!(vision.supported_input_modalities, ["text", "image"]);
    assert_eq!(vision.supported_output_modalities, ["text"]);
    let image = model_named(&models, "compat-image");
    assert_eq!(image.model_type, OPENAI_IMAGE_MODEL_TYPE);
    assert!(image.supported_input_modalities.is_empty());
}

#[test]
fn openai_compatibility_registration_uses_config_index() {
    let rules = compat_rules(vec![
        compat_entry("shared", vec![compat_model("first", "")]),
        compat_entry("shared", vec![compat_model("second", "")]),
    ]);
    let config_auth = |index: &str| {
        auth(
            "shared-auth",
            "openai-compatible-shared",
            &[
                ("source", "config:shared[token-1]"),
                ("config_index", index),
                ("compat_name", "shared"),
                ("provider_key", "openai-compatible-shared"),
            ],
        )
    };
    assert_eq!(
        registration(&config_auth("1"), &rules),
        Some((
            "openai-compatible-shared".to_owned(),
            vec!["second".to_owned()]
        ))
    );
    assert_eq!(ids(&registered(&config_auth("0"), &rules)), ["first"]);
    assert_eq!(
        ids(&registered(&config_auth("7"), &rules)),
        ["first"],
        "an index out of range falls back to the name"
    );

    let mut disabled = rules.clone();
    disabled.openai_compatibility[1].disabled = true;
    assert_eq!(
        ids(&registered(&config_auth("1"), &disabled)),
        ["first"],
        "a disabled entry falls back to the first enabled one with the name"
    );

    let mut file_auth = config_auth("1");
    file_auth.attributes.remove("source");
    assert_eq!(
        ids(&registered(&file_auth, &rules)),
        ["first"],
        "only a config credential is found by index"
    );
}

#[test]
fn openai_compatibility_registers_under_the_provider_key_with_prefix() {
    let rules = RegistrationRules {
        force_model_prefix: true,
        ..compat_rules(vec![compat_entry(
            "Kimi",
            vec![compat_model("kimi-k2", "k2")],
        )])
    };
    let mut auth = auth(
        "kimi-auth",
        "openai-compatible-kimi",
        &[
            ("source", "config:kimi[abc]"),
            ("config_index", "0"),
            ("compat_name", "Kimi"),
            ("provider_key", "openai-compatible-kimi"),
            ("api_key", "sk-compat"),
            ("excluded_models", "k2"),
        ],
    );
    auth.prefix = "team".to_owned();
    let AuthModels::Register { provider, models } = auth_models(&auth, &rules) else {
        panic!("expected a registration");
    };
    assert_eq!(provider, "openai-compatible-kimi");
    assert_eq!(
        ids(&models),
        ["team/k2"],
        "exclusions don't apply; a forced prefix replaces the plain name"
    );
    assert_eq!(models[0].metadata_model_id, "kimi-k2");

    let registry = ModelRegistry::new();
    registry.register_auth(&auth, &rules);
    assert_eq!(
        registry.providers_for_model("team/k2"),
        ["openai-compatible-kimi"]
    );
}

#[test]
fn openai_compatibility_without_an_entry_is_unregistered() {
    let auth = auth(
        "gone",
        "openai-compatible-gone",
        &[
            ("source", "config:gone[abc]"),
            ("config_index", "0"),
            ("compat_name", "gone"),
            ("provider_key", "openai-compatible-gone"),
        ],
    );
    assert_eq!(
        auth_models(&auth, &RegistrationRules::default()),
        AuthModels::Unregister
    );
    let disabled = compat_rules(vec![OpenAiCompatEntry {
        disabled: true,
        ..compat_entry("gone", vec![compat_model("m", "")])
    }]);
    assert_eq!(auth_models(&auth, &disabled), AuthModels::Unregister);
    let empty = compat_rules(vec![compat_entry("gone", Vec::new())]);
    assert_eq!(auth_models(&auth, &empty), AuthModels::Unregister);

    let registry = ModelRegistry::new();
    let rules = compat_rules(vec![compat_entry("gone", vec![compat_model("m", "")])]);
    registry.register_auth(&auth, &rules);
    assert_eq!(ids(&registry.models_for_client("gone")), ["m"]);
    registry.register_auth(&auth, &RegistrationRules::default());
    assert!(registry.models_for_client("gone").is_empty());
}

#[test]
fn other_providers_take_a_compatible_entry_by_name() {
    let rules = compat_rules(vec![
        compat_entry("custom", vec![compat_model("m", "")]),
        compat_entry("gemini", vec![compat_model("g", "")]),
        compat_entry("aistudio", vec![compat_model("s", "")]),
    ]);
    assert_eq!(
        registration(&auth("a", " Custom ", &[]), &rules),
        Some(("custom".to_owned(), vec!["m".to_owned()])),
        "the provider is the key, as upstream's default case gives"
    );
    assert_eq!(
        auth_models(&auth("b", "unknown", &[]), &rules),
        AuthModels::Unregister
    );
    assert_eq!(
        auth_models(&auth("c", "aistudio", &[]), &rules),
        AuthModels::Unregister,
        "a provider upstream lists models of its own for isn't compatible"
    );
    let gemini = registration(&auth("c", "gemini", &[]), &rules).expect("gemini models");
    assert_eq!(gemini.0, "gemini");
    assert!(!gemini.1.contains(&"g".to_owned()));
    assert_eq!(
        registration(
            &auth("d", "openai-compatibility", &[("compat_name", "custom")]),
            &rules
        ),
        Some(("openai-compatible-custom".to_owned(), vec!["m".to_owned()]))
    );
    let labelled = Auth {
        label: "Custom".to_owned(),
        ..auth("e", "openai-compatibility", &[])
    };
    assert_eq!(
        registration(&labelled, &rules),
        Some(("openai-compatible-custom".to_owned(), vec!["m".to_owned()])),
        "a credential of the plain provider goes by its label"
    );
}

#[test]
fn rules_carry_openai_compatibility() {
    let config = Config::parse(concat!(
        "openai-compatibility:\n",
        "  - name: kimi\n",
        "    base-url: https://kimi.example.test/v1\n",
        "    disabled: true\n",
        "    models:\n",
        "      - name: kimi-k2\n",
        "        alias: k2\n",
        "        image: true\n",
        "        max-context-length: 2048\n",
        "        input-modalities: [text]\n",
        "        thinking: {levels: [low]}\n",
    ))
    .unwrap();
    let rules = RegistrationRules::from(&config);
    assert_eq!(
        rules.openai_compatibility,
        [OpenAiCompatEntry {
            name: "kimi".to_owned(),
            disabled: true,
            models: vec![CompatModel {
                max_context_length: 2048,
                image: true,
                input_modalities: vec!["text".to_owned()],
                thinking: Some(ThinkingSupport {
                    levels: vec!["low".to_owned()],
                    ..ThinkingSupport::default()
                }),
                ..compat_model("kimi-k2", "k2")
            }],
        }]
    );
}
