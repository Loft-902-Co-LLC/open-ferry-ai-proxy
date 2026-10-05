// Ported from CLIProxyAPI sdk/auth/manager.go (Manager.Login's save) and
// internal/api/handlers/management/auth_files_fields.go (saveTokenRecord's
// legacy Claude migration) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Saving a credential from a login, as the `-codex-login` and
//! `-claude-login` commands and the management API's logins do: over the
//! file a past login for the account saved, keeping that file's settings,
//! and in place of a Claude file saved under an older name.
//!
//! Deviations from upstream:
//! - Errors don't start with upstream's `cliproxy auth: `, so a command
//!   line login and a management login fail with the same messages, as
//!   the management API words them.

use std::io;

use open_ferry_core::auth::metadata::merge_existing_auth_metadata;
use open_ferry_core::auth::{Auth, AuthStore, FileStore};

use crate::claude::token::find_matching_legacy_credential;

/// Saves a login's credential in `store`'s auth directory, and returns its
/// path: first copies the settings of the file it replaces into `auth`
/// (see [`FileStore::merge_existing`]), then saves as [`save_merged`] does.
pub fn save(store: &FileStore, auth: &mut Auth) -> io::Result<String> {
    store.merge_existing(auth);
    save_merged(store, auth)
}

/// Saves a login's credential whose existing settings were already merged
/// into it, and returns its path. A Claude credential saved under an older
/// name for the same account gives its settings to `auth` and is deleted
/// once `auth` is saved.
pub fn save_merged(store: &FileStore, auth: &mut Auth) -> io::Result<String> {
    let legacy = find_matching_legacy_credential(store, auth)?;
    if let Some(legacy) = &legacy {
        merge_existing_auth_metadata(auth, &legacy.metadata);
    }
    let path = store.save_new_auth(auth)?;
    if let Some(legacy) = legacy {
        if path.trim().is_empty() {
            return Err(io::Error::other(
                "canonical Claude credential was not persisted; legacy credential retained",
            ));
        }
        let id = match legacy.id.trim() {
            "" => legacy.file_name.trim(),
            id => id,
        };
        store.delete(id).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "canonical Claude credential saved but legacy credential cleanup failed: {error}"
                ),
            )
        })?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{Value, json};

    use super::*;
    use crate::claude::token::credential_file_name;

    fn claude_auth(file_name: &str, metadata: Value) -> Auth {
        let Value::Object(metadata) = metadata else {
            unreachable!()
        };
        Auth {
            id: file_name.into(),
            file_name: file_name.into(),
            provider: "claude".into(),
            metadata,
            ..Auth::default()
        }
    }

    // Not upstream's: Manager.Login's legacy Claude migration.
    #[test]
    fn saving_replaces_a_legacy_claude_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let mut legacy = claude_auth(
            "claude-a@b.c.json",
            json!({"type": "claude", "email": "a@b.c", "account_uuid": "acct",
                   "access_token": "old", "prefix": "team"}),
        );
        store.save_new_auth(&mut legacy).unwrap();

        let name = credential_file_name("a@b.c", "", "acct");
        let mut auth = claude_auth(
            &name,
            json!({"type": "claude", "email": "a@b.c", "account_uuid": "acct",
                   "access_token": "new"}),
        );
        let path = save(&store, &mut auth).unwrap();
        assert_eq!(Path::new(&path), dir.path().join(&name));
        assert!(!dir.path().join("claude-a@b.c.json").exists());
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["access_token"], "new");
        assert_eq!(saved["prefix"], "team");
    }

    // Not upstream's: Manager.Login's merge of the file it replaces.
    #[test]
    fn saving_keeps_the_settings_of_the_file_it_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let mut old = Auth {
            id: "codex-a@b.c-plus.json".into(),
            file_name: "codex-a@b.c-plus.json".into(),
            provider: "codex".into(),
            ..Auth::default()
        };
        old.metadata.insert("type".into(), json!("codex"));
        old.metadata.insert("access_token".into(), json!("old"));
        old.metadata.insert("disabled".into(), json!(true));
        old.disabled = true;
        store.save_new_auth(&mut old).unwrap();

        let mut auth = Auth {
            id: "codex-a@b.c-plus.json".into(),
            file_name: "codex-a@b.c-plus.json".into(),
            provider: "codex".into(),
            ..Auth::default()
        };
        auth.metadata.insert("type".into(), json!("codex"));
        auth.metadata.insert("access_token".into(), json!("new"));
        let path = save(&store, &mut auth).unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["access_token"], "new");
        assert_eq!(saved["disabled"], true);
        assert!(auth.disabled);
    }
}
