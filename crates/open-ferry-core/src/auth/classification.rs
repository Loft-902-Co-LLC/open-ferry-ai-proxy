// Ported from CLIProxyAPI sdk/cliproxy/auth/classification.go, and
// AccountInfo and ProxyInfo in sdk/cliproxy/auth/types.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What kind of credential an [`Auth`] is, where it came from, and the
//! attribute names that say so.
//!
//! Deviations from upstream:
//! - Kinds and sources are enums rather than strings; the strings are the
//!   constants here.
//! - `codex_disable_cloaking` isn't defined; the project doesn't port
//!   cloaking.

use super::Auth;
use super::go::equal_fold;

/// The kind of a credential made from an API key.
pub const AUTH_KIND_API_KEY: &str = "apikey";
/// The kind of a credential from an OAuth login.
pub const AUTH_KIND_OAUTH: &str = "oauth";

/// A credential from the config file.
pub const AUTH_SOURCE_CONFIG: &str = "config";
/// A credential from a file in the auth directory.
pub const AUTH_SOURCE_FILE: &str = "file";
/// A credential from a git-backed store.
pub const AUTH_SOURCE_GIT: &str = "git";
/// A credential that lives only in memory.
pub const AUTH_SOURCE_MEMORY: &str = "memory";
/// A credential from an object store.
pub const AUTH_SOURCE_OBJECT_STORE: &str = "objectstore";
/// A credential from a Postgres store.
pub const AUTH_SOURCE_POSTGRES: &str = "postgres";

/// Attribute: the API key.
pub const ATTRIBUTE_API_KEY: &str = "api_key";
/// Attribute: the credential's kind, `apikey` or `oauth`.
pub const ATTRIBUTE_AUTH_KIND: &str = "auth_kind";
/// Attribute: `"true"` when a codex API key may use alpha search.
pub const ATTRIBUTE_CODEX_ALPHA_SEARCH: &str = "codex_alpha_search";
/// Attribute: the credential's position in its config list.
pub const ATTRIBUTE_CONFIG_INDEX: &str = "config_index";
/// Attribute: the credential's file.
pub const ATTRIBUTE_PATH: &str = "path";
/// Attribute: `"true"` when the credential lives only in memory.
pub const ATTRIBUTE_RUNTIME_ONLY: &str = "runtime_only";
/// Attribute: where the credential came from, a path or `config:<name>[<id>]`.
pub const ATTRIBUTE_SOURCE: &str = "source";
/// Attribute: the store behind the credential, such as `file`.
pub const ATTRIBUTE_SOURCE_BACKEND: &str = "source_backend";
/// Attribute and metadata key: the credential's routing weight.
pub const ATTRIBUTE_WEIGHT: &str = "weight";
/// Attribute: overrides what the credential's index is derived from.
pub const ATTRIBUTE_AUTH_INDEX_SEED: &str = "auth_index_seed";
/// Attribute: the credential's model aliases, as JSON.
pub const ATTRIBUTE_MODEL_ALIASES: &str = "model_aliases";

/// How a credential authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthKind {
    /// With an API key.
    ApiKey,
    /// With OAuth tokens.
    OAuth,
}

impl AuthKind {
    /// Upstream's name for the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => AUTH_KIND_API_KEY,
            Self::OAuth => AUTH_KIND_OAUTH,
        }
    }

    fn parse(kind: &str) -> Option<Self> {
        match open_ferry_translate::go::to_lower(kind.trim()).as_str() {
            "apikey" | "api_key" | "api-key" => Some(Self::ApiKey),
            "oauth" | "oauth2" => Some(Self::OAuth),
            _ => None,
        }
    }
}

/// Where a credential came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuthSource {
    /// The config file.
    Config,
    /// A file in the auth directory.
    File,
    /// A git-backed store.
    Git,
    /// Memory only.
    Memory,
    /// An object store.
    ObjectStore,
    /// A Postgres store.
    Postgres,
}

impl AuthSource {
    /// Upstream's name for the source.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => AUTH_SOURCE_CONFIG,
            Self::File => AUTH_SOURCE_FILE,
            Self::Git => AUTH_SOURCE_GIT,
            Self::Memory => AUTH_SOURCE_MEMORY,
            Self::ObjectStore => AUTH_SOURCE_OBJECT_STORE,
            Self::Postgres => AUTH_SOURCE_POSTGRES,
        }
    }

    fn parse(source: &str) -> Option<Self> {
        match open_ferry_translate::go::to_lower(source.trim()).as_str() {
            "config" => Some(Self::Config),
            "file" | "filesystem" => Some(Self::File),
            "git" => Some(Self::Git),
            "memory" | "runtime" | "runtime_only" => Some(Self::Memory),
            "objectstore" | "object-store" => Some(Self::ObjectStore),
            "postgres" | "postgresql" | "database" | "db" => Some(Self::Postgres),
            _ => None,
        }
    }
}

impl Auth {
    /// The credential's kind: from its `auth_kind` attribute or metadata,
    /// else an API key if it has one, else OAuth if its metadata holds
    /// token fields.
    pub fn auth_kind(&self) -> Option<AuthKind> {
        if let Some(kind) = AuthKind::parse(self.trimmed_attribute(ATTRIBUTE_AUTH_KIND)) {
            return Some(kind);
        }
        if let Some(kind) = AuthKind::parse(self.trimmed_metadata_str(ATTRIBUTE_AUTH_KIND)) {
            return Some(kind);
        }
        if !self.trimmed_attribute(ATTRIBUTE_API_KEY).is_empty() {
            return Some(AuthKind::ApiKey);
        }
        self.has_oauth_metadata().then_some(AuthKind::OAuth)
    }

    /// Where the credential came from, as its attributes say.
    pub fn auth_source_kind(&self) -> Option<AuthSource> {
        if equal_fold(self.trimmed_attribute(ATTRIBUTE_RUNTIME_ONLY), "true") {
            return Some(AuthSource::Memory);
        }
        if let Some(source) = AuthSource::parse(self.trimmed_attribute(ATTRIBUTE_SOURCE_BACKEND)) {
            return Some(source);
        }
        let source = self.trimmed_attribute(ATTRIBUTE_SOURCE);
        if !source.is_empty() {
            if open_ferry_translate::go::to_lower(source).starts_with("config:") {
                return Some(AuthSource::Config);
            }
            return Some(AuthSource::parse(source).unwrap_or(AuthSource::File));
        }
        if !self.trimmed_attribute(ATTRIBUTE_PATH).is_empty() || !self.file_name.trim().is_empty() {
            return Some(AuthSource::File);
        }
        None
    }

    /// The account behind the credential, for display: `("oauth", email)`
    /// or `("api_key", key)`, the value empty when unknown.
    ///
    /// For an API key the value is the key itself, a secret: never log it.
    pub fn account_info(&self) -> Option<(&'static str, &str)> {
        match self.auth_kind()? {
            AuthKind::OAuth => Some(("oauth", self.trimmed_metadata_str("email"))),
            AuthKind::ApiKey => Some(("api_key", self.trimmed_attribute(ATTRIBUTE_API_KEY))),
        }
    }

    /// The credential's proxy, described for logs without its address:
    /// `via <scheme> proxy`, `via proxy`, or empty for none.
    pub fn proxy_info(&self) -> String {
        let proxy = self.proxy_url.trim();
        if proxy.is_empty() {
            return String::new();
        }
        match proxy.find("://") {
            Some(index) if index > 0 => format!("via {} proxy", &proxy[..index]),
            _ => "via proxy".to_owned(),
        }
    }

    /// The attribute at `key`, trimmed, or empty.
    pub(crate) fn trimmed_attribute(&self, key: &str) -> &str {
        self.attributes.get(key).map_or("", |value| value.trim())
    }

    /// The metadata string at `key`, trimmed, or empty.
    pub(crate) fn trimmed_metadata_str(&self, key: &str) -> &str {
        self.metadata_str(key).map_or("", str::trim)
    }

    fn has_oauth_metadata(&self) -> bool {
        let token_field = [
            "access_token",
            "refresh_token",
            "id_token",
            "email",
            "token_type",
            "expires_at",
            "expired",
        ]
        .into_iter()
        .any(|key| !self.trimmed_metadata_str(key).is_empty());
        token_field
            || self
                .metadata
                .get("token")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|token| !token.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn attrs(pairs: &[(&str, &str)]) -> Auth {
        Auth {
            attributes: pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..Auth::default()
        }
    }

    fn meta(value: Value) -> Auth {
        Auth {
            metadata: value.as_object().cloned().unwrap(),
            ..Auth::default()
        }
    }

    #[test]
    fn auth_kind() {
        let cases = [
            (
                attrs(&[(ATTRIBUTE_AUTH_KIND, "api_key")]),
                Some(AuthKind::ApiKey),
            ),
            (
                attrs(&[(ATTRIBUTE_AUTH_KIND, "oauth"), (ATTRIBUTE_API_KEY, "k")]),
                Some(AuthKind::OAuth),
            ),
            (meta(json!({"auth_kind": "oauth"})), Some(AuthKind::OAuth)),
            (attrs(&[(ATTRIBUTE_API_KEY, "k")]), Some(AuthKind::ApiKey)),
            (
                meta(json!({"access_token": "token"})),
                Some(AuthKind::OAuth),
            ),
            (meta(json!({"token": {"a": 1}})), Some(AuthKind::OAuth)),
            (meta(json!({"token": {}})), None),
            (meta(json!({"type": "test"})), None),
        ];
        for (auth, want) in cases {
            assert_eq!(auth.auth_kind(), want, "{auth:?}");
        }
    }

    #[test]
    fn auth_source_kind() {
        let cases = [
            (
                attrs(&[
                    (ATTRIBUTE_RUNTIME_ONLY, "true"),
                    (ATTRIBUTE_SOURCE_BACKEND, AUTH_SOURCE_POSTGRES),
                ]),
                Some(AuthSource::Memory),
            ),
            (
                attrs(&[
                    (ATTRIBUTE_SOURCE_BACKEND, "postgresql"),
                    (ATTRIBUTE_PATH, "/tmp/auth.json"),
                ]),
                Some(AuthSource::Postgres),
            ),
            (
                attrs(&[
                    (ATTRIBUTE_SOURCE_BACKEND, "object-store"),
                    (ATTRIBUTE_PATH, "/tmp/auth.json"),
                ]),
                Some(AuthSource::ObjectStore),
            ),
            (
                attrs(&[(ATTRIBUTE_SOURCE, "config:codex[abc]")]),
                Some(AuthSource::Config),
            ),
            (
                attrs(&[(ATTRIBUTE_SOURCE, "/tmp/auth.json")]),
                Some(AuthSource::File),
            ),
            (
                attrs(&[(ATTRIBUTE_PATH, "/tmp/auth.json")]),
                Some(AuthSource::File),
            ),
            (
                Auth {
                    file_name: "codex.json".into(),
                    ..Auth::default()
                },
                Some(AuthSource::File),
            ),
            (Auth::default(), None),
        ];
        for (auth, want) in cases {
            assert_eq!(auth.auth_source_kind(), want, "{auth:?}");
        }
    }

    #[test]
    fn account_info_uses_auth_kind() {
        let api_key = attrs(&[(ATTRIBUTE_AUTH_KIND, "api-key"), (ATTRIBUTE_API_KEY, "k")]);
        assert_eq!(api_key.account_info(), Some(("api_key", "k")));

        let mut oauth = attrs(&[
            (ATTRIBUTE_AUTH_KIND, AUTH_KIND_OAUTH),
            (ATTRIBUTE_API_KEY, "k"),
        ]);
        oauth
            .metadata
            .insert("email".into(), Value::from("user@example.com"));
        assert_eq!(oauth.account_info(), Some(("oauth", "user@example.com")));

        let no_email = meta(json!({"access_token": "token"}));
        assert_eq!(no_email.account_info(), Some(("oauth", "")));
        assert_eq!(Auth::default().account_info(), None);
    }

    #[test]
    fn proxy_info_hides_the_address() {
        let with = |url: &str| Auth {
            proxy_url: url.into(),
            ..Auth::default()
        };
        assert_eq!(with("").proxy_info(), "");
        assert_eq!(
            with(" socks5://u:p@host:1080 ").proxy_info(),
            "via socks5 proxy"
        );
        assert_eq!(with("host:8080").proxy_info(), "via proxy");
        assert_eq!(with("://host").proxy_info(), "via proxy");
    }
}
