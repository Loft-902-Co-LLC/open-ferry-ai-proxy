// Ported from CLIProxyAPI sdk/auth/filestore.go, and the existing-file merge
// in Manager.Login in sdk/auth/manager.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`FileStore`]: credentials kept as JSON files in an auth directory.
//!
//! Synchronous mutation APIs may block for the bounded, 30-second credential
//! lock wait. Async callers must offload the entire operation, not only lock
//! acquisition. The async partition APIs do that, or run inline without a runtime.
//!
//! Each `*.json` file under the directory, at any depth, is one credential.
//! Its ID is its path relative to the directory (lowercased on Windows,
//! whose paths ignore case), and its contents become [`Auth::metadata`] as
//! they are: tokens, settings and any keys nothing here knows, which a save
//! writes back untouched.
//!
//! Listing reads a few keys into fields and attributes:
//!
//! - `type` is the provider (`unknown` when missing). Files of type `gemini`
//!   are skipped.
//! - `disabled` sets [`Auth::disabled`] and the status; `prefix` and
//!   `proxy_url` set the fields of those names. A prefix containing `/` is
//!   dropped.
//! - `label`, else `email`, else `project_id` is the label; `email` is also
//!   an attribute.
//! - `headers` become `header:<name>` attributes.
//! - `weight` is checked, and a file with an invalid weight is skipped; like
//!   `priority`, `model_aliases` and `excluded_models` it stays in the
//!   metadata, and the file synthesizer turns those into attributes.
//! - `request_retry`, `disable_cooling` and `refresh_interval_seconds` stay
//!   in the metadata, where the credential manager reads them.
//! - Legacy spellings such as `request-retry` are renamed first.
//!
//! Files that can't be read or decoded are skipped and logged at debug
//! level, by path only.
//!
//! Saving writes the metadata with `disabled` set from the record. A file
//! whose JSON already matches is left alone. New files are created with mode
//! 0600 in directories created with mode 0700 (on Unix). A runtime save of a
//! disabled credential whose file is gone does nothing, so a deleted file
//! stays deleted; a login uses [`FileStore::save_new_auth`] to create one.
//! A Codex login over a file that isn't a JSON object (or is over 8 MiB)
//! first keeps a copy of it beside it, with `.invalid-<time>` added to its
//! name, and over one it can't read fails and leaves it.
//!
//! [`FileStore::write_file`] and [`FileStore::remove_file`] write the bytes
//! of one file in the auth directory as they are, and remove one, by name,
//! for the management API's uploads and deletes.
//!
//! Deviations from upstream:
//! - Writes are atomic: a temporary file in the same directory, flushed and
//!   renamed over the target, keeping an existing file's permissions on
//!   Unix. Upstream truncates and rewrites in place. On Windows the rename
//!   fails while another process holds the file open without sharing
//!   deletes. A symlinked credential file is written through to its target;
//!   a dangling symlink is replaced by the file.
//! - Records keep tokens in their metadata; upstream's `TokenStorage` and
//!   plugin auth parsers aren't ported.
//! - A record with empty metadata has nothing to persist, as upstream's nil
//!   metadata; upstream would write `{"disabled":false}` for an empty map.
//! - Numbers are written back as they were read, where Go reformats them
//!   (`1.0` stays `1.0`, not `1`). Equality checks compare them as float64,
//!   as upstream does.
//! - Files over 8 MiB are skipped rather than read whole; skipped files are
//!   logged where upstream drops them silently.
//! - A path that isn't valid UTF-8 is skipped when listing and refused when
//!   saving.
//! - Deleting a relative ID joins it to the auth directory even when it
//!   contains a separator, so a nested credential's ID deletes its file;
//!   upstream resolves such IDs against the working directory.
//! - An auth directory that is a symlink is followed; upstream's walk lists
//!   nothing under it.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};

use crate::gate::{Activation, ActiveGate, WritePermit};

use serde_json::{Map, Value};

use super::classification::{
    ATTRIBUTE_PATH, ATTRIBUTE_SOURCE, ATTRIBUTE_SOURCE_BACKEND, AUTH_SOURCE_FILE,
};
use super::go::equal_fold;
use super::json::{json_equal, marshal_map, unmarshal_object};
use super::metadata::{
    apply_custom_headers_from_metadata, merge_existing_auth_metadata, normalize_credential_metadata,
};
use super::path::{join, rel};
use super::weight::validate_weights;
use super::{
    Auth, AuthStore, CodexCredentialPartition, CodexLease, CodexLeaseToken, CodexOrigin,
    GuardedSave, OriginProof, SaveDeferral, Status, WithheldPartitions,
};

/// `save_guarded`'s guard, as `save_with` takes it.
type RefusedFn<'a> = dyn FnMut(&CodexOrigin, OriginProof) + 'a;
type SaveGuard<'a> =
    dyn FnMut(Option<&CodexOrigin>) -> Result<WithheldPartitions, SaveDeferral> + 'a;

/// What `save_with` did.
enum SaveWith {
    /// Saved: the file's path, or empty for a disabled credential whose file is gone.
    Written(String),
    /// Fence 2 refused: the path was not the origin's file. Nothing was written.
    OriginRefused,
    /// Stage G or W refused. Nothing was written.
    Deferred(SaveDeferral),
}

impl SaveWith {
    /// The path for a save with no bound and no guard, which only writes.
    fn into_path(self) -> String {
        match self {
            Self::Written(path) => path,
            Self::OriginRefused | Self::Deferred(_) => String::new(),
        }
    }
}

/// The largest credential file read.
pub const MAX_AUTH_FILE_SIZE: u64 = 8 << 20;

/// Identity of an open file, without retaining a handle or exposing its values.
#[derive(Clone, Copy)]
pub struct FileId {
    volume: u64,
    index: u128,
    #[cfg(windows)]
    extended: Option<(u64, u128)>,
}

impl PartialEq for FileId {
    fn eq(&self, other: &Self) -> bool {
        #[cfg(windows)]
        match (self.extended, other.extended) {
            (Some(left), Some(right)) => return left == right,
            (None, None) => {}
            _ => return false,
        }
        self.volume == other.volume && self.index == other.index
    }
}

impl Eq for FileId {}

impl FileId {
    fn supported(self) -> io::Result<Self> {
        #[cfg(windows)]
        if self.extended.is_none() && self.index == 0 {
            return Err(invalid("SIWC bind target identity changed"));
        }
        Ok(self)
    }

    /// Scan-only conservative matching: a possible alias must be refused.
    /// Unlike strict equality for change checks, a mixed-mode pair can still
    /// alias when its legacy IDs match, even if path resolution differs by driver.
    fn may_alias(self, other: Self) -> io::Result<bool> {
        self.supported()?;
        other.supported()?;
        #[cfg(windows)]
        if let (Some(left), Some(right)) = (self.extended, other.extended) {
            return Ok(left == right);
        }
        Ok(self.volume == other.volume && self.index == other.index)
    }
}

/// Computes the volume/file identity on Windows or device/inode on Unix.
pub fn file_id(file: &fs::File) -> io::Result<FileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(FileId {
            volume: metadata.dev(),
            index: u128::from(metadata.ino()),
        })
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct Information {
            volume: u64,
            id: [u8; 16],
        }
        let mut info = Information::default();
        let extended = query_file_information(file, 18, &mut info)
            .map(|()| (info.volume, u128::from_le_bytes(info.id)));
        windows_file_id(file, extended)
    }
}

#[cfg(windows)]
fn windows_file_id(file: &fs::File, extended: io::Result<(u64, u128)>) -> io::Result<FileId> {
    use std::os::windows::io::AsRawHandle as _;
    let extended = match extended {
        Ok(id) => Some(id),
        Err(error) if matches!(error.raw_os_error(), Some(1 | 87 | 50)) => None,
        Err(error) => return Err(error),
    };
    #[repr(C)]
    #[derive(Default)]
    struct Information {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume: u32,
        size: [u32; 2],
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            information: *mut Information,
        ) -> i32;
    }
    let mut info = Information::default();
    // SAFETY: file owns a live handle and info has the writable Win32
    // BY_HANDLE_FILE_INFORMATION layout for the duration of the call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Preserve extended IDs without truncation. An unsupported query uses a
    // separate legacy mode; a mode change during a bind requires retry.
    FileId {
        volume: u64::from(info.volume),
        index: u128::from((u64::from(info.index_high) << 32) | u64::from(info.index_low)),
        extended,
    }
    .supported()
}

#[cfg(windows)]
fn query_file_information<T>(file: &fs::File, class: i32, info: &mut T) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut std::ffi::c_void,
            class: i32,
            information: *mut std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    // SAFETY: file owns a live handle; callers supply the Win32 layout for
    // class, and info remains writable for its complete size during the call.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            class,
            std::ptr::from_mut(info).cast(),
            std::mem::size_of::<T>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn name_surrogate(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

#[cfg(windows)]
fn reparse_tag(file: &fs::File) -> io::Result<u32> {
    #[repr(C)]
    #[derive(Default)]
    struct Information {
        attributes: u32,
        tag: u32,
    }
    let mut info = Information::default();
    query_file_information(file, 9, &mut info)?;
    Ok(if info.attributes & 0x400 == 0 {
        0
    } else {
        info.tag
    })
}

/// The selected outer identity and a private grant fingerprint for the
/// no-user fallback. Neither values nor fingerprints are Debug-printable.
/// This detects substitution, not ID-token authenticity or SIWC matching.
pub struct BindIdentity {
    evidence: [Option<String>; 6],
    grant: [u8; 32],
    file: FileId,
}

impl BindIdentity {
    /// Captures the outer identity without keeping the credential file open.
    pub fn capture(metadata: &Map<String, Value>, file: FileId) -> Self {
        Self {
            evidence: bind_account(metadata),
            grant: bind_grant(metadata),
            file,
        }
    }

    /// Refuses changed account or independent user claims. With no user
    /// evidence, requires both the same file and the unchanged outer grant.
    pub fn check(&self, metadata: &Map<String, Value>, file: FileId) -> io::Result<()> {
        self.file.supported()?;
        file.supported()?;
        let evidence = bind_account(metadata);
        let same = self
            .evidence
            .iter()
            .zip(&evidence)
            .all(|(before, after)| before.is_none() || before == after);
        let has_user = self.evidence[2..].iter().any(Option::is_some);
        if same && (has_user || (self.file == file && self.grant == bind_grant(metadata))) {
            Ok(())
        } else {
            Err(invalid("SIWC bind target identity changed"))
        }
    }
}

fn bind_grant(metadata: &Map<String, Value>) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(
        metadata
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .as_bytes(),
    )
    .into()
}

fn bind_account(metadata: &Map<String, Value>) -> [Option<String>; 6] {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = metadata
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(|token| token.split('.').nth(1))
        .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let claims = claims.as_ref();
    let auth = claims.and_then(|c| c.get("https://api.openai.com/auth"));
    [
        text(metadata.get("account_id")),
        text(auth.and_then(|a| a.get("chatgpt_account_id"))),
        text(metadata.get("email")),
        text(claims.and_then(|c| c.get("email"))),
        text(claims.and_then(|c| c.get("sub"))),
        text(auth.and_then(|a| a.get("chatgpt_user_id"))),
    ]
}

/// Refuses leaf-link aliases that would use a different upstream lock pathname.
/// Called under the target's credential lock in a trusted auth directory.
fn refuse_siwc_bind_aliases(
    directory: &Path,
    target_path: &Path,
    target: FileId,
) -> io::Result<()> {
    target
        .supported()
        .map_err(|_| invalid("SIWC bind alias scan failed"))?;
    let Some(parent) = scan_optional(
        target_path
            .parent()
            .ok_or_else(|| invalid("SIWC bind alias scan failed"))?
            .canonicalize(),
    )?
    else {
        return Err(invalid("SIWC bind alias scan failed"));
    };
    let target_destination = parent.join(
        target_path
            .file_name()
            .ok_or_else(|| invalid("SIWC bind alias scan failed"))?,
    );
    let root = directory;
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let Some(entries) = scan_optional(fs::read_dir(&directory))? else {
            if directory == root {
                return Err(invalid("SIWC bind alias scan failed"));
            }
            continue;
        };
        for entry in entries {
            let Some(entry) = scan_optional(entry)? else {
                continue;
            };
            let path = entry.path();
            #[cfg(test)]
            super::credential_lock::test_event(
                &super::credential_lock::credential_lock_path(&path)?,
                "bind-scan-entry",
            );
            let Some(metadata) = scan_optional(fs::symlink_metadata(&path))? else {
                continue;
            };
            #[cfg(windows)]
            let (is_directory, linked) = {
                use std::os::windows::fs::MetadataExt;
                let tag = if metadata.file_attributes() & 0x400 != 0 {
                    let Some(file) = scan_optional(open_bind_attributes(&path, false))? else {
                        continue;
                    };
                    let Some(tag) = scan_optional(reparse_tag(&file))? else {
                        continue;
                    };
                    tag
                } else {
                    0
                };
                (metadata.file_attributes() & 0x10 != 0, name_surrogate(tag))
            };
            #[cfg(not(windows))]
            let (is_directory, linked) = (metadata.is_dir(), metadata.file_type().is_symlink());
            if is_directory {
                if !linked {
                    directories.push(path);
                }
                continue;
            }
            if !linked {
                continue;
            }
            let Some(destination) = scan_optional(path.canonicalize())? else {
                continue;
            };
            #[cfg(unix)]
            let id = {
                use std::os::unix::fs::MetadataExt;
                let Some(metadata) = scan_optional(fs::metadata(&path))? else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                FileId {
                    volume: metadata.dev(),
                    index: u128::from(metadata.ino()),
                }
            };
            #[cfg(windows)]
            let id = {
                let Some(file) = scan_optional(open_bind_attributes(&path, true))? else {
                    continue;
                };
                let Some(metadata) = scan_optional(file.metadata())? else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let Some(id) = scan_optional(file_id(&file))? else {
                    continue;
                };
                id
            };
            refuse_bind_alias(&destination, &target_destination, id, target)?;
        }
    }
    Ok(())
}

fn refuse_bind_alias(
    destination: &Path,
    target_destination: &Path,
    id: FileId,
    target: FileId,
) -> io::Result<()> {
    let matches = id
        .may_alias(target)
        .map_err(|_| invalid("SIWC bind alias scan failed"))?;
    if destination == target_destination || matches {
        return Err(invalid("SIWC bind target has a linked alias"));
    }
    Ok(())
}

// Directory churn and dangling/looping links are not alias evidence. Other
// errors fail closed, without carrying a credential path into the error.
fn scan_optional<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => Ok(None),
        #[cfg(windows)]
        Err(error) if error.raw_os_error() == Some(1921) => Ok(None),
        Err(error) => Err(io::Error::new(error.kind(), "SIWC bind alias scan failed")),
    }
}

#[cfg(windows)]
fn open_bind_attributes(path: &Path, follow: bool) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .access_mode(0x80) // FILE_READ_ATTRIBUTES, never a data-stream read.
        .custom_flags(if follow { 0x0200_0000 } else { 0x0220_0000 })
        .open(path)
}

/// Reads and validates a bind target through one no-follow handle, then closes it.
/// The caller must independently check containment in a trusted auth directory.
pub fn read_siwc_bind_target(path: &Path) -> io::Result<(Map<String, Value>, FileId)> {
    let (_file, metadata, id) = open_siwc_bind_target(path)?;
    Ok((metadata, id))
}

fn open_siwc_bind_target(path: &Path) -> io::Result<(fs::File, Map<String, Value>, FileId)> {
    require_bind_file(&fs::symlink_metadata(path)?)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    require_bind_file(&file.metadata()?)?;
    let id = file_id(&file)?;
    #[cfg(windows)]
    let file = {
        let tag = reparse_tag(&file)?;
        if name_surrogate(tag) {
            return Err(invalid("SIWC bind target is a leaf link"));
        }
        if tag != 0 {
            let reopened = fs::File::open(path)?;
            if file_id(&reopened)? != id {
                return Err(invalid("SIWC bind target identity changed"));
            }
            require_bind_file(&reopened.metadata()?)?;
            reopened
        } else {
            file
        }
    };
    let mut data = Vec::new();
    (&file)
        .take(MAX_AUTH_FILE_SIZE + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_AUTH_FILE_SIZE {
        return Err(invalid("SIWC bind target exceeds size cap"));
    }
    let metadata = unmarshal_object(&data)
        .map_err(|_| invalid("invalid SIWC bind target JSON"))?
        .ok_or_else(|| invalid("invalid SIWC bind target JSON"))?;
    if !metadata
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("codex"))
    {
        return Err(invalid("SIWC bind requires a Codex credential"));
    }
    for key in ["access_token", "refresh_token"] {
        if !metadata
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(invalid("SIWC bind requires outer tokens"));
        }
    }
    validate_weights(&Default::default(), &metadata)
        .map_err(|_| invalid("invalid SIWC bind settings"))?;
    Ok((file, metadata, id))
}

fn require_bind_file(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(invalid("SIWC bind target is a leaf link"));
    }
    if !metadata.is_file() {
        return Err(invalid("invalid SIWC bind target"));
    }
    Ok(())
}

type BindCheck<'a> = dyn Fn(&Map<String, Value>, FileId) -> io::Result<()> + 'a;

/// Credentials kept as JSON files under an auth directory (upstream's
/// `FileTokenStore`).
#[derive(Debug, Default)]
pub struct FileStore {
    base_dir: RwLock<PathBuf>,
    file_guard: Option<super::file_guard::FileGuard>,
    /// Counts the binds of `base_dir`; see `binding_generation`.
    binding_generation: AtomicU64,
    save_lock: Mutex<()>,
    /// Whether this process may write; always open until a switch sets one.
    gate: RwLock<ActiveGate>,
}

impl FileStore {
    /// Test-only bind: skips account identity checking, but uses strict no-follow I/O.
    /// Replaces only `siwc`, sets `use_siwc`, and returns the persisted record.
    /// Caller validates path containment and OAuth grant; no account matching
    /// or ID-token signature/nonce verification is performed here.
    /// The owned blocking operation continues if its awaiting caller cancels.
    #[cfg(test)]
    pub(crate) async fn bind_siwc(
        self: &std::sync::Arc<Self>,
        auth: Auth,
        siwc: Map<String, Value>,
    ) -> io::Result<Auth> {
        let store = self.clone();
        run_codex_write(move || {
            store
                .persist_codex_partition(
                    &auth,
                    &super::codex_refresh::CodexPersistIntent::SiwcBind(siwc),
                )
                .map(|p| p.auth)
        })
        .await
    }

    /// [`read_siwc_bind_target`] once this store's file guard allows the read.
    pub fn read_bind_target(&self, path: &Path) -> io::Result<(Map<String, Value>, FileId)> {
        self.check_read(path)?;
        read_siwc_bind_target(path)
    }

    /// Binds SIWC while the caller holds this credential's lock. The check
    /// runs under the save mutex on metadata read through a no-follow handle,
    /// which is closed before atomic replacement. The auth directory must be
    /// trusted against uncooperative writers changing its entries.
    /// Never reacquires that lock. A guard for any other path is refused.
    /// This synchronous API performs blocking file I/O; async callers must offload it.
    pub fn bind_siwc_locked(
        &self,
        auth: &Auth,
        siwc: Map<String, Value>,
        guard: &super::credential_lock::CredentialLock,
        check: &BindCheck<'_>,
    ) -> io::Result<Auth> {
        let permit = self.permit()?;
        self.persist_codex_partition_with_permit(
            &permit,
            auth,
            &super::codex_refresh::CodexPersistIntent::SiwcBind(siwc),
            guard,
            Some(check),
        )
        .map(|p| p.auth)
    }

    /// Applies successful rotations against the locked, fresh disk baseline.
    /// Returns the record actually persisted and per-partition match results;
    /// a superseded rotation never replaces a newer disk partition.
    pub async fn apply_codex_refresh(
        self: &std::sync::Arc<Self>,
        auth: Auth,
        patch: super::codex_refresh::CredentialRefreshPatch,
    ) -> io::Result<CodexPersisted> {
        let store = self.clone();
        run_codex_write(move || {
            store.persist_codex_partition(
                &auth,
                &super::codex_refresh::CodexPersistIntent::Refresh(patch),
            )
        })
        .await
    }

    fn persist_codex_partition(
        &self,
        auth: &Auth,
        intent: &super::codex_refresh::CodexPersistIntent,
    ) -> io::Result<CodexPersisted> {
        let path = self.resolve_auth_path(auth)?;
        refuse_lock_name(&path)?;
        self.check_file(&path)?;
        let permit = self.permit()?;
        let credential = super::credential_lock::acquire_sync(&path)?;
        self.persist_codex_partition_with_permit(&permit, auth, intent, &credential, None)
    }

    /// Persists a refresh while the caller holds this credential's lock.
    /// Never reacquires that lock. A guard for any other path is refused.
    /// This synchronous API performs blocking file I/O; async callers must offload it.
    pub fn apply_codex_refresh_locked(
        &self,
        auth: &Auth,
        patch: &super::codex_refresh::CredentialRefreshPatch,
        guard: &super::credential_lock::CredentialLock,
    ) -> io::Result<CodexPersisted> {
        self.persist_codex_partition_locked(
            auth,
            &super::codex_refresh::CodexPersistIntent::Refresh(patch.clone()),
            guard,
        )
    }

    fn persist_codex_partition_locked(
        &self,
        auth: &Auth,
        intent: &super::codex_refresh::CodexPersistIntent,
        guard: &super::credential_lock::CredentialLock,
    ) -> io::Result<CodexPersisted> {
        let permit = self.permit()?;
        self.persist_codex_partition_with_permit(&permit, auth, intent, guard, None)
    }

    // The caller retains one write permit across the whole write. Resolves
    // after the caller's lock wait, so a rebind during the wait is caught by
    // the guard check in the body.
    fn persist_codex_partition_with_permit(
        &self,
        permit: &WritePermit,
        auth: &Auth,
        intent: &super::codex_refresh::CodexPersistIntent,
        guard: &super::credential_lock::CredentialLock,
        check: Option<&BindCheck<'_>>,
    ) -> io::Result<CodexPersisted> {
        let path = self.resolve_auth_path(auth)?;
        self.persist_codex_partition_at(permit, &path, auth, intent, guard, check)
    }

    /// The write body: the caller holds `_permit` for the whole write (the
    /// gate's permit survives a quiesce only when it was granted inside an
    /// operation or before the quiesce began). Takes no permit and never
    /// resolves: `path` is the file the caller means.
    fn persist_codex_partition_at(
        &self,
        _permit: &WritePermit,
        path: &Path,
        auth: &Auth,
        intent: &super::codex_refresh::CodexPersistIntent,
        guard: &super::credential_lock::CredentialLock,
        check: Option<&BindCheck<'_>>,
    ) -> io::Result<CodexPersisted> {
        refuse_lock_name(path)?;
        if !guard.guards(path)? {
            return Err(invalid(
                "auth filestore: credential lock guard is for another file",
            ));
        }
        let _save = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.check_file(path)?;
        if matches!(
            intent,
            super::codex_refresh::CodexPersistIntent::SiwcBind(_)
        ) {
            let (file, disk, file_id) = open_siwc_bind_target(path)?;
            drop(file);
            #[cfg(test)]
            super::credential_lock::test_event(
                &super::credential_lock::credential_lock_path(path)?,
                "bind-read",
            );
            refuse_siwc_bind_aliases(&self.base_dir(), path, file_id)?;
            if let Some(check) = check {
                check(&disk, file_id)?;
            }
            let (metadata, applied) =
                super::codex_refresh::merge_persist(Some(&disk), &auth.metadata, intent);
            write_atomic_no_follow(path, marshal_map(&metadata).as_bytes(), file_id)?;
            #[cfg(test)]
            super::credential_lock::test_event(
                &super::credential_lock::credential_lock_path(path)?,
                "bind-written",
            );
            let persisted = auth_from_metadata(
                path,
                &self.base_dir(),
                metadata,
                fs::symlink_metadata(path)
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .map(chrono::DateTime::<chrono::Utc>::from),
            )?
            .ok_or_else(|| invalid("auth filestore: persisted credential is empty"))?;
            return Ok(CodexPersisted {
                auth: persisted,
                applied,
            });
        }
        // Partition APIs update an existing file; explicit login creation uses save_new.
        fs::metadata(path).map_err(|error| at(&error, "stat", path))?;
        let disk = self
            .codex_disk(path, false)?
            .unwrap_or_else(|| auth.metadata.clone());
        #[cfg(test)]
        super::credential_lock::test_event(
            &super::credential_lock::credential_lock_path(path)?,
            "read",
        );
        if !disk
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("codex"))
        {
            return Err(invalid(
                "auth filestore: SIWC requires an existing Codex credential",
            ));
        }
        let intent = match intent {
            super::codex_refresh::CodexPersistIntent::Refresh(patch) => {
                super::codex_refresh::CodexPersistIntent::Refresh(
                    patch.with_pending_operations(&auth.codex_pending_refresh),
                )
            }
            intent => intent.clone(),
        };
        let (metadata, applied) =
            super::codex_refresh::merge_persist(Some(&disk), &auth.metadata, &intent);
        let raw = marshal_map(&metadata);
        write_atomic(path, raw.as_bytes(), self.file_guard.is_none())?;
        let persisted = read_auth_file(path, &self.base_dir())?
            .ok_or_else(|| invalid("auth filestore: persisted credential is empty"))?;
        Ok(CodexPersisted {
            auth: persisted,
            applied,
        })
    }

    /// A store over `base_dir`. An empty directory leaves the store
    /// unconfigured: it can then only save to absolute paths.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        // The initializer is private and needs no permit: a store that is
        // being made has written nothing.
        let store = Self::default();
        store.bind_base_dir(base_dir.as_ref());
        store
    }

    /// Sets authorization for every credential read and save target.
    pub fn with_file_guard(mut self, guard: Option<super::file_guard::FileGuard>) -> Self {
        self.file_guard = guard;
        self
    }

    /// The credential check and warning history shared with other readers.
    pub fn file_guard(&self) -> Option<super::file_guard::FileGuard> {
        self.file_guard.clone()
    }

    fn check_file(&self, path: &Path) -> io::Result<()> {
        if let Some(guard) = &self.file_guard {
            guard
                .check(path)
                .map_err(|reason| io::Error::new(io::ErrorKind::PermissionDenied, reason))?;
        }
        Ok(())
    }

    fn check_read(&self, path: &Path) -> io::Result<()> {
        if let Some(guard) = &self.file_guard {
            guard
                .check_read(path)
                .map_err(|reason| io::Error::new(io::ErrorKind::PermissionDenied, reason))?;
        }
        Ok(())
    }

    /// Whether removing `path` takes the Codex credential lock. Not
    /// upstream's: a credential the file guard refuses isn't read to tell,
    /// so it's removed without the lock, as before SIWC.
    fn locks_removal(&self, path: &Path) -> bool {
        self.check_read(path).is_ok() && codex_file_mutation(path, None)
    }

    /// Not upstream's: puts the store under `gate`, which every writer of
    /// the store then asks for a permit.
    pub fn set_gate(&self, gate: ActiveGate) {
        *self.gate.write().unwrap_or_else(PoisonError::into_inner) = gate;
    }

    /// The gate this store's writers ask.
    pub fn gate(&self) -> ActiveGate {
        self.gate
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn permit(&self) -> io::Result<WritePermit> {
        Ok(self.gate().write_permit()?)
    }

    fn bind_base_dir(&self, base_dir: &Path) {
        let dir = trim_path(base_dir);
        let mut bound = self
            .base_dir
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // Counted while the write lock is held, so a reader that resolved a
        // path under the old directory sees the count move.
        self.binding_generation.fetch_add(1, Ordering::SeqCst);
        *bound = dir;
    }

    /// Changes the auth directory. Surrounding whitespace is ignored. A
    /// store whose gate is not open keeps its directory, and this logs
    /// why; use [`try_set_base_dir`](Self::try_set_base_dir) to see the
    /// refusal, or [`set_base_dir_under`](Self::set_base_dir_under) to bind
    /// while the gate is closed.
    pub fn set_base_dir(&self, base_dir: impl AsRef<Path>) {
        if let Err(error) = self.try_set_base_dir(base_dir) {
            tracing::warn!(%error, "auth directory not changed");
        }
    }

    /// As [`set_base_dir`](Self::set_base_dir), returning the gate's refusal.
    ///
    /// # Errors
    /// [`NotActive`](crate::gate::NotActive), as an [`io::Error`], when the
    /// gate refuses a write.
    pub fn try_set_base_dir(&self, base_dir: impl AsRef<Path>) -> io::Result<()> {
        let _permit = self.permit()?;
        self.bind_base_dir(base_dir.as_ref());
        Ok(())
    }

    /// Binds the auth directory with an [`Activation`], which the take-over
    /// path holds while the gate is closed.
    ///
    /// # Errors
    /// [`NotActive`](crate::gate::NotActive), as an [`io::Error`], when the
    /// activation is not for this store's gate, or that gate has opened
    /// since it was made.
    pub fn set_base_dir_under(
        &self,
        activation: &Activation,
        base_dir: impl AsRef<Path>,
    ) -> io::Result<()> {
        // The check and the binding are one step under the gate's mutex, so
        // the gate can't reopen between them.
        activation.bind(&self.gate(), || self.bind_base_dir(base_dir.as_ref()))?;
        Ok(())
    }

    /// The auth directory, empty when unconfigured.
    pub fn base_dir(&self) -> PathBuf {
        self.base_dir
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Saves `auth` from runtime state, as after a refresh, and updates its
    /// path attributes. Returns the file's path, or an empty string when
    /// `auth` is disabled and its file is gone.
    pub fn save_auth(&self, auth: &mut Auth) -> io::Result<String> {
        Ok(self.save_with(auth, false, None, None)?.into_path())
    }

    /// Saves a credential from a login or migration: as
    /// [`save_auth`](Self::save_auth), but creates the file even for a
    /// disabled credential.
    pub fn save_new_auth(&self, auth: &mut Auth) -> io::Result<String> {
        Ok(self.save_with(auth, true, None, None)?.into_path())
    }

    /// Before a login saves `auth` over an existing file, copies that
    /// file's settings into it: every key `auth` lacks except tokens, and
    /// the file's `disabled` flag unless `auth` sets its own.
    pub fn merge_existing(&self, auth: &mut Auth) {
        if let Some(existing) = self.existing_metadata(auth)
            && !existing.is_empty()
        {
            merge_existing_auth_metadata(auth, &existing);
        }
    }

    /// The JSON object in the file a login would save `auth` over: the one
    /// named by its file name, else its ID, under the auth directory. `None`
    /// when the store is unconfigured, or the file is missing, empty,
    /// unreadable or not a JSON object, where upstream's `json.Unmarshal`
    /// leaves its map nil.
    pub fn existing_metadata(&self, auth: &Auth) -> Option<Map<String, Value>> {
        self.read_existing_metadata(auth, read_capped)
            .ok()
            .flatten()
    }

    /// As [`existing_metadata`](Self::existing_metadata), with the file
    /// read by `read`, whose error is returned: for a caller that reads it
    /// more strictly, as without following a link.
    pub fn read_existing_metadata(
        &self,
        auth: &Auth,
        read: impl FnOnce(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<Option<Map<String, Value>>> {
        let dir = self.base_dir();
        if dir.as_os_str().is_empty() {
            return Ok(None);
        }
        let target = if auth.file_name.is_empty() {
            &auth.id
        } else {
            &auth.file_name
        };
        if target.is_empty() {
            return Ok(None);
        }
        let path = join(&dir, Path::new(target));
        self.check_read(&path)?;
        let raw = read(&path)?;
        if raw.is_empty() {
            return Ok(None);
        }
        Ok(unmarshal_object(&raw).ok().flatten())
    }

    /// The path of the file `name` in the auth directory. `name` must be
    /// one plain path component: no separator, no `.` or `..`, and no drive
    /// or root. Callers check anything stricter, such as the `.json`
    /// suffix or the names Windows reserves.
    pub fn file_path(&self, name: &str) -> io::Result<PathBuf> {
        let mut components = Path::new(name).components();
        let plain = matches!(
            (components.next(), components.next()),
            (Some(std::path::Component::Normal(_)), None)
        );
        if !plain || name.contains(['/', '\\']) {
            return Err(invalid("auth filestore: invalid file name"));
        }
        let dir = self.base_dir();
        if dir.as_os_str().is_empty() {
            return Err(not_configured());
        }
        Ok(dir.join(name))
    }

    /// Writes `data` as the file `name` in the auth directory, as it is,
    /// creating the directory if needed (mode 0700 on Unix), and returns
    /// the file's path. `name` is checked as [`file_path`](Self::file_path)
    /// checks it.
    ///
    /// The write is atomic, as a save's: a temporary file in the directory,
    /// renamed over the target, so the watcher and readers see the old file
    /// or the new one. A new file gets mode 0600 and an existing one keeps
    /// its mode (on Unix). A symlink at `name` is written through to its
    /// target, and a dangling one is replaced by the file, unless a file
    /// guard is configured, which refuses links before any write. Writes
    /// and saves of this store take turns. Codex uploads take the credential's
    /// process and OS locks, but retain this API's explicit byte replacement
    /// semantics. Names ending in `.lock` are reserved and refused.
    pub fn write_file(&self, name: &str, data: &[u8]) -> io::Result<PathBuf> {
        let path = self.file_path(name)?;
        refuse_lock_name(&path)?;
        let _permit = self.permit()?;
        self.check_file(&path)?;
        if let Some(parent) = path.parent() {
            create_dir_all(parent).map_err(|err| wrap(err, "auth filestore: create dir failed"))?;
        }
        let _credential = if codex_file_mutation(&path, Some(data)) {
            Some(super::credential_lock::acquire_sync(&path)?)
        } else {
            None
        };
        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.check_file(&path)?;
        write_atomic(&path, data, self.file_guard.is_none())
            .map_err(|err| wrap(err, "auth filestore: write file failed"))?;
        Ok(path)
    }

    /// Removes the file `name` from the auth directory, and returns its
    /// path. `name` is checked as [`file_path`](Self::file_path) checks it.
    /// A missing file is an error of kind [`io::ErrorKind::NotFound`],
    /// unlike [`AuthStore::delete`]'s. A symlink is removed itself, not its
    /// target.
    pub fn remove_file(&self, name: &str) -> io::Result<PathBuf> {
        let path = self.file_path(name)?;
        refuse_lock_name(&path)?;
        let _permit = self.permit()?;
        let _credential = if self.locks_removal(&path) {
            Some(super::credential_lock::acquire_sync(&path)?)
        } else {
            None
        };
        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        fs::remove_file(&path).map_err(|err| wrap(err, "auth filestore: delete failed"))?;
        Ok(path)
    }

    fn save_with(
        &self,
        auth: &mut Auth,
        creation_intent: bool,
        bound: Option<(&CodexOrigin, &mut RefusedFn<'_>)>,
        mut guard: Option<&mut SaveGuard<'_>>,
    ) -> io::Result<SaveWith> {
        // A SIWC bind is saved only by bind_siwc_locked, whose strict
        // persistence refuses links, aliases and corrupt files.
        if matches!(
            auth.codex_persist_intent,
            super::codex_refresh::CodexPersistIntent::SiwcBind(_)
        ) {
            return Err(invalid(
                "auth filestore: a SIWC bind is saved only through bind_siwc_locked",
            ));
        }
        normalize_credential_metadata(&mut auth.metadata);
        validate_weights(&auth.attributes, &auth.metadata)
            .map_err(|err| invalid(format!("auth filestore: {err}")))?;

        let path = self.resolve_auth_path(auth)?;
        if path.as_os_str().is_empty() {
            return Err(invalid(format!(
                "auth filestore: missing file path attribute for {}",
                auth.id
            )));
        }
        refuse_lock_name(&path)?;
        let Some(path_str) = path.to_str().map(str::to_owned) else {
            return Err(invalid("auth filestore: path is not valid UTF-8"));
        };

        self.check_file(&path)?;

        // A runtime save must not bring back a disabled credential whose
        // file was deliberately removed.
        if auth.disabled
            && !creation_intent
            && fs::metadata(&path).is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
        {
            return Ok(SaveWith::Written(String::new()));
        }

        let _permit = self.permit()?;
        let codex = super::codex_refresh::is_codex_auth(auth);
        let _credential_lock = if codex {
            Some(super::credential_lock::acquire_sync(&path)?)
        } else {
            None
        };
        // Fence 2: the path this call locks and writes must be the origin's
        // file. A record that is not Codex holds no credential lock, so its
        // check is lexical and a difference is never `Different`.
        if let Some((origin, refused)) = bound {
            let current = CodexOrigin::of(&path)?;
            let proof = if codex {
                origin.prove(&current)
            } else if *origin == current {
                OriginProof::Same
            } else {
                OriginProof::Unproven
            };
            if proof != OriginProof::Same {
                refused(&current, proof);
                return Ok(SaveWith::OriginRefused);
            }
        }
        // Stage G: the guard decides which partitions stay as disk holds them.
        let withheld = match guard.as_mut() {
            None => WithheldPartitions::default(),
            Some(guard) => match guard(Some(&CodexOrigin::of(&path)?)) {
                Ok(withheld) => withheld,
                Err(reason) => return Ok(SaveWith::Deferred(reason)),
            },
        };
        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        self.check_file(&path)?;
        // Recheck after both locks: deletion while acquisition waited must not
        // be undone by a runtime save of a disabled credential.
        if auth.disabled
            && !creation_intent
            && fs::metadata(&path).is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
        {
            return Ok(SaveWith::Written(String::new()));
        }

        // Stage W: a withheld partition needs a destination that holds it.
        // The read is made under the credential lock and is reused below.
        let mut locked_read = None;
        if withheld.any() {
            if !codex {
                return Ok(SaveWith::Deferred(SaveDeferral::Unsupported));
            }
            let Some(locked) = self.codex_disk(&path, false)? else {
                return Ok(SaveWith::Deferred(SaveDeferral::MissingFile));
            };
            for (on, partition) in [
                (withheld.outer, CodexCredentialPartition::Outer),
                (withheld.siwc, CodexCredentialPartition::Siwc),
            ] {
                if on
                    && super::codex_refresh::holds_partition(&auth.metadata, partition)
                    && !super::codex_refresh::holds_partition(&locked, partition)
                {
                    return Ok(SaveWith::Deferred(SaveDeferral::MissingPartition));
                }
            }
            locked_read = Some(Some(locked));
        }

        if let Some(parent) = path.parent() {
            create_dir_all(parent).map_err(|err| wrap(err, "auth filestore: create dir failed"))?;
        }

        if auth.metadata.is_empty() {
            return Err(invalid(format!(
                "auth filestore: nothing to persist for {}",
                auth.id
            )));
        }
        auth.metadata
            .insert("disabled".to_owned(), Value::Bool(auth.disabled));
        if codex {
            let explicit = creation_intent
                || matches!(
                    auth.codex_persist_intent,
                    super::codex_refresh::CodexPersistIntent::OuterLogin
                );
            let disk = match locked_read.take() {
                Some(locked) => locked,
                None => self.codex_disk(&path, explicit)?,
            };
            #[cfg(test)]
            super::credential_lock::test_event(
                &super::credential_lock::credential_lock_path(&path)?,
                "read",
            );
            let intent = if creation_intent {
                super::codex_refresh::CodexPersistIntent::OuterLogin
            } else {
                match &auth.codex_persist_intent {
                    super::codex_refresh::CodexPersistIntent::Refresh(patch) => {
                        super::codex_refresh::CodexPersistIntent::Refresh(
                            patch.with_pending_operations(&auth.codex_pending_refresh),
                        )
                    }
                    intent => intent.clone(),
                }
            };
            auth.metadata =
                super::codex_refresh::merge_persist(disk.as_ref(), &auth.metadata, &intent).0;
            if matches!(
                intent,
                super::codex_refresh::CodexPersistIntent::Configuration
            ) && !auth.codex_pending_refresh.is_empty()
            {
                // Check the original locked disk, not the settings merge's token fields.
                let baseline = disk.as_ref().unwrap_or(&auth.metadata);
                let patch = super::codex_refresh::CredentialRefreshPatch::default()
                    .with_pending_operations(&auth.codex_pending_refresh);
                let (rotated, applied) = patch.apply(baseline);
                if applied.outer == Some(true) {
                    for key in super::codex_refresh::OUTER_KEYS {
                        if let Some(value) = rotated.get(*key) {
                            auth.metadata.insert((*key).into(), value.clone());
                        }
                    }
                }
                if applied.siwc == Some(true)
                    && let Some(value) = rotated.get("siwc")
                {
                    auth.metadata.insert("siwc".into(), value.clone());
                }
                normalize_credential_metadata(&mut auth.metadata);
            }
            // Stage C: each withheld partition is exactly what disk holds.
            if withheld.any()
                && let Some(locked) = disk.as_ref()
            {
                super::codex_refresh::constrain_withheld(&mut auth.metadata, locked, withheld);
            }
            if let Some(disabled) = auth.metadata.get("disabled").and_then(Value::as_bool) {
                auth.disabled = disabled;
            }
        }
        let raw = marshal_map(&auth.metadata);
        match read_capped(&path) {
            Ok(existing) => {
                if !json_equal(&existing, raw.as_bytes()) {
                    write_atomic(&path, raw.as_bytes(), self.file_guard.is_none())
                        .map_err(|err| wrap(err, "auth filestore: write existing failed"))?;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                write_atomic(&path, raw.as_bytes(), self.file_guard.is_none())
                    .map_err(|err| wrap(err, "auth filestore: write file failed"))?;
            }
            // A file over the cap is replaced. A Codex sign-in has kept a copy
            // of it, and a runtime Codex save has already refused it in
            // `codex_disk`; a save of any other kind replaces it as before.
            Err(err) if err.kind() == io::ErrorKind::FileTooLarge => {
                write_atomic(&path, raw.as_bytes(), self.file_guard.is_none())
                    .map_err(|err| wrap(err, "auth filestore: write existing failed"))?;
            }
            Err(err) => return Err(wrap(err, "auth filestore: read existing failed")),
        }

        if codex {
            auth.codex_persist_intent = Default::default();
            auth.codex_pending_refresh.clear();
        }
        for key in [ATTRIBUTE_PATH, ATTRIBUTE_SOURCE] {
            auth.attributes.insert(key.to_owned(), path_str.clone());
        }
        auth.attributes.insert(
            ATTRIBUTE_SOURCE_BACKEND.to_owned(),
            AUTH_SOURCE_FILE.to_owned(),
        );
        if auth.file_name.trim().is_empty() {
            auth.file_name = auth.id.clone();
        }
        Ok(SaveWith::Written(path_str))
    }

    /// The Codex credential file as disk holds it, or `None` when there is
    /// none to merge. A file that can't be read is an error.
    ///
    /// An `explicit` read (a Codex sign-in) differs from a runtime one in
    /// what it does with a file that holds nothing usable. A missing or
    /// empty file is `None`. A file that isn't a JSON object, or is over
    /// [`MAX_AUTH_FILE_SIZE`], is also `None`, after a copy of it is kept
    /// beside it ([`keep_invalid_copy`]). A file that can't be read is
    /// refused, with an error that names no path, and left as it is: it may
    /// be fine, held by another process. A runtime read refuses all of them.
    ///
    /// The copy covers what this read saw: a writer that ignores the
    /// credential lock and rewrites the file mid-save isn't covered, as for
    /// every save.
    fn codex_disk(&self, path: &Path, explicit: bool) -> io::Result<Option<Map<String, Value>>> {
        let bytes = match read_capped(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::FileTooLarge && explicit => None,
            Err(error) if explicit => return Err(refuse_unreadable(error.kind())),
            Err(error) => return Err(runtime_unreadable(path, error.kind())),
        };
        if let Some(bytes) = &bytes
            && let Ok(Some(map)) = unmarshal_object(bytes)
        {
            return Ok(Some(map));
        }
        if !explicit {
            return Err(runtime_unreadable(path, io::ErrorKind::InvalidInput));
        }
        // Nothing to keep in an empty file.
        if bytes.as_deref().is_none_or(|bytes| !bytes.is_empty()) {
            keep_invalid_copy(path, bytes.as_deref())?;
            tracing::warn!(
                "the existing Codex credential file was not valid; a copy was kept beside it with the .invalid-<time> suffix"
            );
        }
        Ok(None)
    }

    /// The path a save writes `auth` to: its `path` attribute, else its
    /// file name, else its ID, under the auth directory unless absolute.
    pub fn resolve_auth_path(&self, auth: &Auth) -> io::Result<PathBuf> {
        let attr = auth.trimmed_attribute(ATTRIBUTE_PATH);
        if !attr.is_empty() {
            return Ok(PathBuf::from(attr));
        }
        let file_name = auth.file_name.trim();
        if !file_name.is_empty() {
            let file_name = Path::new(file_name);
            if file_name.is_absolute() {
                return Ok(file_name.to_path_buf());
            }
            let dir = self.base_dir();
            if dir.as_os_str().is_empty() {
                return Ok(file_name.to_path_buf());
            }
            return Ok(join(&dir, file_name));
        }
        if auth.id.is_empty() {
            return Err(invalid("auth filestore: missing id"));
        }
        let id = Path::new(&auth.id);
        if id.is_absolute() {
            return Ok(id.to_path_buf());
        }
        let dir = self.base_dir();
        if dir.as_os_str().is_empty() {
            return Err(not_configured());
        }
        Ok(join(&dir, id))
    }

    fn resolve_delete_path(&self, id: &str) -> io::Result<PathBuf> {
        let path = Path::new(id);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        let dir = self.base_dir();
        if !dir.as_os_str().is_empty() {
            return Ok(join(&dir, path));
        }
        if id.contains(std::path::MAIN_SEPARATOR) {
            return Ok(path.to_path_buf());
        }
        Err(not_configured())
    }

    /// Every credential file under the auth directory, in the order Go's
    /// `filepath.WalkDir` visits them: depth-first, by name.
    fn list_auths(&self) -> io::Result<Vec<Auth>> {
        let dir = self.base_dir();
        if dir.as_os_str().is_empty() {
            return Err(not_configured());
        }
        let mut out = Vec::new();
        let root = fs::metadata(&dir).map_err(|err| at(&err, "lstat", &dir))?;
        if !root.is_dir() {
            if is_json_name(dir.file_name()) {
                self.list_file(&dir, &dir, &mut out);
            }
            return Ok(out);
        }
        // One iterator per directory being read; the deepest is last.
        let mut stack = vec![read_dir_sorted(&dir)?.into_iter()];
        while let Some(entries) = stack.last_mut() {
            let Some((path, is_dir)) = entries.next() else {
                stack.pop();
                continue;
            };
            if is_dir {
                stack.push(read_dir_sorted(&path)?.into_iter());
            } else if is_json_name(path.file_name()) {
                self.list_file(&path, &dir, &mut out);
            }
        }
        Ok(out)
    }

    fn list_file(&self, path: &Path, base_dir: &Path, out: &mut Vec<Auth>) {
        if self
            .file_guard
            .as_ref()
            .is_some_and(|guard| !guard.allows_read(path))
        {
            return;
        }
        match read_auth_file(path, base_dir) {
            Ok(Some(auth)) => out.push(auth),
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(path = %path.display(), error = %err, "skipping auth file");
            }
        }
    }

    fn delete_auth(&self, id: &str) -> io::Result<()> {
        let id = id.trim();
        if id.is_empty() {
            return Err(invalid("auth filestore: id is empty"));
        }
        let path = self.resolve_delete_path(id)?;
        refuse_lock_name(&path)?;
        let _permit = self.permit()?;
        let _credential = if self.locks_removal(&path) {
            Some(super::credential_lock::acquire_sync(&path)?)
        } else {
            None
        };
        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                Err(wrap(err, "auth filestore: delete failed"))
            }
            _ => Ok(()),
        }
    }
}

/// A partition write's actual persisted record and baseline check results.
#[derive(Debug)]
pub struct CodexPersisted {
    /// Disk state after the operation, including concurrent settings.
    pub auth: Auth,
    /// Which requested partitions matched; false means disk was newer.
    pub applied: super::codex_refresh::PatchApplied,
}

async fn run_codex_write<T: Send + 'static>(
    write: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    if tokio::runtime::Handle::try_current().is_err() {
        return write();
    }
    let result = if let Some(operation) = crate::gate::Operation::current() {
        operation.run_blocking(write).await
    } else {
        // Standalone callers have no operation; the store takes its write permit
        // before acquiring either lock or writing the credential.
        #[allow(clippy::disallowed_methods)]
        tokio::task::spawn_blocking(write).await
    };
    result.map_err(|_| io::Error::other("credential persistence task failed"))?
}

impl AuthStore for FileStore {
    fn list(&self) -> io::Result<Vec<Auth>> {
        self.list_auths()
    }

    fn save(&self, auth: &Auth) -> io::Result<String> {
        self.save_auth(&mut auth.clone())
    }

    fn save_new(&self, auth: &Auth) -> io::Result<String> {
        self.save_new_auth(&mut auth.clone())
    }

    fn save_bound(
        &self,
        auth: &Auth,
        origin: &CodexOrigin,
        refused: &mut dyn FnMut(&CodexOrigin, OriginProof),
    ) -> io::Result<Option<String>> {
        Ok(
            match self.save_with(&mut auth.clone(), false, Some((origin, refused)), None)? {
                SaveWith::Written(path) => Some(path),
                SaveWith::OriginRefused | SaveWith::Deferred(_) => None,
            },
        )
    }

    fn save_guarded(
        &self,
        auth: &Auth,
        guard: &mut dyn FnMut(Option<&CodexOrigin>) -> Result<WithheldPartitions, SaveDeferral>,
    ) -> io::Result<GuardedSave> {
        Ok(
            match self.save_with(&mut auth.clone(), false, None, Some(guard))? {
                SaveWith::Written(path) => GuardedSave::Written(path),
                SaveWith::Deferred(reason) => GuardedSave::Deferred(reason),
                // Only a bound save is refused by its origin.
                SaveWith::OriginRefused => GuardedSave::Written(String::new()),
            },
        )
    }

    fn delete(&self, id: &str) -> io::Result<()> {
        self.delete_auth(id)
    }

    fn supports_codex_refresh_patches(&self) -> bool {
        true
    }

    fn same_record(&self, a: &Auth, b: &Auth) -> io::Result<bool> {
        let a = fs::canonicalize(self.resolve_auth_path(a)?)?;
        let b = fs::canonicalize(self.resolve_auth_path(b)?)?;
        #[cfg(windows)]
        {
            Ok(a.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()))
        }
        #[cfg(not(windows))]
        {
            Ok(a == b)
        }
    }

    fn binding_generation(&self) -> u64 {
        self.binding_generation.load(Ordering::SeqCst)
    }

    fn read_for_adoption(&self, auth: &Auth) -> io::Result<super::AdoptionRead> {
        use super::AdoptionRead;
        // Only Codex records have the partitioned fresh-read contract. A
        // requested Gemini record stays unsupported even if its file changes.
        if !super::codex_refresh::is_codex_auth(auth) {
            return Ok(AdoptionRead::Unsupported);
        }
        let path = self.resolve_auth_path(auth)?;
        self.check_read(&path)?;
        match read_auth_file_for_adoption(&path, &self.base_dir()) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(AdoptionRead::Missing),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
                ) =>
            {
                Ok(AdoptionRead::Invalid)
            }
            other => other,
        }
    }

    fn read_back(&self, auth: &Auth) -> io::Result<Option<Auth>> {
        let path = self.resolve_auth_path(auth)?;
        let base_dir = self.base_dir();
        self.check_read(&path)?;
        match read_auth_file(&path, &base_dir) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            other => other,
        }
    }

    fn codex_refresh_lease(&self, auth: &Auth) -> io::Result<CodexLease> {
        // No lock file is made for a record that is not Codex.
        if !super::codex_refresh::is_codex_auth(auth) {
            return Ok(CodexLease::Unsupported);
        }
        // A resolve error means the record has no file identity: no id, or a
        // relative id with no base directory.
        let Ok(path) = self.resolve_auth_path(auth) else {
            return Ok(CodexLease::Unsupported);
        };
        refuse_lock_name(&path)?;
        self.check_file(&path)?;
        // The file may not exist yet: the lock file is separate.
        let origin = CodexOrigin::of(&path)?;
        let guard = super::credential_lock::acquire_sync(&path)?;
        Ok(CodexLease::Held(CodexLeaseToken {
            path,
            guard,
            origin,
        }))
    }

    fn codex_origin(&self, auth: &Auth) -> io::Result<Option<CodexOrigin>> {
        let Ok(path) = self.resolve_auth_path(auth) else {
            return Ok(None);
        };
        CodexOrigin::of(&path).map(Some)
    }

    fn codex_lease_read(&self, token: &CodexLeaseToken) -> io::Result<Option<Map<String, Value>>> {
        if !token.guard().guards(token.path())? {
            return Err(invalid(
                "auth filestore: credential lock guard is for another file",
            ));
        }
        self.check_read(token.path())?;
        self.codex_disk(token.path(), false)
    }

    fn codex_lease_commit(
        &self,
        token: &CodexLeaseToken,
        auth: &Auth,
        patch: &super::codex_refresh::CredentialRefreshPatch,
    ) -> io::Result<CodexPersisted> {
        let permit = self.permit()?;
        self.persist_codex_partition_at(
            &permit,
            token.path(),
            auth,
            &super::codex_refresh::CodexPersistIntent::Refresh(patch.clone()),
            token.guard(),
            None,
        )
    }
}

/// Mirrors upstream, where only Codex credentials take the credential lock.
/// An upload locks if either its new object or the existing regular file is Codex.
/// Classification is advisory: a credential whose kind changes before the
/// mutation can be written or removed without the Codex lock. This provides
/// no cross-process exclusion across kind changes, but never less locking
/// than upstream's unlocked management mutations and FileTokenStore.Delete.
fn codex_file_mutation(path: &Path, uploaded: Option<&[u8]>) -> bool {
    let is_codex = |bytes: &[u8]| {
        unmarshal_object(bytes)
            .ok()
            .flatten()
            .is_some_and(|metadata| {
                super::codex_refresh::is_codex_auth(&Auth {
                    metadata,
                    ..Auth::default()
                })
            })
    };
    let codex = uploaded.is_some_and(is_codex)
        || read_regular_capped(path).is_ok_and(|bytes| is_codex(&bytes));
    #[cfg(test)]
    tests::MUTATION_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(path);
        }
    });
    codex
}

fn read_regular_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A swap to a FIFO must not block before the handle can be inspected.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    #[cfg(not(unix))]
    if !fs::metadata(path)?.is_file() {
        return Err(invalid("not a regular credential file"));
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("not a regular credential file"));
    }
    let mut data = Vec::new();
    file.take(MAX_AUTH_FILE_SIZE + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_AUTH_FILE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "credential file exceeds read cap",
        ));
    }
    Ok(data)
}

fn refuse_lock_name(path: &Path) -> io::Result<()> {
    if path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| name.to_lowercase().ends_with(".lock"))
    {
        return Err(invalid(
            "auth filestore: credential lock files are reserved",
        ));
    }
    Ok(())
}

/// The entries of `dir` sorted by name, each with whether it is a
/// directory (not following symlinks).
fn read_dir_sorted(dir: &Path) -> io::Result<Vec<(PathBuf, bool)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(|err| at(&err, "open", dir))? {
        let entry = entry.map_err(|err| at(&err, "readdir", dir))?;
        let path = join(dir, Path::new(&entry.file_name()));
        let file_type = entry.file_type().map_err(|err| at(&err, "lstat", &path))?;
        entries.push((entry.file_name(), path, file_type.is_dir()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries
        .into_iter()
        .map(|(_, path, is_dir)| (path, is_dir))
        .collect())
}

fn is_json_name(name: Option<&std::ffi::OsStr>) -> bool {
    name.and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| open_ferry_translate::go::to_lower(name).ends_with(".json"))
}

/// Reads one credential file into a record, skipping empty and Gemini files.
/// Legacy read-back still treats JSON null as an unknown record; adoption does not.
fn read_auth_file(path: &Path, base_dir: &Path) -> io::Result<Option<Auth>> {
    read_auth_file_classified(path, base_dir, false).map(|read| match read {
        super::AdoptionRead::Record(auth) => Some(auth),
        _ => None,
    })
}

fn read_auth_file_for_adoption(path: &Path, base_dir: &Path) -> io::Result<super::AdoptionRead> {
    read_auth_file_classified(path, base_dir, true)
}

fn read_auth_file_classified(
    path: &Path,
    base_dir: &Path,
    adoption: bool,
) -> io::Result<super::AdoptionRead> {
    use super::AdoptionRead;
    if path.to_str().is_none() {
        return Err(invalid("path is not valid UTF-8"));
    }
    let data = read_capped(path).map_err(|err| wrap(err, "read file"))?;
    if data.is_empty() {
        return Ok(AdoptionRead::Invalid);
    }
    let metadata =
        unmarshal_object(&data).map_err(|err| invalid(format!("unmarshal auth json: {err}")))?;
    if adoption && metadata.as_ref().is_none_or(|metadata| metadata.is_empty()) {
        return Ok(AdoptionRead::Invalid);
    }
    record_from_metadata(
        path,
        base_dir,
        metadata.unwrap_or_default(),
        adoption,
        || {
            Ok(fs::metadata(path)
                .map_err(|err| wrap(err, "stat file"))?
                .modified()
                .ok()
                .map(chrono::DateTime::<chrono::Utc>::from))
        },
    )
}

/// The record for credential metadata already in hand, built as a file read
/// builds it.
fn auth_from_metadata(
    path: &Path,
    base_dir: &Path,
    metadata: Map<String, Value>,
    modified: Option<chrono::DateTime<chrono::Utc>>,
) -> io::Result<Option<Auth>> {
    record_from_metadata(path, base_dir, metadata, false, || Ok(modified)).map(|read| match read {
        super::AdoptionRead::Record(auth) => Some(auth),
        _ => None,
    })
}

/// Classifies credential metadata. `modified` runs only once the provider
/// checks pass, where upstream stats the file.
fn record_from_metadata(
    path: &Path,
    base_dir: &Path,
    mut metadata: Map<String, Value>,
    adoption: bool,
    modified: impl FnOnce() -> io::Result<Option<chrono::DateTime<chrono::Utc>>>,
) -> io::Result<super::AdoptionRead> {
    use super::AdoptionRead;
    let path_str = path
        .to_str()
        .ok_or_else(|| invalid("path is not valid UTF-8"))?;
    normalize_credential_metadata(&mut metadata);
    validate_weights(&Default::default(), &metadata).map_err(|err| invalid(err.to_string()))?;

    let provider = metadata
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if adoption && !equal_fold(&provider, "codex") {
        return Ok(AdoptionRead::Invalid);
    }
    if equal_fold(&provider, "gemini") {
        return Ok(AdoptionRead::Unsupported);
    }
    let provider = if provider.is_empty() {
        "unknown".to_owned()
    } else {
        provider
    };
    let modified = modified()?;

    let id = id_for(path, path_str, base_dir);
    let disabled = metadata
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let proxy_url = metadata
        .get("proxy_url")
        .and_then(Value::as_str)
        .map(|proxy| proxy.trim().to_owned())
        .unwrap_or_default();
    let prefix = metadata
        .get("prefix")
        .and_then(Value::as_str)
        .map(clean_prefix)
        .unwrap_or_default();
    let label = label_for(&metadata);
    let email = metadata
        .get("email")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .map(str::to_owned);

    let mut auth = Auth {
        id: id.clone(),
        provider,
        file_name: id,
        label,
        prefix,
        proxy_url,
        status: if disabled {
            Status::Disabled
        } else {
            Status::Active
        },
        disabled,
        metadata,
        created_at: modified,
        updated_at: modified,
        ..Auth::default()
    };
    for key in [ATTRIBUTE_PATH, ATTRIBUTE_SOURCE] {
        auth.attributes.insert(key.to_owned(), path_str.to_owned());
    }
    auth.attributes.insert(
        ATTRIBUTE_SOURCE_BACKEND.to_owned(),
        AUTH_SOURCE_FILE.to_owned(),
    );
    if let Some(email) = email {
        auth.attributes.insert("email".to_owned(), email);
    }
    apply_custom_headers_from_metadata(&mut auth);
    Ok(AdoptionRead::Record(auth))
}

/// A credential file's ID: its path relative to the auth directory, else
/// the path itself; lowercased on Windows, whose paths ignore case.
/// The stable credential id used by file reads and synthesis.
#[doc(hidden)]
pub fn id_for(path: &Path, path_str: &str, base_dir: &Path) -> String {
    let mut id = path_str.to_owned();
    if !base_dir.as_os_str().is_empty()
        && let Some(relative) = rel(base_dir, path)
        && let Some(relative) = relative.to_str()
        && !relative.is_empty()
    {
        id = relative.to_owned();
    }
    if cfg!(windows) {
        id = open_ferry_translate::go::to_lower(&id);
    }
    id
}

/// A credential file's `prefix`: trimmed of spaces and slashes, and dropped
/// if it still holds a slash.
pub(crate) fn clean_prefix(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed.contains('/') {
        String::new()
    } else {
        trimmed.to_owned()
    }
}

fn label_for(metadata: &serde_json::Map<String, Value>) -> String {
    ["label", "email", "project_id"]
        .into_iter()
        .find_map(|key| {
            metadata
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_default()
        .to_owned()
}

fn runtime_unreadable(path: &Path, kind: io::ErrorKind) -> io::Error {
    io::Error::new(
        kind,
        format!(
            "auth filestore: cannot read existing credential {}",
            path.display()
        ),
    )
}

/// The error for a credential file a sign-in can't read. The text names no
/// path or file name.
fn refuse_unreadable(kind: io::ErrorKind) -> io::Error {
    io::Error::new(
        kind,
        "auth filestore: cannot read the existing credential file, so the sign-in did not replace it",
    )
}

/// The error for a credential file a sign-in could read but not copy. The
/// text names no path or file name.
fn refuse_uncopyable(kind: io::ErrorKind) -> io::Error {
    io::Error::new(
        kind,
        "auth filestore: cannot keep a copy of the existing credential file, so the sign-in did not replace it",
    )
}

/// The error for a credential file that is not a regular file (a device, a
/// FIFO, a directory behind a link), which a sign-in won't copy. The text
/// names no path or file name.
fn refuse_not_regular() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "auth filestore: the existing credential file is not a regular file, so the sign-in did not replace it",
    )
}

/// Keeps a copy of the credential file at `path` beside it, named after it
/// with `.invalid-<UTC time>` added (`-2`, `-3` and so on when that name is
/// taken), before a sign-in replaces it. The name ends in neither `.json`
/// nor `.lock`, so nothing loads it as a credential. `bytes` is what was
/// read; `None` copies a file over [`MAX_AUTH_FILE_SIZE`] from disk, which
/// must be a regular file and is copied for no more than the length it had
/// when opened. The copy is created new, mode 0600 on Unix, and synced. If
/// it can't be made, the sign-in is refused with [`refuse_uncopyable`]'s
/// error (or [`refuse_not_regular`]'s, before any copy is created), and a
/// partial copy is removed on a best-effort basis: a failure to remove it
/// is ignored.
fn keep_invalid_copy(path: &Path, bytes: Option<&[u8]>) -> io::Result<()> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    keep_invalid_copy_stamped(path, bytes, &stamp)
}

/// [`keep_invalid_copy`] with the time part of the name given.
fn keep_invalid_copy_stamped(path: &Path, bytes: Option<&[u8]>, stamp: &str) -> io::Result<()> {
    let refuse = |error: io::Error| refuse_uncopyable(error.kind());
    let Some(name) = path.file_name() else {
        return Err(refuse_uncopyable(io::ErrorKind::InvalidInput));
    };
    // Open the source once, and judge the handle, before any copy exists.
    let mut source = None;
    if bytes.is_none() {
        let file = fs::File::open(path).map_err(refuse)?;
        let metadata = file.metadata().map_err(refuse)?;
        if !metadata.is_file() {
            return Err(refuse_not_regular());
        }
        source = Some((file, metadata.len()));
    }
    for attempt in 1..=1000u32 {
        let mut copy_name = name.to_owned();
        copy_name.push(format!(".invalid-{stamp}"));
        if attempt > 1 {
            copy_name.push(format!("-{attempt}"));
        }
        let copy = path.with_file_name(copy_name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&copy) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(refuse(error)),
        };
        let written = match (bytes, source.as_mut()) {
            (Some(bytes), _) => file.write_all(bytes),
            (None, Some((source, len))) => {
                // One byte past the length it had, to notice a file that grew;
                // fewer than that length is a short copy.
                let len = *len;
                io::copy(&mut source.take(len + 1), &mut file).and_then(|copied| {
                    match copied.cmp(&len) {
                        std::cmp::Ordering::Equal => Ok(()),
                        std::cmp::Ordering::Greater => Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the file grew while it was copied",
                        )),
                        std::cmp::Ordering::Less => Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "the file shrank while it was copied",
                        )),
                    }
                })
            }
            (None, None) => Err(io::Error::from(io::ErrorKind::InvalidInput)),
        }
        .and_then(|()| file.sync_all());
        if let Err(error) = written {
            drop(file);
            let _ = fs::remove_file(&copy);
            return Err(refuse(error));
        }
        return Ok(());
    }
    Err(refuse_uncopyable(io::ErrorKind::AlreadyExists))
}

/// Reads a file of at most [`MAX_AUTH_FILE_SIZE`] bytes.
pub fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    fs::File::open(path)?
        .take(MAX_AUTH_FILE_SIZE + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_AUTH_FILE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("file exceeds {MAX_AUTH_FILE_SIZE} bytes"),
        ));
    }
    Ok(data)
}

// Bind-only replacement: never resolve a link destination. The directory must
// be trusted against uncooperative writers; rename replaces the leaf entry.
fn write_atomic_no_follow(path: &Path, data: &[u8], expected: FileId) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    require_bind_file(&metadata)?;
    let directory = path
        .parent()
        .ok_or_else(|| invalid("invalid SIWC bind directory"))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".")
        .suffix(".tmp")
        .tempfile_in(directory)?;
    #[cfg(unix)]
    temp.as_file().set_permissions(metadata.permissions())?;
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    #[cfg(test)]
    super::credential_lock::test_event(
        &super::credential_lock::credential_lock_path(path)?,
        "bind-before-replace",
    );
    let (file, _, current) = open_siwc_bind_target(path)
        .map_err(|_| invalid("SIWC bind target changed before replacement"))?;
    if current != expected {
        return Err(invalid("SIWC bind target changed before replacement"));
    }
    drop(file);
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Replaces the file at `path` with `data` through a temporary file in the
/// same directory, so readers see the old file or the new one, never a
/// partial write.
fn write_atomic(path: &Path, data: &[u8], follow_links: bool) -> io::Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(meta) if follow_links && meta.file_type().is_symlink() => {
            fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    };
    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut temp = tempfile::Builder::new()
        .prefix(".")
        .suffix(".tmp")
        .tempfile_in(&dir)?;
    // New files keep tempfile's 0600; an existing file keeps its mode.
    #[cfg(unix)]
    if let Ok(existing) = fs::metadata(&target) {
        temp.as_file().set_permissions(existing.permissions())?;
    }
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(&target).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn create_dir_all(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
pub(crate) fn create_dir_all(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

fn trim_path(path: &Path) -> PathBuf {
    match path.to_str() {
        Some(text) => PathBuf::from(text.trim()),
        None => path.to_path_buf(),
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn not_configured() -> io::Error {
    io::Error::other("auth filestore: directory not configured")
}

#[derive(Debug)]
struct ContextError {
    context: String,
    source: io::Error,
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.context, self.source)
    }
}

impl std::error::Error for ContextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn wrap(err: io::Error, context: &str) -> io::Error {
    io::Error::new(
        err.kind(),
        ContextError {
            context: context.to_owned(),
            source: err,
        },
    )
}

fn at(err: &io::Error, op: &str, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{op} {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    fn guarded_scratch() -> tempfile::TempDir {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    #[test]
    fn a_file_guard_refuses_existing_reads_and_saves_but_allows_new_and_trusted_files() {
        use super::super::file_guard::FileGuard;
        let dir = guarded_scratch();
        let unsafe_path = dir.path().join("unsafe.json");
        fs::write(&unsafe_path, br#"{"type":"codex"}"#).unwrap();
        fs::write(dir.path().join("trusted.json"), br#"{"type":"codex"}"#).unwrap();
        let store = FileStore::new(dir.path()).with_file_guard(Some(FileGuard::new(|path| {
            if path.file_name().unwrap() == "unsafe.json" {
                Err("untrusted owner".to_owned())
            } else {
                Ok(())
            }
        })));
        assert_eq!(store.list().unwrap().len(), 1);
        let mut auth = Auth {
            id: "unsafe.json".into(),
            metadata: meta(json!({"type":"codex", "n":1})),
            ..Auth::default()
        };
        let reads = std::cell::Cell::new(0);
        assert!(
            store
                .read_existing_metadata(&auth, |_| {
                    reads.set(reads.get() + 1);
                    Ok(Vec::new())
                })
                .is_err()
        );
        assert_eq!(reads.get(), 0);
        assert!(store.read_back(&auth).is_err());
        assert!(store.save_auth(&mut auth).is_err());
        assert!(store.save_new_auth(&mut auth).is_err());
        assert!(store.write_file("unsafe.json", b"{}").is_err());
        assert_eq!(fs::read(&unsafe_path).unwrap(), br#"{"type":"codex"}"#);
        auth.id = "new.json".into();
        store.save_new_auth(&mut auth).unwrap();
        store.write_file("trusted.json", b"{}").unwrap();
        assert!(dir.path().join("new.json").exists());
        FileStore::new(dir.path())
            .write_file("unsafe.json", b"{}")
            .unwrap();
        assert_eq!(fs::read(&unsafe_path).unwrap(), b"{}");
    }

    // Not upstream's: a guard that refuses a credential also refuses its SIWC
    // bind read, fresh read, refresh and bind, and no lock file is made for it.
    #[test]
    fn a_file_guard_refuses_siwc_reads_binds_and_refreshes() {
        use super::super::codex_refresh::{CodexPersistIntent, CredentialRefreshPatch};
        use super::super::credential_lock::{acquire_sync, credential_lock_path};
        use super::super::file_guard::FileGuard;
        let dir = guarded_scratch();
        let data = br#"{"type":"codex","access_token":"a","refresh_token":"r"}"#;
        let unsafe_path = dir.path().join("unsafe.json");
        let trusted_path = dir.path().join("trusted.json");
        fs::write(&unsafe_path, data).unwrap();
        fs::write(&trusted_path, data).unwrap();
        let store = FileStore::new(dir.path()).with_file_guard(Some(FileGuard::new(|path| {
            if path.file_name().unwrap() == "unsafe.json" {
                Err("untrusted owner".to_owned())
            } else {
                Ok(())
            }
        })));
        let denied = |error: io::Error| {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        };
        let refresh = || CodexPersistIntent::Refresh(CredentialRefreshPatch::default());
        let auth = Auth {
            id: "unsafe.json".into(),
            file_name: "unsafe.json".into(),
            provider: "codex".into(),
            metadata: meta(json!({"type":"codex", "access_token":"a", "refresh_token":"r"})),
            ..Auth::default()
        };
        denied(store.read_bind_target(&unsafe_path).err().unwrap());
        denied(store.read_for_adoption(&auth).err().unwrap());
        denied(
            store
                .persist_codex_partition(&auth, &refresh())
                .err()
                .unwrap(),
        );
        assert!(!credential_lock_path(&unsafe_path).unwrap().exists());
        let guard = acquire_sync(&unsafe_path).unwrap();
        denied(
            store
                .apply_codex_refresh_locked(&auth, &CredentialRefreshPatch::default(), &guard)
                .err()
                .unwrap(),
        );
        denied(
            store
                .bind_siwc_locked(&auth, Map::new(), &guard, &|_, _| Ok(()))
                .err()
                .unwrap(),
        );
        drop(guard);
        assert!(fs::read(&unsafe_path).unwrap() == data);
        let trusted = Auth {
            id: "trusted.json".into(),
            file_name: "trusted.json".into(),
            ..auth
        };
        store.read_bind_target(&trusted_path).unwrap();
        store.read_for_adoption(&trusted).unwrap();
        store.persist_codex_partition(&trusted, &refresh()).unwrap();
    }

    // Not upstream's: a removal doesn't read a credential its guard refuses
    // to tell whether it's Codex's, so it removes it without the lock.
    #[test]
    fn a_file_guard_removes_a_refused_credential_without_reading_it() {
        use super::super::credential_lock::credential_lock_path;
        use super::super::file_guard::FileGuard;
        let dir = guarded_scratch();
        let data = br#"{"type":"codex","access_token":"a","refresh_token":"r"}"#;
        let store = FileStore::new(dir.path()).with_file_guard(Some(FileGuard::new(|path| {
            if path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("unsafe")
            {
                Err("untrusted owner".to_owned())
            } else {
                Ok(())
            }
        })));
        for name in ["unsafe.json", "unsafe-auth.json", "trusted.json"] {
            let path = dir.path().join(name);
            fs::write(&path, data).unwrap();
            if name == "unsafe-auth.json" {
                store.delete_auth(name).unwrap();
            } else {
                store.remove_file(name).unwrap();
            }
            assert!(!path.exists());
            assert_eq!(
                credential_lock_path(&path).unwrap().exists(),
                name == "trusted.json",
                "{name}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_file_guard_never_loads_or_saves_through_a_link() {
        use super::super::file_guard::FileGuard;
        let dir = guarded_scratch();
        let auth_dir = dir.path().join("auth");
        fs::create_dir(&auth_dir).unwrap();
        let target = dir.path().join("target.json");
        fs::write(&target, br#"{"type":"codex"}"#).unwrap();
        let link = auth_dir.join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let store = FileStore::new(&auth_dir).with_file_guard(Some(FileGuard::new(|_| Ok(()))));
        assert!(store.list().unwrap().is_empty());
        assert!(store.write_file("link.json", b"{}").is_err());
        let mut auth = Auth {
            id: "link.json".into(),
            metadata: meta(json!({"type":"codex", "n":1})),
            ..Auth::default()
        };
        assert!(store.save_new_auth(&mut auth).is_err());
        assert_eq!(fs::read(&target).unwrap(), br#"{"type":"codex"}"#);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        assert_eq!(fs::read_dir(&auth_dir).unwrap().count(), 1);
        FileStore::new(&auth_dir)
            .write_file("link.json", b"{}")
            .unwrap();
        assert_eq!(fs::read(target).unwrap(), b"{}");
    }

    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn meta(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn path_string(path: &Path) -> String {
        path.to_str().unwrap().to_owned()
    }

    #[test]
    fn bind_identity_tolerates_refresh_but_checks_the_user_and_file_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");
        write(
            &path,
            r#"{"type":"codex","access_token":"test","refresh_token":"test","account_id":"workspace","email":"first@example.test"}"#,
        );
        let (before, id) = read_siwc_bind_target(&path).unwrap();
        let identity = BindIdentity::capture(&before, id);
        let mut after = before.clone();
        after.insert("access_token".into(), json!("rotated-test"));
        write_atomic_no_follow(&path, marshal_map(&after).as_bytes(), id).unwrap();
        let (fresh, fresh_id) = read_siwc_bind_target(&path).unwrap();
        assert!(id != fresh_id);
        assert!(identity.check(&fresh, fresh_id).is_ok());
        after.insert("email".into(), json!("second@example.test"));
        assert!(identity.check(&after, fresh_id).is_err());
        after = before.clone();
        after.insert("account_id".into(), json!("other-workspace"));
        assert!(identity.check(&after, fresh_id).is_err());
        after.remove("email");
        let fallback = BindIdentity::capture(&after, id);
        assert!(fallback.check(&after, id).is_ok());
        assert!(fallback.check(&after, fresh_id).is_err());
        // JWT user claims are usable substitution markers, not verified identity.
        use base64::Engine as _;
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"first-test"}"#);
        after.insert("id_token".into(), json!(format!("x.{payload}.x")));
        let claims = BindIdentity::capture(&after, id);
        assert!(claims.check(&after, fresh_id).is_ok());
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"second-test"}"#);
        after.insert("id_token".into(), json!(format!("x.{payload}.x")));
        assert!(claims.check(&after, fresh_id).is_err());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn bind_reader_and_writer_refuse_leaf_file_links() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        let link = dir.path().join("link.json");
        let data = br#"{"type":"codex","access_token":"test","refresh_token":"test"}"#;
        fs::write(&target, data).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &link) {
            if error.raw_os_error() == Some(1314) {
                eprintln!("UNVERIFIED: Windows file-symlink coverage needs privilege (OS 1314)");
                return;
            }
            panic!(
                "file-symlink fixture failed ({:?}, OS {:?})",
                error.kind(),
                error.raw_os_error()
            );
        }
        let (_, id) = read_siwc_bind_target(&target).unwrap();
        let guard = super::super::credential_lock::acquire_sync(&target).unwrap();
        assert_eq!(
            refuse_siwc_bind_aliases(dir.path(), &target, id)
                .err()
                .unwrap()
                .to_string(),
            "SIWC bind target has a linked alias"
        );
        let store = FileStore::new(dir.path());
        let auth = Auth {
            file_name: "target.json".into(),
            ..Auth::default()
        };
        assert_eq!(
            store
                .bind_siwc_locked(&auth, Map::new(), &guard, &|_, _| Ok(()))
                .err()
                .unwrap()
                .to_string(),
            "SIWC bind target has a linked alias"
        );
        drop(guard);
        assert!(read_siwc_bind_target(&link).is_err());
        assert!(write_atomic_no_follow(&link, b"replacement-test", id).is_err());
        assert!(fs::read(&target).unwrap() == data);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn bind_refuses_alias_refresh_between_read_and_scan() {
        use std::sync::{Arc, Mutex, mpsc};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("real.json");
        let alias = dir.path().join("alias.json");
        write(
            &path,
            r#"{"type":"codex","access_token":"selected","refresh_token":"selected"}"#,
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(&path, &alias) {
            if error.raw_os_error() == Some(1314) {
                eprintln!(
                    "UNVERIFIED: alias-refresh race needs Windows symlink privilege (OS 1314)"
                );
                return;
            }
            panic!("file-symlink fixture failed");
        }
        let store = FileStore::new(dir.path());
        let auth = Auth {
            file_name: "real.json".into(),
            ..Auth::default()
        };
        let guard = super::super::credential_lock::acquire_sync(&path).unwrap();
        let (read, reached) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let done = Mutex::new(done);
        super::super::credential_lock::set_test_hook(
            &path,
            Some(Arc::new(move |event| {
                if event == "bind-read" {
                    read.send(())
                        .expect("alias writer stopped before bind read");
                    done.lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .expect("alias writer did not finish");
                }
            })),
        );
        let directory = dir.path().to_owned();
        let writer = std::thread::spawn(move || {
            reached
                .recv_timeout(Duration::from_secs(10))
                .map_err(|_| io::Error::other("bind did not reach its read barrier"))?;
            let result = (|| {
                let store = FileStore::new(directory);
                let original = meta(
                    json!({"type":"codex", "access_token":"selected", "refresh_token":"selected"}),
                );
                let rotated = meta(
                    json!({"type":"codex", "access_token":"rotated", "refresh_token":"rotated"}),
                );
                let patch = super::super::codex_refresh::CredentialRefreshPatch::new(
                    &original, &rotated, true, false,
                );
                let auth = Auth {
                    file_name: "alias.json".into(),
                    metadata: rotated,
                    ..Auth::default()
                };
                let guard = super::super::credential_lock::acquire_sync(&alias)?;
                store
                    .apply_codex_refresh_locked(&auth, &patch, &guard)
                    .map(|_| ())
            })();
            let _ = finished.send(());
            result
        });
        let result = store.bind_siwc_locked(&auth, Map::new(), &guard, &|_, _| Ok(()));
        super::super::credential_lock::set_test_hook(&path, None);
        assert!(writer.join().unwrap().is_ok(), "alias refresh failed");
        let Err(error) = result else {
            panic!("bind accepted an alias");
        };
        assert_eq!(error.to_string(), "SIWC bind target has a linked alias");
        let saved = read_json(&path);
        assert!(saved["access_token"] == "rotated");
        assert!(saved["refresh_token"] == "rotated");
        assert!(saved.get("use_siwc").is_none());
    }

    #[tokio::test]
    async fn bind_refuses_replacement_after_scan_before_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bind.json");
        write(
            &path,
            r#"{"type":"codex","access_token":"selected","refresh_token":"selected"}"#,
        );
        let store = FileStore::new(dir.path());
        let auth = store.list().unwrap().remove(0);
        let replacement = path.clone();
        super::super::credential_lock::set_test_hook(
            &path,
            Some(std::sync::Arc::new(move |event| {
                if event == "bind-before-replace" {
                    write_atomic(
                        &replacement,
                        br#"{"type":"codex","access_token":"rotated","refresh_token":"rotated"}"#,
                        true,
                    )
                    .unwrap();
                }
            })),
        );
        let result = std::sync::Arc::new(store).bind_siwc(auth, Map::new()).await;
        super::super::credential_lock::set_test_hook(&path, None);
        let Err(error) = result else {
            panic!("bind overwrote a replacement");
        };
        assert_eq!(
            error.to_string(),
            "SIWC bind target changed before replacement"
        );
        let saved = read_json(&path);
        assert!(saved["access_token"] == "rotated");
        assert!(saved["refresh_token"] == "rotated");
        assert!(saved.get("use_siwc").is_none());
    }

    #[test]
    fn bind_alias_scan_refuses_missing_parent_or_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.json");
        write(
            &path,
            r#"{"type":"codex","access_token":"test","refresh_token":"test"}"#,
        );
        let (_, id) = read_siwc_bind_target(&path).unwrap();
        let missing = dir.path().join("missing");
        for (root, target) in [
            (dir.path().to_owned(), missing.join("target.json")),
            (missing, path),
        ] {
            let Err(error) = refuse_siwc_bind_aliases(&root, &target, id) else {
                panic!("missing scan boundary was accepted");
            };
            assert_eq!(error.to_string(), "SIWC bind alias scan failed");
        }
    }

    #[test]
    fn bind_alias_scan_ignores_absence_but_fails_closed_with_fixed_errors() {
        assert!(
            scan_optional::<()>(Err(io::Error::from(io::ErrorKind::NotFound)))
                .unwrap()
                .is_none()
        );
        #[cfg(unix)]
        assert!(
            scan_optional::<()>(Err(io::Error::from_raw_os_error(libc::ELOOP)))
                .unwrap()
                .is_none()
        );
        #[cfg(windows)]
        assert!(
            scan_optional::<()>(Err(io::Error::from_raw_os_error(1921)))
                .unwrap()
                .is_none()
        );
        let error = scan_optional::<()>(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private path",
        )))
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "SIWC bind alias scan failed");
    }

    #[tokio::test]
    async fn bind_ignores_entries_vanishing_during_alias_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bind.json");
        write(
            &path,
            r#"{"type":"codex","access_token":"selected","refresh_token":"selected"}"#,
        );
        let store = FileStore::new(dir.path());
        let auth = store.list().unwrap().remove(0);
        let fleeting = dir.path().join("fleeting.tmp");
        write(&fleeting, "unrelated");
        let remove = fleeting.clone();
        super::super::credential_lock::set_test_hook(
            &fleeting,
            Some(std::sync::Arc::new(move |event| {
                if event == "bind-scan-entry" {
                    fs::remove_file(&remove).unwrap();
                }
            })),
        );
        let result = std::sync::Arc::new(store).bind_siwc(auth, Map::new()).await;
        super::super::credential_lock::set_test_hook(&fleeting, None);
        assert!(result.is_ok(), "vanished scan entry failed the bind");
        assert!(!fleeting.exists(), "scan barrier did not execute");
        assert!(read_json(&path)["use_siwc"] == true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_alias_scan_never_opens_an_unrelated_fifo_stream() {
        use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bind.json");
        write(
            &path,
            r#"{"type":"codex","access_token":"selected","refresh_token":"selected"}"#,
        );
        let store = FileStore::new(dir.path());
        let auth = store.list().unwrap().remove(0);
        let fifo = dir.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live, NUL-terminated fixture path, with no aliasing.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink(&fifo, dir.path().join("fifo-link")).unwrap();
        let store = std::sync::Arc::new(store);
        let binding = store.bind_siwc(auth, Map::new());
        tokio::pin!(binding);
        let result = tokio::time::timeout(Duration::from_secs(2), &mut binding).await;
        if result.is_err() {
            // Unblock a regressed blocking reader before failing, so the test
            // runtime can shut down instead of retaining a stuck worker.
            let wake = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo);
            assert!(wake.is_ok(), "FIFO cleanup could not wake a blocked scan");
            // Hold a writer even if a loaded host has not scheduled the reader yet.
            let stopped = tokio::time::timeout(Duration::from_secs(5), &mut binding).await;
            drop(wake);
            assert!(stopped.is_ok(), "FIFO scan did not shut down");
        }
        assert!(result.is_ok(), "alias scan opened a FIFO data stream");
        assert!(
            result.unwrap().is_ok(),
            "FIFO alias failed an unrelated bind"
        );
    }

    #[cfg(windows)]
    #[test]
    fn bind_alias_scan_refuses_mixed_mode_identity_matches() {
        let extended = FileId {
            volume: 7,
            index: 11,
            extended: Some((7, 1 << 80)),
        };
        let legacy = FileId {
            extended: None,
            ..extended
        };
        let destination = Path::new("other-destination");
        let target = Path::new("target-destination");
        for (id, selected) in [(legacy, extended), (extended, legacy)] {
            let Err(error) = refuse_bind_alias(destination, target, id, selected) else {
                panic!("mixed-mode alias was accepted");
            };
            assert_eq!(error.to_string(), "SIWC bind target has a linked alias");
        }
        let different = FileId {
            index: 12,
            ..legacy
        };
        assert!(refuse_bind_alias(destination, target, different, extended).is_ok());
        assert!(refuse_bind_alias(destination, target, extended, different).is_ok());
        let different = FileId {
            extended: Some((7, (1 << 80) + 1)),
            ..extended
        };
        assert!(refuse_bind_alias(destination, target, different, extended).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn bind_legacy_zero_identity_is_unsupported_for_scans_and_changes() {
        let legacy = FileId {
            volume: 7,
            index: 0,
            extended: None,
        };
        let extended = FileId {
            extended: Some((7, 1 << 80)),
            ..legacy
        };
        assert!(legacy.supported().is_err());
        assert!(extended.supported().is_ok());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.json");
        write(
            &path,
            r#"{"type":"codex","access_token":"test","refresh_token":"test"}"#,
        );
        let Err(error) = refuse_siwc_bind_aliases(dir.path(), &path, legacy) else {
            panic!("unsupported scan target was accepted");
        };
        assert_eq!(error.to_string(), "SIWC bind alias scan failed");
        for (id, target) in [(legacy, legacy), (legacy, extended), (extended, legacy)] {
            let Err(error) = refuse_bind_alias(Path::new("other"), Path::new("target"), id, target)
            else {
                panic!("unsupported scan identity was accepted");
            };
            assert_eq!(error.to_string(), "SIWC bind alias scan failed");
        }
        let metadata = meta(json!({"account_id":"workspace", "email":"same@example.test"}));
        for (selected, current) in [(legacy, legacy), (legacy, extended), (extended, legacy)] {
            let identity = BindIdentity::capture(&metadata, selected);
            let Err(error) = identity.check(&metadata, current) else {
                panic!("unsupported change-check identity was accepted");
            };
            assert_eq!(error.to_string(), "SIWC bind target identity changed");
        }
        assert!(
            BindIdentity::capture(&metadata, extended)
                .check(&metadata, extended)
                .is_ok()
        );
    }

    #[cfg(windows)]
    #[test]
    fn bind_file_id_equality_never_mixes_extended_and_legacy_modes() {
        let first = FileId {
            volume: 7,
            index: 11,
            extended: Some((7, 1 << 80)),
        };
        let second = FileId {
            extended: Some((7, (1 << 80) + 1)),
            ..first
        };
        let legacy = FileId {
            extended: None,
            ..first
        };
        assert!(
            first != second,
            "different extended IDs shared a legacy identity"
        );
        assert!(first != legacy, "extended and legacy modes compared equal");
        assert!(legacy != first, "mixed-mode comparison was asymmetric");
        assert!(
            second != legacy,
            "a legacy identity bridged different extended IDs"
        );
        assert!(
            legacy
                == FileId {
                    volume: 7,
                    index: 11,
                    extended: None
                }
        );
    }

    #[cfg(windows)]
    #[test]
    fn bind_file_id_falls_back_only_for_unsupported_queries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");
        write(&path, "identity fixture");
        let file = fs::File::open(&path).unwrap();
        let selected = file_id(&file).unwrap();
        let legacy = windows_file_id(&file, Err(io::Error::from_raw_os_error(87))).unwrap();
        for code in [1, 87, 50] {
            let fallback = windows_file_id(&file, Err(io::Error::from_raw_os_error(code))).unwrap();
            assert!(legacy == fallback, "legacy identities did not compare");
        }
        if selected.extended.is_some() {
            assert!(selected != legacy, "mixed identity modes compared equal");
        }
        assert!(windows_file_id(&file, Err(io::Error::from_raw_os_error(5))).is_err());
    }

    #[test]
    fn bind_identity_refuses_disappearing_claims_and_accepts_new_claims() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");
        let mut original = meta(
            json!({"type":"codex", "access_token":"test", "refresh_token":"test", "account_id":"workspace", "email":"same@example.test"}),
        );
        write(&path, &marshal_map(&original));
        let (_, id) = read_siwc_bind_target(&path).unwrap();
        let before_claims = BindIdentity::capture(&original, id);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"stable","email":"same@example.test","https://api.openai.com/auth":{"chatgpt_account_id":"workspace","chatgpt_user_id":"stable"}}"#);
        original.insert("id_token".into(), json!(format!("x.{payload}.x")));
        assert!(
            before_claims.check(&original, id).is_ok(),
            "new claims rejected unchanged evidence"
        );
        let identity = BindIdentity::capture(&original, id);
        for key in ["account_id", "email", "id_token"] {
            let mut changed = original.clone();
            changed.remove(key);
            assert!(
                identity.check(&changed, id).is_err(),
                "captured claim disappearance accepted"
            );
        }
    }

    #[tokio::test]
    async fn bind_returns_written_map_when_target_vanishes_after_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bind.json");
        write(
            &path,
            r#"{"type":"codex","access_token":"selected","refresh_token":"selected"}"#,
        );
        let store = FileStore::new(dir.path());
        let auth = store.list().unwrap().remove(0);
        let remove = path.clone();
        super::super::credential_lock::set_test_hook(
            &path,
            Some(std::sync::Arc::new(move |event| {
                if event == "bind-written" {
                    fs::remove_file(&remove).unwrap();
                }
            })),
        );
        let result = std::sync::Arc::new(store).bind_siwc(auth, Map::new()).await;
        super::super::credential_lock::set_test_hook(&path, None);
        assert!(
            result.is_ok(),
            "optional timestamp turned a completed bind into failure"
        );
        assert!(result.unwrap().metadata["use_siwc"] == true);
        assert!(!path.exists());
    }

    #[test]
    fn bind_reparse_tag_classifies_only_name_surrogates() {
        assert!(name_surrogate(0xa000_000c)); // symlink
        assert!(name_surrogate(0xa000_0003)); // mount point
        assert!(!name_surrogate(0x9000_001a)); // cloud
        assert!(!name_surrogate(0x8000_0013)); // dedup
        assert!(!name_surrogate(0));
    }

    #[test]
    fn bind_identity_refuses_same_file_account_subject_and_grant_changes() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");
        let mut original = meta(
            json!({"type":"codex","access_token":"test", "refresh_token":"test", "account_id":"first"}),
        );
        write(&path, &marshal_map(&original));
        let (_, id) = read_siwc_bind_target(&path).unwrap();
        let identity = BindIdentity::capture(&original, id);
        let mut changed = original.clone();
        changed.insert("account_id".into(), json!("second"));
        write(&path, &marshal_map(&changed));
        let (disk, same_id) = read_siwc_bind_target(&path).unwrap();
        assert!(same_id == id);
        assert!(
            identity.check(&disk, same_id).is_err(),
            "same-file account substitution accepted"
        );
        changed = original.clone();
        changed.insert("refresh_token".into(), json!("different-grant"));
        write(&path, &marshal_map(&changed));
        let (disk, same_id) = read_siwc_bind_target(&path).unwrap();
        assert!(same_id == id);
        assert!(
            identity.check(&disk, same_id).is_err(),
            "same-file grant substitution accepted"
        );
        original.insert("email".into(), json!("same@example.test"));
        let token = |sub| {
            format!("x.{}.x", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"{sub}","https://api.openai.com/auth":{{"chatgpt_account_id":"first","chatgpt_user_id":"stable"}}}}"#)))
        };
        original.insert("id_token".into(), json!(token("first")));
        let identity = BindIdentity::capture(&original, id);
        changed = original.clone();
        changed.insert("id_token".into(), json!(token("second")));
        assert!(
            identity.check(&changed, id).is_err(),
            "email masked changed subject"
        );
        changed = original.clone();
        changed.insert("refresh_token".into(), json!("normal-rotation"));
        assert!(
            identity
                .check(
                    &changed,
                    FileId {
                        volume: 99,
                        index: 99,
                        #[cfg(windows)]
                        extended: None
                    }
                )
                .is_ok()
        );
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"first","https://api.openai.com/auth":{"chatgpt_account_id":"second","chatgpt_user_id":"stable"}}"#);
        changed.insert("id_token".into(), json!(format!("x.{payload}.x")));
        assert!(
            identity.check(&changed, id).is_err(),
            "top-level account masked changed JWT account"
        );
    }

    #[tokio::test]
    async fn bind_returns_written_map_not_a_post_write_path_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bind.json");
        write(
            &path,
            r#"{"type":"codex","access_token":"selected","refresh_token":"selected"}"#,
        );
        let store = FileStore::new(dir.path());
        let auth = store.list().unwrap().remove(0);
        let replacement = path.clone();
        super::super::credential_lock::set_test_hook(
            &path,
            Some(std::sync::Arc::new(move |event| {
                if event == "bind-written" {
                    write(
                        &replacement,
                        r#"{"type":"codex","access_token":"substituted","refresh_token":"substituted"}"#,
                    );
                }
            })),
        );
        let saved = std::sync::Arc::new(store)
            .bind_siwc(auth, Map::new())
            .await
            .unwrap();
        super::super::credential_lock::set_test_hook(&path, None);
        assert!(saved.metadata["access_token"] == "selected");
        assert!(saved.metadata["use_siwc"] == true);
        assert!(read_json(&path)["access_token"] == "substituted");
    }

    // Not upstream's: a store under a closed gate refuses every mutator and
    // changes no file, and reading still works.
    #[tokio::test]
    async fn closed_gate_refuses_every_mutator() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        write(&path, r#"{"type":"codex","disabled":false}"#);
        let store = FileStore::new(dir.path());
        let (gate, key) = ActiveGate::standby();
        store.set_gate(gate);
        let mut auth = Auth {
            id: "a.json".into(),
            file_name: "a.json".into(),
            metadata: meta(json!({"type": "codex", "n": 1})),
            ..Auth::default()
        };
        assert!(store.save_auth(&mut auth).is_err());
        assert!(store.save_new_auth(&mut auth).is_err());
        assert!(store.write_file("b.json", b"{}").is_err());
        assert!(store.remove_file("a.json").is_err());
        assert!(AuthStore::delete(&store, "a.json").is_err());
        assert!(store.try_set_base_dir(dir.path().join("x")).is_err());
        store.set_base_dir(dir.path().join("x"));
        assert_eq!(store.base_dir(), dir.path());
        assert!(!dir.path().join("b.json").exists());
        assert_eq!(
            read_json(&path),
            json!({"type": "codex", "disabled": false})
        );
        assert_eq!(AuthStore::list(&store).unwrap().len(), 1);

        // The take-over path binds the directory with its activation.
        let activation = key.activate(&store.gate()).unwrap();
        store
            .set_base_dir_under(&activation, dir.path().join("x"))
            .unwrap();
        assert_eq!(store.base_dir(), dir.path().join("x"));
        store.gate().reopen();
        assert!(
            store
                .set_base_dir_under(&activation, dir.path().join("y"))
                .is_err()
        );
        assert_eq!(store.base_dir(), dir.path().join("x"));
        store.try_set_base_dir(dir.path()).unwrap();
        assert!(store.save_auth(&mut auth).is_ok());
    }

    // Not upstream's: a quiescing store lets a save through only inside an
    // operation, so a refresh that began before the switch still saves.
    #[tokio::test]
    async fn quiescing_store_saves_only_inside_an_operation() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let gate = ActiveGate::open();
        store.set_gate(gate.clone());
        let op = gate.begin_operation().unwrap();
        let closing = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.close(0).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(store.write_file("a.json", b"{}").is_err());
        op.clone()
            .scope(async {
                store.write_file("a.json", b"{}").unwrap();
            })
            .await;
        drop(op);
        closing.await.unwrap().unwrap();
        assert!(dir.path().join("a.json").exists());
    }

    /// The marks that start a physical write or removal in the audited code.
    const WRITE_MARKS: [&str; 4] = [
        "write_atomic(&path",
        "write_atomic(path",
        "write_atomic_no_follow(path",
        "fs::remove_file(&path)",
    ];
    const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod tests";

    /// Whether `line` starts a function at column 0 or 4 (nested functions
    /// at 8 are part of their enclosing function).
    fn starts_function(line: &str) -> bool {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent != 0 && indent != 4 {
            return false;
        }
        let mut rest = &line[indent..];
        if let Some(after) = rest.strip_prefix("pub") {
            rest = match after.strip_prefix(' ') {
                Some(after) => after,
                None => match after.strip_prefix('(').and_then(|a| a.split_once(") ")) {
                    Some((scope, after))
                        if !scope.is_empty() && scope.bytes().all(|b| b.is_ascii_lowercase()) =>
                    {
                        after
                    }
                    _ => return false,
                },
            };
        }
        let rest = rest.strip_prefix("async ").unwrap_or(rest);
        rest.starts_with("fn ")
    }

    /// The names of the functions in `src` that hold a physical write or
    /// removal without a write permit: `self.permit()?` before the mark, or a
    /// `WritePermit` parameter. A mark's call may be split across lines after
    /// its parenthesis, as rustfmt splits a long call. Not upstream's: a new
    /// writer can't skip the gate.
    fn audit_permits(src: &str) -> Vec<String> {
        let src = src.replace("\r\n", "\n");
        assert_eq!(
            src.matches(TEST_MODULE_MARKER).count(),
            1,
            "the test-module marker must occur exactly once"
        );
        let code = src.split(TEST_MODULE_MARKER).next().unwrap();
        let lines: Vec<&str> = code.lines().collect();
        let mut offenders = Vec::new();
        for mark in WRITE_MARKS {
            let (call, arg) = mark.split_at(mark.find('(').unwrap() + 1);
            let mut found = false;
            for (offset, _) in code.match_indices(call) {
                // The primitives' own definitions spell the marks too, as
                // `fn write_atomic(path`; any other occurrence is a call.
                if code[..offset].ends_with("fn ")
                    || !code[offset + call.len()..].trim_start().starts_with(arg)
                {
                    continue;
                }
                found = true;
                let at = code[..offset].matches('\n').count();
                let Some(start) = lines[..=at].iter().rposition(|l| starts_function(l)) else {
                    offenders.push(format!("(no function before `{mark}` at line {})", at + 1));
                    continue;
                };
                let name = lines[start]
                    .split_once("fn ")
                    .and_then(|(_, rest)| rest.split(['(', '<']).next())
                    .unwrap_or("?")
                    .to_string();
                let span = lines[start..=at].join("\n");
                let signature: String = lines[start..=at]
                    .iter()
                    .take_while(|l| !l.ends_with('{'))
                    .chain(lines[start..=at].iter().find(|l| l.ends_with('{')))
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n");
                let covered = span.contains("self.permit()?")
                    || signature.contains("&crate::gate::WritePermit")
                    || signature.contains("&WritePermit");
                if !covered && !offenders.contains(&name) {
                    offenders.push(name);
                }
            }
            assert!(
                found,
                "write mark `{mark}` matches nothing in the audited code"
            );
        }
        offenders
    }

    // Part A1: the lease, the origin identity and the permit audit.

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, mpsc};

    use super::super::OriginProof;

    /// The scratch directory's path, lexically cleaned as the store joins it.
    fn clean_dir(dir: &tempfile::TempDir) -> PathBuf {
        super::super::path::clean(dir.path())
    }

    fn lease_record(file_name: &str, token: &str) -> Auth {
        Auth {
            id: file_name.into(),
            file_name: file_name.into(),
            provider: "codex".into(),
            metadata: meta(json!({
                "type": "codex",
                "access_token": token,
                "refresh_token": "refresh-test",
            })),
            ..Auth::default()
        }
    }

    fn write_lease_file(path: &Path, token: &str) {
        write(
            path,
            &json!({"type": "codex", "access_token": token, "refresh_token": "refresh-test"})
                .to_string(),
        );
    }

    fn held(lease: io::Result<CodexLease>) -> CodexLeaseToken {
        match lease {
            Ok(CodexLease::Held(token)) => token,
            Ok(CodexLease::Unsupported) => panic!("lease is unsupported"),
            Err(error) => panic!("lease failed: {error}"),
        }
    }

    /// A second spelling of the directory `dir`: on Windows a case variant
    /// (needs a case-insensitive directory, the NTFS default), on Unix a
    /// directory symlink made inside `holder`.
    #[cfg(any(unix, windows))]
    fn alias_dir(dir: &Path, holder: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let _ = holder;
            let name = dir.file_name().unwrap().to_string_lossy().to_uppercase();
            dir.with_file_name(name)
        }
        #[cfg(unix)]
        {
            let alias = holder.join("alias");
            std::os::unix::fs::symlink(dir, &alias).unwrap();
            alias
        }
    }

    /// The precondition of every alias test, made with the crate directly so
    /// that it does not assume what `prove` does: both spellings open to one
    /// file. A case-sensitive directory fails here instead of passing vacuously.
    #[cfg(any(unix, windows))]
    fn assert_alias(dir: &Path, alias: &Path) {
        assert_ne!(dir, alias, "the alias must differ lexically");
        let a = same_file::Handle::from_path(dir).unwrap();
        let b = same_file::Handle::from_path(alias).unwrap();
        assert!(a == b, "{alias:?} is not an alias of {dir:?}");
    }

    #[test]
    fn lease_fixes_path_and_origin_before_the_lock() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let e = mutation_scratch();
        let ep = clean_dir(&e);
        let store = Arc::new(FileStore::new(dp.as_path()));
        let auth = lease_record("f", "t");
        let file = dp.join("f");
        let blocker = super::super::credential_lock::acquire_sync(&file).unwrap();
        let (tx, rx) = mpsc::channel();
        let rebinding = store.clone();
        let target = ep.clone();
        super::super::credential_lock::set_test_hook(
            &file,
            Some(Arc::new(move |event| {
                if event == "waiting" {
                    rebinding.set_base_dir(&target);
                    let _ = tx.send(());
                }
            })),
        );
        let leasing = store.clone();
        let record = auth.clone();
        let worker = std::thread::spawn(move || leasing.codex_refresh_lease(&record));
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(blocker);
        let token = held(worker.join().unwrap());
        super::super::credential_lock::set_test_hook(&file, None);
        assert_eq!(token.path(), file);
        assert_eq!(*token.origin(), CodexOrigin::of(token.path()).unwrap());
        assert!(token.guard().guards(token.path()).unwrap());
        // The rebind moved the registration, and not the lease.
        assert_eq!(
            store.codex_origin(&auth).unwrap().unwrap(),
            CodexOrigin::of(&ep.join("f")).unwrap()
        );
        assert_ne!(*token.origin(), store.codex_origin(&auth).unwrap().unwrap());
    }

    #[test]
    fn lease_read_reports_missing_corrupt_and_foreign_guards() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        let auth = lease_record("f", "t");
        let file = dp.join("f");
        let token = held(store.codex_refresh_lease(&auth));
        assert!(store.codex_lease_read(&token).unwrap().is_none());
        write(&file, "{not json");
        assert!(store.codex_lease_read(&token).is_err());
        write_lease_file(&file, "t");
        let read = store.codex_lease_read(&token).unwrap().unwrap();
        assert_eq!(read["access_token"], "t");
        drop(token);
        // A token whose guard is for another file.
        let other = dp.join("g");
        let foreign = CodexLeaseToken {
            origin: CodexOrigin::of(&other).unwrap(),
            path: other,
            guard: super::super::credential_lock::acquire_sync(&file).unwrap(),
        };
        let error = store.codex_lease_read(&foreign).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn lease_commit_writes_the_leased_path_after_a_rebind() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let e = mutation_scratch();
        let ep = clean_dir(&e);
        let store = FileStore::new(dp.as_path());
        let auth = lease_record("f", "before");
        let file = dp.join("f");
        write_lease_file(&file, "before");
        let token = held(store.codex_refresh_lease(&auth));
        store.set_base_dir(ep.as_path());
        let after = meta(json!({
            "type": "codex",
            "access_token": "after",
            "refresh_token": "refresh-test",
        }));
        let patch = super::super::codex_refresh::CredentialRefreshPatch::new(
            &auth.metadata,
            &after,
            true,
            false,
        );
        let persisted = store.codex_lease_commit(&token, &auth, &patch).unwrap();
        assert_eq!(persisted.applied.outer, Some(true));
        assert_eq!(read_json(&file)["access_token"], "after");
        assert!(!ep.join("f").exists());
        assert_eq!(fs::read_dir(ep.as_path()).unwrap().count(), 0);
    }

    #[test]
    fn partition_write_re_resolves_after_the_lock_wait() {
        let p = mutation_scratch();
        let pp = clean_dir(&p);
        let q = mutation_scratch();
        let qp = clean_dir(&q);
        let store = Arc::new(FileStore::new(pp.as_path()));
        let auth = lease_record("f", "before");
        let file = pp.join("f");
        write_lease_file(&file, "before");
        let blocker = super::super::credential_lock::acquire_sync(&file).unwrap();
        let (tx, rx) = mpsc::channel();
        let rebinding = store.clone();
        let target = qp.clone();
        super::super::credential_lock::set_test_hook(
            &file,
            Some(Arc::new(move |event| {
                if event == "waiting" {
                    rebinding.set_base_dir(&target);
                    let _ = tx.send(());
                }
            })),
        );
        let after = meta(json!({
            "type": "codex",
            "access_token": "after",
            "refresh_token": "refresh-test",
        }));
        let intent = super::super::codex_refresh::CodexPersistIntent::Refresh(
            super::super::codex_refresh::CredentialRefreshPatch::new(
                &auth.metadata,
                &after,
                true,
                false,
            ),
        );
        let writing = store.clone();
        let record = auth.clone();
        let worker = std::thread::spawn(move || writing.persist_codex_partition(&record, &intent));
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(blocker);
        let error = worker.join().unwrap().unwrap_err();
        super::super::credential_lock::set_test_hook(&file, None);
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(read_json(&file)["access_token"], "before");
        assert!(!qp.join("f").exists());
    }

    #[test]
    fn lease_is_unsupported_without_a_file_identity() {
        struct Bare;
        impl AuthStore for Bare {
            fn list(&self) -> io::Result<Vec<Auth>> {
                Ok(Vec::new())
            }
            fn save(&self, _auth: &Auth) -> io::Result<String> {
                Ok(String::new())
            }
            fn delete(&self, _id: &str) -> io::Result<()> {
                Ok(())
            }
        }
        let auth = lease_record("f", "t");
        assert!(matches!(
            Bare.codex_refresh_lease(&auth),
            Ok(CodexLease::Unsupported)
        ));
        assert!(Bare.codex_origin(&auth).unwrap().is_none());

        // The default `save_bound` saves; it never calls the callback.
        let origin = CodexOrigin::of(&std::env::temp_dir().join("bare-origin")).unwrap();
        let saved = Bare
            .save_bound(&auth, &origin, &mut |_, _| panic!("no callback"))
            .unwrap();
        assert_eq!(saved, Some(String::new()));

        // A record that is not Codex gets no lease and no lock file.
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        let mut gemini = lease_record("f", "t");
        gemini.provider = "gemini".into();
        gemini.metadata.insert("type".into(), json!("gemini"));
        assert!(matches!(
            store.codex_refresh_lease(&gemini),
            Ok(CodexLease::Unsupported)
        ));
        assert_eq!(fs::read_dir(dp.as_path()).unwrap().count(), 0);

        // No base dir and a relative id: no file identity, `Unsupported`.
        let unconfigured = FileStore::new("");
        let relative = Auth {
            id: "unconfigured-lease-test.json".into(),
            provider: "codex".into(),
            ..Auth::default()
        };
        assert!(matches!(
            unconfigured.codex_refresh_lease(&relative),
            Ok(CodexLease::Unsupported)
        ));
        assert!(!Path::new("unconfigured-lease-test.json.lock").exists());
        assert!(unconfigured.codex_origin(&relative).unwrap().is_none());
    }

    #[test]
    fn audit_permits_accepts_the_source_and_names_offenders() {
        let source = include_str!("file_store.rs");
        assert_eq!(audit_permits(source), Vec::<String>::new());

        // (a), (b): a function whose permit is gone is named.
        let without_permit = |header: &str| {
            let at = source.find(header).unwrap();
            let permit = at + source[at..].find("self.permit()?").unwrap();
            format!(
                "{}self.gate(){}",
                &source[..permit],
                &source[permit + "self.permit()?".len()..]
            )
        };
        assert_eq!(
            audit_permits(&without_permit("    pub fn write_file(")),
            ["write_file"]
        );
        assert_eq!(
            audit_permits(&without_permit("    fn save_with(")),
            ["save_with"]
        );

        // (c): a missing or duplicated test-module marker fails loudly.
        let panics = |source: String| std::panic::catch_unwind(|| audit_permits(&source)).is_err();
        assert!(panics(
            source.replace(TEST_MODULE_MARKER, "\n#[cfg(test)]\nmod elsewhere")
        ));
        assert!(panics(format!("{source}{TEST_MODULE_MARKER}")));

        // (d): a write mark before any function is reported.
        let early = format!("let _ = fs::remove_file(&path);\n{source}");
        let offenders = audit_permits(&early);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert!(offenders[0].contains("no function"), "{offenders:?}");

        // (e): a respelled mark fails the at-least-once assertion. A space
        // after the parenthesis is not a respelling (see (g)), so the
        // argument is renamed.
        assert!(panics(source.replace(
            "write_atomic_no_follow(path",
            "write_atomic_no_follow(target"
        )));

        // (f): a writer whose body shares its `fn` line is named, whether
        // the write is direct or in a closure.
        let with_writer = |writer: &str| {
            source.replacen(
                TEST_MODULE_MARKER,
                &format!("{writer}{TEST_MODULE_MARKER}"),
                1,
            )
        };
        assert_eq!(
            audit_permits(&with_writer(
                "fn unpermitted(path: &Path) -> io::Result<()> { fs::remove_file(&path) }"
            )),
            ["unpermitted"]
        );
        assert_eq!(
            audit_permits(&with_writer(
                "fn in_closure(path: &Path) { let _ = || fs::remove_file(&path); }"
            )),
            ["in_closure"]
        );

        // (g): a call split after its parenthesis is still a write.
        let split = r"fn split_writer(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic(
        &path,
        bytes,
    )
}";
        assert_eq!(audit_permits(&with_writer(split)), ["split_writer"]);

        // CRLF line endings are normalized.
        assert_eq!(
            audit_permits(&source.replace('\n', "\r\n")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn codex_origin_names_the_file_a_save_would_write() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let e = mutation_scratch();
        let ep = clean_dir(&e);
        let store = FileStore::new(dp.as_path());
        let file = dp.join("f");
        let origin = |store: &FileStore, auth: &Auth| store.codex_origin(auth).unwrap().unwrap();
        let of = |path: &Path| CodexOrigin::of(path).unwrap();
        let relative = lease_record("f", "t");
        let mut absolute = lease_record("f", "t");
        absolute
            .attributes
            .insert(ATTRIBUTE_PATH.into(), path_string(&file));
        assert_eq!(origin(&store, &relative), of(&file));
        assert_eq!(origin(&store, &absolute), of(&file));
        assert_eq!(of(&file).path(), std::path::absolute(&file).unwrap());

        store.set_base_dir(ep.as_path());
        assert_eq!(origin(&store, &relative), of(&ep.join("f")));
        assert_ne!(origin(&store, &relative), of(&file));
        assert_eq!(origin(&store, &absolute), of(&file));

        assert_eq!(of(&dp.join(".").join("f")), of(&file));
        let dotdot = of(&dp.join("sub").join("..").join("f"));
        #[cfg(unix)]
        assert_ne!(dotdot, of(&file));
        #[cfg(windows)]
        assert_eq!(dotdot, of(&file));
        #[cfg(not(any(unix, windows)))]
        let _ = dotdot;
        #[cfg(unix)]
        assert_ne!(of(&dp.join(" f")), of(&file));
        // The resolver trims, so a padded attribute still names the file.
        let mut padded = lease_record("f", "t");
        padded
            .attributes
            .insert(ATTRIBUTE_PATH.into(), format!("  {}  ", path_string(&file)));
        assert_eq!(origin(&store, &padded), of(&file));
        // Equality is lexical on every platform; `prove` decides sameness.
        assert_ne!(of(&dp.join("F")), of(&file));

        let nameless = Auth {
            provider: "codex".into(),
            ..Auth::default()
        };
        assert!(store.codex_origin(&nameless).unwrap().is_none());
        let relative_id = Auth {
            id: "x.json".into(),
            provider: "codex".into(),
            ..Auth::default()
        };
        assert!(
            FileStore::new("")
                .codex_origin(&relative_id)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn prove_decides_sameness_under_a_lock() {
        use super::super::credential_lock::acquire_sync;
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let h = mutation_scratch();
        let hp = clean_dir(&h);
        let of = |path: &Path| CodexOrigin::of(path).unwrap();
        let f = dp.join("f");
        let g = dp.join("g");
        let missing = dp.join("h");
        write_lease_file(&f, "t");
        write_lease_file(&g, "t");

        // Lexically equal: no lock, no I/O, even when the file is missing.
        assert_eq!(of(&missing).prove(&of(&missing)), OriginProof::Same);

        let alias = alias_dir(dp.as_path(), hp.as_path());
        assert_alias(dp.as_path(), &alias);
        let via = alias.join("f");
        {
            let _lock = acquire_sync(&via).unwrap();
            assert_eq!(of(&f).prove(&of(&via)), OriginProof::Same);
        }
        #[cfg(unix)]
        {
            fs::create_dir(dp.join("sub")).unwrap();
            let dotdot = dp.join("sub").join("..").join("f");
            let _lock = acquire_sync(&dotdot).unwrap();
            assert_eq!(of(&f).prove(&of(&dotdot)), OriginProof::Same);
        }

        // Two files.
        {
            let _lock = acquire_sync(&g).unwrap();
            assert_eq!(of(&f).prove(&of(&g)), OriginProof::Different);
        }
        // A missing current path is Different; a missing pinned path is not.
        {
            let _lock = acquire_sync(&missing).unwrap();
            assert_eq!(of(&f).prove(&of(&missing)), OriginProof::Different);
        }
        {
            let _lock = acquire_sync(&f).unwrap();
            assert_eq!(of(&missing).prove(&of(&f)), OriginProof::Unproven);
        }

        // Any other open error is never Different. Without a lock to take.
        #[cfg(unix)]
        {
            let below = f.join("x");
            let error = fs::File::open(&below).unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::NotFound, "{error:?}");
            assert_ne!(of(&f).prove(&of(&below)), OriginProof::Different);
        }

        // A hard link names one file until a save renames a new file over it.
        let link = dp.join("link");
        fs::hard_link(&f, &link).unwrap();
        {
            let _lock = acquire_sync(&link).unwrap();
            assert_eq!(of(&f).prove(&of(&link)), OriginProof::Same);
        }
        FileStore::new(dp.as_path())
            .save(&lease_record("link", "split"))
            .unwrap();
        {
            let _lock = acquire_sync(&link).unwrap();
            assert_eq!(of(&f).prove(&of(&link)), OriginProof::Different);
        }
    }

    /// Saves a Codex record `f` with `token` as an outer login, from a store
    /// of its own bound to `dir`, on a thread of its own.
    #[cfg(any(unix, windows))]
    fn peer_login(dir: &Path, token: &str) -> std::thread::JoinHandle<()> {
        let dir = dir.to_path_buf();
        let token = token.to_owned();
        std::thread::spawn(move || {
            let mut auth = lease_record("f", &token);
            auth.codex_persist_intent = super::super::codex_refresh::CodexPersistIntent::OuterLogin;
            FileStore::new(&dir).save(&auth).unwrap();
        })
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_peer_cannot_replace_the_file_between_the_opens_while_the_lock_is_held() {
        use super::super::credential_lock::{acquire_sync, set_test_hook};
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let h = mutation_scratch();
        let hp = clean_dir(&h);
        let file = dp.join("f");
        write_lease_file(&file, "T1");
        let alias = alias_dir(dp.as_path(), hp.as_path());
        assert_alias(dp.as_path(), &alias);
        let via = alias.join("f");

        let (tx, rx) = mpsc::channel::<()>();
        set_test_hook(
            &file,
            Some(Arc::new(move |event| {
                if event == "waiting" {
                    let _ = tx.send(());
                }
            })),
        );
        let rx = std::sync::Mutex::new(rx);
        let peer = Arc::new(std::sync::Mutex::new(None));
        let between = Arc::new(AtomicUsize::new(0));
        let hook = {
            let (peer, between, file, dir) =
                (peer.clone(), between.clone(), file.clone(), dp.clone());
            move |event: &str| {
                if event != "origin-between" || between.fetch_add(1, AtomicOrdering::SeqCst) > 0 {
                    return;
                }
                *peer.lock().unwrap() = Some(peer_login(&dir, "T2"));
                rx.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the peer reaches the lock wait");
                // The peer waits on the shared lock, so the file is still T1.
                assert_eq!(read_json(&file)["access_token"], "T1");
            }
        };
        set_test_hook(&via, Some(Arc::new(hook)));

        let lock = acquire_sync(&via).unwrap();
        let proof = CodexOrigin::of(&file)
            .unwrap()
            .prove(&CodexOrigin::of(&via).unwrap());
        assert_eq!(proof, OriginProof::Same);
        assert_eq!(between.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(read_json(&file)["access_token"], "T1");
        drop(lock);
        peer.lock().unwrap().take().unwrap().join().unwrap();
        assert_eq!(read_json(&file)["access_token"], "T2");
        set_test_hook(&file, None);
        set_test_hook(&via, None);
    }

    /// The control: with no lock held, a peer that replaces the file between
    /// the opens makes `prove` say Different for one directory entry. This is
    /// why `prove` needs the lock. Unix only: a rename over a file that
    /// another handle holds open on Windows depends on the share mode.
    #[cfg(unix)]
    #[test]
    fn without_the_lock_a_replacement_between_the_opens_looks_like_two_files() {
        use super::super::credential_lock::set_test_hook;
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let h = mutation_scratch();
        let hp = clean_dir(&h);
        let file = dp.join("f");
        write_lease_file(&file, "T1");
        let alias = alias_dir(dp.as_path(), hp.as_path());
        assert_alias(dp.as_path(), &alias);
        let via = alias.join("f");
        let between = Arc::new(AtomicUsize::new(0));
        let hook = {
            let (between, dir) = (between.clone(), dp.clone());
            move |event: &str| {
                if event == "origin-between" && between.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    // Nothing holds the lock, so the peer finishes inside the hook.
                    peer_login(&dir, "T2").join().unwrap();
                }
            }
        };
        set_test_hook(&via, Some(Arc::new(hook)));
        let proof = CodexOrigin::of(&file)
            .unwrap()
            .prove(&CodexOrigin::of(&via).unwrap());
        set_test_hook(&via, None);
        assert_eq!(between.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(proof, OriginProof::Different);
        assert_eq!(read_json(&file)["access_token"], "T2");
    }

    type MutationHook = Box<dyn FnOnce(&Path)>;
    thread_local! {
        pub(super) static MUTATION_HOOK: std::cell::RefCell<Option<MutationHook>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(unix)]
    #[test]
    fn fifo_json_delete_does_not_block_classification() {
        use std::os::unix::ffi::OsStrExt;
        for store_delete in [false, true] {
            let dir = mutation_scratch();
            let path = dir.path().join("x.json");
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: name is a valid NUL-terminated pathname and no pointers escape.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            let (tx, rx) = std::sync::mpsc::channel();
            let store = FileStore::new(dir.path());
            let worker = std::thread::spawn(move || {
                let result = if store_delete {
                    store.delete_auth("x.json")
                } else {
                    store.remove_file("x.json").map(drop)
                };
                tx.send(result).unwrap();
            });
            rx.recv_timeout(Duration::from_secs(2))
                .expect("FIFO deletion returns promptly")
                .unwrap();
            worker.join().unwrap();
            assert!(!path.exists());
        }
    }

    #[test]
    fn kind_change_after_classification_completes_without_codex_lock() {
        for mutation in ["write", "remove", "delete"] {
            let dir = mutation_scratch();
            let store = FileStore::new(dir.path());
            let path = dir.path().join("x.json");
            write(&path, r#"{"type":"gemini"}"#);
            // Hold the Codex lock: a reclassification/reacquisition would deadlock.
            let guard = super::super::credential_lock::acquire_sync(&path).unwrap();
            MUTATION_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(|path| {
                    write(path, r#"{"type":"codex"}"#);
                }))
            });
            match mutation {
                "write" => {
                    store.write_file("x.json", br#"{"type":"gemini"}"#).unwrap();
                }
                "remove" => {
                    store.remove_file("x.json").unwrap();
                }
                _ => {
                    store.delete_auth("x.json").unwrap();
                }
            }
            // Success while the guard is still held proves no Codex lock was
            // reacquired; filesystem latency is not part of this guarantee.
            if mutation == "write" {
                assert_eq!(read_json(&path)["type"], "gemini");
            } else {
                assert!(!path.exists());
            }
            drop(guard);
        }
    }

    #[test]
    fn deleted_disabled_credential_is_not_recreated_after_lock_wait() {
        let dir = mutation_scratch();
        let store = std::sync::Arc::new(FileStore::new(dir.path()));
        let path = dir.path().join("x.json");
        // Classification of this record is deliberately non-Codex, so deletion
        // can land while the Codex runtime snapshot waits for its lock.
        write(&path, r#"{"type":"gemini"}"#);
        let guard = super::super::credential_lock::acquire_sync(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        super::super::credential_lock::set_test_hook(
            &path,
            Some(std::sync::Arc::new(move |event| {
                if event == "waiting" {
                    let _ = tx.send(());
                }
            })),
        );
        let saving = store.clone();
        let worker = std::thread::spawn(move || {
            let auth = Auth {
                id: "x.json".into(),
                provider: "codex".into(),
                disabled: true,
                metadata: meta(json!({"type":"codex", "access_token":"snapshot"})),
                ..Auth::default()
            };
            saving.save(&auth).unwrap()
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        store.delete_auth("x.json").unwrap();
        assert!(!path.exists());
        drop(guard);
        assert!(worker.join().unwrap().is_empty());
        super::super::credential_lock::set_test_hook(&path, None);
        assert!(
            !path.exists(),
            "a deleted disabled credential must stay deleted"
        );
    }

    #[test]
    fn partition_write_keeps_one_permit_through_lock_wait_and_quiesce() {
        let dir = mutation_scratch();
        let store = std::sync::Arc::new(FileStore::new(dir.path()));
        let gate = ActiveGate::open();
        store.set_gate(gate.clone());
        let auth = Auth {
            id: "x.json".into(),
            provider: "codex".into(),
            metadata: meta(json!({"type":"codex", "access_token":"before"})),
            ..Auth::default()
        };
        store.save_new(&auth).unwrap();
        let path = dir.path().join("x.json");
        let held = super::super::credential_lock::acquire_sync(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        super::super::credential_lock::set_test_hook(
            &path,
            Some(std::sync::Arc::new(move |event| {
                if event == "waiting" {
                    let _ = tx.send(());
                }
            })),
        );
        let writing = store.clone();
        let worker = std::thread::spawn(move || {
            let after = meta(json!({"type":"codex", "access_token":"after"}));
            let intent = super::super::codex_refresh::CodexPersistIntent::Refresh(
                super::super::codex_refresh::CredentialRefreshPatch::new(
                    &auth.metadata,
                    &after,
                    true,
                    false,
                ),
            );
            writing.persist_codex_partition(&auth, &intent)
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        gate.quiesce(gate.epoch()).unwrap();
        drop(held);
        worker
            .join()
            .unwrap()
            .expect("the original write permit survives quiesce");
        super::super::credential_lock::set_test_hook(&path, None);
        assert_eq!(read_json(&path)["access_token"], "after");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "investigates Windows/tempfile replacement behavior; susceptible to external readers"]
    fn open_read_handles_block_atomic_replace_until_closed() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = mutation_scratch();
        let path = dir.path().join("reader.json");
        write_atomic(&path, b"old", true).unwrap();
        let reader = fs::File::open(&path).unwrap();
        // Exercise the exact reader and replacement used by list/save. On
        // Windows an overlapping default reader also prevents this replacement.
        let error = write_atomic(&path, b"new", true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.raw_os_error(), Some(5));
        drop(reader);
        write_atomic(&path, b"new", true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let deny_delete = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .open(&path)
            .unwrap();
        assert!(
            write_atomic(&path, b"blocked", true).is_err(),
            "a handle denying delete sharing blocks replacement"
        );
        drop(deny_delete);
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn wrapped_error_preserves_os_error_source() {
        let error = wrap(io::Error::from_raw_os_error(5), "write failed");
        let original = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(error.kind(), original.kind());
        assert_eq!(original.raw_os_error(), Some(5));
    }

    fn mutation_scratch() -> tempfile::TempDir {
        let root = std::env::var_os("OFP_TEST_SCRATCH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ofp-scratch")
            });
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    #[test]
    fn non_codex_upload_and_delete_leave_no_lock() {
        for store_delete in [false, true] {
            for data in [
                br#"{"type":"gemini"}"#.as_slice(),
                br#"{"type":"antigravity"}"#.as_slice(),
                b"{}".as_slice(),
                b"not JSON".as_slice(),
            ] {
                let dir = mutation_scratch();
                let store = FileStore::new(dir.path());
                let path = store.write_file("a.json", data).unwrap();
                let lock = dir.path().join("a.json.lock");
                assert!(!lock.exists());
                if store_delete {
                    store.delete_auth("a.json").unwrap();
                } else {
                    store.remove_file("a.json").unwrap();
                }
                assert!(!path.exists());
                assert!(!lock.exists());
            }
        }
    }

    #[test]
    fn codex_upload_and_delete_keep_permanent_lock() {
        for store_delete in [false, true] {
            let dir = mutation_scratch();
            let store = FileStore::new(dir.path());
            let path = store
                .write_file("a.json", br#"{"type":" CoDeX "}"#)
                .unwrap();
            let lock = dir.path().join("a.json.lock");
            assert!(lock.is_file());
            if store_delete {
                store.delete_auth("a.json").unwrap();
            } else {
                store.remove_file("a.json").unwrap();
            }
            assert!(!path.exists());
            assert!(lock.is_file());
        }
    }

    #[test]
    fn replacing_existing_codex_with_non_codex_takes_lock() {
        let dir = mutation_scratch();
        let store = FileStore::new(dir.path());
        let path = dir.path().join("a.json");
        write(&path, r#"{"type":"codex"}"#);
        store.write_file("a.json", br#"{"type":"gemini"}"#).unwrap();
        assert!(dir.path().join("a.json.lock").is_file());
    }

    #[test]
    fn deleting_codex_waits_for_held_lock() {
        for store_delete in [false, true] {
            let dir = mutation_scratch();
            let store = std::sync::Arc::new(FileStore::new(dir.path()));
            let path = store.write_file("a.json", br#"{"type":"codex"}"#).unwrap();
            let guard = super::super::credential_lock::acquire_sync(&path).unwrap();
            let (waiting, waited) = std::sync::mpsc::channel();
            super::super::credential_lock::set_test_hook(
                &path,
                Some(std::sync::Arc::new(move |event| {
                    if event == "waiting" {
                        let _ = waiting.send(());
                    }
                })),
            );
            let (finished, done) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                if store_delete {
                    store.delete_auth("a.json").unwrap();
                } else {
                    store.remove_file("a.json").unwrap();
                }
                finished.send(()).unwrap();
            });
            waited.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(
                done.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            assert!(path.exists());
            drop(guard);
            done.recv_timeout(Duration::from_secs(5)).unwrap();
            worker.join().unwrap();
            super::super::credential_lock::set_test_hook(&path, None);
            assert!(!path.exists());
            assert!(dir.path().join("a.json.lock").is_file());
        }
    }

    #[test]
    fn save_existing_metadata_sets_file_attributes() {
        for (existing_token, saved_token) in [("token", "token"), ("old-token", "new-token")] {
            let dir = tempfile::tempdir().unwrap();
            let file_name = "antigravity-user.json";
            let path = dir.path().join(file_name);
            write(
                &path,
                &format!(
                    r#"{{"type":"antigravity","access_token":"{existing_token}","disabled":false}}"#
                ),
            );
            let store = FileStore::new(dir.path());
            let mut auth = Auth {
                id: file_name.into(),
                file_name: file_name.into(),
                metadata: meta(json!({"type": "antigravity", "access_token": saved_token})),
                ..Auth::default()
            };
            let saved = store.save_auth(&mut auth).unwrap();
            assert_eq!(saved, path_string(&path));
            assert_eq!(auth.attribute(ATTRIBUTE_PATH), Some(saved.as_str()));
            assert_eq!(auth.attribute(ATTRIBUTE_SOURCE), Some(saved.as_str()));
            assert_eq!(
                auth.attribute(ATTRIBUTE_SOURCE_BACKEND),
                Some(AUTH_SOURCE_FILE)
            );
            assert_eq!(
                read_json(&path),
                json!({"type": "antigravity", "access_token": saved_token, "disabled": false})
            );
        }
    }

    #[test]
    fn unchanged_content_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        // Same JSON, different formatting: the file must stay as it is.
        let original = "{ \"type\": \"codex\", \"n\": 1.0, \"disabled\": false }";
        write(&path, original);
        let store = FileStore::new(dir.path());
        let mut auth = Auth {
            id: "a.json".into(),
            metadata: meta(json!({"type": "codex", "n": 1})),
            ..Auth::default()
        };
        store.save_auth(&mut auth).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(auth.file_name, "a.json");
    }

    #[test]
    fn normalizes_legacy_credential_metadata_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let mut auth = Auth {
            id: "legacy-save.json".into(),
            file_name: "legacy-save.json".into(),
            metadata: meta(json!({
                "type": "codex",
                "request-retry": 2,
                "request_retry": 0,
                "disable-cooling": true,
            })),
            ..Auth::default()
        };
        let path = store.save_auth(&mut auth).unwrap();
        assert_eq!(
            read_json(Path::new(&path)),
            json!({"type": "codex", "request_retry": 0, "disable_cooling": true, "disabled": false})
        );
    }

    #[test]
    fn normalizes_legacy_credential_metadata_on_list() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("legacy-list.json"),
            r#"{"type":"codex","request-retry":2,"disable-cooling":true}"#,
        );
        let auths = FileStore::new(dir.path()).list().unwrap();
        assert_eq!(auths.len(), 1);
        let metadata = &auths[0].metadata;
        assert_eq!(metadata["request_retry"].as_f64(), Some(2.0));
        assert_eq!(metadata["disable_cooling"], Value::Bool(true));
        assert!(!metadata.contains_key("request-retry"));
        assert!(!metadata.contains_key("disable-cooling"));
    }

    #[test]
    fn save_rejects_invalid_weight() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let mut auth = Auth {
            id: "invalid.json".into(),
            file_name: "invalid.json".into(),
            metadata: meta(json!({"type": "test", "weight": 1.5})),
            ..Auth::default()
        };
        let err = store.save_auth(&mut auth).unwrap_err();
        assert_eq!(
            err.to_string(),
            "auth filestore: invalid metadata weight: weight must be an integer"
        );
        assert!(!dir.path().join("invalid.json").exists());
    }

    #[test]
    fn save_refuses_a_siwc_bind_intent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let path = dir.path().join("codex.json");
        std::fs::write(&path, b"{corrupt").unwrap();
        let bind = || Auth {
            id: "codex.json".into(),
            file_name: "codex.json".into(),
            provider: "codex".into(),
            metadata: meta(json!({"type": "codex", "access_token": "a", "refresh_token": "r"})),
            codex_persist_intent: super::super::codex_refresh::CodexPersistIntent::SiwcBind(
                Map::new(),
            ),
            ..Auth::default()
        };
        // Neither save may replace the corrupt file a strict bind refuses.
        let err = store.save_auth(&mut bind()).unwrap_err();
        assert!(err.to_string().contains("bind_siwc_locked"), "{err}");
        let err = store.save_new_auth(&mut bind()).unwrap_err();
        assert!(err.to_string().contains("bind_siwc_locked"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{corrupt");
    }

    #[test]
    fn disabled_save_persists_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disabled.json");
        write(&path, r#"{"type":"test","disabled":true}"#);
        let store = FileStore::new(dir.path());
        let mut auth = Auth {
            id: "disabled.json".into(),
            provider: "test".into(),
            file_name: "disabled.json".into(),
            disabled: true,
            metadata: meta(json!({"type": "test"})),
            ..Auth::default()
        };
        store.save_auth(&mut auth).unwrap();
        assert_eq!(read_json(&path)["disabled"], Value::Bool(true));
    }

    #[test]
    fn disabled_login_creates_canonical_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("canonical-disabled.json");
        let store = FileStore::new(dir.path());
        let auth = Auth {
            id: "canonical-disabled.json".into(),
            provider: "test".into(),
            file_name: "canonical-disabled.json".into(),
            disabled: true,
            metadata: meta(json!({"type": "test"})),
            ..Auth::default()
        };
        let saved = store.save_new(&auth).unwrap();
        assert_eq!(saved, path_string(&path));
        assert_eq!(read_json(&path)["disabled"], Value::Bool(true));
    }

    #[test]
    fn disabled_runtime_save_does_not_recreate_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removed-disabled.json");
        let store = FileStore::new(dir.path());
        let auth = Auth {
            id: "removed-disabled.json".into(),
            provider: "test".into(),
            file_name: "removed-disabled.json".into(),
            disabled: true,
            metadata: meta(json!({"type": "test"})),
            ..Auth::default()
        };
        assert_eq!(store.save(&auth).unwrap(), "");
        assert!(!path.exists());
    }

    #[test]
    fn list_loads_proxy_url_and_prefix() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("antigravity.json"),
            r#"{
                "type": "antigravity",
                "proxy_url": " http://127.0.0.1:7890 ",
                "prefix": "/custom-prefix/",
                "project_id": "test-project",
                "access_token": "ya29.test-token"
            }"#,
        );
        let started = Instant::now();
        let auths = FileStore::new(dir.path()).list().unwrap();
        // Loading files must not wait on the network.
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.proxy_url, "http://127.0.0.1:7890");
        assert_eq!(auth.prefix, "custom-prefix");
        assert_eq!(auth.provider, "antigravity");
        assert_eq!(auth.label, "test-project");
        assert_eq!(auth.status, Status::Active);
        assert!(auth.created_at.is_some());
    }

    #[test]
    fn list_reads_fields_and_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex-a.json");
        write(
            &path,
            r#"{"type":" codex ","email":"a@example.com","label":"","disabled":true,
                "prefix":"a/b","headers":{" X-Team ":" blue ","X-Empty":" "},"priority":3}"#,
        );
        let auths = FileStore::new(dir.path()).list().unwrap();
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.id, "codex-a.json");
        assert_eq!(auth.file_name, "codex-a.json");
        assert_eq!(auth.provider, "codex");
        assert_eq!(auth.label, "a@example.com");
        assert_eq!(auth.prefix, "");
        assert!(auth.disabled);
        assert_eq!(auth.status, Status::Disabled);
        assert_eq!(auth.attribute("email"), Some("a@example.com"));
        assert_eq!(auth.attribute("header:X-Team"), Some("blue"));
        assert_eq!(auth.attribute("header:X-Empty"), None);
        assert_eq!(
            auth.attribute(ATTRIBUTE_PATH),
            Some(path_string(&path).as_str())
        );
        assert_eq!(
            auth.attribute(ATTRIBUTE_SOURCE_BACKEND),
            Some(AUTH_SOURCE_FILE)
        );
        // Priority and weight are the synthesizer's to turn into attributes.
        assert_eq!(auth.attribute("priority"), None);
    }

    #[test]
    fn list_walks_subdirectories_and_skips_what_it_cannot_use() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("b.json"), r#"{"type":"claude"}"#);
        write(&root.join("sub").join("a.JSON"), r#"{"type":"codex"}"#);
        write(&root.join(".hidden.json"), r#"{"type":"codex"}"#);
        write(&root.join("no-type.json"), r#"{"email":"x@example.com"}"#);
        write(&root.join("null.json"), "null");
        write(&root.join("empty.json"), "");
        write(&root.join("invalid.json"), "not json");
        write(&root.join("array.json"), "[1]");
        write(
            &root.join("huge-number.json"),
            r#"{"type":"codex","n":1e400}"#,
        );
        write(&root.join("gemini.json"), r#"{"type":"Gemini"}"#);
        write(
            &root.join("bad-weight.json"),
            r#"{"type":"codex","weight":"heavy"}"#,
        );
        write(&root.join("notes.txt"), r#"{"type":"codex"}"#);
        fs::create_dir(root.join("dir.json")).unwrap();

        let auths = FileStore::new(root).list().unwrap();
        let ids: Vec<(&str, &str)> = auths
            .iter()
            .map(|auth| (auth.id.as_str(), auth.provider.as_str()))
            .collect();
        let nested = Path::new("sub").join("a.JSON");
        let nested = if cfg!(windows) {
            nested.to_str().unwrap().to_lowercase()
        } else {
            nested.to_str().unwrap().to_owned()
        };
        assert_eq!(
            ids,
            [
                (".hidden.json", "codex"),
                ("b.json", "claude"),
                ("no-type.json", "unknown"),
                ("null.json", "unknown"),
                (nested.as_str(), "codex"),
            ]
        );
    }

    #[test]
    fn list_needs_a_directory() {
        let store = FileStore::new("  ");
        assert_eq!(
            store.list().unwrap_err().to_string(),
            "auth filestore: directory not configured"
        );
        let dir = tempfile::tempdir().unwrap();
        let missing = FileStore::new(dir.path().join("missing"));
        assert_eq!(missing.list().unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn unknown_keys_survive_a_save_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude-a.json");
        write(
            &path,
            r#"{"type":"claude","access_token":"old","claude_device_ids":{"opaque":["x",1.50]},"extra":null}"#,
        );
        let store = FileStore::new(dir.path());
        let mut auths = store.list().unwrap();
        let mut auth = auths.remove(0);
        auth.metadata
            .insert("access_token".into(), Value::from("new"));
        store.save_auth(&mut auth).unwrap();
        // Written as Go writes a map: keys sorted, values as they were read.
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{"access_token":"new","claude_device_ids":{"opaque":["x",1.50]},"disabled":false,"extra":null,"type":"claude"}"#
        );
    }

    #[test]
    fn save_creates_directories_and_resolves_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let mut nested = Auth {
            id: "ignored".into(),
            file_name: "sub/dir/./x.json".into(),
            metadata: meta(json!({"type": "codex"})),
            ..Auth::default()
        };
        let saved = store.save_auth(&mut nested).unwrap();
        let want = dir.path().join("sub").join("dir").join("x.json");
        assert_eq!(saved, path_string(&want));
        assert!(want.is_file());

        let explicit = dir.path().join("explicit.json");
        let mut by_attr = Auth {
            id: "whatever".into(),
            metadata: meta(json!({"type": "codex"})),
            ..Auth::default()
        };
        by_attr.attributes.insert(
            ATTRIBUTE_PATH.into(),
            format!(" {} ", path_string(&explicit)),
        );
        assert_eq!(
            store.save_auth(&mut by_attr).unwrap(),
            path_string(&explicit)
        );
        assert_eq!(by_attr.file_name, "whatever");

        let unconfigured = FileStore::default();
        let mut by_id = Auth {
            id: "x.json".into(),
            metadata: meta(json!({"type": "codex"})),
            ..Auth::default()
        };
        assert_eq!(
            unconfigured.save_auth(&mut by_id).unwrap_err().to_string(),
            "auth filestore: directory not configured"
        );
        let mut no_id = Auth::default();
        assert_eq!(
            store.save_auth(&mut no_id).unwrap_err().to_string(),
            "auth filestore: missing id"
        );
        let mut empty = Auth {
            id: "empty.json".into(),
            ..Auth::default()
        };
        assert_eq!(
            store.save_auth(&mut empty).unwrap_err().to_string(),
            "auth filestore: nothing to persist for empty.json"
        );
    }

    #[cfg(unix)]
    #[test]
    fn new_files_are_private_and_existing_modes_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("auths"));
        let mut auth = Auth {
            id: "a.json".into(),
            metadata: meta(json!({"type": "codex"})),
            ..Auth::default()
        };
        let path = PathBuf::from(store.save_auth(&mut auth).unwrap());
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir.path().join("auths")), 0o700);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        auth.metadata.insert("n".into(), Value::from(1));
        store.save_auth(&mut auth).unwrap();
        assert_eq!(mode(&path), 0o640);
    }

    #[test]
    fn save_leaves_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        for n in 0..3 {
            let mut auth = Auth {
                id: "a.json".into(),
                metadata: meta(json!({"type": "codex", "n": n})),
                ..Auth::default()
            };
            store.save_auth(&mut auth).unwrap();
        }
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        // The credential lock is permanent; temporary replacement files aren't.
        assert_eq!(names, ["a.json", "a.json.lock"]);
    }

    #[test]
    fn delete_removes_files_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        write(&dir.path().join("a.json"), "{}");
        write(&dir.path().join("sub").join("b.json"), "{}");

        store.delete(" a.json ").unwrap();
        assert!(!dir.path().join("a.json").exists());

        let nested_id = Path::new("sub").join("b.json");
        store.delete(nested_id.to_str().unwrap()).unwrap();
        assert!(!dir.path().join("sub").join("b.json").exists());

        store.delete("missing.json").unwrap();
        assert_eq!(
            store.delete("  ").unwrap_err().to_string(),
            "auth filestore: id is empty"
        );
        assert_eq!(
            FileStore::default()
                .delete("a.json")
                .unwrap_err()
                .to_string(),
            "auth filestore: directory not configured"
        );
    }

    #[test]
    fn merge_existing_keeps_file_settings() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("codex-a.json"),
            r#"{"type":"codex","access_token":"old","prefix":"team","disabled":true,"note":"keep"}"#,
        );
        let store = FileStore::new(dir.path());
        let mut fresh = Auth {
            id: "codex-a.json".into(),
            metadata: meta(json!({"type": "codex", "access_token": "new"})),
            ..Auth::default()
        };
        store.merge_existing(&mut fresh);
        assert_eq!(fresh.metadata["access_token"], Value::from("new"));
        assert_eq!(fresh.metadata["prefix"], Value::from("team"));
        assert_eq!(fresh.metadata["note"], Value::from("keep"));
        assert!(fresh.disabled);

        let mut absent = Auth {
            id: "other.json".into(),
            ..Auth::default()
        };
        store.merge_existing(&mut absent);
        assert!(absent.metadata.is_empty());
    }

    // Not upstream's: the existing file's settings, told apart from no file.
    #[test]
    fn existing_metadata_is_none_without_an_object() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let named = |name: &str| Auth {
            id: name.into(),
            ..Auth::default()
        };
        for (name, content) in [
            ("empty.json", ""),
            ("null.json", "null"),
            ("broken.json", "{"),
            ("list.json", "[]"),
        ] {
            write(&dir.path().join(name), content);
            assert_eq!(store.existing_metadata(&named(name)), None, "{name}");
        }
        assert_eq!(store.existing_metadata(&named("missing.json")), None);
        write(&dir.path().join("bare.json"), "{}");
        assert_eq!(
            store.existing_metadata(&named("bare.json")),
            Some(Map::new())
        );
        let mut by_file_name = named("other-id");
        by_file_name.file_name = "list.json".into();
        write(&dir.path().join("list.json"), r#"{"prefix":"team"}"#);
        assert_eq!(
            store.existing_metadata(&by_file_name),
            Some(meta(json!({"prefix": "team"})))
        );
        assert_eq!(
            FileStore::new("").existing_metadata(&named("bare.json")),
            None
        );
    }

    // Not upstream's: raw writes and removes of one file in the directory.
    #[test]
    fn files_are_written_and_removed_by_plain_name() {
        let dir = tempfile::tempdir().unwrap();
        let auth_dir = dir.path().join("auths");
        let store = FileStore::new(&auth_dir);
        let path = store
            .write_file("codex-a.json", b"{\"type\":\"codex\"}")
            .unwrap();
        assert_eq!(path, auth_dir.join("codex-a.json"));
        assert_eq!(fs::read(&path).unwrap(), b"{\"type\":\"codex\"}");
        store.write_file("codex-a.json", b"not json").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"not json");
        assert_eq!(fs::read_dir(&auth_dir).unwrap().count(), 2);
        assert!(auth_dir.join("codex-a.json.lock").is_file());

        assert_eq!(store.remove_file("codex-a.json").unwrap(), path);
        assert!(!path.exists());
        let missing = store.remove_file("codex-a.json").unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);

        for name in [
            "",
            ".",
            "..",
            "a/b.json",
            "a\\b.json",
            "../a.json",
            "/a.json",
        ] {
            let err = store.write_file(name, b"{}").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name:?}");
            let err = store.remove_file(name).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        }
        #[cfg(windows)]
        for name in ["C:a.json", "C:\\a.json", "\\\\server\\share\\a.json"] {
            assert!(store.file_path(name).is_err(), "{name:?}");
        }
        assert!(FileStore::new("").write_file("a.json", b"{}").is_err());
    }

    // Not upstream's: how raw writes and removes treat symlinks.
    #[cfg(unix)]
    #[test]
    fn raw_writes_follow_symlinks_and_removes_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        write(&target, "{}");
        let auth_dir = dir.path().join("auths");
        fs::create_dir(&auth_dir).unwrap();
        std::os::unix::fs::symlink(&target, auth_dir.join("link.json")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone"), auth_dir.join("dangling.json"))
            .unwrap();

        let store = FileStore::new(&auth_dir);
        store.write_file("link.json", b"{\"a\":1}").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"{\"a\":1}");
        store.write_file("dangling.json", b"{}").unwrap();
        let dangling = fs::symlink_metadata(auth_dir.join("dangling.json")).unwrap();
        assert!(dangling.file_type().is_file());

        store.remove_file("link.json").unwrap();
        assert!(!auth_dir.join("link.json").exists());
        assert!(target.exists());
    }

    #[cfg(windows)]
    #[test]
    fn ids_are_lowercase_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Codex-A.JSON"), r#"{"type":"codex"}"#);
        let auths = FileStore::new(dir.path()).list().unwrap();
        assert_eq!(auths[0].id, "codex-a.json");
        assert!(
            auths[0]
                .attribute(ATTRIBUTE_PATH)
                .unwrap()
                .ends_with("Codex-A.JSON")
        );
    }

    // Part A2: the bound and guarded saves, and the sign-in compose rule.

    type BoundCalls = Vec<(CodexOrigin, OriginProof)>;

    /// `save_bound`, with the callback's calls collected.
    fn bound(
        store: &FileStore,
        auth: &Auth,
        origin: &CodexOrigin,
    ) -> (io::Result<Option<String>>, BoundCalls) {
        let mut calls = Vec::new();
        let result = store.save_bound(auth, origin, &mut |current, proof| {
            calls.push((current.clone(), proof));
        });
        (result, calls)
    }

    /// Whether another thread of this process times out waiting for the
    /// credential lock of `path`, that is, whether somebody holds it.
    fn lock_is_held(path: &Path) -> bool {
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(super::super::credential_lock::acquire_with_timeout(
                path,
                Duration::from_millis(50),
            ))
        })
        .join()
        .unwrap()
        .is_err_and(|error| error.kind() == io::ErrorKind::TimedOut)
    }

    fn lock_file(path: &Path) -> PathBuf {
        super::super::credential_lock::credential_lock_path(path).unwrap()
    }

    #[test]
    fn save_bound_writes_the_origin_and_refuses_a_rebound_file() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let e = mutation_scratch();
        let ep = clean_dir(&e);
        let store = FileStore::new(dp.as_path());
        let auth = lease_record("f", "t");
        let origin = CodexOrigin::of(&dp.join("f")).unwrap();

        // (a) The relative record resolves to the origin: lexical Same.
        let (result, calls) = bound(&store, &auth, &origin);
        assert_eq!(result.unwrap(), Some(path_string(&dp.join("f"))));
        assert!(calls.is_empty());
        let written = fs::read(dp.join("f")).unwrap();

        // (b) After a rebind to E, with no E/f: Different, under E/f's lock.
        store.set_base_dir(&ep);
        let mut calls = Vec::new();
        let mut held = None;
        let result = store.save_bound(&auth, &origin, &mut |current, proof| {
            held = Some(lock_is_held(&ep.join("f")));
            calls.push((current.clone(), proof));
        });
        assert_eq!(result.unwrap(), None);
        assert_eq!(
            calls,
            vec![(
                CodexOrigin::of(&ep.join("f")).unwrap(),
                OriginProof::Different
            )]
        );
        assert_eq!(held, Some(true), "the callback runs under E/f's lock");
        assert!(!ep.join("f").exists());
        assert!(lock_file(&ep.join("f")).exists());
        assert_eq!(fs::read(dp.join("f")).unwrap(), written);

        // (c) Rebound to D again: it writes with no callback.
        store.set_base_dir(&dp);
        let mut changed = auth.clone();
        changed
            .metadata
            .insert("note".into(), json!("written again"));
        let (result, calls) = bound(&store, &changed, &origin);
        assert_eq!(result.unwrap(), Some(path_string(&dp.join("f"))));
        assert!(calls.is_empty());
        assert_eq!(read_json(&dp.join("f"))["note"], "written again");

        // (d) An absolute path attribute names D/f whatever the base dir.
        store.set_base_dir(&ep);
        let mut absolute = lease_record("f", "t");
        absolute
            .attributes
            .insert(ATTRIBUTE_PATH.into(), path_string(&dp.join("f")));
        absolute.metadata.insert("note".into(), json!("by path"));
        let (result, calls) = bound(&store, &absolute, &origin);
        assert_eq!(result.unwrap(), Some(path_string(&dp.join("f"))));
        assert!(calls.is_empty());
        assert_eq!(read_json(&dp.join("f"))["note"], "by path");
        assert!(!ep.join("f").exists());
    }

    /// A record whose path attribute names `file`.
    #[cfg(any(unix, windows))]
    fn at_path(record: Auth, file: &Path) -> Auth {
        let mut record = record;
        record
            .attributes
            .insert(ATTRIBUTE_PATH.into(), path_string(file));
        record
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn save_bound_proves_an_alias_under_its_lock() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let h = mutation_scratch();
        let hp = clean_dir(&h);
        let store = FileStore::new(dp.as_path());
        let alias = alias_dir(dp.as_path(), hp.as_path());
        assert_alias(dp.as_path(), &alias);
        let via = alias.join("f");
        let origin = CodexOrigin::of(&dp.join("f")).unwrap();
        let auth = lease_record("f", "t");
        let (result, _) = bound(&store, &auth, &origin);
        assert!(result.unwrap().is_some());

        // (e) The alias differs lexically; the proof finds Same and it writes.
        let mut by_alias = at_path(auth.clone(), &via);
        by_alias.metadata.insert("note".into(), json!("via alias"));
        let (result, calls) = bound(&store, &by_alias, &origin);
        assert_eq!(result.unwrap(), Some(path_string(&via)));
        assert!(calls.is_empty());
        assert_eq!(read_json(&dp.join("f"))["note"], "via alias");

        // (f) With D/f removed the pinned open fails: Unproven, nothing written.
        fs::remove_file(dp.join("f")).unwrap();
        let (result, calls) = bound(&store, &by_alias, &origin);
        assert_eq!(result.unwrap(), None);
        assert_eq!(
            calls,
            vec![(CodexOrigin::of(&via).unwrap(), OriginProof::Unproven)]
        );
        assert!(!dp.join("f").exists());
        assert!(!via.exists());
    }

    #[test]
    fn save_bound_does_not_call_a_pinned_origin_that_never_existed_different() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        write_lease_file(&dp.join("f"), "disk");
        let before = fs::read(dp.join("f")).unwrap();
        // (g) The pinned D/h never existed.
        let origin = CodexOrigin::of(&dp.join("h")).unwrap();
        let (result, calls) = bound(&store, &lease_record("f", "t"), &origin);
        assert_eq!(result.unwrap(), None);
        assert_eq!(
            calls,
            vec![(
                CodexOrigin::of(&dp.join("f")).unwrap(),
                OriginProof::Unproven
            )]
        );
        assert_eq!(fs::read(dp.join("f")).unwrap(), before);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn save_bound_checks_a_record_that_is_not_codex_lexically_only() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let h = mutation_scratch();
        let hp = clean_dir(&h);
        let store = FileStore::new(dp.as_path());
        let alias = alias_dir(dp.as_path(), hp.as_path());
        assert_alias(dp.as_path(), &alias);
        write_lease_file(&dp.join("f"), "disk");
        let before = fs::read(dp.join("f")).unwrap();
        let via = alias.join("f");
        // (h) No credential lock for it, so the alias is Unproven, never Different.
        let mut gemini = at_path(lease_record("f", "t"), &via);
        gemini.provider = "gemini".into();
        gemini.metadata.insert("type".into(), json!("gemini"));
        let origin = CodexOrigin::of(&dp.join("f")).unwrap();
        let (result, calls) = bound(&store, &gemini, &origin);
        assert_eq!(result.unwrap(), None);
        assert_eq!(
            calls,
            vec![(CodexOrigin::of(&via).unwrap(), OriginProof::Unproven)]
        );
        assert_eq!(fs::read(dp.join("f")).unwrap(), before);
        let locks = fs::read_dir(dp.as_path())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".lock")
            })
            .count();
        assert_eq!(locks, 0);
    }

    /// A-10 (b): the same proof through the store, with a peer that cannot
    /// replace the file while the save holds the alias's lock.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_bound_save_proves_under_the_lock_while_a_peer_waits() {
        use super::super::credential_lock::set_test_hook;
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let h = mutation_scratch();
        let hp = clean_dir(&h);
        let file = dp.join("f");
        write_lease_file(&file, "T1");
        let alias = alias_dir(dp.as_path(), hp.as_path());
        assert_alias(dp.as_path(), &alias);
        let via = alias.join("f");

        let (tx, rx) = mpsc::channel::<()>();
        set_test_hook(
            &file,
            Some(Arc::new(move |event| {
                if event == "waiting" {
                    let _ = tx.send(());
                }
            })),
        );
        let rx = std::sync::Mutex::new(rx);
        let peer = Arc::new(std::sync::Mutex::new(None));
        let between = Arc::new(AtomicUsize::new(0));
        let hook = {
            let (peer, between, file, dir) =
                (peer.clone(), between.clone(), file.clone(), dp.clone());
            move |event: &str| {
                if event != "origin-between" || between.fetch_add(1, AtomicOrdering::SeqCst) > 0 {
                    return;
                }
                *peer.lock().unwrap() = Some(peer_login(&dir, "T2"));
                rx.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the peer reaches the lock wait");
                assert_eq!(read_json(&file)["access_token"], "T1");
            }
        };
        set_test_hook(&via, Some(Arc::new(hook)));

        let store = FileStore::new(alias.as_path());
        // The Configuration intent keeps the file's tokens, T1.
        let auth = lease_record("f", "other");
        let origin = CodexOrigin::of(&file).unwrap();
        let (result, calls) = bound(&store, &auth, &origin);
        set_test_hook(&file, None);
        set_test_hook(&via, None);
        assert_eq!(result.unwrap(), Some(path_string(&via)));
        assert!(calls.is_empty());
        assert_eq!(between.load(AtomicOrdering::SeqCst), 1);
        peer.lock().unwrap().take().unwrap().join().unwrap();
        assert_eq!(read_json(&file)["access_token"], "T2");
    }

    /// A full set of `OUTER_KEYS` for `token`.
    fn full_outer(token: &str) -> Value {
        json!({
            "id_token": format!("id-{token}"),
            "access_token": format!("access-{token}"),
            "refresh_token": format!("refresh-{token}"),
            "account_id": format!("account-{token}"),
            "last_refresh": "2026-10-12T12:00:00Z",
            "email": format!("{token}@example.com"),
            "expired": "2099-01-01T00:00:00Z",
            "plan_type": "plus",
        })
    }

    /// A Codex record `f` holding `parts` (objects merged over `type`).
    fn codex_holding(parts: &[Value]) -> Auth {
        let mut metadata = meta(json!({"type": "codex"}));
        for part in parts {
            metadata.extend(part.as_object().unwrap().clone());
        }
        Auth {
            id: "f".into(),
            file_name: "f".into(),
            provider: "codex".into(),
            metadata,
            ..Auth::default()
        }
    }

    fn disk_with(path: &Path, parts: &[Value]) {
        write(
            path,
            &Value::Object(codex_holding(parts).metadata).to_string(),
        );
    }

    type GuardSeen = Vec<Option<CodexOrigin>>;

    /// `save_guarded`, with the origins the guard received collected.
    fn guarded(
        store: &FileStore,
        auth: &Auth,
        result: Result<WithheldPartitions, SaveDeferral>,
    ) -> (io::Result<GuardedSave>, GuardSeen) {
        let mut seen = Vec::new();
        let saved = store.save_guarded(auth, &mut |origin| {
            seen.push(origin.cloned());
            result
        });
        (saved, seen)
    }

    const OUTER_ONLY: WithheldPartitions = WithheldPartitions {
        outer: true,
        siwc: false,
    };
    const SIWC_ONLY: WithheldPartitions = WithheldPartitions {
        outer: false,
        siwc: true,
    };

    #[test]
    fn a_guard_that_withholds_nothing_saves_as_save_does() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let e = mutation_scratch();
        let ep = clean_dir(&e);
        let start = [full_outer("A"), json!({"prefix": "old"})];
        disk_with(&dp.join("f"), &start);
        disk_with(&ep.join("f"), &start);
        let record = codex_holding(&[full_outer("B"), json!({"prefix": "new"})]);

        let store = FileStore::new(dp.as_path());
        let mut held = None;
        let mut seen = Vec::new();
        let saved = store
            .save_guarded(&record, &mut |origin| {
                held = Some(lock_is_held(&dp.join("f")));
                seen.push(origin.cloned());
                Ok(WithheldPartitions::default())
            })
            .unwrap();
        assert!(matches!(saved, GuardedSave::Written(path) if path == path_string(&dp.join("f"))));
        assert_eq!(seen, vec![Some(CodexOrigin::of(&dp.join("f")).unwrap())]);
        assert_eq!(held, Some(true), "the guard runs under the credential lock");

        FileStore::new(ep.as_path())
            .save_auth(&mut record.clone())
            .unwrap();
        assert_eq!(
            fs::read(dp.join("f")).unwrap(),
            fs::read(ep.join("f")).unwrap()
        );
    }

    #[test]
    fn a_guard_refusal_writes_nothing() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        let record = codex_holding(&[full_outer("B")]);
        disk_with(&dp.join("f"), &[full_outer("A")]);
        let before = fs::read(dp.join("f")).unwrap();
        let (saved, seen) = guarded(&store, &record, Err(SaveDeferral::Stale));
        assert!(matches!(
            saved.unwrap(),
            GuardedSave::Deferred(SaveDeferral::Stale)
        ));
        assert_eq!(seen.len(), 1);
        assert_eq!(fs::read(dp.join("f")).unwrap(), before);

        // With no file, none is made; the lock file is.
        fs::remove_file(dp.join("f")).unwrap();
        let (saved, _) = guarded(&store, &record, Err(SaveDeferral::Stale));
        assert!(matches!(
            saved.unwrap(),
            GuardedSave::Deferred(SaveDeferral::Stale)
        ));
        assert!(!dp.join("f").exists());
        assert!(lock_file(&dp.join("f")).exists());
    }

    #[test]
    fn a_withheld_outer_partition_is_written_as_the_file_holds_it() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        disk_with(&dp.join("f"), &[full_outer("A"), json!({"prefix": "old"})]);
        // Outer B, a changed prefix, and no plan_type.
        let mut incoming = full_outer("B");
        incoming.as_object_mut().unwrap().remove("plan_type");
        let record = codex_holding(&[incoming, json!({"prefix": "new"})]);
        let (saved, _) = guarded(&store, &record, Ok(OUTER_ONLY));
        assert!(matches!(saved.unwrap(), GuardedSave::Written(_)));
        let on_disk = read_json(&dp.join("f"));
        let want = full_outer("A");
        for key in crate::auth::codex_refresh::OUTER_KEYS {
            assert_eq!(on_disk[*key], want[*key], "{key}");
        }
        assert_eq!(on_disk["plan_type"], "plus");
        assert_eq!(on_disk["prefix"], "new");
    }

    #[test]
    fn a_withheld_partition_of_a_missing_file_is_not_created() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let e = mutation_scratch();
        let ep = clean_dir(&e);
        let record = codex_holding(&[full_outer("B")]);
        let (saved, _) = guarded(&FileStore::new(dp.as_path()), &record, Ok(OUTER_ONLY));
        assert!(matches!(
            saved.unwrap(),
            GuardedSave::Deferred(SaveDeferral::MissingFile)
        ));
        assert!(!dp.join("f").exists());
        // The control shows the creation the stage stops.
        FileStore::new(ep.as_path())
            .save_auth(&mut record.clone())
            .unwrap();
        assert_eq!(read_json(&ep.join("f"))["refresh_token"], "refresh-B");
    }

    #[test]
    fn a_file_lacking_a_withheld_partition_the_record_holds_defers() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        disk_with(&dp.join("f"), &[full_outer("A")]);
        let before = fs::read(dp.join("f")).unwrap();
        let siwc = json!({"siwc": {"access_token": "s", "refresh_token": "s-refresh"}});
        let record = codex_holding(&[full_outer("A"), siwc]);
        let (saved, _) = guarded(&store, &record, Ok(SIWC_ONLY));
        assert!(matches!(
            saved.unwrap(),
            GuardedSave::Deferred(SaveDeferral::MissingPartition)
        ));
        assert_eq!(fs::read(dp.join("f")).unwrap(), before);

        // A record with no SIWC partition either is written, and gains none.
        let record = codex_holding(&[full_outer("A"), json!({"prefix": "new"})]);
        let (saved, _) = guarded(&store, &record, Ok(SIWC_ONLY));
        assert!(matches!(saved.unwrap(), GuardedSave::Written(_)));
        let on_disk = read_json(&dp.join("f"));
        assert!(on_disk.get("siwc").is_none());
        assert_eq!(on_disk["prefix"], "new");
    }

    #[test]
    fn a_withheld_outer_partition_ignores_the_missing_oauth_markers() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let e = mutation_scratch();
        let ep = clean_dir(&e);
        let start = json!({"access_token": "access-A", "account_id": "account-A"});
        disk_with(&dp.join("f"), std::slice::from_ref(&start));
        disk_with(&ep.join("f"), std::slice::from_ref(&start));
        let record = codex_holding(&[full_outer("B")]);

        let (saved, _) = guarded(&FileStore::new(dp.as_path()), &record, Ok(OUTER_ONLY));
        assert!(matches!(saved.unwrap(), GuardedSave::Written(_)));
        let on_disk = read_json(&dp.join("f"));
        for key in crate::auth::codex_refresh::OUTER_KEYS {
            assert_eq!(on_disk.get(*key), start.get(*key), "{key}");
        }
        assert!(on_disk.get("refresh_token").is_none());

        // Today's merge imports the record's refresh token from a markerless file.
        FileStore::new(ep.as_path())
            .save_auth(&mut record.clone())
            .unwrap();
        assert_eq!(read_json(&ep.join("f"))["refresh_token"], "refresh-B");
    }

    #[test]
    fn a_withheld_siwc_partition_survives_an_outer_login_save() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        let siwc = |token: &str| json!({"siwc": {"refresh_token": token}, "use_siwc": true});
        disk_with(&dp.join("f"), &[full_outer("A"), siwc("siwc-A")]);
        let mut record = codex_holding(&[full_outer("B"), siwc("siwc-B")]);
        record.codex_persist_intent = super::super::codex_refresh::CodexPersistIntent::OuterLogin;
        let (saved, _) = guarded(&store, &record, Ok(SIWC_ONLY));
        assert!(matches!(saved.unwrap(), GuardedSave::Written(_)));
        let on_disk = read_json(&dp.join("f"));
        assert_eq!(on_disk["refresh_token"], "refresh-B");
        assert_eq!(on_disk["siwc"], json!({"refresh_token": "siwc-A"}));
    }

    #[test]
    fn a_withheld_save_over_an_unreadable_file_fails_closed() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        write(&dp.join("f"), "{");
        let record = codex_holding(&[full_outer("B")]);
        let (saved, _) = guarded(&FileStore::new(dp.as_path()), &record, Ok(OUTER_ONLY));
        assert!(saved.is_err());
        assert_eq!(fs::read_to_string(dp.join("f")).unwrap(), "{");
    }

    #[test]
    fn the_default_save_guarded_saves_or_refuses_a_withheld_set() {
        struct Counting(AtomicUsize);
        impl AuthStore for Counting {
            fn list(&self) -> io::Result<Vec<Auth>> {
                Ok(Vec::new())
            }
            fn save(&self, _auth: &Auth) -> io::Result<String> {
                self.0.fetch_add(1, AtomicOrdering::SeqCst);
                Ok("saved".into())
            }
            fn delete(&self, _id: &str) -> io::Result<()> {
                Ok(())
            }
        }
        let store = Counting(AtomicUsize::new(0));
        let auth = lease_record("f", "t");
        let saved = store
            .save_guarded(&auth, &mut |origin| {
                assert!(origin.is_none());
                Ok(WithheldPartitions::default())
            })
            .unwrap();
        assert!(matches!(saved, GuardedSave::Written(path) if path == "saved"));
        assert_eq!(store.0.load(AtomicOrdering::SeqCst), 1);

        let saved = store.save_guarded(&auth, &mut |_| Ok(OUTER_ONLY)).unwrap();
        assert!(matches!(
            saved,
            GuardedSave::Deferred(SaveDeferral::Unsupported)
        ));
        let saved = store
            .save_guarded(&auth, &mut |_| Err(SaveDeferral::Stale))
            .unwrap();
        assert!(matches!(saved, GuardedSave::Deferred(SaveDeferral::Stale)));
        assert_eq!(store.0.load(AtomicOrdering::SeqCst), 1, "no further save");
    }

    /// A-12: the merge, a rotation and a delete, then the save, as
    /// `credentials::save` makes them.
    #[test]
    fn a_login_never_writes_the_siwc_it_copied_from_an_earlier_read() {
        let siwc = |token: &str| json!({"siwc": {"refresh_token": token}, "use_siwc": true});
        let login = |dir: &Path| {
            let store = FileStore::new(dir);
            let mut record = codex_holding(&[full_outer("N")]);
            store.merge_existing(&mut record);
            (store, record)
        };

        // (a) A peer rotates B to C and a delete follows: the file is
        // recreated with the login's outer tokens and no SIWC.
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        disk_with(&dp.join("f"), &[full_outer("A"), siwc("B")]);
        let (store, mut record) = login(dp.as_path());
        assert_eq!(record.metadata["siwc"], json!({"refresh_token": "B"}));
        assert_eq!(record.metadata["use_siwc"], true);
        disk_with(&dp.join("f"), &[full_outer("A"), siwc("C")]);
        fs::remove_file(dp.join("f")).unwrap();
        let path = store.save_new_auth(&mut record).unwrap();
        assert_eq!(path, path_string(&dp.join("f")));
        let on_disk = read_json(&dp.join("f"));
        assert_eq!(on_disk["refresh_token"], "refresh-N");
        assert!(on_disk.get("siwc").is_none());
        assert!(on_disk.get("use_siwc").is_none());

        // (b) With no delete the file keeps C and its switch.
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        disk_with(&dp.join("f"), &[full_outer("A"), siwc("B")]);
        let (store, mut record) = login(dp.as_path());
        disk_with(&dp.join("f"), &[full_outer("A"), siwc("C")]);
        store.save_new_auth(&mut record).unwrap();
        let on_disk = read_json(&dp.join("f"));
        assert_eq!(on_disk["refresh_token"], "refresh-N");
        assert_eq!(on_disk["siwc"], json!({"refresh_token": "C"}));
        assert_eq!(on_disk["use_siwc"], true);

        // (c) A record that copied SIWC from a file since rebound adds none.
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        disk_with(&dp.join("f"), &[full_outer("A")]);
        let store = FileStore::new(dp.as_path());
        let mut record = codex_holding(&[full_outer("N"), siwc("B")]);
        store.save_new_auth(&mut record).unwrap();
        let on_disk = read_json(&dp.join("f"));
        assert_eq!(on_disk["refresh_token"], "refresh-N");
        assert!(on_disk.get("siwc").is_none());
        assert!(on_disk.get("use_siwc").is_none());
    }

    // A Codex sign-in over a credential file that holds nothing usable.

    /// The names beside `dir`'s `f.json` that keep an invalid copy of it.
    fn invalid_copies(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.contains(".invalid-"))
            .collect();
        names.sort();
        names
    }

    /// Whether `name` is `f.json.invalid-<YYYYMMDDTHHMMSSZ>` with `-n` or not.
    fn is_copy_name(name: &str) -> bool {
        let Some(rest) = name.strip_prefix("f.json.invalid-") else {
            return false;
        };
        let (stamp, extra) = rest.split_once('-').unwrap_or((rest, "2"));
        stamp.len() == 16
            && stamp.ends_with('Z')
            && stamp.as_bytes()[8] == b'T'
            && stamp[..8].bytes().all(|b| b.is_ascii_digit())
            && extra.bytes().all(|b| b.is_ascii_digit())
    }

    fn sign_in(store: &FileStore, token: &str) -> io::Result<String> {
        store.save_new_auth(&mut lease_record("f.json", token))
    }

    #[test]
    fn a_sign_in_keeps_a_copy_of_a_file_that_is_not_a_json_object() {
        let over = vec![b'x'; MAX_AUTH_FILE_SIZE as usize + 1];
        let cases: [(&str, &[u8]); 5] = [
            ("invalid JSON", b"{not json"),
            ("null", b"null"),
            ("array", b"[]"),
            (
                "blanks", b"  
",
            ),
            ("over the cap", &over),
        ];
        for (what, old) in cases {
            let d = mutation_scratch();
            let dp = clean_dir(&d);
            let store = FileStore::new(dp.as_path());
            fs::write(dp.join("f.json"), old).unwrap();
            sign_in(&store, "new-token").unwrap();

            let on_disk = read_json(&dp.join("f.json"));
            assert_eq!(on_disk["access_token"], "new-token", "{what}");
            assert!(on_disk.get("siwc").is_none(), "{what}");
            assert!(on_disk.get("use_siwc").is_none(), "{what}");
            let copies = invalid_copies(&dp);
            assert_eq!(copies.len(), 1, "{what}: {copies:?}");
            assert!(is_copy_name(&copies[0]), "{what}: {}", copies[0]);
            assert!(fs::read(dp.join(&copies[0])).unwrap() == old, "{what}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(dp.join(&copies[0]))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{what}");
            }
            // The store and its listing never load the copy as a credential.
            let listed = store.list().unwrap();
            assert_eq!(listed.len(), 1, "{what}");
            assert_eq!(listed[0].file_name, "f.json", "{what}");
        }
    }

    #[test]
    fn copies_made_with_the_same_stamp_get_distinct_names() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let path = dp.join("f.json");
        let stamp = "20260101T000000Z";
        for old in ["first", "second", "third"] {
            keep_invalid_copy_stamped(&path, Some(old.as_bytes()), stamp).unwrap();
        }
        let expected = [
            ("f.json.invalid-20260101T000000Z", "first"),
            ("f.json.invalid-20260101T000000Z-2", "second"),
            ("f.json.invalid-20260101T000000Z-3", "third"),
        ];
        let names: Vec<&str> = expected.iter().map(|(name, _)| *name).collect();
        assert_eq!(invalid_copies(&dp), names);
        for (name, old) in expected {
            assert_eq!(fs::read(dp.join(name)).unwrap(), old.as_bytes(), "{name}");
        }
    }

    #[test]
    fn a_second_sign_in_keeps_its_own_copy_beside_the_first() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        for old in ["first bad", "second bad"] {
            fs::write(dp.join("f.json"), old).unwrap();
            sign_in(&store, "new-token").unwrap();
        }
        let copies = invalid_copies(&dp);
        assert_eq!(copies.len(), 2, "{copies:?}");
        assert!(copies.iter().all(|name| is_copy_name(name)), "{copies:?}");
        let mut kept: Vec<Vec<u8>> = copies
            .iter()
            .map(|name| fs::read(dp.join(name)).unwrap())
            .collect();
        kept.sort();
        assert_eq!(kept, [b"first bad".to_vec(), b"second bad".to_vec()]);
    }

    #[test]
    fn a_copy_that_cannot_be_made_says_so_and_names_no_path() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        // The directory is missing, so creating the copy fails with NotFound.
        let path = dp.join("missing-dir").join("secret-name.json");
        let error =
            keep_invalid_copy_stamped(&path, Some(b"{not json"), "20260101T000000Z").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let text = format!("{error} {error:?}");
        assert!(
            text.contains("cannot keep a copy of the existing credential file"),
            "{text}"
        );
        assert!(
            !text.contains("cannot read the existing credential file"),
            "{text}"
        );
        for secret in ["secret-name", "missing-dir", dp.to_str().unwrap()] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_sign_in_refuses_to_copy_a_file_that_is_not_a_regular_file() {
        use std::os::unix::fs::symlink;
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        symlink("/dev/zero", dp.join("f.json")).unwrap();

        let error = sign_in(&store, "new-token").unwrap_err();
        let text = format!("{error} {error:?}");
        assert!(text.contains("is not a regular file"), "{text}");
        for secret in ["f.json", dp.to_str().unwrap(), "new-token"] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(invalid_copies(&dp).is_empty());
        assert_eq!(
            fs::read_link(dp.join("f.json")).unwrap(),
            Path::new("/dev/zero")
        );
    }

    #[test]
    fn a_sign_in_replaces_an_empty_file_without_a_copy() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        fs::write(dp.join("f.json"), b"").unwrap();
        sign_in(&store, "new-token").unwrap();
        assert_eq!(read_json(&dp.join("f.json"))["access_token"], "new-token");
        assert!(invalid_copies(&dp).is_empty());
    }

    /// Holds the file at `path` so that it can't be read, as long as the
    /// returned value lives. `None` where this platform can't.
    fn unreadable(path: &Path) -> Option<Box<dyn std::any::Any>> {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let held = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(path)
                .unwrap();
            Some(Box::new(held))
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // A directory can't be read as a file, root or not.
            fs::remove_file(path).unwrap();
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            Some(Box::new(()))
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = path;
            None
        }
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn a_sign_in_refuses_a_file_it_cannot_read_and_says_nothing_of_the_path() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        let old = br#"{"type":"codex","access_token":"old"}"#;
        fs::write(dp.join("f.json"), old).unwrap();
        let held = unreadable(&dp.join("f.json")).unwrap();

        let error = sign_in(&store, "new-token").unwrap_err();
        let text = format!("{error} {error:?}");
        assert!(
            text.contains("cannot read the existing credential file"),
            "{text}"
        );
        for secret in ["f.json", dp.to_str().unwrap(), "new-token"] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(invalid_copies(&dp).is_empty());
        drop(held);
        #[cfg(windows)]
        assert_eq!(fs::read(dp.join("f.json")).unwrap(), old);
        #[cfg(unix)]
        assert!(dp.join("f.json").is_dir());
    }

    #[test]
    fn a_runtime_codex_save_still_refuses_an_invalid_file() {
        let d = mutation_scratch();
        let dp = clean_dir(&d);
        let store = FileStore::new(dp.as_path());
        for old in [&b"{not json"[..], b"null", b"[]", b""] {
            fs::write(dp.join("f.json"), old).unwrap();
            let error = store
                .save(&lease_record("f.json", "new-token"))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(
                error
                    .to_string()
                    .contains("cannot read existing credential")
            );
            assert_eq!(fs::read(dp.join("f.json")).unwrap(), old);
            assert!(invalid_copies(&dp).is_empty());
        }
    }
}
