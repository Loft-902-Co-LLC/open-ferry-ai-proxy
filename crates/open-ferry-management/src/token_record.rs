// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_fields.go (mergeExistingAuthFileMetadata, saveTokenRecord)
// (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Saving the credential a login or an import made, and serving it at
//! once: [`save_token_record`].
//!
//! Deviations from upstream:
//! - The credential is saved through the store the service saves with,
//!   in its auth directory, and the existing file it replaces is read from
//!   there too; upstream points its token store at the config's
//!   `auth-dir` on each save.
//! - When there is no file to merge, a registered credential lends its
//!   settings only if its metadata isn't empty, and a credential found by
//!   file name only if that name isn't empty; upstream takes one whose
//!   metadata is empty but not nil, and matches a credential without a
//!   file name to a record without one. Of several with the file name,
//!   the first by ID is taken, where upstream takes whichever its map
//!   yields first.
//! - The merge, the save, the readback and the call telling the service
//!   hold the credential lock that the management API's other changes of
//!   credential files and records hold, so the change takes its revision
//!   under it, and the call is awaited once the lock is released (see
//!   [`crate::credential_sync`]); upstream takes none, so a status change
//!   made meanwhile could save its stale copy over the new credential.
//! - The file replaced and the one saved are never read through a symlink
//!   or, on Windows, any reparse point, and a symlink where the file would
//!   be saved is refused (checked just before the save): the save fails
//!   with `<path> is a symlink`. Upstream reads and writes through it.
//! - Upstream's post-auth hook, which only plugins and embedders set,
//!   isn't ported.
//! - The service is sent the saved file and makes the credential from it,
//!   as [`CredentialSync::file_written`] describes, logging a file it can't
//!   read a credential from; upstream makes the credential here first and
//!   fails with `synthesize persisted auth failed` when it can't.

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use http::StatusCode;
use open_ferry_core::auth::Auth;
use open_ferry_core::auth::file_store::FileStore;
use open_ferry_core::auth::metadata::merge_existing_auth_metadata;
use open_ferry_core::config::AuthFile;
use open_ferry_providers::credentials;

use crate::auth_files::run_blocking;
use crate::credential_files::{is_linked, linked, read_unlinked};
use crate::credential_sync::{CredentialSync, SyncError};
use crate::state::ManagementState;

/// Why [`save_token_record`] failed.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "for the import and OAuth login routes")
)]
#[derive(Debug)]
pub(crate) enum SaveError {
    /// No credential store or sync was set.
    Unavailable,
    /// The credential couldn't be saved.
    Save(io::Error),
    /// The credential was saved at `path`, but the service couldn't be
    /// told.
    Sync {
        /// Where the credential was saved.
        path: String,
        /// Why the service couldn't be told.
        error: SyncError,
    },
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "for the import and OAuth login routes")
)]
impl SaveError {
    /// The status to answer with: 503 when the store or the service isn't
    /// there, else 500.
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Save(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Sync { error, .. } => error.status(),
        }
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("credential store unavailable"),
            Self::Save(error) => write!(f, "{error}"),
            Self::Sync { error, .. } => write!(f, "post-auth persist hook failed: {error}"),
        }
    }
}

impl std::error::Error for SaveError {}

/// Saves `record`, a credential a login or an import made, in the auth
/// directory, and returns the file's path (upstream's `saveTokenRecord`):
///
/// 1. The settings of the file it replaces, else of the credential
///    registered with its ID or file name, are merged into it (upstream's
///    `mergeExistingAuthFileMetadata`).
/// 2. It is saved, in place of a Claude file saved under an older name for
///    the same account, as [`credentials::save_merged`] does.
/// 3. The service is sent the saved file, or `record` when the file can't
///    be read back, and serves the credential once this returns.
///
/// The credential lock is held from the merge until the service is sent
/// the change, which takes its revision under it, and released before the
/// change is awaited (see [`crate::credential_sync`]). The file replaced
/// and the one
/// saved are never read through a link (see
/// [`read_unlinked`](crate::credential_files::read_unlinked)), and a
/// symlink where the file would be saved is refused; either fails with
/// `<path> is a symlink`.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "for the import and OAuth login routes")
)]
pub(crate) async fn save_token_record(
    state: &ManagementState,
    mut record: Auth,
) -> Result<String, SaveError> {
    let store = state
        .credential_store()
        .map_err(|_| SaveError::Unavailable)?;
    // Held from the merge until the service is sent the change, as every
    // change of a credential file or record is, so none is lost to another
    // made meanwhile and the service applies them in order.
    let guard = state.credential_lock().lock().await;
    let files = Arc::clone(&store.files);
    let (returned, existing) = run_blocking(move || {
        let existing = match files.read_existing_metadata(&record, read_unlinked) {
            Ok(existing) => Ok(existing),
            Err(error) if is_linked(&error) => Err(error),
            Err(_) => Ok(None),
        };
        (record, existing)
    })
    .await;
    record = returned;
    if let Some(existing) = existing
        .map_err(SaveError::Save)?
        .or_else(|| registered_metadata(state, &record))
        && !existing.is_empty()
    {
        merge_existing_auth_metadata(&mut record, &existing);
    }

    let command = capture_destination_command(
        state,
        &store.files,
        &record,
        open_ferry_core::auth::CodexCredentialPartition::Outer,
    );
    let files = Arc::clone(&store.files);
    let (record, saved) = run_blocking(move || {
        let saved = save_unlinked(&files, &mut record);
        (record, saved)
    })
    .await;
    let (path, data) = saved.map_err(SaveError::Save)?;
    let sync: &dyn CredentialSync = store.sync.as_ref();
    // Made under the lock, so the change takes its revision there.
    let file = data.map(|data| AuthFile {
        path: PathBuf::from(&path),
        data: data.into(),
    });
    let synced = match command {
        Some(command) if !path.is_empty() => sync.completed_token_command(record, file, command),
        _ => match file {
            Some(file) => sync.file_written(file),
            None => sync.upsert(record),
        },
    };
    drop(guard);
    match synced.await {
        Ok(()) => Ok(path),
        Err(error) => Err(SaveError::Sync { path, error }),
    }
}

/// Saves `record` as [`credentials::save_merged`] does, unless a symlink is
/// where it would go, and reads the file back, never through a link.
/// Returns the file's path and contents, `None` when it can't be read back
/// or is empty.
fn save_unlinked(files: &FileStore, record: &mut Auth) -> io::Result<(String, Option<Vec<u8>>)> {
    if let Ok(path) = files.resolve_auth_path(record)
        && fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(linked(&path));
    }
    let path = credentials::save_merged(files, record)?;
    let data = read_back(files, path.as_ref(), read_unlinked);
    Ok((path, data))
}

fn read_back(
    files: &FileStore,
    path: &std::path::Path,
    read: impl FnOnce(&std::path::Path) -> io::Result<Vec<u8>>,
) -> Option<Vec<u8>> {
    if files
        .file_guard()
        .is_some_and(|guard| guard.check_read(path).is_err())
    {
        return None;
    }
    match read(path) {
        Ok(data) if !data.is_empty() => Some(data),
        _ => None,
    }
}

/// The metadata of the credential registered with `record`'s ID, else of
/// the first with its file name, for a record with no file to merge.
fn registered_metadata(
    state: &ManagementState,
    record: &Auth,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let manager = state.manager();
    if let Some(existing) = manager.get(&record.id)
        && !existing.metadata.is_empty()
    {
        return Some(existing.metadata.clone());
    }
    if record.file_name.is_empty() {
        return None;
    }
    manager
        .list()
        .into_iter()
        .find(|auth| auth.file_name == record.file_name && !auth.metadata.is_empty())
        .map(|auth| auth.metadata.clone())
}

/// The token command for `partition` of the one credential registered for
/// `record`'s file, captured before the login or bind that writes it.
/// Login ids may be empty until save_new_auth normalizes them, so the
/// destination picks the registration, not a transient id. No registration
/// there, or several, captures none.
pub(crate) fn capture_destination_command(
    state: &ManagementState,
    files: &FileStore,
    record: &Auth,
    partition: open_ferry_core::auth::CodexCredentialPartition,
) -> Option<open_ferry_core::manager::CompletedTokenCommand> {
    let destination = files.resolve_auth_path(record).ok()?;
    let registrations = state.manager().list();
    let mut matching = registrations
        .into_iter()
        .filter(|auth| files.resolve_auth_path(auth).ok().as_ref() == Some(&destination));
    let registered = matching.next().filter(|_| matching.next().is_none())?;
    state
        .manager()
        .capture_token_command(&registered.id, partition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use open_ferry_core::auth::file_guard::FileGuard;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_refused_readback_uses_the_failed_read_path_without_calling_the_reader() {
        let root = std::env::var_os("OFP_TEST_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
            });
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let refused = Arc::new(AtomicBool::new(false));
        let checked = Arc::clone(&refused);
        let store = FileStore::new(dir.path()).with_file_guard(Some(FileGuard::new(move |_| {
            if checked.load(Ordering::SeqCst) {
                Err("untrusted readback".into())
            } else {
                Ok(())
            }
        })));
        let mut record = Auth {
            id: "account.json".into(),
            provider: "codex".into(),
            ..Auth::default()
        };
        record
            .metadata
            .insert("type".into(), serde_json::json!("codex"));
        let path = credentials::save_merged(&store, &mut record).unwrap();
        refused.store(true, Ordering::SeqCst);
        let reads = std::cell::Cell::new(0);
        assert!(
            read_back(&store, path.as_ref(), |_| {
                reads.set(reads.get() + 1);
                Ok(b"secret".to_vec())
            })
            .is_none()
        );
        assert_eq!(reads.get(), 0);
        refused.store(false, Ordering::SeqCst);
        assert!(
            read_back(&store, path.as_ref(), |_| Err(
                io::ErrorKind::PermissionDenied.into()
            ))
            .is_none()
        );
        assert!(read_back(&store, path.as_ref(), |_| Ok(Vec::new())).is_none());
        assert_eq!(
            read_back(&store, path.as_ref(), |_| Ok(b"trusted".to_vec())),
            Some(b"trusted".to_vec())
        );
    }
}
