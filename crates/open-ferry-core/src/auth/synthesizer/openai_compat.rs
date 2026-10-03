// Ported from CLIProxyAPI internal/watcher/synthesizer/config.go
// (synthesizeOpenAICompat), ComputeOpenAICompatModelsHash and
// normalizeModalities in internal/modelconfig/model_hash.go, and the
// openai-compatibility part of ValidateCredentialWeights in
// internal/config/weight.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Records for the config's `openai-compatibility` providers.
//!
//! Each enabled provider makes one record per entry of its
//! `api-key-entries`, or a single keyless record when it has none:
//!
//! - The provider is the provider's key, `openai-compatible-<name>` (see
//!   [`openai_compatible_provider_key`]), and the label its configured name.
//! - The ID is `openai-compatibility:<name>:<hash>`, from a hash of the key,
//!   base URL and proxy URL.
//! - Attributes: `source` (`config:<name>[<hash>]`), `base_url`,
//!   `compat_name`, `provider_key`, `config_index`, `api_key`, `priority`,
//!   `weight`, `models_hash` and `header:<name>`.
//! - Metadata: `disable_cooling`, `request_retry` and
//!   `request_scoped_errors`, as for API keys.
//!
//! Deviations from upstream: none.

use std::collections::BTreeMap;

use open_ferry_translate::go::to_lower;

use super::super::classification::{
    ATTRIBUTE_API_KEY, ATTRIBUTE_CONFIG_INDEX, ATTRIBUTE_SOURCE, ATTRIBUTE_WEIGHT,
};
use super::super::compat::{
    ATTRIBUTE_COMPAT_NAME, ATTRIBUTE_PROVIDER_KEY, OPENAI_COMPATIBILITY,
    openai_compatible_provider_key,
};
use super::super::weight::normalize_weight;
use super::super::{Auth, Status};
use super::api_key::{retry_metadata, thinking_json};
use super::{
    StableIdGenerator, SynthesisContext, SynthesisError, add_config_headers_to_attrs, sha256_hex,
};
use crate::config::{OpenAiCompatibility, OpenAiCompatibilityModel};

/// Records for every enabled provider in `providers`, after checking every
/// weight. An invalid weight fails the whole list, naming the entry.
pub fn synthesize_openai_compat_auths(
    providers: &[OpenAiCompatibility],
    ctx: &SynthesisContext,
    ids: &mut StableIdGenerator,
) -> Result<Vec<Auth>, SynthesisError> {
    validate_openai_compat_weights(providers)?;
    let mut out = Vec::new();
    for (index, compat) in providers.iter().enumerate() {
        if !compat.disabled {
            out.extend(openai_compat_auths(index, compat, ctx, ids));
        }
    }
    Ok(out)
}

/// Checks the weights of every provider's API keys.
pub fn validate_openai_compat_weights(
    providers: &[OpenAiCompatibility],
) -> Result<(), SynthesisError> {
    for (index, compat) in providers.iter().enumerate() {
        for (key_index, entry) in compat.api_key_entries.iter().enumerate() {
            if let Some(weight) = entry.weight
                && let Err(err) = normalize_weight(weight)
            {
                return Err(SynthesisError::new(format!(
                    "synthesize config API key auths: \
                     openai-compatibility[{index}].api-key-entries[{key_index}].weight: {err}"
                )));
            }
        }
    }
    Ok(())
}

/// The records of the provider at `index` in the config's list, whether or
/// not it is disabled. The weights aren't checked; see
/// [`validate_openai_compat_weights`].
pub fn openai_compat_auths(
    index: usize,
    compat: &OpenAiCompatibility,
    ctx: &SynthesisContext,
    ids: &mut StableIdGenerator,
) -> Vec<Auth> {
    let prefix = compat.prefix.trim();
    let mut provider_name = to_lower(compat.name.trim());
    if provider_name.is_empty() {
        provider_name = OPENAI_COMPATIBILITY.to_owned();
    }
    let provider_key = openai_compatible_provider_key(&provider_name);
    let base_url = compat.base_url.trim();
    let id_kind = format!("openai-compatibility:{provider_name}");
    let models_hash = compute_openai_compat_models_hash(&compat.models);

    // The attributes and metadata every record of the provider shares.
    let record = |token: &str, proxy_url: &str, id: String| {
        let mut attrs = BTreeMap::new();
        attrs.insert(
            ATTRIBUTE_SOURCE.to_owned(),
            format!("config:{provider_name}[{token}]"),
        );
        attrs.insert("base_url".to_owned(), base_url.to_owned());
        attrs.insert(ATTRIBUTE_COMPAT_NAME.to_owned(), compat.name.clone());
        attrs.insert(ATTRIBUTE_PROVIDER_KEY.to_owned(), provider_key.clone());
        attrs.insert(ATTRIBUTE_CONFIG_INDEX.to_owned(), index.to_string());
        if compat.priority != 0 {
            attrs.insert("priority".to_owned(), compat.priority.to_string());
        }
        if !models_hash.is_empty() {
            attrs.insert("models_hash".to_owned(), models_hash.clone());
        }
        add_config_headers_to_attrs(&compat.headers, &mut attrs);
        Auth {
            id,
            provider: provider_key.clone(),
            label: compat.name.clone(),
            prefix: prefix.to_owned(),
            status: Status::Active,
            proxy_url: proxy_url.to_owned(),
            attributes: attrs,
            metadata: retry_metadata(
                compat.disable_cooling,
                compat.request_retry,
                &compat.request_scoped_errors,
            ),
            created_at: Some(ctx.now),
            updated_at: Some(ctx.now),
            ..Auth::default()
        }
    };

    let mut out = Vec::with_capacity(compat.api_key_entries.len().max(1));
    for entry in &compat.api_key_entries {
        let key = entry.api_key.trim();
        let proxy_url = entry.proxy_url.trim();
        let (id, token) = ids.next(&id_kind, &[key, base_url, proxy_url]);
        let mut auth = record(&token, proxy_url, id);
        if let Some(weight) = entry.weight {
            auth.attributes
                .insert(ATTRIBUTE_WEIGHT.to_owned(), weight.max(0).to_string());
        }
        if !key.is_empty() {
            auth.attributes
                .insert(ATTRIBUTE_API_KEY.to_owned(), key.to_owned());
        }
        out.push(auth);
    }
    if out.is_empty() {
        let (id, token) = ids.next(&id_kind, &[base_url]);
        out.push(record(&token, "", id));
    }
    out
}

/// A hash of a provider's model list, to notice when it changes: the
/// SHA-256, in hex, of one line per model with a name or alias, in order.
/// Empty for no models.
pub fn compute_openai_compat_models_hash(models: &[OpenAiCompatibilityModel]) -> String {
    let lines: Vec<String> = models
        .iter()
        .filter_map(|model| {
            let name = model.name.trim();
            let alias = model.alias.trim();
            if name.is_empty() && alias.is_empty() {
                return None;
            }
            Some(format!(
                "{}|{}|{}|image={}|force-mapping={}|is-compat={}|use-max-completion-tokens={}\
                 |input={}|output={}|thinking={}",
                to_lower(name),
                to_lower(alias),
                model.display_name.trim(),
                model.image,
                model.force_mapping,
                model.is_compat,
                model.use_max_completion_tokens,
                normalize_modalities(&model.input_modalities).join(","),
                normalize_modalities(&model.output_modalities).join(","),
                thinking_json(model.thinking.as_ref()),
            ))
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    sha256_hex(lines.join("\n").as_bytes())
}

/// Modalities trimmed and in lower case, without blanks or repeats, in
/// order (upstream's `normalizeModalities`).
pub fn normalize_modalities(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for value in raw {
        let value = to_lower(value.trim());
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // Ports the OpenAI-compatible tests of
    // internal/watcher/synthesizer/config_test.go
    // (TestConfigSynthesizer_OpenAICompat, _UsesNamespacedProviderKey,
    // _WithModelsHash and _FallbackWithModels, and the openai-compatibility
    // cases of _RejectsInvalidWeightsForAllAPIKeyTypes,
    // _PropagatesWeightsForAllAPIKeyTypes, _RequestRetry and
    // _RequestScopedErrors) and of internal/watcher/diff/model_hash_test.go
    // (TestComputeOpenAICompatModelsHash_Deterministic, _IncludesImageFlag,
    // _Empty, IncludesModalities, PreservesRoutingOrderAndDuplicates,
    // IncludesUseMaxCompletionTokens, and the OpenAI-compatible cases of
    // TestComputeModelHashesIncludeDisplayName, IncludeForceMapping and
    // IncludeThinking). The hash is also checked against a value Go computed.

    use super::*;
    use crate::config::{OpenAiCompatibilityApiKey, RequestScopedErrorRule, ThinkingSupport};
    use chrono::TimeZone;
    use serde_json::{Value, json};

    fn ctx() -> SynthesisContext {
        SynthesisContext::new("", chrono::Utc.timestamp_opt(100, 0).unwrap())
    }

    fn synth(providers: &[OpenAiCompatibility]) -> Result<Vec<Auth>, SynthesisError> {
        synthesize_openai_compat_auths(providers, &ctx(), &mut StableIdGenerator::new())
    }

    fn keys(keys: &[&str]) -> Vec<OpenAiCompatibilityApiKey> {
        keys.iter()
            .map(|key| OpenAiCompatibilityApiKey {
                api_key: (*key).to_owned(),
                ..OpenAiCompatibilityApiKey::default()
            })
            .collect()
    }

    fn provider(name: &str, base_url: &str, api_keys: &[&str]) -> OpenAiCompatibility {
        OpenAiCompatibility {
            name: name.into(),
            base_url: base_url.into(),
            api_key_entries: keys(api_keys),
            ..OpenAiCompatibility::default()
        }
    }

    fn model(name: &str, alias: &str) -> OpenAiCompatibilityModel {
        OpenAiCompatibilityModel {
            name: name.into(),
            alias: alias.into(),
            ..OpenAiCompatibilityModel::default()
        }
    }

    #[test]
    fn records_per_key() {
        let with_keys = OpenAiCompatibility {
            disable_cooling: Some(true),
            ..provider(
                "CustomProvider",
                "https://custom.api.com",
                &["key-1", "key-2"],
            )
        };
        let auths = synth(&[with_keys]).unwrap();
        assert_eq!(auths.len(), 2);
        for auth in &auths {
            assert_eq!(
                auth.metadata.get("disable_cooling"),
                Some(&Value::Bool(true))
            );
        }
        let empty_keys = provider("EmptyKeys", "https://empty.api.com", &["", "   "]);
        let auths = synth(&[empty_keys]).unwrap();
        assert_eq!(auths.len(), 2, "empty keys still make records");
        assert!(auths.iter().all(|auth| auth.attribute("api_key").is_none()));
        assert_eq!(
            synth(&[provider("NoKeyProvider", "https://no-key.api.com", &[])])
                .unwrap()
                .len(),
            1
        );
        let unnamed = synth(&[provider("", "https://default.api.com", &[])]).unwrap();
        assert_eq!(unnamed.len(), 1);
        assert_eq!(unnamed[0].provider, "openai-compatibility");
        assert!(
            unnamed[0]
                .id
                .starts_with("openai-compatibility:openai-compatibility:")
        );
    }

    #[test]
    fn uses_namespaced_provider_key() {
        let auths = synth(&[provider(
            "kimi",
            "https://kimi-compatible.example.com/v1",
            &["test-key"],
        )])
        .unwrap();
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.provider, "openai-compatible-kimi");
        assert_eq!(
            auth.attribute("provider_key"),
            Some("openai-compatible-kimi")
        );
        assert_eq!(auth.attribute("compat_name"), Some("kimi"));
        assert_eq!(auth.attribute("config_index"), Some("0"));
        assert_eq!(
            auth.openai_compat_info(),
            Some(("openai-compatible-kimi".to_owned(), "kimi".to_owned()))
        );
    }

    #[test]
    fn record_fields() {
        let compat = OpenAiCompatibility {
            priority: 5,
            prefix: "team".into(),
            headers: BTreeMap::from([("X-Team".to_owned(), "blue".to_owned())]),
            api_key_entries: vec![OpenAiCompatibilityApiKey {
                api_key: " sk-compat ".into(),
                weight: Some(-3),
                proxy_url: " http://proxy.local ".into(),
            }],
            ..provider(" Kimi ", " https://kimi.example.com/v1 ", &[])
        };
        let skipped = OpenAiCompatibility {
            disabled: true,
            ..provider("off", "https://off.example.com", &["k"])
        };
        let auths = synth(&[skipped, compat]).unwrap();
        assert_eq!(auths.len(), 1, "a disabled provider makes no records");
        let auth = &auths[0];
        let token = auth
            .id
            .strip_prefix("openai-compatibility:kimi:")
            .expect("the ID names the provider");
        assert_eq!(token.len(), 12);
        let attrs: Vec<(&str, &str)> = auth
            .attributes
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let source = format!("config:kimi[{token}]");
        assert_eq!(
            attrs,
            [
                ("api_key", "sk-compat"),
                ("base_url", "https://kimi.example.com/v1"),
                ("compat_name", " Kimi "),
                ("config_index", "1"),
                ("header:X-Team", "blue"),
                ("priority", "5"),
                ("provider_key", "openai-compatible-kimi"),
                ("source", source.as_str()),
                ("weight", "0"),
            ]
        );
        assert_eq!(auth.provider, "openai-compatible-kimi");
        assert_eq!(auth.label, " Kimi ");
        assert_eq!(auth.prefix, "team");
        assert_eq!(auth.proxy_url, "http://proxy.local");
        assert_eq!(auth.status, Status::Active);
        assert!(auth.metadata.is_empty());
        assert_eq!(auth.created_at, Some(ctx().now));
        assert!(auth.attribute("auth_kind").is_none());

        let mut ids = StableIdGenerator::new();
        let again = openai_compat_auths(
            0,
            &provider("kimi", "https://kimi.example.com/v1", &[]),
            &ctx(),
            &mut ids,
        );
        let (id, _) = StableIdGenerator::new().next(
            "openai-compatibility:kimi",
            &["https://kimi.example.com/v1"],
        );
        assert_eq!(again[0].id, id, "a keyless record hashes the base URL");
        assert!(again[0].attribute("api_key").is_none());
    }

    #[test]
    fn with_models_hash() {
        let compat = OpenAiCompatibility {
            models: vec![model("model-a", ""), model("model-b", "")],
            ..provider("TestProvider", "https://test.api.com", &["key-with-models"])
        };
        let auths = synth(&[compat]).unwrap();
        assert_eq!(auths.len(), 1);
        assert!(auths[0].attribute("models_hash").is_some());
        assert_eq!(auths[0].attribute("api_key"), Some("key-with-models"));
    }

    #[test]
    fn fallback_with_models() {
        let compat = OpenAiCompatibility {
            models: vec![model("model-x", "")],
            headers: BTreeMap::from([("X-API".to_owned(), "header-value".to_owned())]),
            ..provider("NoKeyWithModels", "https://nokey.api.com", &[])
        };
        let auths = synth(&[compat]).unwrap();
        assert_eq!(auths.len(), 1);
        assert!(auths[0].attribute("models_hash").is_some());
        assert_eq!(auths[0].attribute("header:X-API"), Some("header-value"));
    }

    #[test]
    fn weights() {
        let mut compat = provider("compat", "https://compat.example.com", &["key"]);
        compat.api_key_entries[0].weight = Some(1_000_001);
        let err = synth(&[provider("ok", "u", &["k"]), compat]).unwrap_err();
        assert!(
            err.to_string()
                .contains("openai-compatibility[1].api-key-entries[0].weight"),
            "{err}"
        );
        let mut compat = provider("compat", "https://compat.example.com", &["compat", "other"]);
        compat.api_key_entries[0].weight = Some(6);
        let auths = synth(&[compat]).unwrap();
        assert_eq!(auths[0].attribute("weight"), Some("6"));
        assert_eq!(
            auths[1].attribute("weight"),
            None,
            "an omitted weight stays unset"
        );
    }

    #[test]
    fn request_retry_and_scoped_errors() {
        let compat = OpenAiCompatibility {
            request_retry: Some(0),
            request_scoped_errors: vec![RequestScopedErrorRule {
                status: 400,
                matches: vec!["context".into()],
                match_regexr: Vec::new(),
                action: "stop".into(),
            }],
            ..provider("compat", "https://compat.api", &["compat-key"])
        };
        let auths = synth(&[compat]).unwrap();
        assert_eq!(auths[0].metadata.get("request_retry"), Some(&json!(0)));
        assert_eq!(
            auths[0].metadata.get("request_scoped_errors"),
            Some(&json!([{"status": 400, "match": ["context"], "action": "stop"}]))
        );
        let negative = OpenAiCompatibility {
            request_retry: Some(-1),
            ..provider("compat", "https://compat.api", &["compat-key"])
        };
        assert!(synth(&[negative]).unwrap()[0].metadata.is_empty());
    }

    #[test]
    fn models_hash() {
        let models = [model("gpt-4", "gpt4"), model("gpt-3.5-turbo", "")];
        let hash = compute_openai_compat_models_hash(&models);
        assert!(!hash.is_empty());
        assert_eq!(hash, compute_openai_compat_models_hash(&models));
        assert_ne!(
            hash,
            compute_openai_compat_models_hash(&[model("gpt-4", ""), model("gpt-4.1", "")])
        );

        let base = compute_openai_compat_models_hash(&[model("m", "a")]);
        let changed = |change: fn(&mut OpenAiCompatibilityModel)| {
            let mut changed = model("m", "a");
            change(&mut changed);
            compute_openai_compat_models_hash(&[changed])
        };
        for (name, other) in [
            ("image", changed(|m| m.image = true)),
            ("display name", changed(|m| m.display_name = "Two".into())),
            ("force mapping", changed(|m| m.force_mapping = true)),
            (
                "max completion tokens",
                changed(|m| m.use_max_completion_tokens = true),
            ),
            (
                "input",
                changed(|m| m.input_modalities = vec!["text".into()]),
            ),
            (
                "output",
                changed(|m| m.output_modalities = vec!["image".into()]),
            ),
            (
                "thinking",
                changed(|m| m.thinking = Some(ThinkingSupport::default())),
            ),
        ] {
            assert_ne!(base, other, "{name} must change the hash");
        }
        let thinking = |level: &str| {
            compute_openai_compat_models_hash(&[OpenAiCompatibilityModel {
                thinking: Some(ThinkingSupport {
                    levels: vec![level.into()],
                    ..ThinkingSupport::default()
                }),
                ..model("m", "")
            }])
        };
        assert_ne!(thinking("low"), thinking("high"));

        let routing = [
            model("gpt-4", "gpt4"),
            model(" ", ""),
            model("GPT-4", "GPT4"),
            model("", "a1"),
        ];
        let reordered = [model("", "A1"), model("gpt-4", "gpt4")];
        assert_ne!(
            compute_openai_compat_models_hash(&routing),
            compute_openai_compat_models_hash(&reordered),
            "routing order and duplicates change the hash"
        );

        assert_eq!(compute_openai_compat_models_hash(&[]), "");
        assert_eq!(
            compute_openai_compat_models_hash(&[model(" ", ""), model("", "")]),
            ""
        );
    }

    #[test]
    fn models_hash_matches_go() {
        let models = [
            OpenAiCompatibilityModel {
                display_name: " Kimi <K2> ".into(),
                image: true,
                force_mapping: true,
                is_compat: true,
                use_max_completion_tokens: true,
                input_modalities: vec![" Text ".into(), "image".into(), "TEXT".into(), "".into()],
                output_modalities: vec!["text".into()],
                thinking: Some(ThinkingSupport {
                    min: 1,
                    levels: vec!["low".into()],
                    ..ThinkingSupport::default()
                }),
                ..model(" Kimi-K2 ", "K2")
            },
            OpenAiCompatibilityModel::default(),
            model("", "only-alias"),
        ];
        // Computed with upstream's ComputeOpenAICompatModelsHash in Go.
        assert_eq!(
            compute_openai_compat_models_hash(&models),
            "c4bb2618ce015443ebfbdebcba1f37768c14893accdc666d0240d93fb8ea1dfe"
        );
    }
}
