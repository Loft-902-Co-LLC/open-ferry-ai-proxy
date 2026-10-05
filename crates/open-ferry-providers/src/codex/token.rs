// Ported from CLIProxyAPI internal/auth/codex/token.go, filename.go and
// openai.go, and CreateTokenStorage and UpdateTokenStorage in
// openai_auth.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Codex tokens, and the credential file that keeps them.
//!
//! A credential's [`Auth::metadata`](open_ferry_core::auth::Auth::metadata)
//! is the file's JSON: [`TokenStorage::to_json`] writes it, with upstream's
//! field names and `type: "codex"`.
//!
//! Deviations from upstream:
//! - Saving is the credential store's job; this module only builds the JSON.
//! - Plan types are split into words at characters that aren't alphanumeric
//!   by Rust's Unicode tables, where Go uses its letter and digit tables;
//!   they differ only outside ASCII.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{Local, SecondsFormat};
use serde_json::{Map, Value};

use super::jwt::DEFAULT_PLAN_TYPE;

/// The `type` of a Codex credential file.
pub const CREDENTIAL_TYPE: &str = "codex";

/// Tokens from a code exchange or a refresh (upstream's `CodexTokenData`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TokenData {
    /// The OpenID Connect ID token.
    pub id_token: String,
    /// The bearer token for API calls.
    pub access_token: String,
    /// The token that gets new ones.
    pub refresh_token: String,
    /// The ChatGPT account ID from the ID token.
    pub account_id: String,
    /// The account's email from the ID token.
    pub email: String,
    /// When the access token expires, in RFC 3339.
    pub expire: String,
    /// The ChatGPT plan, such as `plus`.
    pub plan_type: String,
}

impl fmt::Debug for TokenData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenData")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("expire", &self.expire)
            .field("plan_type", &self.plan_type)
            .finish_non_exhaustive()
    }
}

/// A login's result (upstream's `CodexAuthBundle`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthBundle {
    /// An API key, when the login gave one. Codex logins don't.
    pub api_key: String,
    /// The tokens.
    pub token_data: TokenData,
    /// When the tokens were fetched, in RFC 3339.
    pub last_refresh: String,
}

/// A Codex credential file (upstream's `CodexTokenStorage`).
#[derive(Clone, Default, PartialEq)]
pub struct TokenStorage {
    /// `id_token`.
    pub id_token: String,
    /// `access_token`.
    pub access_token: String,
    /// `refresh_token`.
    pub refresh_token: String,
    /// `account_id`.
    pub account_id: String,
    /// `last_refresh`, in RFC 3339.
    pub last_refresh: String,
    /// `email`.
    pub email: String,
    /// `expired`: when the access token expires, in RFC 3339.
    pub expire: String,
    /// `plan_type`, left out when empty.
    pub plan_type: String,
    /// Other keys for the file, which win over the fields above.
    pub metadata: Map<String, Value>,
}

impl fmt::Debug for TokenStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenStorage")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("expire", &self.expire)
            .field("plan_type", &self.plan_type)
            .field("metadata", &self.metadata.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl TokenStorage {
    /// The file's JSON: the fields, with `type` set to `codex`, and then the
    /// metadata over them (upstream's `SaveTokenToFile` and
    /// `MergeMetadata`). Keys are sorted, as Go writes a map.
    pub fn to_json(&self) -> Map<String, Value> {
        let mut data = BTreeMap::new();
        let fields = [
            ("id_token", &self.id_token),
            ("access_token", &self.access_token),
            ("refresh_token", &self.refresh_token),
            ("account_id", &self.account_id),
            ("last_refresh", &self.last_refresh),
            ("email", &self.email),
            ("expired", &self.expire),
        ];
        for (key, value) in fields {
            data.insert(key.to_owned(), Value::from(value.as_str()));
        }
        data.insert("type".to_owned(), Value::from(CREDENTIAL_TYPE));
        if !self.plan_type.is_empty() {
            data.insert("plan_type".to_owned(), Value::from(self.plan_type.as_str()));
        }
        for (key, value) in &self.metadata {
            data.insert(key.clone(), value.clone());
        }
        data.into_iter().collect()
    }
}

/// The current local time in RFC 3339, as Go's
/// `time.Now().Format(time.RFC3339)` writes it.
pub(crate) fn now_rfc3339() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `plan_type` trimmed, or [`DEFAULT_PLAN_TYPE`] when blank.
fn plan_or_default(plan_type: &str) -> String {
    match plan_type.trim() {
        "" => DEFAULT_PLAN_TYPE.to_owned(),
        plan => plan.to_owned(),
    }
}

/// The credential file for a login's tokens (`CreateTokenStorage`).
pub fn create_token_storage(bundle: &AuthBundle) -> TokenStorage {
    let data = &bundle.token_data;
    TokenStorage {
        id_token: data.id_token.clone(),
        access_token: data.access_token.clone(),
        refresh_token: data.refresh_token.clone(),
        account_id: data.account_id.clone(),
        last_refresh: bundle.last_refresh.clone(),
        email: data.email.clone(),
        expire: data.expire.clone(),
        plan_type: plan_or_default(&data.plan_type),
        metadata: Map::new(),
    }
}

/// Puts refreshed tokens in `storage` (`UpdateTokenStorage`).
pub fn update_token_storage(storage: &mut TokenStorage, data: &TokenData) {
    storage.id_token.clone_from(&data.id_token);
    storage.access_token.clone_from(&data.access_token);
    storage.refresh_token.clone_from(&data.refresh_token);
    storage.account_id.clone_from(&data.account_id);
    storage.last_refresh = now_rfc3339();
    storage.email.clone_from(&data.email);
    storage.expire.clone_from(&data.expire);
    storage.plan_type = plan_or_default(&data.plan_type);
}

/// A credential's file name: `codex-<hash>-<email>-<plan>.json`, leaving out
/// the parts that are empty (`CredentialFileName`). `hash_account_id` is the
/// start of the account ID's SHA-256, which tells apart accounts that share
/// an email.
pub fn credential_file_name(
    email: &str,
    plan_type: &str,
    hash_account_id: &str,
    include_provider_prefix: bool,
) -> String {
    let email = email.trim();
    let plan = normalize_plan_type_for_filename(plan_type);
    let hash = hash_account_id.trim();
    let prefix = if include_provider_prefix { "codex" } else { "" };
    match (hash.is_empty(), plan.is_empty()) {
        (false, true) => format!("{prefix}-{hash}-{email}.json"),
        (false, false) => format!("{prefix}-{hash}-{email}-{plan}.json"),
        (true, true) => format!("{prefix}-{email}.json"),
        (true, false) => format!("{prefix}-{email}-{plan}.json"),
    }
}

/// The plan's words, lowercased and joined with `-`.
fn normalize_plan_type_for_filename(plan_type: &str) -> String {
    plan_type
        .trim()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| open_ferry_translate::go::to_lower(part.trim()))
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credential_file_names() {
        let cases = [
            (
                "user@example.com",
                "team",
                "abc12345",
                "codex-abc12345-user@example.com-team.json",
            ),
            (
                "user@example.com",
                "k12",
                "def67890",
                "codex-def67890-user@example.com-k12.json",
            ),
            (
                "user@example.com",
                "k12",
                "",
                "codex-user@example.com-k12.json",
            ),
            (
                " user@example.com ",
                "Plus",
                " abc12345 ",
                "codex-abc12345-user@example.com-plus.json",
            ),
            (
                "user@example.com",
                "plus",
                "",
                "codex-user@example.com-plus.json",
            ),
            (
                " user@example.com",
                " Team Plan ",
                "abc12345",
                "codex-abc12345-user@example.com-team-plan.json",
            ),
            (
                "user@example.com",
                "",
                "abc12345",
                "codex-abc12345-user@example.com.json",
            ),
            ("user@example.com", "", "", "codex-user@example.com.json"),
        ];
        for (email, plan, hash, want) in cases {
            assert_eq!(credential_file_name(email, plan, hash, true), want);
        }
        assert_eq!(credential_file_name("a@b.c", "", "", false), "-a@b.c.json");
    }

    #[test]
    fn json_keeps_custom_metadata() {
        let mut storage = TokenStorage {
            email: "user@example.com".into(),
            access_token: "new-access-token".into(),
            refresh_token: "new-refresh-token".into(),
            id_token: "new-id-token".into(),
            account_id: "new-account".into(),
            expire: "2026-12-31T23:59:59Z".into(),
            last_refresh: "2026-04-14T12:00:00Z".into(),
            ..TokenStorage::default()
        };
        storage.metadata = json!({
            "disabled": false,
            "prefix": "my-prefix",
            "websockets": false,
            "note": "my important note",
            "proxy_url": "http://proxy:8080",
            "weight": 42,
        })
        .as_object()
        .unwrap()
        .clone();
        let saved = Value::Object(storage.to_json());
        assert_eq!(saved["access_token"], "new-access-token");
        assert_eq!(saved["refresh_token"], "new-refresh-token");
        assert_eq!(saved["id_token"], "new-id-token");
        assert_eq!(saved["account_id"], "new-account");
        assert_eq!(saved["expired"], "2026-12-31T23:59:59Z");
        assert_eq!(saved["type"], "codex");
        assert_eq!(saved["prefix"], "my-prefix");
        assert_eq!(saved["websockets"], false);
        assert_eq!(saved["note"], "my important note");
        assert_eq!(saved["proxy_url"], "http://proxy:8080");
        assert_eq!(saved["weight"], 42);
        // An empty plan is left out, and keys come sorted.
        assert!(saved.get("plan_type").is_none());
        let keys: Vec<_> = saved.as_object().unwrap().keys().cloned().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn create_and_update_token_storage_plan_type() {
        let bundle = AuthBundle {
            token_data: TokenData {
                id_token: "id-tok".into(),
                access_token: "acc-tok".into(),
                ..TokenData::default()
            },
            ..AuthBundle::default()
        };
        let mut storage = create_token_storage(&bundle);
        assert_eq!(storage.plan_type, "free");

        let update = |plan: &str| TokenData {
            id_token: "id-tok-2".into(),
            access_token: "acc-tok-2".into(),
            plan_type: plan.into(),
            ..TokenData::default()
        };
        update_token_storage(&mut storage, &update("team"));
        assert_eq!(storage.plan_type, "team");
        assert_eq!(storage.access_token, "acc-tok-2");
        assert!(!storage.last_refresh.is_empty());
        update_token_storage(&mut storage, &update(""));
        assert_eq!(storage.plan_type, "free");
    }

    #[test]
    fn debug_leaves_out_tokens() {
        let data = TokenData {
            access_token: "secret-a".into(),
            refresh_token: "secret-r".into(),
            id_token: "secret-i".into(),
            ..TokenData::default()
        };
        assert!(!format!("{data:?}").contains("secret"));
        let storage = TokenStorage {
            access_token: "secret-a".into(),
            ..TokenStorage::default()
        };
        assert!(!format!("{storage:?}").contains("secret"));
    }
}
