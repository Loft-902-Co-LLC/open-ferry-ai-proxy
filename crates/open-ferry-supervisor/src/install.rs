//! Not upstream's: carrying out an install (v3 3.9): replacing the installed
//! file by a verified copy from `versions/`, through the hardened
//! `ReplaceOnDisk`, with the update lock held by the caller. The decision
//! (which of the four hash cases) is `open_ferry_update::recovery`'s, or the
//! confirm step's; this only does the file work.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use open_ferry_update::data_dir::DataDir;
use open_ferry_update::journal::Artifact;
use open_ferry_update::staging::hash_file;
use open_ferry_update::switch::{KeepPolicy, PreparedSwitch, ReplaceOnDisk, SwitchPlan};

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallResult {
    /// The installed file is now the target.
    Replaced,
    /// Something else replaced the installed file: nothing was changed. The
    /// state record says `changed-elsewhere`.
    ChangedElsewhere,
    /// It failed; the installed file is as it was. Retry later.
    Failed(String),
}

/// Everything an install needs besides what to install.
#[derive(Debug, Clone)]
pub struct InstallPaths {
    pub data: DataDir,
    pub installed: PathBuf,
    pub binary_name: String,
}

impl InstallPaths {
    pub fn kept(&self, artifact: &Artifact) -> PathBuf {
        self.data.binary(&artifact.version, &self.binary_name)
    }
}

/// Puts `target`'s kept copy in place of the installed file. `expected` is the
/// hash the installed file had when it was classified (`None`: it was
/// missing). `from` names the version being replaced, for messages.
pub fn place(
    paths: &InstallPaths,
    target: &Artifact,
    expected: Option<&str>,
    from: &str,
    preferred_aside: Option<&Path>,
) -> InstallResult {
    place_with_prepared(paths, target, expected, from, preferred_aside, |_| Ok(()))
}

/// As `place`, but journal the actual prepared aside before the first rename.
/// The caller holds the update lock across this entire operation.
pub(crate) fn place_with_prepared(
    paths: &InstallPaths,
    target: &Artifact,
    expected: Option<&str>,
    from: &str,
    preferred_aside: Option<&Path>,
    journal_prepared: impl FnOnce(Option<&Path>) -> Result<(), String>,
) -> InstallResult {
    place_with_prepared_sync(
        paths,
        target,
        expected,
        from,
        preferred_aside,
        journal_prepared,
        &sync_directory,
    )
}

/// As `place_with_prepared`, with the directory sync given (a test forces it
/// to fail).
pub(crate) fn place_with_prepared_sync(
    paths: &InstallPaths,
    target: &Artifact,
    expected: Option<&str>,
    from: &str,
    preferred_aside: Option<&Path>,
    journal_prepared: impl FnOnce(Option<&Path>) -> Result<(), String>,
    sync: &dyn Fn(&Path) -> std::io::Result<()>,
) -> InstallResult {
    let source = paths.kept(target);
    // The trust walk has checked this path. From here the one handle opened
    // below is what is hashed, copied and read for its mode, so a file swapped
    // in meanwhile (a pipe, a link) is never followed or waited on.
    #[cfg(test)]
    source_seam::run(&source);
    let mut input = match open_source(&source) {
        Ok(input) => input,
        Err(error) => {
            return InstallResult::Failed(format!("could not read {}: {error}", source.display()));
        }
    };
    match hash_open(&mut input) {
        Ok(hash) if hash.eq_ignore_ascii_case(&target.sha256) => {}
        Ok(_) => {
            return InstallResult::Failed(format!("{} failed its hash check", source.display()));
        }
        Err(error) => {
            return InstallResult::Failed(format!("could not read {}: {error}", source.display()));
        }
    }
    if let Err(error) = input.seek(SeekFrom::Start(0)) {
        return InstallResult::Failed(format!("could not read {}: {error}", source.display()));
    }
    if std::fs::symlink_metadata(&paths.installed)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        if let Err(why) = journal_prepared(None) {
            return InstallResult::Failed(why);
        }
        if let Err(why) = before_replace(paths.data.root()) {
            return InstallResult::Failed(why);
        }
        return place_missing(&source, &mut input, &paths.installed, &target.sha256, sync);
    }
    let plan = SwitchPlan {
        from: from.to_owned(),
        to: target.version.clone(),
        new_binary: source,
        installed: paths.installed.clone(),
        keep_installed_at: None,
        keep_root: None,
        keep_policy: KeepPolicy::Immutable,
        preferred_aside: preferred_aside.map(Path::to_owned),
        expected_installed_sha256: expected.map(str::to_owned),
    };
    let switch = ReplaceOnDisk;
    let prepared: PreparedSwitch = match switch.prepare_from_open(&plan, &mut input) {
        Ok(prepared) => prepared,
        Err(error) if error.is_changed_elsewhere() => return InstallResult::ChangedElsewhere,
        Err(error) => return InstallResult::Failed(error.to_string()),
    };
    // The final mode comes from the kept file, not from whatever `prepare`
    // left: its owner bits, nothing for setuid, setgid, sticky or group/other
    // write. (`prepare` creates the copy private, 0o700, and sets its own mode
    // after the bytes are written.) The mode is read from the handle the bytes
    // came from, and set after the copy's last sync, so the copy is synced
    // again before it is journalled, hashed and renamed into place.
    #[cfg(unix)]
    {
        let secured = input
            .metadata()
            .and_then(|source| set_final_mode(&prepared.new, &source))
            .and_then(|()| sync_file(&prepared.new));
        if let Err(error) = secured {
            prepared.discard();
            return InstallResult::Failed(error.to_string());
        }
    }
    if let Err(why) = journal_prepared(prepared.aside.as_deref()) {
        prepared.discard();
        return InstallResult::Failed(why);
    }
    // B-L3: the copy is what gets renamed into place, so it is hashed here,
    // just before the renames; a bad copy is never installed.
    match hash_file(&prepared.new) {
        Ok(hash) if hash.eq_ignore_ascii_case(&target.sha256) => {}
        Ok(_) => {
            prepared.discard();
            return InstallResult::Failed(format!(
                "the copy of {} failed its hash check",
                paths.kept(target).display()
            ));
        }
        Err(error) => {
            prepared.discard();
            return InstallResult::Failed(format!(
                "could not read the copy of {}: {error}",
                paths.kept(target).display()
            ));
        }
    }
    if let Err(why) = before_replace(paths.data.root()) {
        prepared.discard();
        return InstallResult::Failed(why);
    }
    // Test builds only: the barrier between the two renames (Windows).
    #[cfg(feature = "blue-green-test")]
    let committed = if prepared.aside.is_some() {
        open_ferry_update::switch::commit_with(
            &BetweenRenames {
                root: paths.data.root(),
                new: &prepared.new,
            },
            &prepared,
        )
    } else {
        switch.commit(&prepared)
    };
    #[cfg(not(feature = "blue-green-test"))]
    let committed = switch.commit(&prepared);
    match committed {
        Ok(_) => {
            // The updater's `commit` only logs a failed directory sync. A
            // supervised install is acknowledged after the entry is durable:
            // a failed sync is a failed install, `install_pending` stays, and
            // the retry (which finds the target in place) syncs again
            // (`Disk::sync_installed_directory`) before it answers `Replaced`.
            // The prepared files are not discarded: the rename has landed.
            if let Some(parent) = paths.installed.parent()
                && let Err(error) = sync(parent)
            {
                return InstallResult::Failed(format!(
                    "the installed file was replaced, but its directory sync failed: {error}"
                ));
            }
            InstallResult::Replaced
        }
        Err(error) => {
            prepared.discard();
            if error.is_changed_elsewhere() {
                InstallResult::ChangedElsewhere
            } else {
                InstallResult::Failed(error.to_string())
            }
        }
    }
}

/// Not upstream's, test builds only: the named seam `install.before_replace`
/// (see `open_ferry_update::test_seams`), reached on every OS just before the
/// installed file would be replaced or created, with nothing changed yet.
/// `install.before_replace.fail` fails the install there (and writes the
/// `.failed` receipt); `.hold`, `.arrived`, `.release` make it a barrier. Unlike
/// `install.between_renames` it does not depend on a kept aside, so Unix
/// reaches it too. Release builds have no seam.
#[cfg(feature = "blue-green-test")]
fn before_replace(root: &Path) -> Result<(), String> {
    open_ferry_update::test_seams::hit(root, "install.before_replace")
        .map_err(|error| format!("install.before_replace: {error}"))
}

#[cfg(not(feature = "blue-green-test"))]
fn before_replace(_root: &Path) -> Result<(), String> {
    Ok(())
}

/// Not upstream's, test builds only: the named barrier
/// `install.between_renames` (see `open_ferry_update::test_seams`). With the
/// installed file already moved aside and the new copy not yet in place, a test
/// can hold here (`install.between_renames.hold`, then `.arrived`, `.release`)
/// to stop or crash the process at that exact point, or fail the second rename
/// (`install.between_renames.fail`) to see the old file put back.
#[cfg(feature = "blue-green-test")]
struct BetweenRenames<'a> {
    root: &'a Path,
    new: &'a Path,
}

#[cfg(feature = "blue-green-test")]
impl open_ferry_update::switch::FileOps for BetweenRenames<'_> {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if from == self.new {
            open_ferry_update::test_seams::hit(self.root, "install.between_renames")?;
        }
        open_ferry_update::switch::RealFileOps.rename(from, to)
    }

    fn exists(&self, path: &Path) -> bool {
        open_ferry_update::switch::RealFileOps.exists(path)
    }
}

/// The installed file is missing: copy the kept file beside it, check the
/// copy, and link it into place. A link never replaces what is there, so a
/// file that appears meanwhile is left alone (changed elsewhere). Where links
/// are not possible the copy is renamed after a check that nothing is there;
/// the update lock is held, which is what makes that check enough.
fn place_missing(
    source: &Path,
    input: &mut std::fs::File,
    installed: &Path,
    sha256: &str,
    sync: &dyn Fn(&Path) -> std::io::Result<()>,
) -> InstallResult {
    place_missing_with(
        source,
        input,
        installed,
        sha256,
        sync,
        &open_ferry_update::switch::publish_file_no_replace,
    )
}

/// [`place_missing`] with the publication given (a test watches it, or makes
/// it fail). The copy goes in with `publish`, the updater's no-replace move:
/// on Windows `MoveFileExW` with `MOVEFILE_WRITE_THROUGH`, so the entry is on
/// disk when it returns (the directory sync below is a no-op there); on Unix
/// `renameat2` with `RENAME_NOREPLACE`, or a link or a checked rename where
/// that is missing, followed by the directory sync.
///
/// That last-resort Unix fallback (no native no-replace rename and no hard
/// links) checks, then renames: only a writer that skips the update lock (a
/// trusted principal, 2.5) can race it.
fn place_missing_with(
    source: &Path,
    input: &mut std::fs::File,
    installed: &Path,
    sha256: &str,
    sync: &dyn Fn(&Path) -> std::io::Result<()>,
    publish: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> InstallResult {
    let temp = {
        let mut name = installed.as_os_str().to_owned();
        name.push(format!(".placing.{}", std::process::id()));
        PathBuf::from(name)
    };
    let _ = std::fs::remove_file(&temp);
    let placed = (|| -> std::io::Result<InstallResult> {
        copy_private(input, &temp)?;
        if !hash_file(&temp)?.eq_ignore_ascii_case(sha256) {
            return Ok(InstallResult::Failed(format!(
                "the copy of {} failed its hash check",
                source.display()
            )));
        }
        match publish(&temp, installed) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(InstallResult::ChangedElsewhere);
            }
            Err(error) => return Err(error),
        }
        // B-N2: the directory entry is made durable before the install is
        // reported done. The file is in place either way, but a failed sync is
        // a failed install: the pending install stays, and its retry, which
        // finds the target already installed, syncs the directory again
        // (`Disk::sync_installed_directory`) before it answers `Replaced`.
        if let Some(parent) = installed.parent()
            && let Err(error) = sync(parent)
        {
            return Ok(InstallResult::Failed(format!(
                "the installed file was placed, but its directory sync failed: {error}"
            )));
        }
        Ok(InstallResult::Replaced)
    })();
    // Only the file made here.
    let _ = std::fs::remove_file(&temp);
    placed.unwrap_or_else(|error| InstallResult::Failed(error.to_string()))
}

/// Copies the open `input` into the new file `temp` (never opening one that
/// exists), created private (0o700 on Unix) so the bytes are never group or
/// world accessible while they are written; then, on Unix, the final mode is
/// the source's owner bits and read/execute for the rest (`& 0o755`: no
/// setuid, setgid, sticky or group/other write). The copy is written to disk.
fn copy_private(input: &mut std::fs::File, temp: &Path) -> std::io::Result<()> {
    let mut output = create_private(temp)?;
    let copied = std::io::copy(input, &mut output).and_then(|_| {
        #[cfg(unix)]
        set_final_mode(temp, &input.metadata()?)?;
        output.sync_all()
    });
    if copied.is_err() {
        drop(output);
        let _ = std::fs::remove_file(temp);
    }
    copied
}

/// Creates the new file `temp`, private (0o700 on Unix) from its creation.
fn create_private(temp: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700);
    }
    options.open(temp)
}

/// Opens the kept source once for reading, without following a link or
/// waiting on a pipe, and refuses anything but a regular file. Unix:
/// `O_NOFOLLOW | O_NONBLOCK`, then the handle's own type. Windows: a reparse
/// point is opened as itself and refused.
fn open_source(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    };
    #[cfg(not(windows))]
    let linked = false;
    if linked || !metadata.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

/// The SHA-256 of what is left to read of `file`, as lower-case hex.
fn hash_open(file: &mut std::fs::File) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut hash = Sha256::new();
    let mut bytes = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(bytes.get(..n).unwrap_or_default());
    }
    let digest: [u8; 32] = hash.finalize().into();
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Writes `path`'s bytes and metadata to disk: the replacing path, after its
/// final mode is set.
#[cfg(unix)]
fn sync_file(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    sync_failures::check_file(path)?;
    std::fs::File::open(path)?.sync_all()
}

/// The final mode of a copy: the source's `& 0o755`.
#[cfg(unix)]
fn set_final_mode(copy: &Path, source: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        copy,
        std::fs::Permissions::from_mode(source.permissions().mode() & 0o755),
    )
}

/// Makes a directory's entries durable. On Windows it does nothing: a
/// directory can't be opened to sync, and the moves that put an entry there
/// (`publish_file_no_replace`, the updater's commit) are written through
/// (`MOVEFILE_WRITE_THROUGH`) instead.
#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    sync_failures::check(path)?;
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    sync_failures::check(_path)?;
    Ok(())
}

/// Tests only: directories whose sync fails, so a test can fail the real
/// sync on any platform. Keyed by directory, so parallel tests (each with its
/// own scratch directory) do not meet.
#[cfg(test)]
pub(crate) mod sync_failures {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static FAILING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(crate) fn fail(dir: &Path) {
        FAILING.lock().unwrap().push(dir.to_owned());
    }

    pub(crate) fn allow(dir: &Path) {
        FAILING.lock().unwrap().retain(|failing| failing != dir);
    }

    #[cfg(unix)]
    static FAILING_FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Fails the file sync (the replacing path's, after its final mode) of
    /// any file made directly in `dir`.
    #[cfg(unix)]
    pub(crate) fn fail_files_in(dir: &Path) {
        FAILING_FILES.lock().unwrap().push(dir.to_owned());
    }

    #[cfg(unix)]
    pub(crate) fn allow_files_in(dir: &Path) {
        FAILING_FILES
            .lock()
            .unwrap()
            .retain(|failing| failing != dir);
    }

    #[cfg(unix)]
    pub(super) fn check_file(file: &Path) -> std::io::Result<()> {
        let dir = file.parent();
        if FAILING_FILES
            .lock()
            .unwrap()
            .iter()
            .any(|failing| Some(failing.as_path()) == dir)
        {
            return Err(std::io::Error::other("the file sync is forced to fail"));
        }
        Ok(())
    }

    pub(super) fn check(dir: &Path) -> std::io::Result<()> {
        if FAILING.lock().unwrap().iter().any(|failing| failing == dir) {
            return Err(std::io::Error::other(
                "the directory sync is forced to fail",
            ));
        }
        Ok(())
    }
}

/// Tests only: a hook that runs once the trust check has passed and before the
/// kept source is opened, so a test can swap the file (for a pipe or a link)
/// at the one moment a path-based install would be fooled.
#[cfg(test)]
pub(crate) mod source_seam {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    type Hook = Box<dyn FnOnce() + Send>;
    static HOOKS: Mutex<Vec<(PathBuf, Hook)>> = Mutex::new(Vec::new());

    /// Runs `hook` once, the next time `source` is about to be opened.
    #[cfg(unix)]
    pub(crate) fn once(source: &Path, hook: impl FnOnce() + Send + 'static) {
        HOOKS
            .lock()
            .unwrap()
            .push((source.to_owned(), Box::new(hook)));
    }

    pub(super) fn run(source: &Path) {
        let hook = {
            let mut hooks = HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|(path, _)| path == source)
                .map(|at| hooks.remove(at).1)
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A directory of its own for one test. The name carries a counter: the
    /// process id and a timestamp alone repeat when the clock is coarser than
    /// two parallel tests starting together (Windows), and then two tests share
    /// a directory (the `install::tests` flake: one saw the other's files).
    fn scratch() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ofp-s3-install-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// The copy is private from the moment it exists, before any byte is
    /// written and before the final chmod. The test runs in a child of the
    /// test binary with the umask set to 0, so a creation without
    /// `mode(0o700)` gets 0o666 (a file is never created executable) and
    /// fails; under an ordinary umask it would give 0o644 and fail too, but
    /// the umask is process-wide, hence the child.
    #[cfg(unix)]
    #[test]
    fn the_copy_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let Some(done) = crate::testkit::isolated::child_dir() else {
            crate::testkit::isolated::run_in_child(
                module_path!(),
                "the_copy_is_created_private",
                std::time::Duration::from_secs(60),
            );
            return;
        };
        // SAFETY: this child runs only this test, on its own thread.
        unsafe { libc::umask(0) };
        let root = scratch();
        let temp = root.join("placing");
        let file = create_private(&temp).unwrap();
        assert_eq!(
            file.metadata().unwrap().permissions().mode() & 0o7777,
            0o700
        );
        drop(file);
        // `copy_private` goes through it: a source with wide modes still ends
        // at its owner bits and read/execute for the rest.
        let source = root.join("source");
        std::fs::write(&source, b"binary").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o4777)).unwrap();
        let _ = std::fs::remove_file(&temp);
        copy_private(&mut open_source(&source).unwrap(), &temp).unwrap();
        assert_eq!(
            std::fs::metadata(&temp).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        std::fs::remove_dir_all(root).unwrap();
        crate::testkit::isolated::mark_completed(&done);
    }

    #[test]
    fn placing_a_missing_file_syncs_its_directory() {
        let root = scratch();
        let source = root.join("source");
        std::fs::write(&source, b"binary").unwrap();
        let sha = hash_file(&source).unwrap();
        let installed = root.join("installed");
        let synced = RefCell::new(Vec::new());
        let result = place_missing(
            &source,
            &mut open_source(&source).unwrap(),
            &installed,
            &sha,
            &|dir| {
                synced.borrow_mut().push(dir.to_owned());
                Ok(())
            },
        );
        assert_eq!(result, InstallResult::Replaced);
        assert_eq!(*synced.borrow(), vec![root.clone()]);
        // A failed sync is a failed install, though the file stays placed: the
        // retry finds it and syncs again.
        std::fs::remove_file(&installed).unwrap();
        let result = place_missing(
            &source,
            &mut open_source(&source).unwrap(),
            &installed,
            &sha,
            &|_| Err(std::io::Error::other("no sync")),
        );
        assert!(
            matches!(&result, InstallResult::Failed(why) if why.contains("directory sync failed")),
            "{result:?}"
        );
        assert_eq!(std::fs::read(&installed).unwrap(), b"binary");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Item 7: a missing installed file goes in through the updater's no-replace
    /// publication (on Windows its written-through move), and the directory
    /// sync comes after it.
    #[test]
    fn a_missing_file_is_placed_by_the_no_replace_publication_then_synced() {
        let root = scratch();
        let source = root.join("source");
        std::fs::write(&source, b"binary").unwrap();
        let sha = hash_file(&source).unwrap();
        let installed = root.join("installed");
        let events = RefCell::new(Vec::new());
        let result = place_missing_with(
            &source,
            &mut open_source(&source).unwrap(),
            &installed,
            &sha,
            &|_| {
                events.borrow_mut().push("sync".to_owned());
                Ok(())
            },
            &|from, to| {
                assert!(from.exists(), "published from the copy");
                assert!(!to.exists());
                assert_eq!(to, installed);
                events.borrow_mut().push("publish".to_owned());
                open_ferry_update::switch::publish_file_no_replace(from, to)
            },
        );
        assert_eq!(result, InstallResult::Replaced);
        assert_eq!(*events.borrow(), ["publish", "sync"]);
        assert_eq!(std::fs::read(&installed).unwrap(), b"binary");
        assert_eq!(names(&root), ["installed", "source"], "no copy left");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Item 7: a publication that fails fails the install before the
    /// directory sync, and leaves no installed file and no copy.
    #[test]
    fn a_failed_publication_fails_before_anything_is_acknowledged() {
        let root = scratch();
        let source = root.join("source");
        std::fs::write(&source, b"binary").unwrap();
        let sha = hash_file(&source).unwrap();
        let installed = root.join("installed");
        let synced = RefCell::new(0);
        let result = place_missing_with(
            &source,
            &mut open_source(&source).unwrap(),
            &installed,
            &sha,
            &|_| {
                *synced.borrow_mut() += 1;
                Ok(())
            },
            &|_, _| Err(std::io::Error::other("the move was refused")),
        );
        assert!(
            matches!(&result, InstallResult::Failed(why) if why.contains("the move was refused")),
            "{result:?}"
        );
        assert_eq!(*synced.borrow(), 0, "nothing was synced or acknowledged");
        assert_eq!(names(&root), ["source"], "no installed file, no copy");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Item 7: a file that appears meanwhile is left alone.
    #[test]
    fn a_file_that_appeared_meanwhile_is_changed_elsewhere_and_left() {
        let root = scratch();
        let source = root.join("source");
        std::fs::write(&source, b"binary").unwrap();
        let sha = hash_file(&source).unwrap();
        let installed = root.join("installed");
        let result = place_missing_with(
            &source,
            &mut open_source(&source).unwrap(),
            &installed,
            &sha,
            &|_| panic!("nothing placed, nothing synced"),
            &|from, to| {
                std::fs::write(to, b"theirs").unwrap();
                open_ferry_update::switch::publish_file_no_replace(from, to)
            },
        );
        assert_eq!(result, InstallResult::ChangedElsewhere);
        assert_eq!(std::fs::read(&installed).unwrap(), b"theirs");
        assert_eq!(names(&root), ["installed", "source"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Parallel tests must never share a scratch directory: many threads ask
    /// for one at the same instant, round after round.
    #[test]
    fn scratch_directories_are_distinct_even_when_asked_for_together() {
        let threads = 16;
        let gate = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let gate = gate.clone();
                std::thread::spawn(move || {
                    (0..50)
                        .map(|_| {
                            gate.wait();
                            scratch()
                        })
                        .collect::<Vec<PathBuf>>()
                })
            })
            .collect();
        let roots: Vec<PathBuf> = handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect();
        let unique: std::collections::HashSet<&PathBuf> = roots.iter().collect();
        assert_eq!(unique.len(), roots.len(), "two scratch() calls collided");
        for root in roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Item 8, `place_missing`: whatever mode the source has, the published
    /// file is its owner bits and read/execute for the rest, nothing setuid,
    /// setgid or writable by others.
    #[cfg(unix)]
    #[test]
    fn a_missing_file_is_published_without_setuid_or_write_bits() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let source = root.join("source");
        std::fs::write(&source, b"binary").unwrap();
        let sha = hash_file(&source).unwrap();
        for (given, published) in [(0o6777, 0o755), (0o700, 0o700), (0o775, 0o755)] {
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(given)).unwrap();
            let installed = root.join(format!("installed-{given:o}"));
            let seen = RefCell::new(None);
            let result = place_missing_with(
                &source,
                &mut open_source(&source).unwrap(),
                &installed,
                &sha,
                &|_| Ok(()),
                &|from, to| {
                    // The mode is final before the publication, not fixed up after.
                    *seen.borrow_mut() =
                        Some(from.metadata().unwrap().permissions().mode() & 0o7777);
                    open_ferry_update::switch::publish_file_no_replace(from, to)
                },
            );
            assert_eq!(result, InstallResult::Replaced);
            let mode = |path: &Path| path.metadata().unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode(&installed), published, "from {given:o}");
            assert_eq!(*seen.borrow(), Some(published), "from {given:o}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Item 8, replacing an installed file: the published mode comes from the
    /// kept source, not from what `prepare` leaves.
    #[cfg(unix)]
    #[test]
    fn a_replacing_install_publishes_the_kept_files_mode_without_setuid_or_write_bits() {
        use std::os::unix::fs::PermissionsExt;
        for (given, published) in [(0o6777, 0o755), (0o700, 0o700)] {
            let root = scratch();
            let data = DataDir::at(&root);
            let installed = root.join("installed");
            std::fs::write(&installed, b"old binary").unwrap();
            let old_sha = hash_file(&installed).unwrap();
            let kept = data.binary("2.0.0", "open-ferry");
            std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
            std::fs::write(&kept, b"new binary").unwrap();
            std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(given)).unwrap();
            let target = Artifact {
                version: "2.0.0".to_owned(),
                sha256: hash_file(&kept).unwrap(),
                size: 10,
            };
            let paths = InstallPaths {
                data,
                installed: installed.clone(),
                binary_name: "open-ferry".to_owned(),
            };
            let result = place(&paths, &target, Some(&old_sha), "1.0.0", None);
            assert_eq!(result, InstallResult::Replaced);
            assert_eq!(
                installed.metadata().unwrap().permissions().mode() & 0o7777,
                published,
                "from {given:o}"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// A kept target, an installed old binary and a conflicting `.old` that a
    /// journal named as the aside, in one scratch root (also the data root).
    #[cfg(all(windows, feature = "blue-green-test"))]
    fn between_renames_fixture(root: &Path) -> (InstallPaths, Artifact, String, PathBuf) {
        let data = DataDir::at(root);
        let installed = root.join("open-ferry.exe");
        std::fs::write(&installed, b"old binary").unwrap();
        let kept = data.binary("2.0.0", "open-ferry.exe");
        std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
        std::fs::write(&kept, b"new binary").unwrap();
        let old_sha = hash_file(&installed).unwrap();
        let target = Artifact {
            version: "2.0.0".to_owned(),
            sha256: hash_file(&kept).unwrap(),
            size: 10,
        };
        // The aside the journal recorded earlier, still in use: it must stay.
        let preferred = root.join("open-ferry.exe.old");
        std::fs::write(&preferred, b"journal aside").unwrap();
        let paths = InstallPaths {
            data,
            installed,
            binary_name: "open-ferry.exe".to_owned(),
        };
        (paths, target, old_sha, preferred)
    }

    /// Item 15: the barrier `install.between_renames` is held with the old
    /// file moved aside and the new one not yet in place, over a conflicting
    /// aside. The prepared callback has already recorded the actual aside
    /// (not the conflicting one), and starting over from that exact state, as a
    /// restart after a crash does, completes the install.
    #[cfg(all(windows, feature = "blue-green-test"))]
    #[test]
    fn a_hold_between_the_renames_leaves_a_recorded_aside_and_a_state_that_recovers() {
        let root = scratch();
        let (paths, target, old_sha, preferred) = between_renames_fixture(&root);
        let seams = root.join(".blue-green-test");
        std::fs::create_dir_all(&seams).unwrap();
        std::fs::write(seams.join("install.between_renames.hold"), []).unwrap();
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None::<PathBuf>));
        let worker = {
            let (paths, target, old_sha, preferred) = (
                paths.clone(),
                target.clone(),
                old_sha.clone(),
                preferred.clone(),
            );
            let recorded = recorded.clone();
            std::thread::spawn(move || {
                place_with_prepared(
                    &paths,
                    &target,
                    Some(&old_sha),
                    "1.0.0",
                    Some(&preferred),
                    |aside| {
                        *recorded.lock().unwrap() = aside.map(Path::to_owned);
                        Ok(())
                    },
                )
            })
        };
        for _ in 0..1000 {
            if seams.join("install.between_renames.arrived").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(seams.join("install.between_renames.arrived").exists());
        // Held: the old file is aside, the new one is not in place.
        let aside = recorded
            .lock()
            .unwrap()
            .clone()
            .expect("the aside was recorded");
        assert_ne!(aside, preferred, "the conflicting aside was not reused");
        assert!(
            !paths.installed.exists(),
            "the installed file is moved aside"
        );
        assert_eq!(std::fs::read(&aside).unwrap(), b"old binary");
        assert_eq!(std::fs::read(&preferred).unwrap(), b"journal aside");

        // A restart from this state: another root with exactly these files.
        let after = scratch();
        let (paths2, target2, old_sha2, _) = between_renames_fixture(&after);
        std::fs::remove_file(&paths2.installed).unwrap();
        let aside2 = after.join(aside.file_name().unwrap());
        std::fs::write(&aside2, b"old binary").unwrap();
        let again = place_with_prepared(
            &paths2,
            &target2,
            Some(&old_sha2),
            "1.0.0",
            Some(&aside2),
            |_| Ok(()),
        );
        assert_eq!(again, InstallResult::Replaced);
        assert_eq!(std::fs::read(&paths2.installed).unwrap(), b"new binary");

        // Released, the held install completes as well.
        std::fs::write(seams.join("install.between_renames.release"), []).unwrap();
        assert_eq!(worker.join().unwrap(), InstallResult::Replaced);
        assert_eq!(std::fs::read(&paths.installed).unwrap(), b"new binary");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(after).unwrap();
    }

    /// Item 15: the second rename fails at the barrier: the old file is put
    /// back from the recorded aside and the install is reported failed.
    #[cfg(all(windows, feature = "blue-green-test"))]
    #[test]
    fn a_failure_between_the_renames_puts_the_old_file_back() {
        let root = scratch();
        let (paths, target, old_sha, preferred) = between_renames_fixture(&root);
        let seams = root.join(".blue-green-test");
        std::fs::create_dir_all(&seams).unwrap();
        std::fs::write(seams.join("install.between_renames.fail"), []).unwrap();
        let mut recorded = None;
        let result = place_with_prepared(
            &paths,
            &target,
            Some(&old_sha),
            "1.0.0",
            Some(&preferred),
            |aside| {
                recorded = aside.map(Path::to_owned);
                Ok(())
            },
        );
        assert!(matches!(result, InstallResult::Failed(_)), "{result:?}");
        assert!(seams.join("install.between_renames.failed").exists());
        assert_eq!(std::fs::read(&paths.installed).unwrap(), b"old binary");
        let aside = recorded.expect("the aside was recorded");
        assert_ne!(aside, preferred);
        assert!(!aside.exists(), "the aside was renamed back");
        assert_eq!(std::fs::read(&preferred).unwrap(), b"journal aside");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The seam `install.before_replace` is reached on every OS: failing it
    /// fails the install with the installed file and the kept file untouched.
    #[cfg(feature = "blue-green-test")]
    #[test]
    fn a_failure_before_the_replace_leaves_the_installed_file_alone() {
        let root = scratch();
        let data = DataDir::at(&root);
        let installed = root.join("open-ferry-bin");
        std::fs::write(&installed, b"old binary").unwrap();
        let kept = data.binary("2.0.0", "open-ferry-bin");
        std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
        std::fs::write(&kept, b"new binary").unwrap();
        let old_sha = hash_file(&installed).unwrap();
        let target = Artifact {
            version: "2.0.0".to_owned(),
            sha256: hash_file(&kept).unwrap(),
            size: 10,
        };
        let paths = InstallPaths {
            data,
            installed,
            binary_name: "open-ferry-bin".to_owned(),
        };
        let seams = root.join(".blue-green-test");
        std::fs::create_dir_all(&seams).unwrap();
        std::fs::write(seams.join("install.before_replace.fail"), []).unwrap();
        let result =
            place_with_prepared(&paths, &target, Some(&old_sha), "1.0.0", None, |_| Ok(()));
        assert!(matches!(result, InstallResult::Failed(_)), "{result:?}");
        assert!(seams.join("install.before_replace.failed").exists());
        assert_eq!(std::fs::read(&paths.installed).unwrap(), b"old binary");
        assert_eq!(std::fs::read(&kept).unwrap(), b"new binary");
        // Without the fault the same install goes through.
        std::fs::remove_file(seams.join("install.before_replace.fail")).unwrap();
        let again = place_with_prepared(&paths, &target, Some(&old_sha), "1.0.0", None, |_| Ok(()));
        assert!(!matches!(again, InstallResult::Failed(_)), "{again:?}");
        assert_eq!(std::fs::read(&paths.installed).unwrap(), b"new binary");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Replacing an existing installed binary: a directory sync that fails is a
    /// failed install (the caller keeps the install pending) though the new
    /// file is in place; the retry finds the target installed and, once the
    /// sync works, completes (`disk.rs`'s `AlreadyReplaced` path).
    #[tokio::test]
    async fn replacing_an_existing_file_needs_its_directory_sync() {
        use open_ferry_update::journal::{InstallPending, Installed};
        let rig = crate::testkit::Rig::new().await;
        let mut journal = rig.disk.on_disk().unwrap();
        let root = scratch();
        let data = DataDir::at(&root);
        let installed = root.join("installed");
        std::fs::write(&installed, b"old binary").unwrap();
        let old_sha = hash_file(&installed).unwrap();
        let kept = data.binary("2.0.0", "open-ferry");
        std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
        std::fs::write(&kept, b"new binary").unwrap();
        let target = Artifact {
            version: "2.0.0".to_owned(),
            sha256: hash_file(&kept).unwrap(),
            size: 10,
        };
        let paths = InstallPaths {
            data: data.clone(),
            installed: installed.clone(),
            binary_name: "open-ferry".to_owned(),
        };
        let synced = RefCell::new(Vec::new());
        let result = place_with_prepared_sync(
            &paths,
            &target,
            Some(&old_sha),
            "1.0.0",
            None,
            |_| Ok(()),
            &|dir| {
                synced.borrow_mut().push(dir.to_owned());
                Err(std::io::Error::other("no sync"))
            },
        );
        assert!(
            matches!(&result, InstallResult::Failed(why) if why.contains("directory sync failed")),
            "{result:?}"
        );
        assert_eq!(*synced.borrow(), vec![root.clone()]);
        assert_eq!(std::fs::read(&installed).unwrap(), b"new binary");

        // The retry: the target is in place, and the sync that works completes.
        journal.installed = Some(Installed {
            version: "1.0.0".to_owned(),
            sha256: old_sha,
        });
        journal.install_pending = Some(InstallPending {
            target,
            source: kept.to_string_lossy().into_owned(),
            aside: String::new(),
            attempts: 0,
            next_try: None,
            started: "2026-10-09T00:00:00Z".to_owned(),
            record_id: None,
        });
        let disk = crate::disk::RealDisk::new(
            data,
            installed,
            "open-ferry".to_owned(),
            open_ferry_update::journal::JournalStore::new(&root),
        );
        let (_, result) = crate::disk::Disk::install_transaction(&disk, journal).await;
        assert_eq!(result, InstallResult::Replaced);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_copy_that_fails_its_hash_just_before_the_renames_is_not_installed() {
        let root = scratch();
        let data = DataDir::at(&root);
        let installed = root.join("installed");
        std::fs::write(&installed, b"old binary").unwrap();
        let kept = data.binary("2.0.0", "open-ferry");
        std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
        std::fs::write(&kept, b"new binary").unwrap();
        let sha = hash_file(&kept).unwrap();
        let old_sha = hash_file(&installed).unwrap();
        let target = Artifact {
            version: "2.0.0".to_owned(),
            sha256: sha,
            size: 10,
        };
        let paths = InstallPaths {
            data,
            installed: installed.clone(),
            binary_name: "open-ferry".to_owned(),
        };
        let result = place_with_prepared(&paths, &target, Some(&old_sha), "1.0.0", None, |_| {
            // After the journal write and before the renames, the copy changes.
            for entry in std::fs::read_dir(&root).unwrap().flatten() {
                if entry.file_name().to_string_lossy().contains(".new.") {
                    std::fs::write(entry.path(), b"corrupt").unwrap();
                }
            }
            Ok(())
        });
        assert!(
            matches!(&result, InstallResult::Failed(why) if why.contains("hash check")),
            "{result:?}"
        );
        assert_eq!(std::fs::read(&installed).unwrap(), b"old binary");
        std::fs::remove_dir_all(root).unwrap();
    }
}
