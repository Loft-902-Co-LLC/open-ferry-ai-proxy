// Ported from CLIProxyAPI internal/watcher/synthesizer/config.go
// (synthesizeVertexCompat), ComputeVertexCompatModelsHash in
// internal/modelconfig/model_hash.go and the vertex-api-key part of
// ValidateCredentialWeights in internal/config/weight.go (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Records for the config's `vertex-api-key` list: API keys for Vertex AI's
//! express mode, or for a service that takes Vertex AI's paths.
//!
//! Each entry makes one record, keyless or not (loading the config drops
//! entries without a key):
//!
//! - The provider is `vertex` and the label `vertex-apikey`.
//! - The ID is `vertex:apikey:<hash>`, from a hash of the key, base URL and
//!   proxy URL.
//! - Attributes: `source` (`config:vertex-apikey[<hash>]`), `base_url`
//!   (even when empty), `provider_key` (`vertex`), `config_index`,
//!   `priority`, `weight`, `api_key`, `models_hash`, `header:<name>`,
//!   `excluded_models`, `excluded_models_hash`, `auth_kind` (`apikey`),
//!   and optional `openapi` and `openapi_location`.
//! - Metadata: `disable_cooling` and `request_retry`.
//!
//! Service-account credentials come from files of type `vertex` instead,
//! read by [`super::file`].
//!
//! Deviations from upstream: none.

use std::collections::BTreeMap;

use open_ferry_translate::go::to_lower;

use super::super::classification::{
    ATTRIBUTE_API_KEY, ATTRIBUTE_CONFIG_INDEX, ATTRIBUTE_SOURCE, ATTRIBUTE_WEIGHT,
    AUTH_KIND_API_KEY,
};
use super::super::compat::ATTRIBUTE_PROVIDER_KEY;
use super::super::weight::normalize_weight;
use super::super::{Auth, Status};
use super::api_key::{retry_metadata, thinking_json};
use super::{
    StableIdGenerator, SynthesisContext, SynthesisError, add_config_headers_to_attrs,
    apply_auth_excluded_models_meta, sha256_hex,
};
use crate::config::{VertexCompatKey, VertexCompatModel};

/// The provider of Vertex AI credentials.
pub const VERTEX: &str = "vertex";

/// Records for every key in `keys`, after checking every weight. An
/// invalid weight fails the whole list, naming the entry.
pub fn synthesize_vertex_auths(
    keys: &[VertexCompatKey],
    ctx: &SynthesisContext,
    ids: &mut StableIdGenerator,
) -> Result<Vec<Auth>, SynthesisError> {
    validate_vertex_weights(keys)?;
    Ok(keys
        .iter()
        .enumerate()
        .map(|(index, key)| vertex_auth(index, key, ctx, ids))
        .collect())
}

/// Checks the weights of the `vertex-api-key` list.
pub fn validate_vertex_weights(keys: &[VertexCompatKey]) -> Result<(), SynthesisError> {
    for (index, key) in keys.iter().enumerate() {
        if let Some(weight) = key.weight
            && let Err(err) = normalize_weight(weight)
        {
            return Err(SynthesisError::new(format!(
                "synthesize config API key auths: vertex-api-key[{index}].weight: {err}"
            )));
        }
    }
    Ok(())
}

/// The record for the key at `index` in the `vertex-api-key` list. The
/// weight isn't checked; see [`validate_vertex_weights`].
pub fn vertex_auth(
    index: usize,
    entry: &VertexCompatKey,
    ctx: &SynthesisContext,
    ids: &mut StableIdGenerator,
) -> Auth {
    let base_url = entry.base_url.trim();
    let key = entry.api_key.trim();
    let prefix = entry.prefix.trim();
    let proxy_url = entry.proxy_url.trim();
    let (id, token) = ids.next("vertex:apikey", &[key, base_url, proxy_url]);

    let mut attrs = BTreeMap::new();
    attrs.insert(
        ATTRIBUTE_SOURCE.to_owned(),
        format!("config:vertex-apikey[{token}]"),
    );
    attrs.insert("base_url".to_owned(), base_url.to_owned());
    attrs.insert(ATTRIBUTE_PROVIDER_KEY.to_owned(), VERTEX.to_owned());
    attrs.insert(ATTRIBUTE_CONFIG_INDEX.to_owned(), index.to_string());
    if entry.priority != 0 {
        attrs.insert("priority".to_owned(), entry.priority.to_string());
    }
    if let Some(weight) = entry.weight {
        attrs.insert(ATTRIBUTE_WEIGHT.to_owned(), weight.max(0).to_string());
    }
    if !key.is_empty() {
        attrs.insert(ATTRIBUTE_API_KEY.to_owned(), key.to_owned());
    }
    let models_hash = compute_vertex_models_hash(&entry.models);
    if !models_hash.is_empty() {
        attrs.insert("models_hash".to_owned(), models_hash);
    }
    if let Some(openapi) = entry.openapi {
        attrs.insert("openapi".to_owned(), openapi.to_string());
    }
    if !entry.openapi_location.trim().is_empty() {
        attrs.insert(
            "openapi_location".to_owned(),
            entry.openapi_location.trim().to_owned(),
        );
    }
    if entry.interactions == Some(true) {
        attrs.insert("interactions".to_owned(), "true".to_owned());
    }
    add_config_headers_to_attrs(&entry.headers, &mut attrs);
    let metadata = retry_metadata(entry.disable_cooling, entry.request_retry, &[]);

    let mut auth = Auth {
        id,
        provider: VERTEX.to_owned(),
        label: "vertex-apikey".to_owned(),
        prefix: prefix.to_owned(),
        status: Status::Active,
        proxy_url: proxy_url.to_owned(),
        attributes: attrs,
        metadata,
        created_at: Some(ctx.now),
        updated_at: Some(ctx.now),
        ..Auth::default()
    };
    apply_auth_excluded_models_meta(&mut auth, &entry.excluded_models, AUTH_KIND_API_KEY);
    auth
}

/// A hash of a key's model list, to notice when it changes: as
/// [`super::api_key::compute_models_hash`], without `is-compat`.
pub fn compute_vertex_models_hash(models: &[VertexCompatModel]) -> String {
    let lines: Vec<String> = models
        .iter()
        .filter_map(|model| {
            let name = model.name.trim();
            let alias = model.alias.trim();
            if name.is_empty() && alias.is_empty() {
                return None;
            }
            Some(format!(
                "{}|{}|{}|force-mapping={}{}|thinking={}",
                to_lower(name),
                to_lower(alias),
                model.display_name.trim(),
                model.force_mapping,
                model
                    .openapi
                    .map(|v| format!("|openapi={v}"))
                    .unwrap_or_default(),
                thinking_json(model.thinking.as_ref()),
            ))
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    sha256_hex(lines.join("\n").as_bytes())
}

/// Projects Vertex credential-file routing settings into runtime attributes,
/// retaining name-only rules and explicit false independently of alias lists.
pub(super) fn apply_openapi_settings(
    auth: &mut Auth,
    metadata: &serde_json::Map<String, serde_json::Value>,
) {
    use open_ferry_translate::go::equal_fold;
    use serde_json::{Value, json};
    use std::collections::HashSet;
    if auth.provider != VERTEX {
        return;
    }
    let enabled = metadata.get("openapi").and_then(|value| {
        value.as_bool().or_else(|| {
            value
                .as_str()
                .and_then(|s| super::super::go::parse_bool(&to_lower(s.trim())))
        })
    });
    if let Some(enabled) = enabled {
        auth.attributes
            .insert("openapi".into(), enabled.to_string());
    }
    for key in ["openapi_location", "openapi-location"] {
        if let Some(location) = metadata
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            auth.attributes
                .insert("openapi_location".into(), location.into());
            break;
        }
    }
    let mut seen_alias = HashSet::new();
    let mut seen_name = HashSet::new();
    let mut rules = Vec::new();
    for key in ["model_aliases", "model-aliases", "models"] {
        let Some(items) = metadata.get(key).and_then(Value::as_array) else {
            continue;
        };
        for rule in super::file::decode_model_aliases(items) {
            let name = rule.name.trim();
            let alias = rule.alias.trim();
            if name.is_empty() {
                continue;
            }
            if !alias.is_empty() && !equal_fold(name, alias) {
                if !seen_alias.insert(to_lower(alias)) {
                    continue;
                }
            } else if rule.openapi.is_none() || !seen_name.insert(to_lower(name)) {
                continue;
            }
            if let Some(enabled) = rule.openapi {
                rules.push(json!({"name": name, "alias": alias, "openapi": enabled}));
            }
        }
    }
    if !rules.is_empty() {
        auth.attributes.insert(
            "vertex_model_openapi".into(),
            Value::Array(rules).to_string(),
        );
    }
}

// Ported from internal/watcher/synthesizer/config_test.go: the Vertex,
// weight, retry and all-providers tests. The Gemini, interactions, xAI and
// Meta ones are in `api_key`'s tests.
#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::Value;

    use super::super::synthesize_config_auths;
    use super::*;
    use crate::config::{
        ClaudeKey, CodexKey, Config, GeminiKey, OpenAiCompatibility, OpenAiCompatibilityApiKey,
        ThinkingSupport,
    };

    fn ctx() -> SynthesisContext {
        SynthesisContext::new("", chrono::Utc.timestamp_opt(100, 0).unwrap())
    }

    #[test]
    fn credential_settings_project_to_attributes() {
        use serde_json::json;
        for value in [
            json!(true),
            json!(false),
            json!("true"),
            json!("false"),
            json!("1"),
            json!("0"),
            json!("T"),
            json!("f"),
        ] {
            for key in ["openapi_location", "openapi-location"] {
                let enabled = matches!(value, Value::Bool(true))
                    || ["true", "1", "T"].contains(&value.as_str().unwrap_or_default());
                let raw = json!({"type":"vertex", "openapi":value, key:" global "});
                let auth = super::super::file::synthesize_auth_file(
                    &ctx(),
                    std::path::Path::new("vertex.json"),
                    &serde_json::to_vec(&raw).unwrap(),
                )
                .unwrap()
                .unwrap();
                assert_eq!(
                    auth.attribute("openapi"),
                    Some(enabled.to_string().as_str())
                );
                assert_eq!(auth.attribute("openapi_location"), Some("global"));
            }
        }
        for enabled in [None, Some(false), Some(true)] {
            let auths = synth(&vertex(vec![VertexCompatKey {
                openapi: enabled,
                openapi_location: " global ".into(),
                ..key("dummy", "")
            }]))
            .unwrap();
            assert_eq!(
                auths[0].attribute("openapi"),
                enabled.map(|v| v.to_string()).as_deref()
            );
            assert_eq!(auths[0].attribute("openapi_location"), Some("global"));
        }
    }

    #[test]
    fn mixed_model_lists_project_alias_and_name_only_rules() {
        use serde_json::json;
        for source in ["model_aliases", "model-aliases", "models"] {
            for enabled in [false, true] {
                let raw = json!({"type":"vertex", source:["native-model", 3, {"name":"bad", "openapi":"false"}, {"name":" partner/model ", "alias":" public ", "openapi":enabled}, {"name":"custom", "openapi":!enabled}]});
                let auth = super::super::file::synthesize_auth_file(
                    &ctx(),
                    std::path::Path::new("vertex.json"),
                    &serde_json::to_vec(&raw).unwrap(),
                )
                .unwrap()
                .unwrap();
                let rules: Value =
                    serde_json::from_str(auth.attribute("vertex_model_openapi").unwrap()).unwrap();
                assert_eq!(rules.as_array().unwrap().len(), 2);
                assert_eq!(rules[0]["name"], "partner/model");
                assert_eq!(rules[0]["alias"], "public");
                assert_eq!(rules[0]["openapi"], enabled);
                assert_eq!(rules[1]["openapi"], !enabled);
                let aliases: Value =
                    serde_json::from_str(auth.attribute("model_aliases").unwrap()).unwrap();
                assert_eq!(aliases[0]["openapi"], enabled);
                assert_eq!(aliases[0]["alias"], "public");
            }
        }
        // An unset per-account alias reserves its name ahead of later sources.
        let raw = json!({"type":"vertex", "model_aliases":[{"name":"model", "alias":"shared"}], "models":[{"name":"other", "alias":"shared", "openapi":false}]});
        let auth = super::super::file::synthesize_auth_file(
            &ctx(),
            std::path::Path::new("vertex.json"),
            &serde_json::to_vec(&raw).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(auth.attribute("vertex_model_openapi").is_none());
    }

    fn synth(config: &Config) -> Result<Vec<Auth>, SynthesisError> {
        synthesize_config_auths(config, &ctx(), &mut StableIdGenerator::new())
    }

    fn key(api_key: &str, base_url: &str) -> VertexCompatKey {
        VertexCompatKey {
            api_key: api_key.to_owned(),
            base_url: base_url.to_owned(),
            ..VertexCompatKey::default()
        }
    }

    fn vertex(keys: Vec<VertexCompatKey>) -> Config {
        Config {
            vertex_api_key: keys,
            ..Config::default()
        }
    }

    #[test]
    fn interactions_opt_in_becomes_a_credential_attribute() {
        for enabled in [None, Some(false), Some(true)] {
            let auths = synth(&vertex(vec![VertexCompatKey {
                interactions: enabled,
                ..key("vertex-key", "http://127.0.0.1")
            }]))
            .unwrap();
            assert_eq!(
                auths[0].attribute("interactions"),
                if enabled == Some(true) {
                    Some("true")
                } else {
                    None
                }
            );
        }
    }

    #[test]
    fn service_account_json_root_preserves_interactions_opt_in() {
        let auth = super::super::file::synthesize_auth_file(
            &ctx(),
            std::path::Path::new("vertex-test.json"),
            br#"{"type":"vertex","project_id":"proxy-test","location":"global","interactions":true,"service_account":{"type":"service_account"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(auth.metadata.get("interactions"), Some(&Value::Bool(true)));
        assert_eq!(auth.provider, "vertex");
    }

    #[test]
    fn vertex_compat() {
        let auths = synth(&vertex(vec![VertexCompatKey {
            prefix: "vertex-prod".to_owned(),
            ..key("vertex-key-123", "https://vertex.googleapis.com")
        }]))
        .unwrap();
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.provider, "vertex");
        assert_eq!(auth.label, "vertex-apikey");
        assert_eq!(auth.prefix, "vertex-prod");
        assert_eq!(auth.status, Status::Active);
        assert_eq!(auth.attribute("api_key"), Some("vertex-key-123"));
        assert_eq!(auth.attribute("provider_key"), Some("vertex"));
        assert_eq!(auth.attribute("config_index"), Some("0"));
        assert_eq!(
            auth.attribute("base_url"),
            Some("https://vertex.googleapis.com")
        );
        assert_eq!(auth.attribute("auth_kind"), Some("apikey"));
        assert!(auth.metadata.is_empty());
        let token = auth.id.strip_prefix("vertex:apikey:").expect("vertex ID");
        assert_eq!(
            auth.attribute("source"),
            Some(format!("config:vertex-apikey[{token}]").as_str())
        );
        // Not an OpenAI-compatible credential, whatever its provider key.
        assert_eq!(auth.openai_compat_info(), None);
    }

    #[test]
    fn vertex_compat_skips_empty_and_headers() {
        let mut with_header = key("valid-key", "https://vertex.api");
        with_header.headers = BTreeMap::from([("X-Vertex".to_owned(), "test".to_owned())]);
        // The synthesizer keeps keyless entries; loading the config drops
        // them.
        let auths = synth(&vertex(vec![
            key("", "https://vertex.api"),
            key("  ", "https://vertex.api"),
            with_header,
        ]))
        .unwrap();
        assert_eq!(auths.len(), 3);
        assert_eq!(auths[0].attribute("api_key"), None);
        assert_eq!(auths[1].attribute("api_key"), None);
        assert_eq!(auths[2].attribute("header:X-Vertex"), Some("test"));
        // Same key, base URL and proxy: distinct IDs.
        assert_eq!(auths[1].id, format!("{}-1", auths[0].id));
    }

    #[test]
    fn empty_base_url_is_still_an_attribute() {
        let auths = synth(&vertex(vec![key("k", " ")])).unwrap();
        assert_eq!(auths[0].attribute("base_url"), Some(""));
    }

    #[test]
    fn vertex_compat_with_models() {
        let mut entry = key("vertex-key", "https://vertex.api");
        entry.models = vec![
            VertexCompatModel {
                name: "gemini-pro".to_owned(),
                alias: "pro".to_owned(),
                ..VertexCompatModel::default()
            },
            VertexCompatModel {
                name: "gemini-ultra".to_owned(),
                alias: "ultra".to_owned(),
                ..VertexCompatModel::default()
            },
        ];
        let auths = synth(&vertex(vec![entry])).unwrap();
        assert!(auths[0].attribute("models_hash").is_some());
    }

    #[test]
    fn models_hash_has_no_is_compat() {
        let models = [
            VertexCompatModel {
                name: " Gemini-2.5-Pro ".to_owned(),
                alias: "Pro".to_owned(),
                display_name: " Pro ".to_owned(),
                force_mapping: true,
                openapi: None,
                thinking: Some(ThinkingSupport {
                    levels: vec!["low".to_owned()],
                    ..ThinkingSupport::default()
                }),
            },
            VertexCompatModel::default(),
            VertexCompatModel {
                alias: "only-alias".to_owned(),
                ..VertexCompatModel::default()
            },
        ];
        let want = concat!(
            "gemini-2.5-pro|pro|Pro|force-mapping=true|thinking=",
            r#"{"levels":["low"]}"#,
            "\n|only-alias||force-mapping=false|thinking=null",
        );
        assert_eq!(
            compute_vertex_models_hash(&models),
            sha256_hex(want.as_bytes())
        );
        assert_eq!(compute_vertex_models_hash(&[]), "");
    }

    #[test]
    fn rejects_invalid_weights_for_all_api_key_types() {
        let invalid = Some(1_000_001);
        let cases = [
            (
                Config {
                    gemini_api_key: vec![GeminiKey {
                        api_key: "key".to_owned(),
                        weight: invalid,
                        ..GeminiKey::default()
                    }],
                    ..Config::default()
                },
                "gemini-api-key[0].weight",
            ),
            (
                Config {
                    interactions_api_key: vec![GeminiKey {
                        api_key: "key".to_owned(),
                        weight: invalid,
                        ..GeminiKey::default()
                    }],
                    ..Config::default()
                },
                "interactions-api-key[0].weight",
            ),
            (
                Config {
                    claude_api_key: vec![ClaudeKey {
                        api_key: "key".to_owned(),
                        weight: invalid,
                        ..ClaudeKey::default()
                    }],
                    ..Config::default()
                },
                "claude-api-key[0].weight",
            ),
            (
                Config {
                    codex_api_key: vec![CodexKey {
                        api_key: "key".to_owned(),
                        weight: invalid,
                        ..CodexKey::default()
                    }],
                    ..Config::default()
                },
                "codex-api-key[0].weight",
            ),
            (
                Config {
                    xai_api_key: vec![CodexKey {
                        api_key: "key".to_owned(),
                        weight: invalid,
                        ..CodexKey::default()
                    }],
                    ..Config::default()
                },
                "xai-api-key[0].weight",
            ),
            (
                Config {
                    openai_compatibility: vec![OpenAiCompatibility {
                        api_key_entries: vec![OpenAiCompatibilityApiKey {
                            api_key: "key".to_owned(),
                            weight: invalid,
                            ..OpenAiCompatibilityApiKey::default()
                        }],
                        ..OpenAiCompatibility::default()
                    }],
                    ..Config::default()
                },
                "openai-compatibility[0].api-key-entries[0].weight",
            ),
            (
                vertex(vec![VertexCompatKey {
                    weight: invalid,
                    ..key("key", "")
                }]),
                "vertex-api-key[0].weight",
            ),
        ];
        for (config, path) in cases {
            let err = synth(&config).unwrap_err().to_string();
            assert!(
                err.starts_with(&format!("synthesize config API key auths: {path}")),
                "{err}"
            );
        }
        // Upstream checks vertex keys before codex keys.
        let mut config = vertex(vec![VertexCompatKey {
            weight: invalid,
            ..key("key", "")
        }]);
        config.codex_api_key = vec![CodexKey {
            api_key: "key".to_owned(),
            weight: invalid,
            ..CodexKey::default()
        }];
        let err = synth(&config).unwrap_err().to_string();
        assert!(err.contains("vertex-api-key[0]"), "{err}");
    }

    #[test]
    fn propagates_weights_for_all_api_key_types() {
        let config = Config {
            gemini_api_key: vec![GeminiKey {
                api_key: "gemini".to_owned(),
                weight: Some(1),
                ..GeminiKey::default()
            }],
            interactions_api_key: vec![GeminiKey {
                api_key: "interactions".to_owned(),
                weight: Some(2),
                ..GeminiKey::default()
            }],
            claude_api_key: vec![ClaudeKey {
                api_key: "claude".to_owned(),
                weight: Some(3),
                ..ClaudeKey::default()
            }],
            codex_api_key: vec![CodexKey {
                api_key: "codex".to_owned(),
                weight: Some(4),
                ..CodexKey::default()
            }],
            xai_api_key: vec![CodexKey {
                api_key: "xai".to_owned(),
                weight: Some(5),
                ..CodexKey::default()
            }],
            openai_compatibility: vec![OpenAiCompatibility {
                name: "compat".to_owned(),
                base_url: "https://compat.example.com".to_owned(),
                api_key_entries: vec![OpenAiCompatibilityApiKey {
                    api_key: "compat".to_owned(),
                    weight: Some(6),
                    ..OpenAiCompatibilityApiKey::default()
                }],
                ..OpenAiCompatibility::default()
            }],
            vertex_api_key: vec![VertexCompatKey {
                weight: Some(7),
                ..key("vertex", "")
            }],
            ..Config::default()
        };
        let auths = synth(&config).unwrap();
        let weights: Vec<Option<&str>> = auths.iter().map(|a| a.attribute("weight")).collect();
        let want: Vec<String> = (1..=7).map(|weight| weight.to_string()).collect();
        assert_eq!(
            weights,
            want.iter()
                .map(|weight| Some(weight.as_str()))
                .collect::<Vec<_>>()
        );
        let negative = vertex(vec![VertexCompatKey {
            weight: Some(-5),
            ..key("vertex", "")
        }]);
        assert_eq!(synth(&negative).unwrap()[0].attribute("weight"), Some("0"));
        assert_eq!(
            synth(&vertex(vec![key("vertex", "")])).unwrap()[0].attribute("weight"),
            None
        );
    }

    #[test]
    fn all_providers() {
        let config = Config {
            gemini_api_key: vec![GeminiKey {
                api_key: "gemini-key".to_owned(),
                ..GeminiKey::default()
            }],
            claude_api_key: vec![ClaudeKey {
                api_key: "claude-key".to_owned(),
                ..ClaudeKey::default()
            }],
            codex_api_key: vec![CodexKey {
                api_key: "codex-key".to_owned(),
                ..CodexKey::default()
            }],
            xai_api_key: vec![CodexKey {
                api_key: "xai-key".to_owned(),
                ..CodexKey::default()
            }],
            meta_api_key: vec![CodexKey {
                api_key: "meta-key".to_owned(),
                base_url: "https://api.meta.ai/v1".to_owned(),
                ..CodexKey::default()
            }],
            openai_compatibility: vec![OpenAiCompatibility {
                name: "compat".to_owned(),
                base_url: "https://compat.api".to_owned(),
                ..OpenAiCompatibility::default()
            }],
            vertex_api_key: vec![key("vertex-key", "https://vertex.api")],
            ..Config::default()
        };
        let providers: Vec<String> = synth(&config)
            .unwrap()
            .into_iter()
            .map(|auth| auth.provider)
            .collect();
        assert_eq!(
            providers,
            [
                "gemini",
                "claude",
                "codex",
                "xai",
                "meta",
                "openai-compatible-compat",
                "vertex"
            ]
        );
    }

    #[test]
    fn request_retry() {
        let gemini = |name: &str, retry| GeminiKey {
            api_key: name.to_owned(),
            request_retry: retry,
            ..GeminiKey::default()
        };
        let config = Config {
            gemini_api_key: vec![
                gemini("gemini-zero", Some(0)),
                gemini("gemini-positive", Some(2)),
                gemini("gemini-negative", Some(-1)),
                gemini("gemini-unset", None),
            ],
            interactions_api_key: vec![gemini("interactions-zero", Some(0))],
            claude_api_key: vec![ClaudeKey {
                api_key: "claude-positive".to_owned(),
                request_retry: Some(2),
                ..ClaudeKey::default()
            }],
            codex_api_key: vec![CodexKey {
                api_key: "codex-zero".to_owned(),
                request_retry: Some(0),
                ..CodexKey::default()
            }],
            xai_api_key: vec![CodexKey {
                api_key: "xai-positive".to_owned(),
                request_retry: Some(2),
                ..CodexKey::default()
            }],
            vertex_api_key: vec![VertexCompatKey {
                request_retry: Some(2),
                disable_cooling: Some(false),
                ..key("vertex-positive", "https://vertex.api")
            }],
            ..Config::default()
        };
        let auths = synth(&config).unwrap();
        let got: Vec<(Option<&str>, Option<&Value>)> = auths
            .iter()
            .map(|auth| {
                (
                    auth.attribute("api_key"),
                    auth.metadata.get("request_retry"),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (Some("gemini-zero"), Some(&Value::from(0))),
                (Some("gemini-positive"), Some(&Value::from(2))),
                (Some("gemini-negative"), None),
                (Some("gemini-unset"), None),
                (Some("interactions-zero"), Some(&Value::from(0))),
                (Some("claude-positive"), Some(&Value::from(2))),
                (Some("codex-zero"), Some(&Value::from(0))),
                (Some("xai-positive"), Some(&Value::from(2))),
                (Some("vertex-positive"), Some(&Value::from(2))),
            ]
        );
        assert_eq!(auths[8].disable_cooling_override(), Some(false));
    }

    #[test]
    fn excluded_models_use_only_the_key_list() {
        let mut context = ctx();
        context.oauth_excluded_models =
            BTreeMap::from([("vertex".to_owned(), vec!["global".to_owned()])]);
        let mut entry = key("k", "");
        entry.excluded_models = vec![" Model-B ".to_owned(), "model-a".to_owned()];
        let auth = vertex_auth(0, &entry, &context, &mut StableIdGenerator::new());
        assert_eq!(auth.attribute("excluded_models"), Some("model-a,model-b"));
        assert_eq!(auth.attribute("auth_kind"), Some("apikey"));
    }

    #[test]
    fn id_ignores_prefix_and_headers() {
        let mut ids = StableIdGenerator::new();
        let plain = vertex_auth(0, &key("k", "b"), &ctx(), &mut StableIdGenerator::new());
        let mut other = key("k", "b");
        other.prefix = "p".to_owned();
        other.headers = BTreeMap::from([("X".to_owned(), "1".to_owned())]);
        let decorated = vertex_auth(0, &other, &ctx(), &mut ids);
        assert_eq!(plain.id, decorated.id);
        assert_eq!(decorated.prefix, "p");
    }
}
