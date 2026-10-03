// Ported from CLIProxyAPI sdk/cliproxy/service_codex_models_test.go,
// service_excluded_models_test.go, service_oauth_model_alias_test.go,
// service_oauth_settings_test.go, config_model_display_name_test.go,
// config_model_max_context_length_test.go,
// service_models_config_index_test.go, sdk/cliproxy/auth/classification_test.go
// and oauth_model_alias_test.go, internal/config/oauth_model_alias_test.go
// and oauth_settings_test.go, internal/modelconfig/model_info_test.go,
// internal/auth/codex/jwt_parser_test.go and
// internal/watcher/synthesizer/file_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of registration.
//!
//! Upstream's tests that use the global registry use a fresh `ModelRegistry`
//! here. Those for Gemini, Vertex, xAI, Meta, Devin, plugin providers and
//! OpenAI-compatible providers run against Claude or Codex where the rule
//! tested is provider-neutral (`MetaOAuthAliasAndExcludedModels`,
//! `UsesPreMergedExcludedModelsAttribute`), and as written where it is (the
//! alias and channel tests). `RegisterConfigAPIKeyAuthsCodexModelModes`
//! builds the credential the config loader would.
//!
//! Dropped:
//! - `OpenAICompatibilityImageModelType`, `OpenAICompatibilityInputModalities`,
//!   `OpenAICompatibilityRegistrationCacheUsesConfigIndex` and the Gemini,
//!   Vertex, xAI, interactions and OpenAI-compatible cases of the display
//!   name and context length tests: those providers aren't ported.
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
    for (name, model) in [("codex", &codex[0]), ("claude", &claude[0])] {
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

#[test]
fn auth_kind_classification() {
    let with_metadata = |metadata: Value| Auth {
        metadata: object(metadata),
        ..Auth::default()
    };
    let cases = [
        (
            "explicit api key attribute",
            auth("", "", &[("auth_kind", "api_key")]),
            AUTH_KIND_API_KEY,
        ),
        (
            "explicit oauth attribute wins over api key fallback",
            auth("", "", &[("auth_kind", "oauth"), ("api_key", "k")]),
            AUTH_KIND_OAUTH,
        ),
        (
            "explicit oauth metadata",
            with_metadata(json!({"auth_kind": "oauth"})),
            AUTH_KIND_OAUTH,
        ),
        (
            "legacy api key attribute",
            auth("", "", &[("api_key", "k")]),
            AUTH_KIND_API_KEY,
        ),
        (
            "legacy oauth metadata",
            with_metadata(json!({"access_token": "token"})),
            AUTH_KIND_OAUTH,
        ),
        (
            "unknown metadata shape",
            with_metadata(json!({"type": "test"})),
            "",
        ),
    ];
    for (name, auth, want) in cases {
        assert_eq!(auth_kind(&auth), want, "{name}");
    }
}

#[test]
fn auth_source_kind_classification() {
    let cases = [
        (
            "runtime only memory",
            auth(
                "",
                "",
                &[("runtime_only", "true"), ("source_backend", "postgres")],
            ),
            AUTH_SOURCE_MEMORY,
        ),
        (
            "backend postgres",
            auth(
                "",
                "",
                &[("source_backend", "postgresql"), ("path", "/tmp/auth.json")],
            ),
            AUTH_SOURCE_POSTGRES,
        ),
        (
            "backend object store",
            auth(
                "",
                "",
                &[
                    ("source_backend", "object-store"),
                    ("path", "/tmp/auth.json"),
                ],
            ),
            AUTH_SOURCE_OBJECT_STORE,
        ),
        (
            "config source",
            auth("", "", &[("source", "config:codex[abc]")]),
            AUTH_SOURCE_CONFIG,
        ),
        (
            "path source",
            auth("", "", &[("source", "/tmp/auth.json")]),
            AUTH_SOURCE_FILE,
        ),
        (
            "path attribute",
            auth("", "", &[("path", "/tmp/auth.json")]),
            AUTH_SOURCE_FILE,
        ),
        (
            "filename fallback",
            Auth {
                file_name: "codex.json".to_owned(),
                ..Auth::default()
            },
            AUTH_SOURCE_FILE,
        ),
    ];
    for (name, auth, want) in cases {
        assert_eq!(auth_source_kind(&auth), want, "{name}");
    }
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
        auth_models(&auth("gemini.json", "gemini", &[]), &rules),
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
    let entry = api_key_entry("sk-fictional-test-key", Vec::new());
    assert!(!format!("{entry:?}").contains("sk-fictional-test-key"));
}
