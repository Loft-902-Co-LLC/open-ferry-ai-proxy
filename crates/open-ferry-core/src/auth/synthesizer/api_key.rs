// Ported from CLIProxyAPI internal/watcher/synthesizer/config.go (the
// gemini-api-key, interactions-api-key, claude-api-key, codex-api-key,
// xai-api-key and meta-api-key parts, including synthesizeGeminiKeyEntries
// and synthesizeCodexStyleKeys), addRequestRetryToMetadata and
// addRequestScopedErrorsToMetadata in helpers.go, ComputeGeminiModelsHash,
// ComputeClaudeModelsHash and ComputeCodexModelsHash in
// internal/modelconfig/model_hash.go, and ValidateCredentialWeights in
// internal/config/weight.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Records for the API keys in the config's `gemini-api-key`,
//! `interactions-api-key`, `claude-api-key`, `codex-api-key`, `xai-api-key`
//! and `meta-api-key` lists.
//!
//! The config module parses those lists into [`GeminiKey`], [`CodexKey`]
//! and [`ClaudeKey`] entries (the interactions keys are Gemini keys, the xAI
//! and Meta keys Codex keys); each converts into an [`ApiKeyEntry`], and
//! [`api_key_auth`] builds its record:
//!
//! - The ID is `<provider>:apikey:<hash>`, from a hash of the key, base URL,
//!   proxy URL, prefix and headers, so it survives reloads without showing
//!   the key.
//! - The provider is `gemini`, `gemini-interactions`, `claude`, `codex`,
//!   `xai` or `meta`, and the label `<source name>-apikey`, where the source
//!   name is the provider's, apart from `interactions`.
//! - Attributes: `source` (`config:<source name>[<hash>]`), `config_index`,
//!   `api_key`, `base_url`, `priority`, `weight`, `models_hash`,
//!   `header:<name>`, `excluded_models`, `excluded_models_hash` and
//!   `auth_kind` (`apikey`); `rebuild_mid_system_message` for claude;
//!   `websockets` for codex, xai and meta; `codex_alpha_search` for codex.
//! - Metadata: `disable_cooling`, `request_retry` and
//!   `request_scoped_errors`, which the credential manager reads as it reads
//!   them from a credential file.
//!
//! An entry with neither a key nor a base URL makes no record.
//!
//! Deviations from upstream:
//! - The `fingerprint_profile` and `codex_disable_cloaking` attributes
//!   aren't set; the project doesn't forge client fingerprints or cloak
//!   requests.
//! - Request-scoped error rules are stored in the metadata as JSON objects,
//!   the form a credential file holds them in, where upstream stores Go
//!   structs.
//! - The OpenAI-compatible providers are in [`super::openai_compat`] and the
//!   Vertex keys in [`super::vertex`].

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value};

use super::super::classification::{
    ATTRIBUTE_API_KEY, ATTRIBUTE_CODEX_ALPHA_SEARCH, ATTRIBUTE_CONFIG_INDEX, ATTRIBUTE_SOURCE,
    ATTRIBUTE_WEIGHT, AUTH_KIND_API_KEY,
};
use super::super::weight::normalize_weight;
use super::super::{Auth, Status};
use super::{
    StableIdGenerator, SynthesisContext, SynthesisError, add_config_headers_to_attrs,
    apply_auth_excluded_models_meta, format_sorted_headers, sha256_hex,
};
use crate::config::{ClaudeKey, CodexKey, GeminiKey, RedactedUrl};
pub use crate::config::{RequestScopedErrorRule, ThinkingSupport};

/// Which config list an API key comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApiKeyProvider {
    /// `gemini-api-key`.
    Gemini,
    /// `interactions-api-key`: Gemini keys for the Interactions API.
    Interactions,
    /// `claude-api-key`.
    Claude,
    /// `codex-api-key`.
    Codex,
    /// `xai-api-key`.
    Xai,
    /// `meta-api-key`.
    Meta,
}

impl ApiKeyProvider {
    /// The provider name: `gemini`, `gemini-interactions`, `claude`,
    /// `codex`, `xai` or `meta`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::Interactions => "gemini-interactions",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Xai => "xai",
            Self::Meta => "meta",
        }
    }

    /// The name in the record's `source` attribute and label: the provider
    /// name, or `interactions` for the interactions keys.
    pub fn source_name(self) -> &'static str {
        match self {
            Self::Interactions => "interactions",
            _ => self.as_str(),
        }
    }

    /// The config list's name, as error messages give it.
    pub fn config_key(self) -> &'static str {
        match self {
            Self::Gemini => "gemini-api-key",
            Self::Interactions => "interactions-api-key",
            Self::Claude => "claude-api-key",
            Self::Codex => "codex-api-key",
            Self::Xai => "xai-api-key",
            Self::Meta => "meta-api-key",
        }
    }
}

/// One entry of an API-key list, as parsed from the config (upstream's
/// `GeminiKey`, `ClaudeKey` and `CodexKey`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ApiKeyEntry {
    /// The API key. Secret.
    pub api_key: String,
    /// The provider's base URL, if not the default.
    pub base_url: String,
    /// The proxy to reach the provider through.
    pub proxy_url: String,
    /// The model prefix that routes to this key.
    pub prefix: String,
    /// The routing priority; 0 sets none.
    pub priority: i64,
    /// The routing weight, if set.
    pub weight: Option<i64>,
    /// Extra request headers. Values may be secret.
    pub headers: BTreeMap<String, String>,
    /// Model aliases offered through this key.
    pub models: Vec<ApiKeyModel>,
    /// Models this key must not serve.
    pub excluded_models: Vec<String>,
    /// Whether to skip cooling after failures, if set.
    pub disable_cooling: Option<bool>,
    /// Retries per request, if set; negative counts as unset.
    pub request_retry: Option<i64>,
    /// How to handle errors that concern one request only.
    pub request_scoped_errors: Vec<RequestScopedErrorRule>,
    /// Claude only: rebuild system messages found mid-conversation.
    pub rebuild_mid_system_message: bool,
    /// Codex, xAI and Meta only: use the Responses WebSocket transport.
    pub websockets: bool,
    /// Codex only: allow alpha search.
    pub alpha_search: bool,
}

impl fmt::Debug for ApiKeyEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyEntry")
            .field("base_url", &RedactedUrl(&self.base_url))
            .field("prefix", &self.prefix)
            .field("priority", &self.priority)
            .field("weight", &self.weight)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("models", &self.models)
            .finish_non_exhaustive()
    }
}

/// A model offered through an API key (upstream's `GeminiModel`,
/// `ClaudeModel` and `CodexModel`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiKeyModel {
    /// The upstream model.
    pub name: String,
    /// The name clients use.
    pub alias: String,
    /// The name shown in model lists.
    pub display_name: String,
    /// Whether requests for the alias always map to the model.
    pub force_mapping: bool,
    /// Whether the model is a compatibility entry.
    pub is_compat: bool,
    /// The model's thinking limits, if set.
    pub thinking: Option<ThinkingSupport>,
}

impl From<&GeminiKey> for ApiKeyEntry {
    fn from(key: &GeminiKey) -> Self {
        Self {
            api_key: key.api_key.clone(),
            base_url: key.base_url.clone(),
            proxy_url: key.proxy_url.clone(),
            prefix: key.prefix.clone(),
            priority: key.priority,
            weight: key.weight,
            headers: key.headers.clone(),
            models: key
                .models
                .iter()
                .map(|model| ApiKeyModel {
                    name: model.name.clone(),
                    alias: model.alias.clone(),
                    display_name: model.display_name.clone(),
                    force_mapping: model.force_mapping,
                    is_compat: model.is_compat,
                    thinking: model.thinking.clone(),
                })
                .collect(),
            excluded_models: key.excluded_models.clone(),
            disable_cooling: key.disable_cooling,
            request_retry: key.request_retry,
            request_scoped_errors: key.request_scoped_errors.clone(),
            rebuild_mid_system_message: false,
            websockets: false,
            alpha_search: false,
        }
    }
}

impl From<&ClaudeKey> for ApiKeyEntry {
    fn from(key: &ClaudeKey) -> Self {
        Self {
            api_key: key.api_key.clone(),
            base_url: key.base_url.clone(),
            proxy_url: key.proxy_url.clone(),
            prefix: key.prefix.clone(),
            priority: key.priority,
            weight: key.weight,
            headers: key.headers.clone(),
            models: key
                .models
                .iter()
                .map(|model| ApiKeyModel {
                    name: model.name.clone(),
                    alias: model.alias.clone(),
                    display_name: model.display_name.clone(),
                    force_mapping: model.force_mapping,
                    is_compat: model.is_compat,
                    thinking: model.thinking.clone(),
                })
                .collect(),
            excluded_models: key.excluded_models.clone(),
            disable_cooling: key.disable_cooling,
            request_retry: key.request_retry,
            request_scoped_errors: key.request_scoped_errors.clone(),
            rebuild_mid_system_message: key.rebuild_mid_system_message,
            websockets: false,
            alpha_search: false,
        }
    }
}

impl From<&CodexKey> for ApiKeyEntry {
    fn from(key: &CodexKey) -> Self {
        Self {
            api_key: key.api_key.clone(),
            base_url: key.base_url.clone(),
            proxy_url: key.proxy_url.clone(),
            prefix: key.prefix.clone(),
            priority: key.priority,
            weight: key.weight,
            headers: key.headers.clone(),
            models: key
                .models
                .iter()
                .map(|model| ApiKeyModel {
                    name: model.name.clone(),
                    alias: model.alias.clone(),
                    display_name: model.display_name.clone(),
                    force_mapping: model.force_mapping,
                    is_compat: model.is_compat,
                    thinking: model.thinking.clone(),
                })
                .collect(),
            excluded_models: key.excluded_models.clone(),
            disable_cooling: key.disable_cooling,
            request_retry: key.request_retry,
            request_scoped_errors: key.request_scoped_errors.clone(),
            rebuild_mid_system_message: false,
            websockets: key.websockets,
            alpha_search: key.alpha_search,
        }
    }
}

impl RequestScopedErrorRule {
    /// The rule as a credential file holds it: `status`, `match`,
    /// `match-regexr` and `action`, each left out when empty.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        if self.status != 0 {
            out.insert("status".to_owned(), Value::from(self.status));
        }
        if !self.matches.is_empty() {
            out.insert("match".to_owned(), Value::from(self.matches.clone()));
        }
        if !self.match_regexr.is_empty() {
            out.insert(
                "match-regexr".to_owned(),
                Value::from(self.match_regexr.clone()),
            );
        }
        if !self.action.is_empty() {
            out.insert("action".to_owned(), Value::from(self.action.clone()));
        }
        Value::Object(out)
    }
}

/// Records for every claude and then every codex API key, after checking
/// every weight. An invalid weight fails the whole list, naming the entry.
pub fn synthesize_api_key_auths(
    claude: &[ApiKeyEntry],
    codex: &[ApiKeyEntry],
    ctx: &SynthesisContext,
    ids: &mut StableIdGenerator,
) -> Result<Vec<Auth>, SynthesisError> {
    validate_api_key_weights(ApiKeyProvider::Claude, claude)?;
    validate_api_key_weights(ApiKeyProvider::Codex, codex)?;
    let mut out = Vec::with_capacity(claude.len() + codex.len());
    for (provider, entries) in [
        (ApiKeyProvider::Claude, claude),
        (ApiKeyProvider::Codex, codex),
    ] {
        for (index, entry) in entries.iter().enumerate() {
            if let Some(auth) = api_key_auth(provider, index, entry, ctx, ids) {
                out.push(auth);
            }
        }
    }
    Ok(out)
}

/// Checks the weights of one config list.
pub fn validate_api_key_weights(
    provider: ApiKeyProvider,
    entries: &[ApiKeyEntry],
) -> Result<(), SynthesisError> {
    for (index, entry) in entries.iter().enumerate() {
        if let Some(weight) = entry.weight
            && let Err(err) = normalize_weight(weight)
        {
            return Err(SynthesisError::new(format!(
                "synthesize config API key auths: {}[{index}].weight: {err}",
                provider.config_key()
            )));
        }
    }
    Ok(())
}

/// The record for the API key at `index` in its provider's list, or `None`
/// when it has neither a key nor a base URL. The weight isn't checked; see
/// [`validate_api_key_weights`].
pub fn api_key_auth(
    provider: ApiKeyProvider,
    index: usize,
    entry: &ApiKeyEntry,
    ctx: &SynthesisContext,
    ids: &mut StableIdGenerator,
) -> Option<Auth> {
    let key = entry.api_key.trim();
    let base_url = entry.base_url.trim();
    if key.is_empty() && base_url.is_empty() {
        return None;
    }
    let name = provider.as_str();
    let source = provider.source_name();
    let prefix = entry.prefix.trim();
    let proxy_url = entry.proxy_url.trim();
    let headers = format_sorted_headers(&entry.headers);
    let (id, token) = ids.next(
        &format!("{name}:apikey"),
        &[key, base_url, proxy_url, prefix, &headers],
    );

    let mut attrs = BTreeMap::new();
    attrs.insert(
        ATTRIBUTE_SOURCE.to_owned(),
        format!("config:{source}[{token}]"),
    );
    attrs.insert(ATTRIBUTE_CONFIG_INDEX.to_owned(), index.to_string());
    if !key.is_empty() {
        attrs.insert(ATTRIBUTE_API_KEY.to_owned(), key.to_owned());
    }
    let metadata = retry_metadata(
        entry.disable_cooling,
        entry.request_retry,
        &entry.request_scoped_errors,
    );
    if entry.priority != 0 {
        attrs.insert("priority".to_owned(), entry.priority.to_string());
    }
    if let Some(weight) = entry.weight {
        attrs.insert(ATTRIBUTE_WEIGHT.to_owned(), weight.max(0).to_string());
    }
    if !base_url.is_empty() {
        attrs.insert("base_url".to_owned(), base_url.to_owned());
    }
    match provider {
        ApiKeyProvider::Gemini | ApiKeyProvider::Interactions => {}
        ApiKeyProvider::Claude => {
            if entry.rebuild_mid_system_message {
                attrs.insert("rebuild_mid_system_message".to_owned(), "true".to_owned());
            }
        }
        ApiKeyProvider::Codex => {
            if entry.websockets {
                attrs.insert("websockets".to_owned(), "true".to_owned());
            }
            if entry.alpha_search {
                attrs.insert(ATTRIBUTE_CODEX_ALPHA_SEARCH.to_owned(), "true".to_owned());
            }
        }
        ApiKeyProvider::Xai | ApiKeyProvider::Meta => {
            if entry.websockets {
                attrs.insert("websockets".to_owned(), "true".to_owned());
            }
        }
    }
    let models_hash = compute_models_hash(&entry.models);
    if !models_hash.is_empty() {
        attrs.insert("models_hash".to_owned(), models_hash);
    }
    add_config_headers_to_attrs(&entry.headers, &mut attrs);

    let mut auth = Auth {
        id,
        provider: name.to_owned(),
        label: format!("{source}-apikey"),
        prefix: prefix.to_owned(),
        status: Status::Active,
        proxy_url: proxy_url.to_owned(),
        attributes: attrs,
        metadata,
        created_at: Some(ctx.now),
        updated_at: Some(ctx.now),
        ..Auth::default()
    };
    apply_auth_excluded_models_meta(
        &mut auth,
        &ctx.oauth_excluded_models,
        &entry.excluded_models,
        AUTH_KIND_API_KEY,
    );
    Some(auth)
}

/// The metadata that carries an entry's cooling and retry settings:
/// `disable_cooling` if set, `request_retry` if set and not negative, and
/// `request_scoped_errors` if there are rules (upstream's
/// `addRequestRetryToMetadata` and `addRequestScopedErrorsToMetadata`).
pub(super) fn retry_metadata(
    disable_cooling: Option<bool>,
    request_retry: Option<i64>,
    request_scoped_errors: &[RequestScopedErrorRule],
) -> Map<String, Value> {
    let mut metadata = Map::new();
    if let Some(disable_cooling) = disable_cooling {
        metadata.insert("disable_cooling".to_owned(), Value::Bool(disable_cooling));
    }
    if let Some(retry) = request_retry
        && retry >= 0
    {
        metadata.insert("request_retry".to_owned(), Value::from(retry));
    }
    if !request_scoped_errors.is_empty() {
        let rules = request_scoped_errors
            .iter()
            .map(RequestScopedErrorRule::to_json)
            .collect();
        metadata.insert("request_scoped_errors".to_owned(), Value::Array(rules));
    }
    metadata
}

/// A hash of a key's model list, to notice when it changes: the SHA-256, in
/// hex, of one line per model with a name or alias. Empty for no models.
/// Upstream's Gemini, Claude and Codex hashes are alike.
pub fn compute_models_hash(models: &[ApiKeyModel]) -> String {
    let lines: Vec<String> = models
        .iter()
        .filter_map(|model| {
            let name = model.name.trim();
            let alias = model.alias.trim();
            if name.is_empty() && alias.is_empty() {
                return None;
            }
            Some(format!(
                "{}|{}|{}|force-mapping={}|is-compat={}|thinking={}",
                open_ferry_translate::go::to_lower(name),
                open_ferry_translate::go::to_lower(alias),
                model.display_name.trim(),
                model.force_mapping,
                model.is_compat,
                thinking_json(model.thinking.as_ref()),
            ))
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    sha256_hex(lines.join("\n").as_bytes())
}

/// Go's `json.Marshal` of a `*ThinkingSupport`.
pub(super) fn thinking_json(support: Option<&ThinkingSupport>) -> String {
    let Some(support) = support else {
        return "null".to_owned();
    };
    let mut fields = Vec::new();
    if support.min != 0 {
        fields.push(format!("\"min\":{}", support.min));
    }
    if support.max != 0 {
        fields.push(format!("\"max\":{}", support.max));
    }
    if support.zero_allowed {
        fields.push("\"zero_allowed\":true".to_owned());
    }
    if support.dynamic_allowed {
        fields.push("\"dynamic_allowed\":true".to_owned());
    }
    if !support.levels.is_empty() {
        let levels: Vec<String> = support
            .levels
            .iter()
            .map(|level| open_ferry_translate::go::json_string(level))
            .collect();
        fields.push(format!("\"levels\":[{}]", levels.join(",")));
    }
    format!("{{{}}}", fields.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ctx() -> SynthesisContext {
        SynthesisContext::new("", chrono::Utc.timestamp_opt(100, 0).unwrap())
    }

    fn key(api_key: &str) -> ApiKeyEntry {
        ApiKeyEntry {
            api_key: api_key.into(),
            ..ApiKeyEntry::default()
        }
    }

    fn synth(claude: &[ApiKeyEntry], codex: &[ApiKeyEntry]) -> Result<Vec<Auth>, SynthesisError> {
        synthesize_api_key_auths(claude, codex, &ctx(), &mut StableIdGenerator::new())
    }

    #[test]
    fn claude_keys() {
        let entry = ApiKeyEntry {
            api_key: "sk-test-claude".into(),
            prefix: "main".into(),
            base_url: "https://claude.example.com".into(),
            disable_cooling: Some(true),
            rebuild_mid_system_message: true,
            models: vec![
                ApiKeyModel {
                    name: "claude-3-opus".into(),
                    ..ApiKeyModel::default()
                },
                ApiKeyModel {
                    name: "claude-3-sonnet".into(),
                    ..ApiKeyModel::default()
                },
            ],
            ..ApiKeyEntry::default()
        };
        let auths = synth(&[entry], &[]).unwrap();
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.provider, "claude");
        assert_eq!(auth.label, "claude-apikey");
        assert_eq!(auth.prefix, "main");
        assert_eq!(auth.status, Status::Active);
        assert_eq!(auth.attribute("api_key"), Some("sk-test-claude"));
        assert_eq!(auth.attribute("config_index"), Some("0"));
        assert_eq!(
            auth.attribute("base_url"),
            Some("https://claude.example.com")
        );
        assert!(auth.attribute("models_hash").is_some());
        assert_eq!(auth.attribute("rebuild_mid_system_message"), Some("true"));
        assert_eq!(auth.attribute("fingerprint_profile"), None);
        assert_eq!(auth.attribute("websockets"), None);
        assert_eq!(auth.attribute("auth_kind"), Some("apikey"));
        assert_eq!(auth.metadata["disable_cooling"], Value::Bool(true));
        assert!(auth.id.starts_with("claude:apikey:"));
        let token = auth.id.trim_start_matches("claude:apikey:");
        assert_eq!(
            auth.attribute("source"),
            Some(format!("config:claude[{token}]").as_str())
        );
        assert!(!auth.id.contains("sk-test-claude"));
        assert_eq!(auth.created_at, Some(ctx().now));
    }

    #[test]
    fn claude_keys_skip_empty_and_set_headers() {
        let entries = [
            key(""),
            key("   "),
            ApiKeyEntry {
                headers: BTreeMap::from([("X-Custom".to_owned(), "value".to_owned())]),
                ..key("valid-key")
            },
        ];
        let auths = synth(&entries, &[]).unwrap();
        assert_eq!(auths.len(), 1);
        assert_eq!(auths[0].attribute("header:X-Custom"), Some("value"));
        assert_eq!(auths[0].attribute("config_index"), Some("2"));
        assert!(auths[0].metadata.is_empty());
    }

    #[test]
    fn codex_keys() {
        let entry = ApiKeyEntry {
            api_key: "codex-key-123".into(),
            prefix: "dev".into(),
            base_url: "https://codex.example.com".into(),
            proxy_url: " http://proxy.local ".into(),
            websockets: true,
            alpha_search: true,
            rebuild_mid_system_message: true,
            disable_cooling: Some(true),
            ..ApiKeyEntry::default()
        };
        let auths = synth(&[], &[entry]).unwrap();
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.provider, "codex");
        assert_eq!(auth.label, "codex-apikey");
        assert_eq!(auth.proxy_url, "http://proxy.local");
        assert_eq!(auth.attribute("websockets"), Some("true"));
        assert_eq!(auth.attribute(ATTRIBUTE_CODEX_ALPHA_SEARCH), Some("true"));
        assert_eq!(auth.attribute("codex_disable_cloaking"), None);
        assert_eq!(auth.attribute("rebuild_mid_system_message"), None);
        assert_eq!(auth.metadata["disable_cooling"], Value::Bool(true));
        assert!(auth.id.starts_with("codex:apikey:"));
    }

    #[test]
    fn codex_keys_skip_empty_and_set_headers() {
        let entries = [
            key(""),
            key("  "),
            ApiKeyEntry {
                headers: BTreeMap::from([("Authorization".to_owned(), "Bearer xyz".to_owned())]),
                ..key("valid-key")
            },
        ];
        let auths = synth(&[], &entries).unwrap();
        assert_eq!(auths.len(), 1);
        assert_eq!(
            auths[0].attribute("header:Authorization"),
            Some("Bearer xyz")
        );
        assert_eq!(auths[0].attribute(ATTRIBUTE_CODEX_ALPHA_SEARCH), None);
    }

    #[test]
    fn empty_key_with_base_url_is_allowed() {
        for provider in [ApiKeyProvider::Claude, ApiKeyProvider::Codex] {
            let entries = [
                ApiKeyEntry {
                    base_url: "https://custom.example.com".into(),
                    headers: BTreeMap::from([("Custom-Auth".to_owned(), "secret".to_owned())]),
                    ..ApiKeyEntry::default()
                },
                ApiKeyEntry {
                    api_key: "   ".into(),
                    base_url: "https://custom-2.example.com".into(),
                    ..ApiKeyEntry::default()
                },
            ];
            let auths = if provider == ApiKeyProvider::Claude {
                synth(&entries, &[])
            } else {
                synth(&[], &entries)
            }
            .unwrap();
            assert_eq!(auths.len(), 2);
            assert_eq!(
                auths[0].attribute("base_url"),
                Some("https://custom.example.com")
            );
            assert_eq!(auths[0].attribute("header:Custom-Auth"), Some("secret"));
            assert_eq!(auths[0].attribute("auth_kind"), Some("apikey"));
            assert_eq!(auths[0].attribute("api_key"), None);
        }
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let entries = [ApiKeyEntry {
            prefix: "test".into(),
            ..key("stable-key")
        }];
        let first = synth(&entries, &[]).unwrap();
        let second = synth(&entries, &[]).unwrap();
        assert_eq!(first[0].id, second[0].id);

        // The same entry twice gets two IDs.
        let twice = synth(&[key("k"), key("k")], &[]).unwrap();
        assert_eq!(twice[1].id, format!("{}-1", twice[0].id));
        // The same key under each provider gets different IDs.
        let both = synth(&[key("k")], &[key("k")]).unwrap();
        assert_ne!(both[0].id, both[1].id);
    }

    #[test]
    fn id_pins_upstream_hash() {
        // Computed with upstream's StableIDGenerator.Next in Go.
        let auths = synth(&[key(" k ")], &[]).unwrap();
        assert_eq!(auths[0].id, "claude:apikey:3f074c8905eb");
        assert_eq!(
            auths[0].attribute("source"),
            Some("config:claude[3f074c8905eb]")
        );
        assert_eq!(auths[0].attribute("api_key"), Some("k"));
    }

    #[test]
    fn rejects_invalid_weights() {
        let invalid = ApiKeyEntry {
            weight: Some(1_000_001),
            ..key("key")
        };
        let err = synth(std::slice::from_ref(&invalid), &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "synthesize config API key auths: claude-api-key[0].weight: weight must not exceed 1000000"
        );
        let err = synth(&[key("ok")], &[key("a"), invalid.clone()]).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("synthesize config API key auths: codex-api-key[1].weight"),
            "{err}"
        );
        // Claude is checked first.
        let err = synth(
            std::slice::from_ref(&invalid),
            std::slice::from_ref(&invalid),
        )
        .unwrap_err();
        assert!(err.to_string().contains("claude-api-key[0]"), "{err}");
    }

    #[test]
    fn weights() {
        let auths = synth(&[key("key")], &[]).unwrap();
        assert_eq!(auths[0].attribute(ATTRIBUTE_WEIGHT), None);

        let with = |weight| ApiKeyEntry {
            weight: Some(weight),
            ..key("key")
        };
        let auths = synth(&[with(-5)], &[]).unwrap();
        assert_eq!(auths[0].attribute(ATTRIBUTE_WEIGHT), Some("0"));

        let auths = synth(&[with(3)], &[with(4)]).unwrap();
        assert_eq!(auths[0].attribute(ATTRIBUTE_WEIGHT), Some("3"));
        assert_eq!(auths[1].attribute(ATTRIBUTE_WEIGHT), Some("4"));
    }

    #[test]
    fn priority() {
        let entry = ApiKeyEntry {
            priority: -2,
            ..key("key")
        };
        let auths = synth(&[entry, key("zero")], &[]).unwrap();
        assert_eq!(auths[0].attribute("priority"), Some("-2"));
        assert_eq!(auths[1].attribute("priority"), None);
    }

    #[test]
    fn request_retry() {
        let with = |name: &str, retry| ApiKeyEntry {
            request_retry: retry,
            ..key(name)
        };
        let auths = synth(
            &[
                with("claude-positive", Some(2)),
                with("claude-negative", Some(-1)),
            ],
            &[with("codex-zero", Some(0)), with("codex-unset", None)],
        )
        .unwrap();
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
                (Some("claude-positive"), Some(&Value::from(2))),
                (Some("claude-negative"), None),
                (Some("codex-zero"), Some(&Value::from(0))),
                (Some("codex-unset"), None),
            ]
        );
        assert_eq!(auths[0].request_retry_override(), Some(2));
        assert_eq!(auths[2].request_retry_override(), Some(0));
    }

    #[test]
    fn request_scoped_errors() {
        let rules = vec![RequestScopedErrorRule {
            status: 400,
            matches: vec!["maximum_context_length".into()],
            action: "stop".into(),
            ..RequestScopedErrorRule::default()
        }];
        let entry = ApiKeyEntry {
            request_scoped_errors: rules,
            ..key("key")
        };
        let auths = synth(std::slice::from_ref(&entry), std::slice::from_ref(&entry)).unwrap();
        for auth in &auths {
            assert_eq!(
                auth.metadata["request_scoped_errors"],
                serde_json::json!([{"status": 400, "match": ["maximum_context_length"], "action": "stop"}])
            );
        }
    }

    #[test]
    fn explicit_false_cooling_override_is_kept() {
        let entry = ApiKeyEntry {
            disable_cooling: Some(false),
            base_url: "https://example.com".into(),
            ..key("key")
        };
        let auths = synth(std::slice::from_ref(&entry), std::slice::from_ref(&entry)).unwrap();
        assert_eq!(auths.len(), 2);
        for auth in &auths {
            assert_eq!(auth.disable_cooling_override(), Some(false));
        }
    }

    #[test]
    fn excluded_models_use_only_the_key_list() {
        let mut context = ctx();
        context.oauth_excluded_models =
            BTreeMap::from([("claude".to_owned(), vec!["global".to_owned()])]);
        let entry = ApiKeyEntry {
            excluded_models: vec![" Model-B ".into(), "model-a".into()],
            ..key("key")
        };
        let auths =
            synthesize_api_key_auths(&[entry], &[], &context, &mut StableIdGenerator::new())
                .unwrap();
        assert_eq!(
            auths[0].attribute("excluded_models"),
            Some("model-a,model-b")
        );
        assert!(auths[0].attribute("excluded_models_hash").is_some());
    }

    #[test]
    fn models_hash_matches_go() {
        let models = [
            ApiKeyModel {
                name: " GPT-5 ".into(),
                alias: "Fast".into(),
                display_name: " Fast <model> ".into(),
                force_mapping: true,
                thinking: Some(ThinkingSupport {
                    min: 128,
                    levels: vec!["low".into(), "high".into()],
                    dynamic_allowed: true,
                    ..ThinkingSupport::default()
                }),
                ..ApiKeyModel::default()
            },
            ApiKeyModel::default(),
            ApiKeyModel {
                alias: "only-alias".into(),
                is_compat: true,
                thinking: Some(ThinkingSupport::default()),
                ..ApiKeyModel::default()
            },
        ];
        let want = concat!(
            "gpt-5|fast|Fast <model>|force-mapping=true|is-compat=false|thinking=",
            r#"{"min":128,"dynamic_allowed":true,"levels":["low","high"]}"#,
            "\n",
            "|only-alias||force-mapping=false|is-compat=true|thinking={}",
        );
        assert_eq!(compute_models_hash(&models), sha256_hex(want.as_bytes()));
        // Computed with upstream's ComputeClaudeModelsHash logic in Go.
        assert_eq!(
            compute_models_hash(&models),
            "04056a641fbe9dd7eca7e52e1667cde90a0cb74ffd64ce6d9252eb3bb0c3bd68"
        );
        assert_eq!(compute_models_hash(&[ApiKeyModel::default()]), "");
        assert_eq!(
            thinking_json(None),
            "null",
            "a model without thinking limits hashes as Go's nil pointer"
        );
    }

    // config_test.go: TestConfigSynthesizer_GeminiKeys,
    // TestConfigSynthesizer_InteractionsKeys, TestConfigSynthesizer_XAIKeys,
    // TestConfigSynthesizer_MetaKeys,
    // TestConfigSynthesizer_XAIKeys_AllowsEmptyAPIKeyWithBaseURL,
    // TestConfigSynthesizer_GeminiKeys_AllowsEmptyAPIKeyWithBaseURL,
    // TestConfigSynthesizer_IDStability,
    // TestConfigSynthesizer_OmittedWeightRemainsUnset,
    // TestConfigSynthesizer_NormalizesNonPositiveWeightToZero and the Gemini,
    // interactions and xAI parts of TestConfigSynthesizer_RequestScopedErrors.

    fn synth_config(config: &crate::config::Config) -> Vec<Auth> {
        super::super::synthesize_config_auths(config, &ctx(), &mut StableIdGenerator::new())
            .unwrap()
    }

    fn gemini(keys: Vec<GeminiKey>) -> Vec<Auth> {
        synth_config(&crate::config::Config {
            gemini_api_key: keys,
            ..crate::config::Config::default()
        })
    }

    fn header(name: &str, value: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(name.to_owned(), value.to_owned())])
    }

    fn gemini_key(api_key: &str) -> GeminiKey {
        GeminiKey {
            api_key: api_key.to_owned(),
            ..GeminiKey::default()
        }
    }

    #[test]
    fn gemini_keys() {
        let auths = gemini(vec![GeminiKey {
            prefix: "team-a".to_owned(),
            ..gemini_key("test-key-123")
        }]);
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.provider, "gemini");
        assert_eq!(auth.prefix, "team-a");
        assert_eq!(auth.label, "gemini-apikey");
        assert_eq!(auth.attribute("api_key"), Some("test-key-123"));
        assert!(auth.metadata.is_empty());
        assert_eq!(auth.status, Status::Active);
        let token = auth.id.strip_prefix("gemini:apikey:").expect("gemini ID");
        assert_eq!(
            auth.attribute("source"),
            Some(format!("config:gemini[{token}]").as_str())
        );
        assert_eq!(auth.attribute("rebuild_mid_system_message"), None);

        let auths = gemini(vec![GeminiKey {
            prefix: "team-a".to_owned(),
            disable_cooling: Some(true),
            ..gemini_key("test-key-123")
        }]);
        assert_eq!(auths[0].metadata["disable_cooling"], Value::Bool(true));

        let auths = gemini(vec![GeminiKey {
            base_url: "https://custom.api.com".to_owned(),
            proxy_url: "http://proxy.local:8080".to_owned(),
            prefix: "custom".to_owned(),
            ..gemini_key("api-key")
        }]);
        assert_eq!(
            auths[0].attribute("base_url"),
            Some("https://custom.api.com")
        );
        assert_eq!(auths[0].proxy_url, "http://proxy.local:8080");

        let auths = gemini(vec![GeminiKey {
            headers: BTreeMap::from([("X-Custom".to_owned(), "value".to_owned())]),
            ..gemini_key("api-key")
        }]);
        assert_eq!(auths[0].attribute("header:X-Custom"), Some("value"));

        let auths = gemini(vec![
            gemini_key(""),
            gemini_key("  "),
            gemini_key("valid-key"),
        ]);
        assert_eq!(auths.len(), 1);

        let auths = gemini(
            ["a", "b", "c"]
                .iter()
                .map(|prefix| GeminiKey {
                    prefix: (*prefix).to_owned(),
                    ..gemini_key(&format!("key-{prefix}"))
                })
                .collect(),
        );
        assert_eq!(auths.len(), 3);
    }

    #[test]
    fn gemini_keys_allow_empty_api_key_with_base_url() {
        let auths = synth_config(&crate::config::Config {
            gemini_api_key: vec![GeminiKey {
                base_url: "https://custom-gemini.example.com".to_owned(),
                headers: header("Custom-Auth", "secret"),
                ..GeminiKey::default()
            }],
            interactions_api_key: vec![GeminiKey {
                base_url: "https://custom-interactions.example.com".to_owned(),
                ..GeminiKey::default()
            }],
            ..crate::config::Config::default()
        });
        assert_eq!(auths.len(), 2);
        assert_eq!(
            auths[0].attribute("base_url"),
            Some("https://custom-gemini.example.com")
        );
        assert_eq!(auths[0].attribute("header:Custom-Auth"), Some("secret"));
        assert_eq!(auths[0].attribute("auth_kind"), Some("apikey"));
        assert_eq!(auths[0].attribute("api_key"), None);
        assert_eq!(
            auths[1].attribute("base_url"),
            Some("https://custom-interactions.example.com")
        );
    }

    #[test]
    fn interactions_keys() {
        let auths = synth_config(&crate::config::Config {
            interactions_api_key: vec![GeminiKey {
                base_url: "https://interactions.example.com".to_owned(),
                proxy_url: "http://proxy.local:8080".to_owned(),
                prefix: "native".to_owned(),
                headers: header("X-Custom", "value"),
                ..gemini_key("interactions-key")
            }],
            ..crate::config::Config::default()
        });
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.provider, "gemini-interactions");
        assert_eq!(auth.label, "interactions-apikey");
        assert_eq!(auth.prefix, "native");
        assert_eq!(auth.proxy_url, "http://proxy.local:8080");
        assert_eq!(auth.attribute("api_key"), Some("interactions-key"));
        assert_eq!(
            auth.attribute("base_url"),
            Some("https://interactions.example.com")
        );
        assert_eq!(auth.attribute("header:X-Custom"), Some("value"));
        // Not upstream's: the ID and source names.
        let token = auth
            .id
            .strip_prefix("gemini-interactions:apikey:")
            .expect("interactions ID");
        assert_eq!(
            auth.attribute("source"),
            Some(format!("config:interactions[{token}]").as_str())
        );
    }

    #[test]
    fn xai_keys() {
        let auths = synth_config(&crate::config::Config {
            xai_api_key: vec![CodexKey {
                api_key: "xai-key-123".to_owned(),
                prefix: "grok".to_owned(),
                base_url: "https://api.x.ai/v1".to_owned(),
                proxy_url: "http://proxy.local".to_owned(),
                websockets: true,
                alpha_search: true,
                disable_cooling: Some(true),
                headers: header("X-Custom", "value"),
                models: vec![crate::config::CodexModel {
                    name: "grok-4.5".to_owned(),
                    alias: "grok-latest".to_owned(),
                    ..crate::config::CodexModel::default()
                }],
                ..CodexKey::default()
            }],
            ..crate::config::Config::default()
        });
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.provider, "xai");
        assert_eq!(auth.label, "xai-apikey");
        assert_eq!(auth.attribute("websockets"), Some("true"));
        assert_eq!(auth.attribute(ATTRIBUTE_CODEX_ALPHA_SEARCH), None);
        assert_eq!(auth.attribute("base_url"), Some("https://api.x.ai/v1"));
        assert_eq!(auth.attribute("header:X-Custom"), Some("value"));
        assert!(
            auth.attribute("models_hash")
                .is_some_and(|hash| !hash.is_empty())
        );
        assert_eq!(auth.proxy_url, "http://proxy.local");
        assert_eq!(auth.metadata["disable_cooling"], Value::Bool(true));
        // Not upstream's: the ID and source names.
        let token = auth.id.strip_prefix("xai:apikey:").expect("xai ID");
        assert_eq!(
            auth.attribute("source"),
            Some(format!("config:xai[{token}]").as_str())
        );
    }

    #[test]
    fn meta_keys() {
        let auths = synth_config(&crate::config::Config {
            meta_api_key: vec![CodexKey {
                api_key: "meta-secret".to_owned(),
                base_url: "https://api.meta.ai/v1".to_owned(),
                proxy_url: "http://proxy.local".to_owned(),
                disable_cooling: Some(true),
                models: vec![crate::config::CodexModel {
                    name: "muse-spark-1.3".to_owned(),
                    alias: "muse-spark-1.3".to_owned(),
                    ..crate::config::CodexModel::default()
                }],
                headers: header("X-Custom", "value"),
                ..CodexKey::default()
            }],
            ..crate::config::Config::default()
        });
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.provider, "meta");
        assert_eq!(auth.label, "meta-apikey");
        assert_eq!(auth.attribute("base_url"), Some("https://api.meta.ai/v1"));
        assert_eq!(auth.attribute("header:X-Custom"), Some("value"));
        assert_eq!(auth.proxy_url, "http://proxy.local");
        // Not upstream's: the ID prefix.
        assert!(auth.id.starts_with("meta:apikey:"));
    }

    #[test]
    fn xai_keys_allow_empty_api_key_with_base_url() {
        let auths = synth_config(&crate::config::Config {
            xai_api_key: vec![
                CodexKey {
                    base_url: "https://custom-xai.example.com".to_owned(),
                    headers: header("Custom-Auth", "secret"),
                    ..CodexKey::default()
                },
                CodexKey {
                    api_key: "   ".to_owned(),
                    base_url: "https://custom-xai-2.example.com".to_owned(),
                    ..CodexKey::default()
                },
            ],
            ..crate::config::Config::default()
        });
        assert_eq!(auths.len(), 2);
        assert_eq!(
            auths[0].attribute("base_url"),
            Some("https://custom-xai.example.com")
        );
        assert_eq!(auths[0].attribute("header:Custom-Auth"), Some("secret"));
        assert_eq!(auths[0].attribute("auth_kind"), Some("apikey"));
        assert_eq!(auths[0].attribute("api_key"), None);
    }

    #[test]
    fn gemini_ids_are_stable_and_weights_optional() {
        let entry = GeminiKey {
            prefix: "test".to_owned(),
            ..gemini_key("stable-key")
        };
        assert_eq!(
            gemini(vec![entry.clone()])[0].id,
            gemini(vec![entry.clone()])[0].id
        );
        // The ID covers the headers, as for claude.
        let mut with_header = entry.clone();
        with_header.headers = BTreeMap::from([("X".to_owned(), "1".to_owned())]);
        assert_ne!(gemini(vec![entry])[0].id, gemini(vec![with_header])[0].id);

        assert_eq!(
            gemini(vec![gemini_key("key")])[0].attribute(ATTRIBUTE_WEIGHT),
            None
        );
        let negative = GeminiKey {
            weight: Some(-5),
            ..gemini_key("key")
        };
        assert_eq!(
            gemini(vec![negative])[0].attribute(ATTRIBUTE_WEIGHT),
            Some("0")
        );
    }

    #[test]
    fn gemini_request_scoped_errors() {
        let rules = vec![RequestScopedErrorRule {
            status: 400,
            matches: vec!["maximum_context_length".into()],
            action: "stop".into(),
            ..RequestScopedErrorRule::default()
        }];
        let auths = synth_config(&crate::config::Config {
            gemini_api_key: vec![GeminiKey {
                request_scoped_errors: rules.clone(),
                ..gemini_key("gemini-key")
            }],
            interactions_api_key: vec![GeminiKey {
                request_scoped_errors: rules.clone(),
                ..gemini_key("interactions-key")
            }],
            xai_api_key: vec![CodexKey {
                api_key: "xai-key".to_owned(),
                base_url: "https://xai.api".to_owned(),
                request_scoped_errors: rules,
                ..CodexKey::default()
            }],
            ..crate::config::Config::default()
        });
        assert_eq!(auths.len(), 3);
        for auth in &auths {
            assert_eq!(
                auth.metadata["request_scoped_errors"],
                serde_json::json!([{"status": 400, "match": ["maximum_context_length"], "action": "stop"}])
            );
        }
    }

    #[test]
    fn debug_hides_the_key() {
        let entry = ApiKeyEntry {
            headers: BTreeMap::from([("X-Secret".to_owned(), "hidden-value".to_owned())]),
            base_url: "https://gateway.example/?key=sk-hidden".to_owned(),
            ..key("sk-hidden")
        };
        let text = format!("{entry:?}");
        assert!(!text.contains("sk-hidden"), "{text}");
        assert!(!text.contains("hidden-value"), "{text}");
        assert!(
            text.contains(r#""https://gateway.example/?<redacted>""#),
            "{text}"
        );
    }

    #[test]
    fn entries_come_from_the_config() {
        let config = crate::config::Config::parse(concat!(
            "claude-api-key:\n  - api-key: c\n    rebuild-mid-system-message: true\n",
            "    models:\n      - name: claude-opus\n        alias: opus\n",
            "codex-api-key:\n  - api-key: k\n    base-url: https://example.test\n    websockets: true\n    priority: 2\n",
        ))
        .unwrap();
        let claude = ApiKeyEntry::from(&config.claude_api_key[0]);
        assert_eq!(claude.api_key, "c");
        assert!(claude.rebuild_mid_system_message && !claude.websockets);
        assert_eq!(claude.models[0].alias, "opus");
        let codex = ApiKeyEntry::from(&config.codex_api_key[0]);
        assert!(codex.websockets && !codex.rebuild_mid_system_message);
        assert_eq!((codex.api_key.as_str(), codex.priority), ("k", 2));
        let config = crate::config::Config::parse(concat!(
            "gemini-api-key:\n  - api-key: g\n    weight: 3\n    request-retry: 1\n",
            "    models:\n      - name: gemini-2.5-pro\n        alias: pro\n        is-compat: true\n",
        ))
        .unwrap();
        let gemini = ApiKeyEntry::from(&config.gemini_api_key[0]);
        assert_eq!(
            (gemini.api_key.as_str(), gemini.weight, gemini.request_retry),
            ("g", Some(3), Some(1))
        );
        assert!(gemini.models[0].is_compat && !gemini.rebuild_mid_system_message);
    }
}
