// Ported from CLIProxyAPI internal/api/handlers/management/oauth_siwc_mutations.go
// (sameSIWCMutationPath) (v8.0.38, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Snapshot a mutation destination before replacement. Compare it with the
//! bound path's identity at mutation time, as upstream stats both paths.
//! No handle is retained that could interfere with replacement on Windows.

use open_ferry_core::auth::file_store::{FileId, file_id};
use std::fs::{self, OpenOptions};
use std::path::{Component, Path, PathBuf};

#[derive(Clone)]
pub(crate) struct BindMutationPath {
    path: PathBuf,
    identity: Option<FileId>,
}

impl BindMutationPath {
    pub(super) fn bound(path: PathBuf, identity: FileId) -> Self {
        Self {
            path,
            identity: Some(identity),
        }
    }

    pub(super) fn current(&self) -> Self {
        Self::capture(self.path.clone())
    }

    pub(crate) fn capture(path: PathBuf) -> Self {
        let identity = fs::metadata(&path)
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|_| {
                let mut options = OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;
                    options.custom_flags(libc::O_NONBLOCK);
                }
                options
                    .open(&path)
                    .ok()
                    .and_then(|file| file_id(&file).ok())
            });
        Self {
            path: canonical(&path),
            identity,
        }
    }

    pub(super) fn matches(&self, other: &Self) -> bool {
        if self
            .identity
            .zip(other.identity)
            .is_some_and(|(left, right)| left == right)
        {
            return true;
        }
        if self.path == other.path {
            return true;
        }
        #[cfg(windows)]
        {
            if let (Some(left), Some(right)) = (self.path.to_str(), other.path.to_str()) {
                return crate::go::equal_fold(left, right);
            }
        }
        false
    }
}

fn canonical(path: &Path) -> PathBuf {
    let resolved = path
        .canonicalize()
        .or_else(|_| {
            path.parent()
                .zip(path.file_name())
                .ok_or_else(|| std::io::Error::other("missing parent"))
                .and_then(|(parent, name)| parent.canonicalize().map(|parent| parent.join(name)))
        })
        .unwrap_or_else(|_| path.to_owned());
    let absolute = std::path::absolute(&resolved).unwrap_or(resolved);
    let mut cleaned = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                cleaned.pop();
            }
            other => cleaned.push(other.as_os_str()),
        }
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_paths_match_file_identity_before_paths() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.json");
        let alias = dir.path().join("alias.json");
        fs::write(&original, b"{}").unwrap();
        fs::hard_link(&original, &alias).unwrap();
        let original = BindMutationPath::capture(original);
        let alias = BindMutationPath::capture(alias);
        assert_ne!(original.path, alias.path);
        assert!(original.matches(&alias));
    }

    #[test]
    fn mutation_paths_keep_case_distinct_when_platform_requires_it() {
        let dir = tempfile::tempdir().unwrap();
        let lower = BindMutationPath::capture(dir.path().join("missing.json"));
        let upper = BindMutationPath::capture(dir.path().join("MISSING.json"));
        assert_eq!(lower.matches(&upper), cfg!(windows));
        assert!(lower.matches(&BindMutationPath::capture(
            dir.path().join("./missing.json")
        )));
    }
}
