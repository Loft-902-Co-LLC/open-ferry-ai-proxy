// Ported from CLIProxyAPI internal/watcher/synthesizer/context.go,
// helpers.go and config.go (Synthesize), ComputeExcludedModelsHash in
// internal/watcher/diff/model_hash.go and FormatSortedHeaders in
// internal/config/config_normalization.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Synthesizers: build [`Auth`] records from credential files
//! ([`file`](mod@file)), from API keys in the config ([`api_key`]), from
//! the config's OpenAI-compatible providers ([`openai_compat`]) and from its
//! Vertex AI keys ([`vertex`]). [`synthesize_config_auths`] makes every
//! config record.
//!
//! Records from files and API keys get the same settings as attributes:
//! excluded models (`excluded_models`, comma-joined and lowercased, and a
//! hash of them in `excluded_models_hash`), the credential's kind in
//! `auth_kind`, and extra request headers as `header:<name>`.
//!
//! Deviations from upstream:
//! - The context carries the one config setting the synthesizers here read,
//!   `oauth-excluded-models`, rather than the whole config, and the ID
//!   generator is passed on its own. A missing config, which upstream treats
//!   as "set no `auth_kind`", has no counterpart.
//! - Plugin auth parsers and the fingerprint-profile attribute aren't
//!   ported.

pub mod api_key;
pub mod file;
pub mod openai_compat;
pub mod vertex;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use super::classification::ATTRIBUTE_AUTH_KIND;
use super::{Auth, Timestamp};
use crate::config::Config;
use api_key::{ApiKeyEntry, ApiKeyProvider, api_key_auth, validate_api_key_weights};

/// What a synthesizer needs besides its input (upstream's
/// `SynthesisContext`).
#[derive(Clone, Debug)]
pub struct SynthesisContext {
    /// The auth directory; file credential IDs are relative to it.
    pub auth_dir: PathBuf,
    /// The time to stamp new records with.
    pub now: Timestamp,
    /// Models to hide from every OAuth credential of a provider, keyed by
    /// lowercase provider name (the config's `oauth-excluded-models`).
    pub oauth_excluded_models: BTreeMap<String, Vec<String>>,
}

impl SynthesisContext {
    /// A context over `auth_dir` at `now`, with no excluded models.
    pub fn new(auth_dir: impl Into<PathBuf>, now: Timestamp) -> Self {
        Self {
            auth_dir: auth_dir.into(),
            now,
            oauth_excluded_models: BTreeMap::new(),
        }
    }
}

/// A credential that can't be synthesized, such as one with an invalid
/// weight. The message never holds a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SynthesisError(String);

impl SynthesisError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for SynthesisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SynthesisError {}

/// Records for every API key and OpenAI-compatible provider in `config`
/// (upstream's `ConfigSynthesizer.Synthesize`): the Gemini keys, then the
/// interactions, Claude, Codex, xAI, Meta, OpenAI-compatible and Vertex
/// ones. Every weight is checked first, in upstream's order; an invalid one
/// fails the whole lot, naming the entry.
pub fn synthesize_config_auths(
    config: &Config,
    ctx: &SynthesisContext,
    ids: &mut StableIdGenerator,
) -> Result<Vec<Auth>, SynthesisError> {
    let gemini: Vec<ApiKeyEntry> = config.gemini_api_key.iter().map(Into::into).collect();
    let interactions: Vec<ApiKeyEntry> =
        config.interactions_api_key.iter().map(Into::into).collect();
    let claude: Vec<ApiKeyEntry> = config.claude_api_key.iter().map(Into::into).collect();
    let codex: Vec<ApiKeyEntry> = config.codex_api_key.iter().map(Into::into).collect();
    let xai: Vec<ApiKeyEntry> = config.xai_api_key.iter().map(Into::into).collect();
    let meta: Vec<ApiKeyEntry> = config.meta_api_key.iter().map(Into::into).collect();
    validate_api_key_weights(ApiKeyProvider::Gemini, &gemini)?;
    validate_api_key_weights(ApiKeyProvider::Interactions, &interactions)?;
    validate_api_key_weights(ApiKeyProvider::Claude, &claude)?;
    vertex::validate_vertex_weights(&config.vertex_api_key)?;
    validate_api_key_weights(ApiKeyProvider::Codex, &codex)?;
    validate_api_key_weights(ApiKeyProvider::Xai, &xai)?;
    validate_api_key_weights(ApiKeyProvider::Meta, &meta)?;
    openai_compat::validate_openai_compat_weights(&config.openai_compatibility)?;
    let mut out = Vec::new();
    for (provider, entries) in [
        (ApiKeyProvider::Gemini, &gemini),
        (ApiKeyProvider::Interactions, &interactions),
        (ApiKeyProvider::Claude, &claude),
        (ApiKeyProvider::Codex, &codex),
        (ApiKeyProvider::Xai, &xai),
        (ApiKeyProvider::Meta, &meta),
    ] {
        for (index, entry) in entries.iter().enumerate() {
            out.extend(api_key_auth(provider, index, entry, ctx, ids));
        }
    }
    out.extend(openai_compat::synthesize_openai_compat_auths(
        &config.openai_compatibility,
        ctx,
        ids,
    )?);
    out.extend(vertex::synthesize_vertex_auths(
        &config.vertex_api_key,
        ctx,
        ids,
    )?);
    Ok(out)
}

/// Makes IDs for config credentials that stay the same from one reload to
/// the next (upstream's `StableIDGenerator`). One generator should serve a
/// whole reload, so that identical entries get distinct IDs.
#[derive(Clone, Debug, Default)]
pub struct StableIdGenerator {
    counters: HashMap<String, usize>,
}

impl StableIdGenerator {
    /// A generator that has made no IDs yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The next ID for `kind` and the values that identify the entry,
    /// returned as the full ID (`<kind>:<short>`) and the short part: the
    /// first 12 hex digits of a SHA-256 over the kind and the trimmed parts,
    /// with `-1`, `-2` and so on added for repeats.
    ///
    /// The parts may include a secret; only its hash reaches the ID.
    pub fn next(&mut self, kind: &str, parts: &[&str]) -> (String, String) {
        let mut hasher = Sha256::new();
        hasher.update(kind.as_bytes());
        for part in parts {
            hasher.update([0]);
            hasher.update(part.trim().as_bytes());
        }
        let digest = hex(&hasher.finalize());
        let mut short: String = digest.chars().take(12).collect();
        let counter = self.counters.entry(format!("{kind}:{short}")).or_insert(0);
        let index = *counter;
        *counter += 1;
        if index > 0 {
            short = format!("{short}-{index}");
        }
        (format!("{kind}:{short}"), short)
    }
}

/// Sets `auth`'s excluded models and kind (upstream's
/// `ApplyAuthExcludedModelsMeta`). API keys use only their own list; other
/// credentials add the provider's `oauth-excluded-models`. Models are
/// trimmed, lowercased, deduplicated and sorted.
pub fn apply_auth_excluded_models_meta(
    auth: &mut Auth,
    oauth_excluded_models: &BTreeMap<String, Vec<String>>,
    per_key: &[String],
    auth_kind: &str,
) {
    let kind_key = open_ferry_translate::go::to_lower(auth_kind.trim());
    let mut seen = BTreeSet::new();
    let mut add = |list: &[String]| {
        for entry in list {
            let trimmed = entry.trim();
            if !trimmed.is_empty() {
                seen.insert(open_ferry_translate::go::to_lower(trimmed));
            }
        }
    };
    add(per_key);
    if kind_key != "apikey" {
        let provider = open_ferry_translate::go::to_lower(auth.provider.trim());
        if let Some(global) = oauth_excluded_models.get(&provider) {
            add(global);
        }
    }
    let combined: Vec<String> = seen.into_iter().collect();
    let hash = compute_excluded_models_hash(&combined);
    if !hash.is_empty() {
        auth.attributes
            .insert("excluded_models_hash".to_owned(), hash);
    }
    if !combined.is_empty() {
        auth.attributes
            .insert("excluded_models".to_owned(), combined.join(","));
    }
    if !auth_kind.is_empty() {
        auth.attributes
            .insert(ATTRIBUTE_AUTH_KIND.to_owned(), auth_kind.to_owned());
    }
}

/// A hash of an excluded-model list that ignores case, spacing and order:
/// the SHA-256, in hex, of the sorted list as a JSON array. Empty for an
/// empty list.
pub fn compute_excluded_models_hash(excluded: &[String]) -> String {
    let mut normalized: Vec<String> = excluded
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .map(open_ferry_translate::go::to_lower)
        .collect();
    if normalized.is_empty() {
        return String::new();
    }
    normalized.sort();
    let items: Vec<String> = normalized
        .iter()
        .map(|entry| open_ferry_translate::go::json_string(entry))
        .collect();
    sha256_hex(format!("[{}]", items.join(",")).as_bytes())
}

/// Headers as `name\0value\0` pairs in name order, for hashing.
pub fn format_sorted_headers(headers: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (name, value) in headers {
        out.push_str(name);
        out.push('\0');
        out.push_str(value);
        out.push('\0');
    }
    out
}

/// Sets a `header:<name>` attribute for each header with a non-empty name
/// and value, both trimmed.
pub fn add_config_headers_to_attrs(
    headers: &BTreeMap<String, String>,
    attrs: &mut BTreeMap<String, String>,
) {
    for (name, value) in headers {
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        attrs.insert(format!("header:{name}"), value.to_owned());
    }
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn stable_id_generator_next() {
        for (kind, parts) in [
            ("gemini:apikey", &["test-key", ""][..]),
            ("claude:apikey", &["sk-ant-xxx", "https://example.com"][..]),
            ("codex:apikey", &[][..]),
        ] {
            let (id, short) = StableIdGenerator::new().next(kind, parts);
            assert!(id.starts_with(&format!("{kind}:")), "{id}");
            assert_eq!(short.len(), 12);
            assert_eq!(id, format!("{kind}:{short}"));
        }
        // Computed with upstream's StableIDGenerator.Next in Go.
        assert_eq!(
            StableIdGenerator::new().next("codex:apikey", &[]).0,
            "codex:apikey:3afd3a83f071"
        );
    }

    #[test]
    fn stable_id_generator_is_stable_and_trims() {
        let (first, _) = StableIdGenerator::new().next("gemini:apikey", &["k", "https://a"]);
        let (second, _) = StableIdGenerator::new().next("gemini:apikey", &[" k ", "https://a "]);
        assert_eq!(first, second);
        let (other, _) = StableIdGenerator::new().next("gemini:apikey", &["k", "https://b"]);
        assert_ne!(first, other);
    }

    #[test]
    fn stable_id_generator_collision_handling() {
        let mut ids = StableIdGenerator::new();
        let (id1, short1) = ids.next("gemini:apikey", &["same-key"]);
        let (id2, short2) = ids.next("gemini:apikey", &["same-key"]);
        let (_, short3) = ids.next("gemini:apikey", &["same-key"]);
        assert_ne!(id1, id2);
        assert_eq!(short2, format!("{short1}-1"));
        assert_eq!(short3, format!("{short1}-2"));
    }

    #[test]
    fn apply_auth_excluded_models_meta_cases() {
        let none = BTreeMap::new();
        let mut api = Auth {
            provider: "gemini".into(),
            ..Auth::default()
        };
        apply_auth_excluded_models_meta(
            &mut api,
            &none,
            &strings(&["model-a", "MODEL-A", "model-b", "model-a"]),
            "apikey",
        );
        assert_eq!(api.attribute("excluded_models"), Some("model-a,model-b"));
        assert_eq!(
            api.attribute("excluded_models_hash"),
            Some(compute_excluded_models_hash(&strings(&["model-b", "model-a"])).as_str())
        );
        assert_eq!(api.attribute("auth_kind"), Some("apikey"));

        let global = BTreeMap::from([("claude".to_owned(), strings(&["claude-2.0"]))]);
        let mut oauth = Auth {
            provider: "claude".into(),
            ..Auth::default()
        };
        apply_auth_excluded_models_meta(&mut oauth, &global, &[], "oauth");
        assert_eq!(oauth.attribute("excluded_models"), Some("claude-2.0"));
        assert!(oauth.attribute("excluded_models_hash").is_some());
        assert_eq!(oauth.attribute("auth_kind"), Some("oauth"));

        // An API key ignores the provider-wide list.
        let mut api_claude = Auth {
            provider: "claude".into(),
            ..Auth::default()
        };
        apply_auth_excluded_models_meta(&mut api_claude, &global, &[], "apikey");
        assert_eq!(api_claude.attribute("excluded_models"), None);
        assert_eq!(api_claude.attribute("excluded_models_hash"), None);
        assert_eq!(api_claude.attribute("auth_kind"), Some("apikey"));

        let mut no_kind = Auth::default();
        apply_auth_excluded_models_meta(&mut no_kind, &none, &[], "");
        assert!(no_kind.attributes.is_empty());
    }

    #[test]
    fn oauth_merge_writes_combined_models() {
        let global = BTreeMap::from([("claude".to_owned(), strings(&["global-a", "shared"]))]);
        let mut auth = Auth {
            provider: "claude".into(),
            ..Auth::default()
        };
        apply_auth_excluded_models_meta(&mut auth, &global, &strings(&["per", "SHARED"]), "oauth");
        assert_eq!(
            auth.attribute("excluded_models"),
            Some("global-a,per,shared")
        );
        assert_eq!(
            auth.attribute("excluded_models_hash"),
            Some(compute_excluded_models_hash(&strings(&["global-a", "per", "shared"])).as_str())
        );
    }

    #[test]
    fn excluded_models_hash_matches_go() {
        // sha256 of `["a\u003cb","model-x"]`, Go's JSON for the sorted list.
        assert_eq!(
            compute_excluded_models_hash(&strings(&[" Model-X ", "a<b", " "])),
            sha256_hex(br#"["a\u003cb","model-x"]"#)
        );
        // Computed with upstream's ComputeExcludedModelsHash in Go.
        assert_eq!(
            compute_excluded_models_hash(&strings(&[" Model-X ", "a<b", " "])),
            "a3fe228ea726f463e2a956977e67130b7f94243a6b6ab103deff7d1414effbea"
        );
        assert_eq!(compute_excluded_models_hash(&strings(&[" ", ""])), "");
        assert_eq!(compute_excluded_models_hash(&[]), "");
    }

    #[test]
    fn add_config_headers_to_attrs_cases() {
        let mut attrs = BTreeMap::from([("existing".to_owned(), "key".to_owned())]);
        let headers = BTreeMap::from([
            ("Authorization".to_owned(), "Bearer token".to_owned()),
            ("X-Custom".to_owned(), "value".to_owned()),
        ]);
        add_config_headers_to_attrs(&headers, &mut attrs);
        assert_eq!(
            attrs,
            BTreeMap::from([
                ("existing".to_owned(), "key".to_owned()),
                ("header:Authorization".to_owned(), "Bearer token".to_owned()),
                ("header:X-Custom".to_owned(), "value".to_owned()),
            ])
        );

        let mut attrs = BTreeMap::new();
        let headers = BTreeMap::from([
            (String::new(), "value".to_owned()),
            ("key".to_owned(), String::new()),
            ("  ".to_owned(), "value".to_owned()),
            ("valid".to_owned(), "valid-value".to_owned()),
        ]);
        add_config_headers_to_attrs(&headers, &mut attrs);
        assert_eq!(
            attrs,
            BTreeMap::from([("header:valid".to_owned(), "valid-value".to_owned())])
        );
    }

    #[test]
    fn sorted_headers_format() {
        let headers = BTreeMap::from([
            ("b".to_owned(), "2".to_owned()),
            ("A".to_owned(), " 1".to_owned()),
        ]);
        assert_eq!(format_sorted_headers(&headers), "A\0 1\0b\x002\0");
        assert_eq!(format_sorted_headers(&BTreeMap::new()), "");
    }
}
