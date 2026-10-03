// Ported from CLIProxyAPI internal/auth/claude/token.go, anthropic.go and
// filename.go, and CreateTokenStorage and UpdateTokenStorage in
// anthropic_auth.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude tokens, the credential file that keeps them, and its name.
//!
//! A credential's [`Auth::metadata`] is the file's JSON:
//! [`TokenStorage::to_json`] writes it, with upstream's field names and
//! `type: "claude"`. Keys this module doesn't know, such as
//! `claude_device_ids` in files upstream wrote, stay in the metadata as they
//! are and are never read.
//!
//! Deviations from upstream:
//! - Saving is the credential store's job; this module only builds the JSON.
//! - There is no `claude_device_ids` field and no device ID pool: logins
//!   don't make one (see [`super::oauth`]). A file that has one keeps it
//!   untouched, through the metadata.
//! - [`find_matching_legacy_credential`] takes an [`AuthStore`], whose
//!   `list` has no context; a store error other than "not found" comes back
//!   as an [`io::Error`] with upstream's message.

use std::collections::BTreeMap;
use std::fmt;
use std::io;

use chrono::{Local, SecondsFormat, TimeDelta};
use open_ferry_core::auth::{Auth, AuthStore};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::json::eq_fold;

/// The `type` of a Claude credential file.
pub const CREDENTIAL_TYPE: &str = "claude";

/// Tokens from a code exchange or a refresh (upstream's `ClaudeTokenData`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TokenData {
    /// The bearer token for API calls.
    pub access_token: String,
    /// The token that gets new ones.
    pub refresh_token: String,
    /// The account's email.
    pub email: String,
    /// The account's UUID.
    pub account_uuid: String,
    /// The organization's UUID.
    pub organization_uuid: String,
    /// The organization's name.
    pub organization_name: String,
    /// When the access token expires, in RFC 3339.
    pub expire: String,
}

impl fmt::Debug for TokenData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenData")
            .field("email", &self.email)
            .field("account_uuid", &self.account_uuid)
            .field("organization_uuid", &self.organization_uuid)
            .field("organization_name", &self.organization_name)
            .field("expire", &self.expire)
            .finish_non_exhaustive()
    }
}

/// A login's result (upstream's `ClaudeAuthBundle`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthBundle {
    /// An API key, when the login gave one. Claude logins don't.
    pub api_key: String,
    /// The tokens.
    pub token_data: TokenData,
    /// When the tokens were fetched, in RFC 3339.
    pub last_refresh: String,
}

/// A Claude credential file (upstream's `ClaudeTokenStorage`).
#[derive(Clone, Default, PartialEq)]
pub struct TokenStorage {
    /// `id_token`. Claude logins leave it empty.
    pub id_token: String,
    /// `access_token`.
    pub access_token: String,
    /// `refresh_token`.
    pub refresh_token: String,
    /// `last_refresh`, in RFC 3339.
    pub last_refresh: String,
    /// `email`.
    pub email: String,
    /// `account_uuid`, left out when empty.
    pub account_uuid: String,
    /// `organization_uuid`, left out when empty.
    pub organization_uuid: String,
    /// `organization_name`, left out when empty.
    pub organization_name: String,
    /// `expired`: when the access token expires, in RFC 3339.
    pub expire: String,
    /// Other keys for the file, which win over the fields above.
    pub metadata: Map<String, Value>,
}

impl fmt::Debug for TokenStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenStorage")
            .field("email", &self.email)
            .field("account_uuid", &self.account_uuid)
            .field("organization_uuid", &self.organization_uuid)
            .field("organization_name", &self.organization_name)
            .field("expire", &self.expire)
            .field("metadata", &self.metadata.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl TokenStorage {
    /// The file's JSON: the fields, with `type` set to `claude`, and then the
    /// metadata over them (upstream's `SaveTokenToFile` and
    /// `MergeMetadata`). Keys are sorted, as Go writes a map.
    pub fn to_json(&self) -> Map<String, Value> {
        let mut data = BTreeMap::new();
        let fields = [
            ("id_token", &self.id_token),
            ("access_token", &self.access_token),
            ("refresh_token", &self.refresh_token),
            ("last_refresh", &self.last_refresh),
            ("email", &self.email),
            ("expired", &self.expire),
        ];
        for (key, value) in fields {
            data.insert(key.to_owned(), Value::from(value.as_str()));
        }
        let optional = [
            ("account_uuid", &self.account_uuid),
            ("organization_uuid", &self.organization_uuid),
            ("organization_name", &self.organization_name),
        ];
        for (key, value) in optional {
            if !value.is_empty() {
                data.insert(key.to_owned(), Value::from(value.as_str()));
            }
        }
        data.insert("type".to_owned(), Value::from(CREDENTIAL_TYPE));
        for (key, value) in &self.metadata {
            data.insert(key.clone(), value.clone());
        }
        data.into_iter().collect()
    }
}

/// The credential file for a login's tokens (`CreateTokenStorage`).
pub fn create_token_storage(bundle: &AuthBundle) -> TokenStorage {
    let data = &bundle.token_data;
    TokenStorage {
        access_token: data.access_token.clone(),
        refresh_token: data.refresh_token.clone(),
        last_refresh: bundle.last_refresh.clone(),
        email: data.email.clone(),
        account_uuid: data.account_uuid.clone(),
        organization_uuid: data.organization_uuid.clone(),
        organization_name: data.organization_name.clone(),
        expire: data.expire.clone(),
        ..TokenStorage::default()
    }
}

/// Puts refreshed tokens in `storage`, keeping the account fields a refresh
/// leaves empty (`UpdateTokenStorage`).
pub fn update_token_storage(storage: &mut TokenStorage, data: &TokenData) {
    storage.access_token.clone_from(&data.access_token);
    storage.refresh_token.clone_from(&data.refresh_token);
    storage.last_refresh = now_rfc3339();
    let fields = [
        (&mut storage.email, &data.email),
        (&mut storage.account_uuid, &data.account_uuid),
        (&mut storage.organization_uuid, &data.organization_uuid),
        (&mut storage.organization_name, &data.organization_name),
    ];
    for (target, value) in fields {
        if !value.is_empty() {
            target.clone_from(value);
        }
    }
    storage.expire.clone_from(&data.expire);
}

/// The current local time in RFC 3339, as Go's
/// `time.Now().Format(time.RFC3339)` writes it.
pub(crate) fn now_rfc3339() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// When tokens that last `expires_in` seconds from now expire, in RFC 3339.
pub(crate) fn expiry(expires_in: i64) -> String {
    let now = Local::now();
    TimeDelta::try_seconds(expires_in)
        .and_then(|delta| now.checked_add_signed(delta))
        .unwrap_or(now)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A credential's file name (`CredentialFileName`):
/// `claude-<hash>-<email>.json`, where the hash is the start of the
/// organization UUID's SHA-256, or the account UUID's when there is no
/// organization. That tells apart organizations that share an email. With
/// neither, the name is the legacy `claude-<email>.json`.
pub fn credential_file_name(email: &str, organization_uuid: &str, account_uuid: &str) -> String {
    let email = email.trim();
    let identity = match organization_uuid.trim() {
        "" => account_uuid.trim(),
        organization => organization,
    };
    if identity.is_empty() {
        return format!("claude-{email}.json");
    }
    let hash: String = Sha256::digest(identity.as_bytes())
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("claude-{hash}-{email}.json")
}

/// The credential a new login replaces, when it was saved under an older
/// file name: the email-only legacy name, or the account-hashed name before
/// the account gained an organization (`FindMatchingLegacyCredential`).
/// Only a match on organization, or on account where neither has an
/// organization, counts; anything ambiguous gives `None`.
pub fn find_matching_legacy_credential(
    store: &dyn AuthStore,
    target: &Auth,
) -> io::Result<Option<Auth>> {
    if !is_hashed_credential_target(target) {
        return Ok(None);
    }
    let records = match store.list() {
        Ok(records) => records,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("list Claude credentials for legacy migration: {error}"),
            ));
        }
    };

    let target_email = metadata_string(target, "email");
    let target_organization = metadata_string(target, "organization_uuid");
    let target_account = metadata_string(target, "account_uuid");
    let legacy_file_name = credential_file_name(target_email, "", "");
    let account_file_name = if !target_organization.is_empty() && !target_account.is_empty() {
        credential_file_name(target_email, "", target_account)
    } else {
        String::new()
    };

    for candidate in records {
        if !eq_fold(candidate.provider.trim(), "claude") {
            continue;
        }
        let base_name = go_base(file_name_or_id(&candidate));
        let is_email_legacy = eq_fold(base_name, &legacy_file_name);
        let is_account_predecessor =
            !account_file_name.is_empty() && eq_fold(base_name, &account_file_name);
        if !is_email_legacy && !is_account_predecessor {
            continue;
        }

        let candidate_organization = metadata_string(&candidate, "organization_uuid");
        let candidate_account = metadata_string(&candidate, "account_uuid");
        let matches = if !target_organization.is_empty() {
            (!candidate_organization.is_empty()
                && eq_fold(candidate_organization, target_organization))
                || (candidate_organization.is_empty()
                    && is_account_predecessor
                    && !candidate_account.is_empty()
                    && eq_fold(candidate_account, target_account))
        } else if is_email_legacy && candidate_organization.is_empty() && !target_account.is_empty()
        {
            !candidate_account.is_empty() && eq_fold(candidate_account, target_account)
        } else {
            false
        };
        if matches {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Whether `target` is a Claude credential saved under its hashed name.
fn is_hashed_credential_target(target: &Auth) -> bool {
    if !eq_fold(target.provider.trim(), "claude") {
        return false;
    }
    let email = metadata_string(target, "email");
    let organization = metadata_string(target, "organization_uuid");
    let account = metadata_string(target, "account_uuid");
    if email.is_empty() || (organization.is_empty() && account.is_empty()) {
        return false;
    }
    eq_fold(
        go_base(file_name_or_id(target)),
        &credential_file_name(email, organization, account),
    )
}

fn file_name_or_id(auth: &Auth) -> &str {
    match auth.file_name.trim() {
        "" => auth.id.trim(),
        name => name,
    }
}

fn metadata_string<'a>(auth: &'a Auth, key: &str) -> &'a str {
    auth.metadata_str(key).unwrap_or_default().trim()
}

/// Go's `filepath.Base`: the last element of a path, or `.` for an empty
/// one.
fn go_base(path: &str) -> &str {
    let is_separator = |c: char| c == '/' || (cfg!(windows) && c == '\\');
    if path.is_empty() {
        return ".";
    }
    let trimmed = path.trim_end_matches(is_separator);
    if trimmed.is_empty() {
        return if cfg!(windows) { "\\" } else { "/" };
    }
    match trimmed.rfind(is_separator) {
        Some(index) => trimmed.get(index + 1..).unwrap_or(trimmed),
        None => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn credential_file_names() {
        let cases = [
            (
                "user@example.com",
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "claude-00f765af-user@example.com.json",
            ),
            (
                "user@example.com",
                "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "claude-50d86b12-user@example.com.json",
            ),
            (
                " user@example.com ",
                "",
                " aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa ",
                "claude-303617b9-user@example.com.json",
            ),
            (" user@example.com ", "", "", "claude-user@example.com.json"),
        ];
        for (email, organization, account, want) in cases {
            assert_eq!(credential_file_name(email, organization, account), want);
        }
    }

    struct ListStore(Vec<Auth>);

    impl AuthStore for ListStore {
        fn list(&self) -> io::Result<Vec<Auth>> {
            Ok(self.0.clone())
        }

        fn save(&self, _auth: &Auth) -> io::Result<String> {
            unreachable!("unexpected save")
        }

        fn delete(&self, _id: &str) -> io::Result<()> {
            unreachable!("unexpected delete")
        }
    }

    fn metadata(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    #[test]
    fn finds_matching_legacy_credentials() {
        const EMAIL: &str = "user@example.com";
        let legacy_file_name = credential_file_name(EMAIL, "", "");
        let cases = [
            (
                "matching organization",
                json!({"email": EMAIL, "organization_uuid": "organization-a", "account_uuid": "shared-account"}),
                json!({"email": EMAIL, "organization_uuid": "organization-a", "account_uuid": "shared-account"}),
                String::new(),
                true,
            ),
            (
                "different organization with shared account",
                json!({"email": EMAIL, "organization_uuid": "organization-b", "account_uuid": "shared-account"}),
                json!({"email": EMAIL, "organization_uuid": "organization-a", "account_uuid": "shared-account"}),
                String::new(),
                false,
            ),
            (
                "missing legacy organization remains ambiguous",
                json!({"email": EMAIL, "organization_uuid": "organization-a", "account_uuid": "shared-account"}),
                json!({"email": EMAIL, "account_uuid": "shared-account"}),
                String::new(),
                false,
            ),
            (
                "matching account when neither has organization",
                json!({"email": EMAIL, "account_uuid": "account-a"}),
                json!({"email": EMAIL, "account_uuid": "account-a"}),
                String::new(),
                true,
            ),
            (
                "matching account-hashed predecessor when target gains organization",
                json!({"email": EMAIL, "organization_uuid": "organization-a", "account_uuid": "account-a"}),
                json!({"email": EMAIL, "account_uuid": "account-a"}),
                credential_file_name(EMAIL, "", "account-a"),
                true,
            ),
            (
                "different account-hashed predecessor",
                json!({"email": EMAIL, "organization_uuid": "organization-a", "account_uuid": "account-a"}),
                json!({"email": EMAIL, "account_uuid": "account-b"}),
                credential_file_name(EMAIL, "", "account-b"),
                false,
            ),
            (
                "account-hashed predecessor associated with another organization",
                json!({"email": EMAIL, "organization_uuid": "organization-a", "account_uuid": "shared-account"}),
                json!({"email": EMAIL, "organization_uuid": "organization-b", "account_uuid": "shared-account"}),
                credential_file_name(EMAIL, "", "shared-account"),
                false,
            ),
        ];
        for (name, target_metadata, legacy_metadata, candidate_file_name, want) in cases {
            let target_metadata = metadata(target_metadata);
            let get = |key: &str| {
                target_metadata
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let file_name =
                credential_file_name(EMAIL, &get("organization_uuid"), &get("account_uuid"));
            let target = Auth {
                id: file_name.clone(),
                file_name,
                provider: "claude".into(),
                metadata: target_metadata.clone(),
                ..Auth::default()
            };
            let candidate_file_name = if candidate_file_name.is_empty() {
                legacy_file_name.clone()
            } else {
                candidate_file_name
            };
            let legacy = Auth {
                id: candidate_file_name.clone(),
                file_name: candidate_file_name,
                provider: "claude".into(),
                metadata: metadata(legacy_metadata),
                ..Auth::default()
            };
            let got = find_matching_legacy_credential(&ListStore(vec![legacy]), &target).unwrap();
            assert_eq!(got.is_some(), want, "{name}");
        }
    }

    #[test]
    fn legacy_lookup_reports_store_errors() {
        struct Failing(Mutex<io::ErrorKind>);
        impl AuthStore for Failing {
            fn list(&self) -> io::Result<Vec<Auth>> {
                Err(io::Error::new(*self.0.lock().unwrap(), "boom"))
            }
            fn save(&self, _auth: &Auth) -> io::Result<String> {
                unreachable!()
            }
            fn delete(&self, _id: &str) -> io::Result<()> {
                unreachable!()
            }
        }
        let name = credential_file_name("a@b.c", "org", "");
        let target = Auth {
            id: name.clone(),
            file_name: name,
            provider: "claude".into(),
            metadata: metadata(json!({"email": "a@b.c", "organization_uuid": "org"})),
            ..Auth::default()
        };
        let store = Failing(Mutex::new(io::ErrorKind::NotFound));
        assert!(
            find_matching_legacy_credential(&store, &target)
                .unwrap()
                .is_none()
        );
        *store.0.lock().unwrap() = io::ErrorKind::PermissionDenied;
        let error = find_matching_legacy_credential(&store, &target).unwrap_err();
        assert_eq!(
            error.to_string(),
            "list Claude credentials for legacy migration: boom"
        );
        // A target under its legacy name isn't looked up.
        let legacy = Auth {
            file_name: credential_file_name("a@b.c", "", ""),
            ..target
        };
        assert!(
            find_matching_legacy_credential(&store, &legacy)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn json_keeps_custom_metadata() {
        let mut storage = TokenStorage {
            email: "user@example.com".into(),
            access_token: "new-claude-access".into(),
            refresh_token: "new-claude-refresh".into(),
            expire: "2026-12-31T23:59:59Z".into(),
            last_refresh: "2026-04-14T12:00:00Z".into(),
            ..TokenStorage::default()
        };
        storage.metadata = metadata(json!({
            "disabled": false,
            "prefix": "claude-prefix",
            "note": "claude custom note",
            "proxy_url": "http://proxy:8080",
            "weight": 5,
            "claude_device_ids": ["kept-as-is"],
        }));
        let saved = storage.to_json();
        assert_eq!(saved["access_token"], "new-claude-access");
        assert_eq!(saved["prefix"], "claude-prefix");
        assert_eq!(saved["note"], "claude custom note");
        assert_eq!(saved["proxy_url"], "http://proxy:8080");
        assert_eq!(saved["weight"], 5);
        assert_eq!(saved["disabled"], false);
        assert_eq!(saved["type"], "claude");
        assert_eq!(saved["claude_device_ids"], json!(["kept-as-is"]));
        assert!(!saved.contains_key("account_uuid"));
        let keys: Vec<&String> = saved.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn update_keeps_account_when_refresh_omits_it() {
        let mut storage = TokenStorage {
            email: "user@example.com".into(),
            account_uuid: "account-1".into(),
            organization_uuid: "org-1".into(),
            organization_name: "Org".into(),
            ..TokenStorage::default()
        };
        update_token_storage(
            &mut storage,
            &TokenData {
                access_token: "new-access".into(),
                refresh_token: "new-refresh".into(),
                expire: "2026-12-31T00:00:00Z".into(),
                ..TokenData::default()
            },
        );
        assert_eq!(storage.access_token, "new-access");
        assert_eq!(storage.refresh_token, "new-refresh");
        assert_eq!(storage.email, "user@example.com");
        assert_eq!(storage.account_uuid, "account-1");
        assert_eq!(storage.organization_uuid, "org-1");
        assert_eq!(storage.organization_name, "Org");
        assert_eq!(storage.expire, "2026-12-31T00:00:00Z");
        assert!(!storage.last_refresh.is_empty());
    }

    #[test]
    fn debug_leaves_out_tokens() {
        let data = TokenData {
            access_token: "secret-access".into(),
            refresh_token: "secret-refresh".into(),
            ..TokenData::default()
        };
        assert!(!format!("{data:?}").contains("secret"));
        let storage = TokenStorage {
            access_token: "secret-access".into(),
            ..TokenStorage::default()
        };
        assert!(!format!("{storage:?}").contains("secret"));
    }

    #[test]
    fn go_base_takes_the_last_element() {
        assert_eq!(go_base("dir/claude-a.json"), "claude-a.json");
        assert_eq!(go_base("claude-a.json"), "claude-a.json");
        assert_eq!(go_base(""), ".");
        assert_eq!(go_base("dir/"), "dir");
    }
}
