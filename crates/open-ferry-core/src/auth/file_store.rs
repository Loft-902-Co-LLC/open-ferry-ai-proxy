// Ported from CLIProxyAPI sdk/auth/filestore.go, and the existing-file merge
// in Manager.Login in sdk/auth/manager.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! [`FileStore`]: credentials kept as JSON files in an auth directory.
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
use std::sync::{Mutex, PoisonError, RwLock};

use serde_json::Value;

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
use super::{Auth, AuthStore, Status};

/// The largest credential file read.
pub const MAX_AUTH_FILE_SIZE: u64 = 8 << 20;

/// Credentials kept as JSON files under an auth directory (upstream's
/// `FileTokenStore`).
#[derive(Debug, Default)]
pub struct FileStore {
    base_dir: RwLock<PathBuf>,
    save_lock: Mutex<()>,
}

impl FileStore {
    /// A store over `base_dir`. An empty directory leaves the store
    /// unconfigured: it can then only save to absolute paths.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        let store = Self::default();
        store.set_base_dir(base_dir);
        store
    }

    /// Changes the auth directory. Surrounding whitespace is ignored.
    pub fn set_base_dir(&self, base_dir: impl AsRef<Path>) {
        let dir = trim_path(base_dir.as_ref());
        *self
            .base_dir
            .write()
            .unwrap_or_else(PoisonError::into_inner) = dir;
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
        self.save_with(auth, false)
    }

    /// Saves a credential from a login or migration: as
    /// [`save_auth`](Self::save_auth), but creates the file even for a
    /// disabled credential.
    pub fn save_new_auth(&self, auth: &mut Auth) -> io::Result<String> {
        self.save_with(auth, true)
    }

    /// Before a login saves `auth` over an existing file, copies that
    /// file's settings into it: every key `auth` lacks except tokens, and
    /// the file's `disabled` flag unless `auth` sets its own.
    pub fn merge_existing(&self, auth: &mut Auth) {
        let dir = self.base_dir();
        if dir.as_os_str().is_empty() {
            return;
        }
        let target = if auth.file_name.is_empty() {
            &auth.id
        } else {
            &auth.file_name
        };
        if target.is_empty() {
            return;
        }
        let full = join(&dir, Path::new(target));
        let Ok(raw) = read_capped(&full) else {
            return;
        };
        if raw.is_empty() {
            return;
        }
        if let Ok(Some(existing)) = unmarshal_object(&raw)
            && !existing.is_empty()
        {
            merge_existing_auth_metadata(auth, &existing);
        }
    }

    fn save_with(&self, auth: &mut Auth, creation_intent: bool) -> io::Result<String> {
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
        let Some(path_str) = path.to_str().map(str::to_owned) else {
            return Err(invalid("auth filestore: path is not valid UTF-8"));
        };

        // A runtime save must not bring back a disabled credential whose
        // file was deliberately removed.
        if auth.disabled
            && !creation_intent
            && fs::metadata(&path).is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
        {
            return Ok(String::new());
        }

        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        if let Some(parent) = path.parent() {
            create_dir_all(parent)
                .map_err(|err| wrap(&err, "auth filestore: create dir failed"))?;
        }

        if auth.metadata.is_empty() {
            return Err(invalid(format!(
                "auth filestore: nothing to persist for {}",
                auth.id
            )));
        }
        auth.metadata
            .insert("disabled".to_owned(), Value::Bool(auth.disabled));
        let raw = marshal_map(&auth.metadata);
        match read_capped(&path) {
            Ok(existing) => {
                if !json_equal(&existing, raw.as_bytes()) {
                    write_atomic(&path, raw.as_bytes())
                        .map_err(|err| wrap(&err, "auth filestore: write existing failed"))?;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                write_atomic(&path, raw.as_bytes())
                    .map_err(|err| wrap(&err, "auth filestore: write file failed"))?;
            }
            Err(err) if err.kind() == io::ErrorKind::FileTooLarge => {
                write_atomic(&path, raw.as_bytes())
                    .map_err(|err| wrap(&err, "auth filestore: write existing failed"))?;
            }
            Err(err) => return Err(wrap(&err, "auth filestore: read existing failed")),
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
        Ok(path_str)
    }

    fn resolve_auth_path(&self, auth: &Auth) -> io::Result<PathBuf> {
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
        match fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                Err(wrap(&err, "auth filestore: delete failed"))
            }
            _ => Ok(()),
        }
    }
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

    fn delete(&self, id: &str) -> io::Result<()> {
        self.delete_auth(id)
    }
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

/// Reads one credential file into a record, or `None` for a file to skip
/// quietly: empty, `null`, or of type `gemini`.
fn read_auth_file(path: &Path, base_dir: &Path) -> io::Result<Option<Auth>> {
    let Some(path_str) = path.to_str() else {
        return Err(invalid("path is not valid UTF-8"));
    };
    let data = read_capped(path).map_err(|err| wrap(&err, "read file"))?;
    if data.is_empty() {
        return Ok(None);
    }
    let mut metadata = unmarshal_object(&data)
        .map_err(|err| invalid(format!("unmarshal auth json: {err}")))?
        .unwrap_or_default();
    normalize_credential_metadata(&mut metadata);
    validate_weights(&Default::default(), &metadata).map_err(|err| invalid(err.to_string()))?;

    let provider = metadata
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if equal_fold(&provider, "gemini") {
        return Ok(None);
    }
    let provider = if provider.is_empty() {
        "unknown".to_owned()
    } else {
        provider
    };
    let info = fs::metadata(path).map_err(|err| wrap(&err, "stat file"))?;
    let modified = info
        .modified()
        .ok()
        .map(chrono::DateTime::<chrono::Utc>::from);

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
    Ok(Some(auth))
}

/// A credential file's ID: its path relative to the auth directory, else
/// the path itself; lowercased on Windows, whose paths ignore case.
pub(crate) fn id_for(path: &Path, path_str: &str, base_dir: &Path) -> String {
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

/// Replaces the file at `path` with `data` through a temporary file in the
/// same directory, so readers see the old file or the new one, never a
/// partial write.
fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
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
fn create_dir_all(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_all(dir: &Path) -> io::Result<()> {
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

fn wrap(err: &io::Error, context: &str) -> io::Error {
    io::Error::new(err.kind(), format!("{context}: {err}"))
}

fn at(err: &io::Error, op: &str, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{op} {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
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
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a.json"]);
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
}
