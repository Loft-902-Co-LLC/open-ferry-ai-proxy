// Ported from CLIProxyAPI sdk/cliproxy/auth/types.go (EnsureIndex,
// indexSeed and stableAuthIndex) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A credential's index: a short stable ID, the `auth_index` the management
//! API and usage records show, derived from what identifies the credential.
//!
//! A credential file is identified by its type and absolute path; an API key
//! by its provider, base URL and the key itself; anything else by its ID.
//! The seed is hashed, so the index never shows the key.
//!
//! Deviations from upstream: none.

use std::path::Path;

use sha2::{Digest, Sha256};

use super::Auth;
use super::classification::ATTRIBUTE_AUTH_INDEX_SEED;
use super::go::equal_fold;
use super::path::absolute;

impl Auth {
    /// The credential's index, derived and stored in [`Auth::index`] the
    /// first time. Empty when nothing identifies the credential.
    pub fn ensure_index(&mut self) -> &str {
        let existing = self.index.trim();
        if existing.len() != self.index.len() {
            self.index = existing.to_owned();
        }
        if self.index.is_empty() {
            self.index = stable_auth_index(&self.index_seed());
        }
        &self.index
    }

    /// What the index is derived from.
    fn index_seed(&self) -> String {
        let seed = self.trimmed_attribute(ATTRIBUTE_AUTH_INDEX_SEED);
        if !seed.is_empty() {
            return format!("{ATTRIBUTE_AUTH_INDEX_SEED}:{seed}");
        }

        let provider = open_ferry_translate::go::to_lower(self.provider.trim());
        let compat_name = self.trimmed_attribute("compat_name");
        let base_url = self.trimmed_attribute("base_url");
        let api_key = self.trimmed_attribute("api_key");
        let mut file_path = self.trimmed_attribute("path");
        if file_path.is_empty() {
            file_path = self.trimmed_attribute("source");
        }
        if file_path.is_empty() {
            file_path = self.file_name.trim();
        }
        if file_path.is_empty() {
            file_path = self.id.trim();
        }

        if !file_path.is_empty() && open_ferry_translate::go::to_lower(file_path).ends_with(".json")
        {
            let file_path = absolute(Path::new(file_path));
            let mut auth_type = self.trimmed_metadata_str("type");
            if auth_type.is_empty() {
                auth_type = &provider;
            }
            let auth_type = open_ferry_translate::go::to_lower(auth_type.trim());
            if !auth_type.is_empty() {
                return format!("{auth_type}:{}", file_path.display());
            }
        }

        if !api_key.is_empty() {
            let api_prefix =
                if !compat_name.is_empty() || equal_fold(&provider, "openai-compatibility") {
                    Some("openai-compatibility")
                } else {
                    [
                        ("gemini", "gemini-api-key"),
                        ("gemini-interactions", "interactions-api-key"),
                        ("codex", "codex-api-key"),
                        ("xai", "xai-api-key"),
                        ("claude", "claude-api-key"),
                        ("meta", "meta-api-key"),
                    ]
                    .into_iter()
                    .find(|(name, _)| equal_fold(&provider, name))
                    .map(|(_, prefix)| prefix)
                };
            if let Some(prefix) = api_prefix {
                return format!("{prefix}:{base_url}+{api_key}");
            }
        }

        let id = self.id.trim();
        if id.is_empty() {
            String::new()
        } else {
            format!("id:{id}")
        }
    }
}

/// The first 8 bytes of the seed's SHA-256, in hex; empty for an empty seed.
fn stable_auth_index(seed: &str) -> String {
    let seed = seed.trim();
    if seed.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(seed.as_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_attrs(provider: &str, id: &str, attrs: &[(&str, &str)]) -> Auth {
        Auth {
            provider: provider.into(),
            id: id.into(),
            attributes: attrs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..Auth::default()
        }
    }

    #[test]
    fn ensure_index_uses_credential_identity() {
        let mut gemini = with_attrs(
            "gemini",
            "",
            &[
                ("api_key", "shared-key"),
                ("source", "config:gemini[abc123]"),
            ],
        );
        let mut compat = with_attrs(
            "bohe",
            "",
            &[
                ("api_key", "shared-key"),
                ("compat_name", "bohe"),
                ("provider_key", "bohe"),
                ("source", "config:bohe[def456]"),
            ],
        );
        let mut alt_base = with_attrs(
            "gemini",
            "",
            &[
                ("api_key", "shared-key"),
                ("base_url", "https://alt.example.com"),
                ("source", "config:gemini[ghi789]"),
            ],
        );
        let mut duplicate = with_attrs(
            "gemini",
            "",
            &[
                ("api_key", "shared-key"),
                ("source", "config:gemini[abc123-1]"),
            ],
        );
        let gemini = gemini.ensure_index().to_owned();
        let compat = compat.ensure_index().to_owned();
        let alt_base = alt_base.ensure_index().to_owned();
        let duplicate = duplicate.ensure_index().to_owned();
        for index in [&gemini, &compat, &alt_base, &duplicate] {
            assert_eq!(index.len(), 16, "{index}");
        }
        assert_ne!(gemini, compat, "a shared key must not share an index");
        assert_ne!(
            gemini, alt_base,
            "a different base URL must change the index"
        );
        assert_eq!(gemini, duplicate, "the source must not change the index");

        let mut meta1 = with_attrs(
            "meta",
            "meta:apikey:token1",
            &[
                ("api_key", "meta-secret"),
                ("base_url", "https://api.meta.ai/v1"),
                ("proxy_url", "socks5://127.0.0.1:1080"),
                ("prefix", "fast-"),
                ("source", "config:meta[token1]"),
            ],
        );
        let mut meta2 = with_attrs(
            "meta",
            "meta:apikey:token2",
            &[
                ("api_key", "meta-secret"),
                ("base_url", "https://api.meta.ai/v1"),
                ("proxy_url", ""),
                ("prefix", ""),
                ("source", "config:meta[token2]"),
            ],
        );
        let meta1 = meta1.ensure_index().to_owned();
        assert!(!meta1.is_empty());
        assert_eq!(
            meta1,
            meta2.ensure_index(),
            "proxy and prefix must not matter"
        );
    }

    #[test]
    fn ensure_index_uses_oauth_type_and_absolute_path() {
        let cwd = std::env::current_dir().unwrap();
        let expected = stable_auth_index(&format!(
            "antigravity:{}",
            super::super::path::clean(&cwd.join("test-oauth.json")).display()
        ));
        let mut auth = with_attrs("antigravity", "", &[("path", "test-oauth.json")]);
        auth.metadata
            .insert("type".into(), serde_json::Value::from("antigravity"));
        assert_eq!(auth.ensure_index(), expected);
    }

    #[test]
    fn index_seeds() {
        let seed = |auth: Auth| auth.index_seed();
        assert_eq!(
            seed(with_attrs("codex", "x", &[("auth_index_seed", " fixed ")])),
            "auth_index_seed:fixed"
        );
        assert_eq!(
            seed(with_attrs(
                "Codex",
                "",
                &[("api_key", " k "), ("base_url", " https://b ")]
            )),
            "codex-api-key:https://b+k"
        );
        assert_eq!(
            seed(with_attrs("claude", "", &[("api_key", "k")])),
            "claude-api-key:+k"
        );
        assert_eq!(
            seed(with_attrs("unknown", " some-id ", &[("api_key", "k")])),
            "id:some-id"
        );
        // A `.json` path without a type or provider falls through.
        assert_eq!(seed(with_attrs("", "a.json", &[])), "id:a.json");
        assert_eq!(seed(Auth::default()), "");
    }

    #[test]
    fn existing_index_is_kept_trimmed() {
        let mut auth = with_attrs("codex", "x", &[]);
        auth.index = " abc ".into();
        assert_eq!(auth.ensure_index(), "abc");
        assert_eq!(auth.index, "abc");
        let mut nothing = Auth::default();
        assert_eq!(nothing.ensure_index(), "");
    }

    #[test]
    fn index_matches_go() {
        // Computed with Go's crypto/sha256 as stableAuthIndex does.
        assert_eq!(stable_auth_index(" id:x "), "d9af79a0e7d5d36d");
        assert_eq!(stable_auth_index("  "), "");
    }
}
