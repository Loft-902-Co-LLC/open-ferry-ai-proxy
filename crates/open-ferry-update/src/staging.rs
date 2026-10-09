//! Immutable version directories, published with an exclusive rename.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::data_dir::DataDir;
use crate::runner::Runner;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A first-start adoption cannot overwrite a different build of the same version.
#[derive(Debug)]
pub enum StageExclusiveError {
    /// Local disk work failed.
    Io(io::Error),
    /// A newer or unreadable journal forbids changing the version store.
    Journal(crate::journal::JournalError),
    /// The copied binary did not identify itself as a release version.
    Version(String),
    /// The version already belongs to different bytes.
    SwitchingDisabled { version: String },
}

impl std::fmt::Display for StageExclusiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Journal(error) => error.fmt(f),
            Self::Version(error) => write!(f, "the copied binary failed --version: {error}"),
            Self::SwitchingDisabled { version } => write!(
                f,
                "switching disabled: version {version} already holds another hash"
            ),
        }
    }
}
impl std::error::Error for StageExclusiveError {}
impl From<io::Error> for StageExclusiveError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The name every staging directory of this process and run starts with:
/// `.staging.<pid>.<tag>`. PID 1 in a container is the same every run, so the
/// run's tag keeps a new run's names off a crashed run's directories.
fn staging_prefix(tag: u64) -> String {
    format!(".staging.{}.{tag:016x}", std::process::id())
}

/// Whether `name` is a staging directory of this process and the run `tag`
/// (the prefix itself, or the prefix and `.<n>`).
fn is_current_staging(name: &str, tag: u64) -> bool {
    let prefix = staging_prefix(tag);
    name == prefix
        || name
            .strip_prefix(&prefix)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// An owned staging directory. Drop never removes any other call's files.
pub(crate) struct StagingDir(pub PathBuf);
impl StagingDir {
    pub(crate) fn create(versions: &Path) -> io::Result<Self> {
        Self::create_with_tag(versions, crate::journal::run_tag())
    }

    fn create_with_tag(versions: &Path, tag: u64) -> io::Result<Self> {
        // The data root holds `versions`; it is re-synced with the levels
        // below it, and nothing above it unless this call makes it.
        crate::publication::create_dir_all_durable(
            versions,
            versions.parent().unwrap_or(versions),
        )?;
        let prefix = staging_prefix(tag);
        let base = versions.join(&prefix);
        for attempt in 0..100 {
            let path = if attempt == 0 {
                base.clone()
            } else {
                versions.join(format!("{prefix}.{}", NEXT.fetch_add(1, Ordering::Relaxed)))
            };
            match fs::create_dir(&path) {
                Ok(()) => {
                    // Own it before syncing, so a failed sync still cleans up.
                    let staging = Self(path);
                    sync_directory(&staging.0)?;
                    sync_directory(versions)?;
                    return Ok(staging);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no exclusive staging directory available",
        ))
    }
}
impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Removes the `.staging.*` directories under `versions` that this run did
/// not make: a crashed run's, whatever its pid. Nothing in the state or the
/// journal names one. The caller holds the update lock (the one that guards
/// staging and pruning), so no other process is staging; this run's own
/// directories (its tag) are kept. Each removal is made durable by one sync
/// of `versions` afterwards. A missing `versions` is nothing to do.
///
/// Every directory removed is logged. A directory that can't be removed (or
/// that isn't a real directory) is not an error: one warning names its path
/// (once per path per process) and the rest go on. Only listing `versions`,
/// reading an entry, or the final sync of `versions` is returned, as an error
/// whose message names `versions`; the caller logs it and carries on. Returns
/// the paths removed.
///
/// With `blue-green-test`, a `staging.cleanup.fail` file in the data root's
/// `.blue-green-test` directory makes every removal fail (a test seam).
pub(crate) fn cleanup_stale_staging(versions: &Path) -> io::Result<Vec<PathBuf>> {
    cleanup_stale_staging_with(versions, crate::journal::run_tag())
}

/// `error` with the path it is about in its message.
fn naming(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// Warns once per path for the life of the process.
fn warn_path_once(path: &Path, what: &str) {
    static WARNED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    let Ok(mut warned) = WARNED.lock() else {
        return;
    };
    if !warned.iter().any(|seen| seen == path) {
        warned.push(path.to_path_buf());
        tracing::warn!("{}: {what}", path.display());
    }
}

fn cleanup_stale_staging_with(versions: &Path, tag: u64) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(versions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(naming(versions, error)),
    };
    let mut removed = Vec::new();
    let mut first_error = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                first_error.get_or_insert(naming(versions, error));
                continue;
            }
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(".staging.") || is_current_staging(&name, tag) {
            continue;
        }
        let path = entry.path();
        // Only a real directory: a link or a file with this name is not ours.
        if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            warn_path_once(&path, "not a staging directory, left in place");
            continue;
        }
        let attempt =
            crate::test_seams::hit(versions.parent().unwrap_or(versions), "staging.cleanup")
                .and_then(|()| fs::remove_dir_all(&path));
        match attempt {
            Ok(()) => {
                tracing::info!(
                    "removed {}, a crashed run's staging directory",
                    path.display()
                );
                removed.push(path);
            }
            Err(error) => {
                warn_path_once(
                    &path,
                    &format!("a stale staging directory could not be removed: {error}"),
                );
            }
        }
    }
    if !removed.is_empty()
        && let Err(error) = sync_directory(versions)
    {
        first_error.get_or_insert(naming(versions, error));
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(removed),
    }
}

/// Copy the installed file while hashing the copied bytes, then verify the copy's
/// version before publishing it. The caller holds the update lock.
///
/// An existing EMPTY version directory (a crashed publication's claim) is
/// replaced unless the fresh retention set holds that version; a non-empty
/// directory with other bytes is never removed.
pub async fn stage_exclusive(
    data: &DataDir,
    installed: &Path,
    binary_name: &str,
    runner: &dyn Runner,
    timeout: Duration,
) -> Result<crate::journal::Artifact, StageExclusiveError> {
    stage_exclusive_with(data, installed, binary_name, runner, timeout, &|version| {
        crate::updater::retention_keep(data, [])
            .map(|keep| keep.contains(version))
            .map_err(io::Error::other)
    })
    .await
}

/// [`stage_exclusive`], except that an existing EMPTY version directory (the
/// claim a crashed directory publication can leave) is removed and replaced,
/// provided `keep_version` says the version is outside every retention set.
///
/// `keep_version` is called only when such a claim is found, with the version
/// that was just copied, and must compute the keep set fresh under the caller's
/// lock (journal, live pins, newest other version, the state's own retention).
/// Its error, like a `true`, leaves the directory alone. A non-empty directory
/// that differs is never removed.
pub(crate) async fn stage_exclusive_with(
    data: &DataDir,
    installed: &Path,
    binary_name: &str,
    runner: &dyn Runner,
    timeout: Duration,
    keep_version: &dyn Fn(&str) -> io::Result<bool>,
) -> Result<crate::journal::Artifact, StageExclusiveError> {
    crate::journal::JournalView::load(&data.root().join(crate::journal::JOURNAL_FILE))
        .and_then(|view| view.ensure_supported())
        .map_err(StageExclusiveError::Journal)?;
    // The retention preflight (a malformed pin, say) gates only the cleanup of
    // stale staging directories, never staging itself: on failure it warns
    // once and skips the cleanup. A newer journal was refused above.
    // The caller holds the update lock. A crashed run's staging directories
    // are cleaned first; a failure here is logged and doesn't stop staging.
    match crate::updater::retention_keep(data, []) {
        Ok(_) => {
            if let Err(error) = cleanup_stale_staging(&data.versions()) {
                tracing::warn!(%error, "could not clean up a crashed run's staging directories");
            }
        }
        Err(error) => {
            warn_path_once(
                &data.versions(),
                &format!("skipping stale staging cleanup: the retention check failed: {error}"),
            );
        }
    }
    let mut components = Path::new(binary_name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "binary name must be a file name",
        )
        .into());
    }
    let staging = StagingDir::create(&data.versions())?;
    let path = staging.0.join(binary_name);
    let mut source = File::open(installed)?;
    let mut dest = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let bytes = buffer
            .get(..read)
            .ok_or_else(|| io::Error::other("invalid read length"))?;
        dest.write_all(bytes)?;
        hash.update(bytes);
        size += read as u64;
    }
    dest.set_permissions(source.metadata()?.permissions())?;
    dest.sync_all()?;
    drop(dest);
    sync_directory(&staging.0)?;
    let printed = runner
        .version(&path, timeout)
        .await
        .map_err(StageExclusiveError::Version)?;
    let version = printed
        .strip_prefix("open-ferry ")
        .filter(|v| semver::Version::parse(v).is_ok())
        .ok_or_else(|| {
            StageExclusiveError::Version(format!("unexpected version output {printed:?}"))
        })?
        .to_owned();
    let sha256 = hex_digest(hash);
    let target = data.version_dir(&version);
    if target.exists() {
        if hash_file(&target.join(binary_name)).ok().as_deref() != Some(sha256.as_str()) {
            // Only an empty claim that nothing retains may go. `remove_dir`
            // refuses a directory that has gained any entry since the check.
            let removable = is_empty_dir(&target)? && !keep_version(&version)?;
            if !removable || fs::remove_dir(&target).is_err() {
                return Err(StageExclusiveError::SwitchingDisabled { version });
            }
            rename_directory_exclusive(&staging.0, &target)?;
            sync_directory(&data.versions())?;
        }
    } else {
        rename_directory_exclusive(&staging.0, &target)?;
        sync_directory(&data.versions())?;
    }
    Ok(crate::journal::Artifact {
        version,
        sha256,
        size,
    })
}

fn hex_digest(hash: Sha256) -> String {
    use std::fmt::Write as _;
    let digest: [u8; 32] = hash.finalize().into();
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Hash a file without allocating its entire contents.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(
            bytes
                .get(..n)
                .ok_or_else(|| io::Error::other("invalid read length"))?,
        );
    }
    Ok(hex_digest(hash))
}

/// Make a directory's contents and entry durable. The platform work lives in
/// [`crate::publication`].
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    crate::publication::sync_directory(path)
}

/// Atomically publish a directory, never replacing even an empty destination.
pub(crate) fn rename_directory_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    crate::publication::publish_directory(from, to)
}

/// Whether `path` is a directory with no entries.
fn is_empty_dir(path: &Path) -> io::Result<bool> {
    Ok(path.is_dir() && fs::read_dir(path)?.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::BoxFuture;

    use std::sync::atomic::AtomicBool;

    const BINARY: &[u8] = b"a staged build";
    const LIMIT: Duration = Duration::from_secs(5);

    struct Prints(&'static str);
    impl Runner for Prints {
        fn version<'a>(
            &'a self,
            _binary: &'a Path,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move { Ok(format!("open-ferry {}", self.0)) })
        }
    }

    /// A data directory, an installed file, and an empty `versions/0.1.0` claim.
    fn claimed() -> (tempfile::TempDir, DataDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let data = DataDir::at(temp.path().join("data"));
        let installed = temp.path().join("open-ferry");
        fs::write(&installed, BINARY).unwrap();
        let claim = data.version_dir("0.1.0");
        fs::create_dir_all(&claim).unwrap();
        (temp, data, installed, claim)
    }

    async fn stage(
        data: &DataDir,
        installed: &Path,
        keep: &dyn Fn(&str) -> io::Result<bool>,
    ) -> Result<crate::journal::Artifact, StageExclusiveError> {
        stage_exclusive_with(data, installed, "open-ferry", &Prints("0.1.0"), LIMIT, keep).await
    }

    fn no_staging_left(data: &DataDir) {
        let left: Vec<_> = fs::read_dir(data.versions())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(".staging"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    // Not upstream's: an empty claim left by a crash is replaced when no
    // retention set holds the version.
    #[tokio::test]
    async fn empty_claim_is_replaced_when_not_kept() {
        let (_temp, data, installed, claim) = claimed();
        let asked = AtomicBool::new(false);
        let artifact = stage(&data, &installed, &|version| {
            assert_eq!(version, "0.1.0");
            asked.store(true, Ordering::SeqCst);
            Ok(false)
        })
        .await
        .unwrap();
        assert!(asked.load(Ordering::SeqCst));
        assert_eq!(artifact.version, "0.1.0");
        assert_eq!(fs::read(claim.join("open-ferry")).unwrap(), BINARY);
        assert_eq!(
            hash_file(&claim.join("open-ferry")).unwrap(),
            artifact.sha256
        );
        no_staging_left(&data);
    }

    // Not upstream's: a retained version's empty claim is left as it is.
    #[tokio::test]
    async fn empty_claim_that_is_kept_is_refused() {
        let (_temp, data, installed, claim) = claimed();
        let error = stage(&data, &installed, &|_| Ok(true)).await.unwrap_err();
        assert!(matches!(
            error,
            StageExclusiveError::SwitchingDisabled { .. }
        ));
        assert!(is_empty_dir(&claim).unwrap());
        no_staging_left(&data);
    }

    // Not upstream's: a retention set that cannot be computed fails closed.
    #[tokio::test]
    async fn empty_claim_with_unreadable_retention_is_refused() {
        let (_temp, data, installed, claim) = claimed();
        let error = stage(&data, &installed, &|_| Err(io::Error::other("no pins")))
            .await
            .unwrap_err();
        assert!(matches!(error, StageExclusiveError::Io(_)), "{error}");
        assert!(is_empty_dir(&claim).unwrap());
    }

    async fn plain(
        data: &DataDir,
        installed: &Path,
    ) -> Result<crate::journal::Artifact, StageExclusiveError> {
        stage_exclusive(data, installed, "open-ferry", &Prints("0.1.0"), LIMIT).await
    }

    // Not upstream's: the plain entry point recovers an unprotected empty claim.
    #[tokio::test]
    async fn plain_stage_exclusive_recovers_an_unprotected_empty_claim() {
        let (_temp, data, installed, claim) = claimed();
        let artifact = plain(&data, &installed).await.unwrap();
        assert_eq!(
            hash_file(&claim.join("open-ferry")).unwrap(),
            artifact.sha256
        );
        no_staging_left(&data);
    }

    // Not upstream's: the state's `previous` protects an empty claim.
    #[tokio::test]
    async fn plain_stage_exclusive_keeps_the_states_previous() {
        let (_temp, data, installed, claim) = claimed();
        let mut state = crate::state::State::load(&data.state_file());
        state.previous = Some("0.1.0".into());
        state.save(&data.state_file()).unwrap();
        let error = plain(&data, &installed).await.unwrap_err();
        assert!(matches!(
            error,
            StageExclusiveError::SwitchingDisabled { .. }
        ));
        assert!(is_empty_dir(&claim).unwrap());
    }

    // Not upstream's: malformed pins don't block a fresh publish; the stale
    // staging directory is left alone because the cleanup is skipped.
    // This is the U3-1 regression test.
    #[tokio::test]
    async fn malformed_pins_do_not_block_a_fresh_publish() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataDir::at(temp.path().join("data"));
        let installed = temp.path().join("open-ferry");
        fs::write(&installed, BINARY).unwrap();
        let pins = data.versions().join(".pins");
        fs::create_dir_all(&pins).unwrap();
        fs::write(pins.join("bad.json"), b"not-json").unwrap();
        let stale = data
            .versions()
            .join(format!(".staging.{}", std::process::id()));
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("open-ferry"), "crashed").unwrap();

        let artifact = plain(&data, &installed).await.unwrap();
        assert_eq!(artifact.version, "0.1.0");
        assert_eq!(
            hash_file(&data.version_dir("0.1.0").join("open-ferry")).unwrap(),
            artifact.sha256
        );
        assert_eq!(fs::read(stale.join("open-ferry")).unwrap(), b"crashed");
    }

    // Not upstream's: malformed pins fail the retention check closed for an
    // empty claim.
    #[tokio::test]
    async fn plain_stage_exclusive_fails_closed_on_malformed_pins() {
        let (_temp, data, installed, claim) = claimed();
        let pins = data.versions().join(".pins");
        fs::create_dir_all(&pins).unwrap();
        fs::write(pins.join("bad.json"), b"not-json").unwrap();
        assert!(plain(&data, &installed).await.is_err());
        assert!(is_empty_dir(&claim).unwrap());
    }

    // Not upstream's: a non-empty directory holding other bytes is the conflict
    // itself, and is never removed, whatever the retention says.
    #[tokio::test]
    async fn conflicting_artifact_is_never_removed() {
        let (_temp, data, installed, claim) = claimed();
        fs::write(claim.join("open-ferry"), b"another build").unwrap();
        let asked = AtomicBool::new(false);
        let error = stage(&data, &installed, &|_| {
            asked.store(true, Ordering::SeqCst);
            Ok(false)
        })
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            StageExclusiveError::SwitchingDisabled { .. }
        ));
        assert_eq!(
            fs::read(claim.join("open-ferry")).unwrap(),
            b"another build"
        );
        assert!(!asked.load(Ordering::SeqCst));
        // A directory with only some other file in it is not an empty claim.
        fs::remove_file(claim.join("open-ferry")).unwrap();
        fs::write(claim.join("note"), b"x").unwrap();
        let error = stage(&data, &installed, &|_| Ok(false)).await.unwrap_err();
        assert!(matches!(
            error,
            StageExclusiveError::SwitchingDisabled { .. }
        ));
        assert_eq!(fs::read(claim.join("note")).unwrap(), b"x");
        no_staging_left(&data);
    }

    // Not upstream's: an empty destination directory is never overwritten.
    #[test]
    fn directory_publish_is_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("staging");
        let to = temp.path().join("version");
        fs::create_dir(&from).unwrap();
        fs::create_dir(&to).unwrap();
        assert!(rename_directory_exclusive(&from, &to).is_err());
        assert!(from.is_dir());
        fs::remove_dir(&to).unwrap();
        rename_directory_exclusive(&from, &to).unwrap();
        assert!(to.is_dir());
    }

    // Not upstream's: a crashed call's staging directory is not ours to remove.
    #[test]
    fn staging_cleanup_is_owned() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join(format!(".staging.{}", std::process::id()));
        fs::create_dir(&existing).unwrap();
        let stage = StagingDir::create(temp.path()).unwrap();
        let owned = stage.0.clone();
        drop(stage);
        assert!(existing.is_dir());
        assert!(!owned.exists());
    }

    const TAG: u64 = 0xabc;
    const OTHER_TAG: u64 = 0xabcd;

    fn entry_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // Not upstream's: staging names carry the run's tag, so a later run with
    // the same pid (PID 1 in a container) doesn't meet a crashed run's names.
    #[test]
    fn staging_names_carry_the_run_tag() {
        let temp = tempfile::tempdir().unwrap();
        let stage = StagingDir::create_with_tag(temp.path(), TAG).unwrap();
        let name = stage.0.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, staging_prefix(TAG));
        assert!(is_current_staging(&name, TAG));
        assert!(is_current_staging(&format!("{name}.7"), TAG));
        // A longer tag that starts with the same digits is another run's.
        assert!(!is_current_staging(&staging_prefix(OTHER_TAG), TAG));
        // The old name, with no tag, is nobody's current directory.
        let pid = std::process::id();
        assert!(!is_current_staging(&format!(".staging.{pid}"), TAG));
        assert!(!is_current_staging(&format!(".staging.{pid}.3"), TAG));
    }

    // Not upstream's: a stale directory from another run is removed, this
    // run's is kept, and things that aren't staging directories are left.
    #[test]
    fn stale_staging_is_removed_and_this_runs_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let versions = temp.path();
        let pid = std::process::id();
        let current = StagingDir::create_with_tag(versions, TAG).unwrap();
        fs::write(current.0.join("open-ferry"), "mine").unwrap();
        let stale = [
            format!(".staging.{pid}"),
            format!(".staging.{pid}.4"),
            staging_prefix(OTHER_TAG),
            format!("{}.2", staging_prefix(OTHER_TAG)),
            ".staging.999999.0000000000000001".to_owned(),
        ];
        for name in &stale {
            fs::create_dir(versions.join(name)).unwrap();
            fs::write(versions.join(name).join("open-ferry"), "crashed").unwrap();
        }
        fs::create_dir(versions.join("0.1.0")).unwrap();
        fs::write(versions.join(".staging.note"), "not a directory").unwrap();

        let mut removed = cleanup_stale_staging_with(versions, TAG).unwrap();
        removed.sort();
        let mut want: Vec<_> = stale.iter().map(|name| versions.join(name)).collect();
        want.sort();
        assert_eq!(removed, want);
        assert_eq!(fs::read(current.0.join("open-ferry")).unwrap(), b"mine");
        let mut left = vec![
            current
                .0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "0.1.0".to_owned(),
            ".staging.note".to_owned(),
        ];
        left.sort();
        assert_eq!(entry_names(versions), left);
        // Nothing more to do the second time; a missing directory is fine.
        assert!(
            cleanup_stale_staging_with(versions, TAG)
                .unwrap()
                .is_empty()
        );
        assert!(
            cleanup_stale_staging_with(&versions.join("absent"), TAG)
                .unwrap()
                .is_empty()
        );
    }

    // Not upstream's: more than 100 stale directories with this pid, as a
    // container that always runs as PID 1 leaves, don't stop staging, and the
    // cleanup removes them all and keeps this run's.
    #[test]
    fn staging_succeeds_after_over_a_hundred_stale_directories_with_the_same_pid() {
        let temp = tempfile::tempdir().unwrap();
        let versions = temp.path();
        let pid = std::process::id();
        fs::create_dir(versions.join(format!(".staging.{pid}"))).unwrap();
        for n in 0..150 {
            let stale = versions.join(format!(".staging.{pid}.{n}"));
            fs::create_dir(&stale).unwrap();
            fs::write(stale.join("open-ferry"), "crashed").unwrap();
        }
        for n in 0..120 {
            fs::create_dir(versions.join(format!("{}.{n}", staging_prefix(OTHER_TAG)))).unwrap();
        }
        let stage = StagingDir::create_with_tag(versions, TAG).unwrap();
        assert_eq!(stage.0, versions.join(staging_prefix(TAG)));
        let removed = cleanup_stale_staging_with(versions, TAG).unwrap();
        assert_eq!(removed.len(), 1 + 150 + 120);
        assert_eq!(entry_names(versions), [staging_prefix(TAG)]);
    }

    // Not upstream's: a malformed pin skips the stale cleanup but does not stop
    // staging (the claim here still fails closed in `keep_version`), and a
    // newer journal still refuses staging. Every leftover stays where it is.
    #[tokio::test]
    async fn refused_preflight_leaves_every_stale_staging_directory() {
        let (_temp, data, installed, claim) = claimed();
        let stale = data
            .versions()
            .join(format!(".staging.{}", std::process::id()));
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("open-ferry"), "crashed").unwrap();

        let pins = data.versions().join(".pins");
        fs::create_dir_all(&pins).unwrap();
        fs::write(pins.join("bad.json"), b"not-json").unwrap();
        assert!(plain(&data, &installed).await.is_err());
        assert!(is_empty_dir(&claim).unwrap());
        assert_eq!(fs::read(stale.join("open-ferry")).unwrap(), b"crashed");
        fs::remove_dir_all(&pins).unwrap();

        fs::write(
            data.root().join(crate::journal::JOURNAL_FILE),
            br#"{"format":2,"seq":1}"#,
        )
        .unwrap();
        let error = plain(&data, &installed).await.unwrap_err();
        assert!(error.to_string().contains("newer"), "{error}");
        assert_eq!(fs::read(stale.join("open-ferry")).unwrap(), b"crashed");
    }

    // Not upstream's: a stale directory that can't be removed is a warning,
    // not an error: the cleanup returns Ok, staging still succeeds, and the
    // directory goes once removal works again.
    #[cfg(feature = "blue-green-test")]
    #[tokio::test]
    async fn a_failed_stale_removal_does_not_stop_staging() {
        let (_temp, data, installed, claim) = claimed();
        let stale = data
            .versions()
            .join(format!(".staging.{}", std::process::id()));
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("open-ferry"), "crashed").unwrap();
        let seams = data.root().join(".blue-green-test");
        fs::create_dir_all(&seams).unwrap();
        let fail = seams.join("staging.cleanup.fail");
        fs::write(&fail, b"").unwrap();

        assert!(cleanup_stale_staging(&data.versions()).unwrap().is_empty());
        assert!(stale.is_dir());
        let artifact = stage(&data, &installed, &|_| Ok(false)).await.unwrap();
        assert_eq!(artifact.version, "0.1.0");
        assert_eq!(fs::read(claim.join("open-ferry")).unwrap(), BINARY);
        assert_eq!(fs::read(stale.join("open-ferry")).unwrap(), b"crashed");

        fs::remove_file(&fail).unwrap();
        assert_eq!(
            cleanup_stale_staging(&data.versions()).unwrap().as_slice(),
            std::slice::from_ref(&stale)
        );
        assert!(!stale.exists());
    }

    // Not upstream's: staging itself cleans a crashed run's directories first.
    #[tokio::test]
    async fn stage_exclusive_removes_a_crashed_runs_staging_directory() {
        let (_temp, data, installed, claim) = claimed();
        let stale = data
            .versions()
            .join(format!(".staging.{}", std::process::id()));
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("open-ferry"), "crashed").unwrap();
        let artifact = stage(&data, &installed, &|_| Ok(false)).await.unwrap();
        assert_eq!(artifact.version, "0.1.0");
        assert!(!stale.exists());
        assert_eq!(fs::read(claim.join("open-ferry")).unwrap(), BINARY);
        no_staging_left(&data);
    }
}
