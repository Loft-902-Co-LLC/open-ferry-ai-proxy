//! Switching the installed binary to another version.
//!
//! [`ReplaceOnDisk`] is the one way: it keeps a copy of the installed
//! binary for a rollback, copies the new one beside the installed one, and
//! renames it into place, so the path never holds half a binary. On Unix
//! the rename replaces the old file, which a running process keeps open.
//! Windows won't replace a running `.exe`, but will rename it: the old one
//! is renamed to `open-ferry.exe.old` first (or `.old.<n>` while an older
//! one is still in use), and renamed back if the new one can't be put in
//! its place. Leftover `.old` files are removed at the next switch.
//!
//! The switch is two steps, so a caller that keeps a journal can write the
//! name of the aside before anything is renamed: [`ReplaceOnDisk::prepare`]
//! copies the new binary beside the installed one and chooses the aside,
//! and [`ReplaceOnDisk::commit`] does the renames. [`Switch::switch`] does
//! both.
//!
//! The renames on Windows are `MoveFileExW` with `MOVEFILE_WRITE_THROUGH`
//! and without `MOVEFILE_REPLACE_EXISTING` (both destinations are absent,
//! so a surprise file fails the call instead of being overwritten), each
//! retried a few times for the errors antivirus programs cause. Nothing
//! deletes the only copy: if the installed file is missing and an `.old`
//! exists (power was lost between the two renames), the newest `.old` is
//! renamed back first, and the cleanup does nothing while the installed
//! file is missing and never touches the aside a journal names. A
//! [`FileOps`] seam lets the tests fail a rename.
//!
//! A switch doesn't restart anything: the running server goes on with the
//! old binary until it is restarted.
//!
//! Not upstream's: nothing here is a port.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Not upstream's: the no-replace, written-through file publication, for the
/// supervisor's placing of a missing installed file.
pub use crate::publication::publish_file_no_replace;

/// How many times a rename is retried after a transient error.
pub const RENAME_TRIES: usize = 5;

/// The wait before each retry of a rename, in milliseconds.
pub const RENAME_WAITS_MS: [u64; RENAME_TRIES] = [50, 100, 200, 400, 800];

/// How many `.old.<n>` names are tried for the aside.
const MAX_ASIDE_NAMES: usize = 100;

/// A switch to make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchPlan {
    /// The version installed now.
    pub from: String,
    /// The version to switch to.
    pub to: String,
    /// The binary to switch to.
    pub new_binary: PathBuf,
    /// The installed binary, to replace.
    pub installed: PathBuf,
    /// Where to keep a copy of the installed binary first, if anywhere.
    /// A name that is free is taken with a no-replace link. One that is
    /// taken is never overwritten: if it holds the installed binary's bytes
    /// it is reused, and if it holds anything else the switch fails.
    pub keep_installed_at: Option<PathBuf>,
    /// Not upstream's: the directory `keep_installed_at` must be inside and
    /// whose chain of directories is made and synced again, never above it
    /// (the updater's data root). `None`: the kept copy's own directory.
    pub keep_root: Option<PathBuf>,
    /// What to do when `keep_installed_at` holds other bytes.
    pub keep_policy: KeepPolicy,
    /// The aside a journal recorded, if any: restored first when the
    /// installed file is missing, and never removed by the cleanup. `None`
    /// keeps the choice of the newest `.old`.
    pub preferred_aside: Option<PathBuf>,
    /// The hash of the installed binary the caller classified (`old`). The
    /// switch fails with [`SwitchError::is_changed_elsewhere`] if the
    /// installed file is anything else, at prepare and again at commit.
    /// `None` takes the hash of what is installed when `prepare` starts.
    pub expected_installed_sha256: Option<String>,
}

/// What a switch does when the kept copy exists and holds other bytes than
/// the installed binary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeepPolicy {
    /// Fail the switch; the kept copy is never touched. For a caller with a
    /// journal, which may be running from it.
    #[default]
    Immutable,
    /// Replace it atomically (a temporary file, a replacing rename and a
    /// directory sync). For a caller with no journal: nothing runs from
    /// `versions/` without a supervisor.
    ReplaceMismatched,
}

/// What a switch leaves to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchOutcome {
    /// The binary is replaced; open-ferry runs it once restarted.
    RestartNeeded,
}

/// Why a switch failed; the installed binary is as it was, except where
/// the message says otherwise.
#[derive(Debug)]
pub struct SwitchError {
    /// What was being done.
    pub step: &'static str,
    /// What went wrong.
    pub error: io::Error,
}

impl fmt::Display for SwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.step, self.error)
    }
}

impl std::error::Error for SwitchError {}

/// The error inside a [`SwitchError`] that says another writer replaced the
/// installed binary after it was classified.
#[derive(Debug)]
struct ChangedElsewhere(String);

impl fmt::Display for ChangedElsewhere {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChangedElsewhere {}

fn changed_elsewhere(text: String) -> io::Error {
    io::Error::other(ChangedElsewhere(text))
}

impl SwitchError {
    /// Whether the installed binary was not the one that was classified:
    /// something outside the update replaced it, so the switch was refused
    /// (or undone) and what is installed was left as that writer made it.
    /// The caller records `installed_binary = changed-elsewhere`.
    pub fn is_changed_elsewhere(&self) -> bool {
        self.error
            .get_ref()
            .is_some_and(|inner| inner.is::<ChangedElsewhere>())
    }
}

/// Puts a version in place of the installed binary.
pub trait Switch: Send + Sync {
    /// Makes the switch `plan` describes.
    fn switch(&self, plan: &SwitchPlan) -> Result<SwitchOutcome, SwitchError>;
}

/// The file operations a replacement makes, behind a trait so tests can
/// fail one.
pub trait FileOps: Send + Sync {
    /// Renames `from` to `to`, which must not exist (on Windows; Unix
    /// replaces it).
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Whether `path` exists (a dangling link does).
    fn exists(&self, path: &Path) -> bool;
    /// Hard-links `from` at `to`, which must not exist (never replaces).
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::hard_link(from, to)
    }
    /// Syncs the directory holding `path` after publishing or replacing it.
    fn sync_parent(&self, path: &Path) -> io::Result<()> {
        crate::publication::sync_parent(path)
    }
    /// Waits `wait` before a retry.
    fn sleep(&self, wait: Duration) {
        std::thread::sleep(wait);
    }
}

/// The real file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealFileOps;

impl FileOps for RealFileOps {
    #[cfg(windows)]
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

        let wide =
            |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
        let (from, to) = (wide(from), wide(to));
        // SAFETY: both are NUL-terminated UTF-16 strings that outlive the call.
        let moved = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
        if moved == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn exists(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok()
    }
}

/// Whether a rename that failed with `error` is worth trying again. Only on
/// Windows: access denied, a sharing violation or a lock violation, which
/// an antivirus scan of the file causes for a moment. On Unix a permission
/// error is real, and retrying it would only cost the full wait.
pub fn is_transient(error: &io::Error) -> bool {
    /// `ERROR_SHARING_VIOLATION` and `ERROR_LOCK_VIOLATION`.
    const SHARING_AND_LOCK: [i32; 2] = [32, 33];
    cfg!(windows)
        && (error.kind() == io::ErrorKind::PermissionDenied
            || error
                .raw_os_error()
                .is_some_and(|code| SHARING_AND_LOCK.contains(&code)))
}

/// Renames `from` to `to`, again after each of [`RENAME_WAITS_MS`] while
/// the error is transient.
fn rename_retrying(ops: &dyn FileOps, from: &Path, to: &Path) -> io::Result<()> {
    let mut retries = 0;
    loop {
        match ops.rename(from, to) {
            Err(error) if retries < RENAME_TRIES && is_transient(&error) => {
                ops.sleep(Duration::from_millis(RENAME_WAITS_MS[retries]));
                retries += 1;
            }
            result => return result,
        }
    }
}

/// A switch whose new binary is beside the installed one and whose aside is
/// chosen, with nothing renamed yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedSwitch {
    /// The installed binary, to replace.
    pub installed: PathBuf,
    /// The new binary, beside the installed one.
    pub new: PathBuf,
    /// Where the installed binary is moved to first (Windows); a caller
    /// with a journal records it before [`ReplaceOnDisk::commit`].
    pub aside: Option<PathBuf>,
    /// The hash the installed binary had when it was classified; the commit
    /// refuses (or undoes) the switch if the file it replaces is another.
    /// `None`: the installed binary was missing, so nothing is checked.
    pub expected_installed_sha256: Option<String>,
    /// Not upstream's: whether the commit needs hard links to check the file it
    /// replaces (Unix). `true` (`Immutable`, a caller with a journal) fails
    /// closed where links aren't possible; `false` (`ReplaceMismatched`, no
    /// journal) relies on the hash taken just before the rename, as before
    /// links were used.
    pub require_links: bool,
}

impl PreparedSwitch {
    /// Removes the new binary beside the installed one, for a switch that
    /// won't be committed.
    pub fn discard(&self) {
        let _ = fs::remove_file(&self.new);
    }
}

/// Replaces the binary on disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReplaceOnDisk;

impl ReplaceOnDisk {
    /// Keeps a copy of the installed binary if the plan says, copies the new
    /// binary beside it and, on Windows, chooses the aside. Nothing is
    /// renamed.
    pub fn prepare(&self, plan: &SwitchPlan) -> Result<PreparedSwitch, SwitchError> {
        prepare_with(&RealFileOps, plan, cfg!(windows))
    }

    /// Not upstream's: as [`prepare`](Self::prepare), but the new binary is
    /// copied from `source`, a file the caller opened and checked itself,
    /// read from its start; `plan.new_binary` is not opened. For a caller that
    /// must not reopen the path (it may have been swapped for a link or a pipe).
    pub fn prepare_from_open(
        &self,
        plan: &SwitchPlan,
        source: &mut fs::File,
    ) -> Result<PreparedSwitch, SwitchError> {
        // Whatever the caller read first (a hash, a size check) moved the
        // offset: the copy is of the whole file.
        io::Seek::seek(source, io::SeekFrom::Start(0)).map_err(step("rewind the new binary"))?;
        prepare_with_source(&RealFileOps, plan, cfg!(windows), Some(source))
    }

    /// Puts the prepared binary in place of the installed one.
    pub fn commit(&self, prepared: &PreparedSwitch) -> Result<SwitchOutcome, SwitchError> {
        commit_with(&RealFileOps, prepared)
    }
}

fn step(step: &'static str) -> impl FnOnce(io::Error) -> SwitchError {
    move |error| SwitchError { step, error }
}

/// Copies `from` to `to` through a temporary file, so `to` is whole or
/// absent, and fails with `AlreadyExists`, leaving what is there, if `to`
/// exists or appears first. The caller holds the update lock, including on
/// file systems that need the locked publication fallback. The directory
/// is synced once the copy is published, before any installed-file rename.
#[cfg(test)]
fn copy_whole_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    copy_whole_exclusive_with(&RealFileOps, from, to, None, to.parent().unwrap())
}

/// [`copy_whole_exclusive`] over `ops`. With `expected`, the copy is hashed
/// before it is published and dropped (a changed-elsewhere error) if it
/// isn't those bytes: the installed file changed while it was copied.
fn copy_whole_exclusive_with(
    ops: &dyn FileOps,
    from: &Path,
    to: &Path,
    expected: Option<&str>,
    root: &Path,
) -> io::Result<()> {
    let temp = copy_to_temp(from, to, root)?;
    let placed = verify_copy(&temp, expected)
        .and_then(|()| crate::publication::publish_file(&temp, to))
        .and_then(|()| ops.sync_parent(to));
    // Only the temporary file this call made.
    let _ = fs::remove_file(&temp);
    placed
}

/// Like [`copy_whole_exclusive_with`], but replaces what is at `to`,
/// atomically: a temporary file, a replacing rename and a directory sync.
fn copy_replacing_with(
    ops: &dyn FileOps,
    from: &Path,
    to: &Path,
    expected: Option<&str>,
    root: &Path,
) -> io::Result<()> {
    let temp = copy_to_temp(from, to, root)?;
    let placed = verify_copy(&temp, expected)
        .and_then(|()| crate::publication::replace_file(&temp, to))
        .and_then(|()| ops.sync_parent(to));
    let _ = fs::remove_file(&temp);
    placed
}

/// Fails with a changed-elsewhere error if `copy` isn't the `expected` bytes.
fn verify_copy(copy: &Path, expected: Option<&str>) -> io::Result<()> {
    let Some(want) = expected else {
        return Ok(());
    };
    let got = crate::staging::hash_file(copy)?;
    if got.eq_ignore_ascii_case(want) {
        Ok(())
    } else {
        Err(changed_elsewhere(
            "the installed binary changed while it was being kept".into(),
        ))
    }
}

/// Copies `from` beside `to` into a temporary file this call creates (it
/// never opens one that exists, so a crashed run's partial file is left
/// alone), keeping `from`'s permissions capped at 0755 on Unix, written to disk.
fn copy_to_temp(from: &Path, to: &Path, root: &Path) -> io::Result<PathBuf> {
    let dir = to.parent().unwrap_or_else(|| Path::new("."));
    crate::publication::create_dir_all_durable(dir, root)?;
    copy_beside(from, to, &format!(".partial.{}", std::process::id()), false)
}

/// Creates `<to><suffix>` (or `<to><suffix>.<n>`: the first that doesn't
/// exist) and copies `from` into it: contents, then permissions
/// (`executable` for 0o755 on Unix, else `from`'s capped at 0755), then `sync_all`. On an
/// error only the file made here is removed.
fn copy_beside(from: &Path, to: &Path, suffix: &str, executable: bool) -> io::Result<PathBuf> {
    let mut source = fs::File::open(from)?;
    copy_beside_open(&mut source, to, suffix, executable)
}

/// [`copy_beside`] from a file that is already open.
fn copy_beside_open(
    source: &mut fs::File,
    to: &Path,
    suffix: &str,
    executable: bool,
) -> io::Result<PathBuf> {
    let (mut file, temp) = create_unique(to, suffix)?;
    let copied = io::copy(source, &mut file)
        .and_then(|_| {
            let permissions = if executable {
                executable_permissions(source.metadata()?.permissions())
            } else {
                source.metadata()?.permissions()
            };
            #[cfg(unix)]
            let permissions = {
                use std::os::unix::fs::PermissionsExt;
                fs::Permissions::from_mode(permissions.mode() & 0o755)
            };
            file.set_permissions(permissions)
        })
        .and_then(|()| file.sync_all());
    match copied {
        Ok(()) => Ok(temp),
        Err(error) => {
            drop(file);
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// Creates a new file `<path><suffix>`, or `<path><suffix>.<n>` if that
/// exists, and returns it open for writing. An existing file is never
/// opened.
fn create_unique(path: &Path, suffix: &str) -> io::Result<(fs::File, PathBuf)> {
    for n in 0..MAX_ASIDE_NAMES {
        let candidate = match n {
            0 => sibling(path, suffix),
            n => sibling(path, &format!("{suffix}.{n}")),
        };
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Not upstream's: private while the bytes are written; the final
        // permissions are set after the copy.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(format!(
        "no free name for {}{suffix}",
        path.display()
    )))
}

/// Syncs the directory holding `path`, so a rename or link in it survives a
/// crash (a no-op where directories can't be synced).
#[cfg(all(test, unix))]
fn sync_parent(path: &Path) -> io::Result<()> {
    crate::publication::sync_parent(path)
}

/// Whether `keep` already holds the installed binary's bytes (`expected`,
/// else the installed file is hashed): `Ok` if it does, `AlreadyExists` if
/// it holds anything else.
fn kept_copy_matches(installed: &Path, keep: &Path, expected: Option<&str>) -> io::Result<()> {
    let hash = crate::staging::hash_file;
    let want = match expected {
        Some(want) => want.to_owned(),
        None => hash(installed)?,
    };
    if want.eq_ignore_ascii_case(&hash(keep)?) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not a copy of {}; a kept copy is never overwritten",
                keep.display(),
                installed.display()
            ),
        ))
    }
}

/// Keeps a copy of `installed` at `keep`: a no-replace copy if the name is
/// free, the file already there if it holds the same bytes, and if it holds
/// others, an error (`Immutable`) or an atomic replacement
/// (`ReplaceMismatched`).
fn keep_installed_copy(
    ops: &dyn FileOps,
    installed: &Path,
    keep: &Path,
    policy: KeepPolicy,
    expected: Option<&str>,
    root: &Path,
) -> io::Result<()> {
    if !ops.exists(keep) {
        match copy_whole_exclusive_with(ops, installed, keep, expected, root) {
            // Appeared since the check: the same rule applies.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            result => return result,
        }
    }
    match kept_copy_matches(installed, keep, expected) {
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && policy == KeepPolicy::ReplaceMismatched =>
        {
            copy_replacing_with(ops, installed, keep, expected, root)
        }
        // Not upstream's: a matching copy may be left by a run whose
        // publication sync failed, so its bytes and directories are
        // acknowledged again before the installed file is renamed.
        Ok(()) => reack_kept_copy(ops, keep, root),
        result => result,
    }
}

/// Syncs an existing kept copy's bytes, the chain of directories down to it
/// (up to `root`, not above), then its directory entry, so a retry after a
/// failed publication sync does not trust unsynced state.
fn reack_kept_copy(ops: &dyn FileOps, keep: &Path, root: &Path) -> io::Result<()> {
    // Windows can't open a running executable for sync; its moves are
    // written through already.
    #[cfg(unix)]
    fs::File::open(keep)?.sync_all()?;
    if let Some(dir) = keep.parent() {
        crate::publication::create_dir_all_durable(dir, root)?;
    }
    ops.sync_parent(keep)
}

/// `path` with `suffix` added to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// The permissions of an installed binary: executable by everyone, as the
/// installers make it (`source`'s elsewhere than Unix).
#[cfg(unix)]
fn executable_permissions(_source: fs::Permissions) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(0o755)
}

#[cfg(not(unix))]
fn executable_permissions(source: fs::Permissions) -> fs::Permissions {
    source
}

/// [`ReplaceOnDisk::prepare`] over `ops`. `use_aside` is whether the
/// installed file is renamed aside (Windows) rather than replaced.
pub fn prepare_with(
    ops: &dyn FileOps,
    plan: &SwitchPlan,
    use_aside: bool,
) -> Result<PreparedSwitch, SwitchError> {
    prepare_with_source(ops, plan, use_aside, None)
}

/// [`prepare_with`], copying from `source` when given instead of opening
/// `plan.new_binary`.
fn prepare_with_source(
    ops: &dyn FileOps,
    plan: &SwitchPlan,
    use_aside: bool,
    source: Option<&mut fs::File>,
) -> Result<PreparedSwitch, SwitchError> {
    if use_aside {
        // A copy a lost power left in the installed file's place comes first:
        // the aside a journal recorded, else the newest `.old`. Nothing is
        // cleaned up before this.
        restore_missing_installed(ops, &plan.installed, plan.preferred_aside.as_deref())?;
    }
    // The installed file is hashed before anything is kept or copied, so
    // the copies and the commit are bound to the file the caller classified.
    let measured = crate::installed_file::hash_installed(&plan.installed)
        .map_err(step("hash the installed binary"))?;
    let expected = match (&plan.expected_installed_sha256, measured) {
        (Some(want), Some(got)) if want.eq_ignore_ascii_case(&got) => Some(got),
        (Some(_), _) => {
            return Err(SwitchError {
                step: "check the installed binary",
                error: changed_elsewhere(format!(
                    "{} is not the binary the update classified",
                    plan.installed.display()
                )),
            });
        }
        (None, measured) => measured,
    };
    if let Some(keep) = &plan.keep_installed_at {
        let root = plan
            .keep_root
            .as_deref()
            .or_else(|| keep.parent())
            .unwrap_or_else(|| Path::new("."));
        keep_installed_copy(
            ops,
            &plan.installed,
            keep,
            plan.keep_policy,
            expected.as_deref(),
            root,
        )
        .map_err(step("keep a copy of the installed binary"))?;
    }
    // A new file of this call's own: a crashed run's `.new` is not touched.
    let suffix = format!(".new.{}", std::process::id());
    let new = match source {
        Some(source) => copy_beside_open(source, &plan.installed, &suffix, true),
        None => copy_beside(&plan.new_binary, &plan.installed, &suffix, true),
    }
    .and_then(|new| match ops.sync_parent(&new) {
        Ok(()) => Ok(new),
        Err(error) => {
            let _ = fs::remove_file(&new);
            Err(error)
        }
    })
    .map_err(step("copy the new binary beside the installed one"))?;
    let aside = if use_aside {
        // The journal's aside, if it still exists (the old binary may still
        // run), is protected until the journal releases it.
        let protect: Vec<&Path> = plan.preferred_aside.as_deref().into_iter().collect();
        remove_old_copies(ops, &plan.installed, &protect);
        match choose_aside(ops, &plan.installed) {
            Ok(aside) => Some(aside),
            Err(error) => {
                let _ = fs::remove_file(&new);
                return Err(error);
            }
        }
    } else {
        None
    };
    Ok(PreparedSwitch {
        installed: plan.installed.clone(),
        new,
        aside,
        expected_installed_sha256: expected,
        require_links: plan.keep_policy == KeepPolicy::Immutable,
    })
}

/// What the file a commit replaces must be.
#[derive(Clone, Copy, Debug)]
enum Expected<'a> {
    /// Not checked (the public [`replace_running_exe_with`]).
    Unchecked,
    /// The binary with this hash.
    Installed(&'a str),
    /// Nothing: it was missing when classified.
    Missing,
}

/// [`ReplaceOnDisk::commit`] over `ops`.
///
/// The commit is bound to the file that was classified at prepare, as far as
/// a file system without compare-and-swap allows. With an aside (Windows)
/// the installed file is renamed aside and the aside hashed, and put back
/// if it isn't the expected binary. Without one (Unix) the installed file is
/// hashed, hard-linked to `<installed>.prev.<pid>`, hashed again just before
/// the rename, and after the rename the link is hashed, and renamed back if
/// it isn't the expected binary. Either way a mismatch is a
/// [`SwitchError::is_changed_elsewhere`] error and the new binary is not
/// installed.
///
/// What remains: a writer that doesn't take the update lock and replaces the
/// installed file (or writes it in place) after the last read, but before
/// the rename, can go unnoticed on Unix, and one that writes in place
/// between the hash and the move aside on Windows. The installers take the
/// update lock to close this.
pub fn commit_with(
    ops: &dyn FileOps,
    prepared: &PreparedSwitch,
) -> Result<SwitchOutcome, SwitchError> {
    let expected = match &prepared.expected_installed_sha256 {
        Some(hash) => Expected::Installed(hash),
        None => Expected::Missing,
    };
    let placed = match &prepared.aside {
        Some(aside) => {
            replace_running_exe_checked(ops, &prepared.installed, &prepared.new, aside, expected)
        }
        None => replace_in_place(
            ops,
            &prepared.installed,
            &prepared.new,
            expected,
            prepared.require_links,
        ),
    };
    placed?;
    // The rename landed: a failed durability acknowledgement cannot undo
    // it or turn it into an uncommitted switch in reporting state.
    if let Err(error) = ops.sync_parent(&prepared.installed) {
        tracing::warn!(%error, "the binary was replaced, but its directory sync failed");
    }
    Ok(SwitchOutcome::RestartNeeded)
}

impl Switch for ReplaceOnDisk {
    fn switch(&self, plan: &SwitchPlan) -> Result<SwitchOutcome, SwitchError> {
        let prepared = self.prepare(plan)?;
        let placed = self.commit(&prepared);
        if placed.is_err() {
            prepared.discard();
        }
        placed
    }
}

/// The first of `<installed>.old`, `.old.1`, `.old.2`... that doesn't
/// exist: where the installed binary is moved to. It is chosen before any
/// rename so a journal can name it.
pub fn choose_aside(ops: &dyn FileOps, installed: &Path) -> Result<PathBuf, SwitchError> {
    (0..MAX_ASIDE_NAMES)
        .map(|n| match n {
            0 => sibling(installed, ".old"),
            n => sibling(installed, &format!(".old.{n}")),
        })
        .find(|path| !ops.exists(path))
        .ok_or_else(|| SwitchError {
            step: "find a name to move the installed binary aside to",
            error: io::Error::other("too many .old copies"),
        })
}

/// Windows: renames the installed binary to `aside`, then `new` into its
/// place, renaming the old one back if that fails. Each rename is retried
/// for transient errors. A missing installed binary is first restored from
/// `aside`, or else the newest `.old`, so a repeat of a step that stopped
/// half way, or a lost power, doesn't lose the only copy.
///
/// If the old binary can't be put back, the error names the installed path,
/// the aside and the new file; the caller keeps its record of the pending
/// install and tries again.
///
/// This does not check which binary it moves aside; [`commit_with`] does.
pub fn replace_running_exe_with(
    ops: &dyn FileOps,
    installed: &Path,
    new: &Path,
    aside: &Path,
) -> Result<(), SwitchError> {
    replace_running_exe_checked(ops, installed, new, aside, Expected::Unchecked)
}

/// What a check that failed leaves to say: the installed file `error` is
/// about is renamed from `aside` back to `installed`.
fn put_back_after_check(
    ops: &dyn FileOps,
    installed: &Path,
    aside: &Path,
    error: io::Error,
) -> SwitchError {
    match rename_retrying(ops, aside, installed) {
        Ok(()) => SwitchError {
            step: "check the installed binary",
            error,
        },
        Err(back) => SwitchError {
            step: "check the installed binary, and put it back",
            error: io::Error::other(format!(
                "the binary that was at {} is at {}, and is not the one the update classified: {error}; moving it back failed: {back}",
                installed.display(),
                aside.display()
            )),
        },
    }
}

fn replace_running_exe_checked(
    ops: &dyn FileOps,
    installed: &Path,
    new: &Path,
    aside: &Path,
    expected: Expected<'_>,
) -> Result<(), SwitchError> {
    if matches!(expected, Expected::Missing) && ops.exists(installed) {
        return Err(SwitchError {
            step: "check the installed binary",
            error: changed_elsewhere(format!(
                "{} appeared after it was found missing",
                installed.display()
            )),
        });
    }
    if let Restore::NothingToRestore = restore_missing_installed(ops, installed, Some(aside))? {
        return Err(SwitchError {
            step: "find the installed binary",
            error: io::Error::other(format!(
                "{} is missing and there is no .old copy to restore",
                installed.display()
            )),
        });
    }
    if ops.exists(aside) {
        return Err(SwitchError {
            step: "move the installed binary aside",
            error: io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists", aside.display()),
            ),
        });
    }
    rename_retrying(ops, installed, aside).map_err(step("move the installed binary aside"))?;
    if let Expected::Installed(want) = expected {
        // What was moved is what is hashed: nothing can change the path
        // between the check and the rename now.
        let mismatch = match crate::staging::hash_file(aside) {
            Ok(got) if got.eq_ignore_ascii_case(want) => None,
            Ok(_) => Some(changed_elsewhere(format!(
                "{} was replaced after the update classified it; it was put back",
                installed.display()
            ))),
            Err(error) => Some(error),
        };
        if let Some(error) = mismatch {
            return Err(put_back_after_check(ops, installed, aside, error));
        }
    }
    if let Err(error) = rename_retrying(ops, new, installed) {
        return Err(match rename_retrying(ops, aside, installed) {
            Ok(()) => SwitchError {
                step: "put the new binary in place",
                error,
            },
            Err(back) => SwitchError {
                step: "put the new binary in place, and put the old one back",
                error: io::Error::other(format!(
                    "the installed binary {} is at {}, and the new one at {}: {error}; moving it back failed: {back}",
                    installed.display(),
                    aside.display(),
                    new.display()
                )),
            },
        });
    }
    // Removed now if nothing runs it; else at the next switch.
    let _ = fs::remove_file(aside);
    Ok(())
}

/// Fails with a changed-elsewhere error unless `installed` is the binary
/// with hash `want`.
fn check_installed(installed: &Path, want: &str) -> Result<(), SwitchError> {
    match crate::installed_file::hash_installed(installed) {
        Ok(Some(got)) if got.eq_ignore_ascii_case(want) => Ok(()),
        Ok(found) => Err(SwitchError {
            step: "check the installed binary",
            error: changed_elsewhere(match found {
                Some(_) => format!(
                    "{} was replaced after the update classified it",
                    installed.display()
                ),
                None => format!(
                    "{} was removed after the update classified it",
                    installed.display()
                ),
            }),
        }),
        Err(error) => Err(SwitchError {
            step: "hash the installed binary",
            error,
        }),
    }
}

/// Hard-links `installed` at a name of this call's own, `<installed>.prev.<pid>`
/// (or `.<pid>.<n>`): one that exists, a crashed run's, is never used or
/// removed. If links aren't possible it fails closed, with the file untouched,
/// when `require_links`; otherwise (Not upstream's: no journal, as at 5b17f01)
/// it returns `None`, and the caller relies on the hash just before the rename.
fn link_installed(
    ops: &dyn FileOps,
    installed: &Path,
    require_links: bool,
) -> Result<Option<PathBuf>, SwitchError> {
    let suffix = format!(".prev.{}", std::process::id());
    for n in 0..MAX_ASIDE_NAMES {
        let candidate = match n {
            0 => sibling(installed, &suffix),
            n => sibling(installed, &format!("{suffix}.{n}")),
        };
        match ops.hard_link(installed, &candidate) {
            Ok(()) => return Ok(Some(candidate)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) if !require_links && crate::publication::link_unsupported(&error) => {
                note_reduced_check(installed, &error);
                return Ok(None);
            }
            Err(error) => {
                return Err(SwitchError {
                    step: "link the installed binary to check it (refused: the switch needs hard links)",
                    error,
                });
            }
        }
    }
    Err(SwitchError {
        step: "link the installed binary to check it",
        error: io::Error::other("no free .prev name"),
    })
}

/// Logs, once per process, that a switch without a journal went on without a
/// hard link to the installed file.
fn note_reduced_check(installed: &Path, error: &io::Error) {
    static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(
            %error,
            "{} is on a file system without hard links, so the switch checks the \
             installed binary only before the rename, not after it",
            installed.display()
        );
    }
}

/// Unix: renames `new` over `installed`, checking before and after that the
/// file it replaces is the `expected` one (see [`commit_with`]). The link
/// this makes is removed on every path that leaves it ours. Without links
/// (and with `require_links` false) only the checks before the rename run.
fn replace_in_place(
    ops: &dyn FileOps,
    installed: &Path,
    new: &Path,
    expected: Expected<'_>,
    require_links: bool,
) -> Result<(), SwitchError> {
    let want = match expected {
        Expected::Unchecked => {
            return rename_retrying(ops, new, installed)
                .map_err(step("put the new binary in place"));
        }
        Expected::Missing => {
            // A rename can't refuse to replace; the check is as narrow as
            // the window after it.
            if ops.exists(installed) {
                return Err(SwitchError {
                    step: "check the installed binary",
                    error: changed_elsewhere(format!(
                        "{} appeared after it was found missing",
                        installed.display()
                    )),
                });
            }
            return rename_retrying(ops, new, installed)
                .map_err(step("put the new binary in place"));
        }
        Expected::Installed(want) => want,
    };
    check_installed(installed, want)?;
    let link = link_installed(ops, installed, require_links)?;
    let remove_link = || {
        if let Some(link) = &link {
            let _ = fs::remove_file(link);
        }
    };
    // The last read before the rename: a file that replaced the installed
    // one since the first is caught here, and one that did after it by the
    // hash of the link below.
    if let Err(error) = check_installed(installed, want) {
        remove_link();
        return Err(error);
    }
    if let Err(error) = rename_retrying(ops, new, installed) {
        remove_link();
        return Err(SwitchError {
            step: "put the new binary in place",
            error,
        });
    }
    // No link: the check just before the rename is all there is.
    let Some(link) = link else {
        return Ok(());
    };
    // The link holds what the rename replaced.
    let error = match crate::staging::hash_file(&link) {
        Ok(got) if got.eq_ignore_ascii_case(want) => {
            let _ = fs::remove_file(&link);
            return Ok(());
        }
        Ok(_) => changed_elsewhere(format!(
            "the binary at {} was replaced after the update classified it; it was put back",
            installed.display()
        )),
        Err(error) => error,
    };
    // Not the binary that was classified: put it back over the new one.
    Err(match rename_retrying(ops, &link, installed) {
        Ok(()) => SwitchError {
            step: "check the replaced binary",
            error,
        },
        Err(back) => SwitchError {
            // The link is left: it is the only name of that file.
            step: "check the replaced binary, and put it back",
            error: io::Error::other(format!(
                "the binary that was at {} is now only at {}: {error}; moving it back failed: {back}",
                installed.display(),
                link.display()
            )),
        },
    })
}

/// What [`restore_missing_installed`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restore {
    /// The installed file is there.
    NotNeeded,
    /// It was missing and `from` was renamed to its place.
    Restored {
        /// The copy that came back.
        from: PathBuf,
    },
    /// It is missing and there is no copy to bring back.
    NothingToRestore,
}

/// If the installed file is missing, renames `preferred` (the aside a
/// journal recorded) or else the newest `.old` copy back to its place.
pub fn restore_missing_installed(
    ops: &dyn FileOps,
    installed: &Path,
    preferred: Option<&Path>,
) -> Result<Restore, SwitchError> {
    if ops.exists(installed) {
        return Ok(Restore::NotNeeded);
    }
    let copy = preferred
        .filter(|path| ops.exists(path))
        .map(Path::to_path_buf)
        .or_else(|| newest_old_copy(installed));
    let Some(from) = copy else {
        return Ok(Restore::NothingToRestore);
    };
    tracing::warn!(
        "{} is missing, so {} is renamed back to it",
        installed.display(),
        from.display()
    );
    rename_retrying(ops, &from, installed)
        .map_err(step("rename the old binary back to the installed one"))?;
    Ok(Restore::Restored { from })
}

/// Whether `rest`, what follows `<name>.old` in a file name, is empty or
/// `.<digits>`.
fn is_old_suffix(rest: &str) -> bool {
    rest.is_empty()
        || rest
            .strip_prefix('.')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The `.old` copies beside `installed`.
fn old_copies(installed: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (installed.parent(), installed.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.old", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let file_name = entry.file_name();
            file_name
                .to_string_lossy()
                .strip_prefix(&prefix)
                .is_some_and(is_old_suffix)
        })
        .map(|entry| entry.path())
        .collect()
}

/// The `.old` copy modified last (the lower name on a tie).
fn newest_old_copy(installed: &Path) -> Option<PathBuf> {
    let mut copies: Vec<_> = old_copies(installed)
        .into_iter()
        .map(|path| {
            let modified = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (modified, path)
        })
        .collect();
    copies.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    copies.into_iter().next().map(|(_, path)| path)
}

/// Removes the `.old` copies earlier switches left beside `installed`
/// that are no longer in use, except those named in `protect` (by file
/// name). Nothing is removed while `installed` is missing: an `.old` may be
/// the only copy.
fn remove_old_copies(ops: &dyn FileOps, installed: &Path, protect: &[&Path]) {
    if !ops.exists(installed) {
        return;
    }
    for copy in old_copies(installed) {
        let protected = protect
            .iter()
            .any(|kept| kept.file_name().is_some() && kept.file_name() == copy.file_name());
        if !protected {
            let _ = fs::remove_file(&copy);
        }
    }
}

/// Removes the `.old` copies a switch left beside `installed` (Windows
/// keeps one while the old binary still runs). It does nothing while
/// `installed` is missing.
pub fn clean_up_after_switch(installed: &Path) {
    remove_old_copies(&RealFileOps, installed, &[]);
}

/// [`clean_up_after_switch`] that leaves the files named in `protect` (a
/// journal's aside) alone.
pub fn clean_up_after_switch_keeping(installed: &Path, protect: &[&Path]) {
    remove_old_copies(&RealFileOps, installed, protect);
}

/// What [`cleanup_stale_prev`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrevCleanup {
    /// The stale `.prev` links removed.
    pub removed: Vec<PathBuf>,
    /// Those left, whose bytes match nothing the caller can name.
    pub kept: Vec<PathBuf>,
}

/// Whether `rest`, what follows `<name>.prev.` in a file name, is `<pid>` or
/// `<pid>.<n>`, digits only.
fn is_prev_suffix(rest: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    match rest.split_once('.') {
        None => digits(rest),
        Some((pid, n)) => digits(pid) && digits(n),
    }
}

/// The `<installed>.prev.<pid>[.<n>]` files beside `installed`.
fn prev_links_beside(installed: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (installed.parent(), installed.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.prev.", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut links: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix(&prefix)
                .is_some_and(is_prev_suffix)
        })
        .map(|entry| entry.path())
        .collect();
    links.sort();
    links
}

/// Paths already warned about, so a repeated cleanup warns once per file.
fn warn_once(path: &Path, why: &str) {
    static WARNED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    let Ok(mut warned) = WARNED.lock() else {
        return;
    };
    if !warned.iter().any(|seen| seen == path) {
        warned.push(path.to_path_buf());
        tracing::warn!("{} is left in place: {why}", path.display());
    }
}

/// Not upstream's: removes the `<installed>.prev.<pid>[.<n>]` links crashed
/// switches left beside `installed`, [`cleanup_stale_prev_with`] over the
/// real file system. The caller holds the update lock.
pub fn cleanup_stale_prev(installed: &Path, verified_sha256: &[String]) -> io::Result<PrevCleanup> {
    cleanup_stale_prev_with(&RealFileOps, installed, verified_sha256)
}

/// Removes a stale `.prev` link only when its bytes equal the installed
/// file's, or the hash of a kept copy the caller verified against a recorded
/// hash (`verified_sha256`: the version copies under `versions/` that
/// matched the state's or journal's hash). The link may be the only copy of
/// an old binary, so any other is left, with one warning per file for the
/// life of the process; an unreadable one is left too. With the installed
/// file missing only the verified hashes count. The caller holds the update
/// lock, so no switch of this process has a link in use. The directory is
/// synced when anything was removed; a failed sync is returned, the links
/// being already gone.
pub fn cleanup_stale_prev_with(
    ops: &dyn FileOps,
    installed: &Path,
    verified_sha256: &[String],
) -> io::Result<PrevCleanup> {
    remove_stale_prev_with(
        ops,
        installed,
        stale_prev_candidates(installed, verified_sha256)
            .map(|(path, candidate)| (path, candidate.map(|_| ()))),
    )
}

/// A read-only plan; dropping it (including after cancellation) deletes nothing.
pub(crate) struct PrevCleanupPlan {
    installed: PathBuf,
    links: Vec<(PathBuf, Result<PrevIdentity, String>)>,
}

/// Captured only for a no-follow regular file, before its bytes are hashed.
#[derive(PartialEq, Eq)]
struct PrevIdentity {
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl PrevIdentity {
    fn of(metadata: &fs::Metadata) -> io::Result<Self> {
        if !metadata.is_file() {
            return Err(io::Error::other("it is not a regular file"));
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
        })
    }
}

/// Whole-file reads belong on a blocking worker. The update lock must be held
/// from planning through application, unless the plan is discarded.
pub(crate) fn plan_stale_prev(installed: &Path, verified_sha256: &[String]) -> PrevCleanupPlan {
    PrevCleanupPlan {
        installed: installed.to_owned(),
        links: stale_prev_candidates(installed, verified_sha256)
            .map(|(path, candidate)| {
                let identity = candidate.and_then(|metadata| {
                    PrevIdentity::of(&metadata)
                        .map_err(|error| format!("its identity could not be recorded: {error}"))
                });
                (path, identity)
            })
            .collect(),
    }
}

/// No hashes or whole-file reads: only no-follow metadata is rechecked here.
/// Every `.prev` writer holds the update lock, which the caller still owns.
/// Revalidation also keeps an unexpected replacement whose identity differs;
/// it cannot protect against a writer bypassing the lock and preserving all
/// recorded metadata, or racing the metadata check and unlink.
pub(crate) fn apply_stale_prev(plan: PrevCleanupPlan) -> io::Result<PrevCleanup> {
    apply_stale_prev_with(&RealFileOps, plan)
}

fn apply_stale_prev_with(ops: &dyn FileOps, plan: PrevCleanupPlan) -> io::Result<PrevCleanup> {
    let candidates = plan.links.into_iter().map(|(path, candidate)| {
        let checked = candidate.and_then(|identity| {
            fs::symlink_metadata(&path)
                .and_then(|metadata| PrevIdentity::of(&metadata))
                .ok()
                .filter(|now| *now == identity)
                .map(|_| ())
                .ok_or_else(|| "its identity changed after cleanup was planned".to_owned())
        });
        (path, checked)
    });
    remove_stale_prev_with(ops, &plan.installed, candidates)
}

/// Shared classification, evaluated lazily so sync cleanup retains its
/// hash-then-remove order. This iterator never deletes or warns.
fn stale_prev_candidates<'a>(
    installed: &Path,
    verified_sha256: &'a [String],
) -> impl Iterator<Item = (PathBuf, Result<fs::Metadata, String>)> + use<'a> {
    let links = prev_links_beside(installed);
    let installed_hash = (!links.is_empty())
        .then(|| {
            crate::installed_file::hash_installed(installed)
                .ok()
                .flatten()
        })
        .flatten();
    // Where the installed path really leads: it may be a symlink to a `.prev`
    // file, whose bytes the installed hash then counts, and which is the only
    // copy. `None` is a missing installed file; any other failure keeps every
    // candidate. Names are compared, not inodes: an ordinary hard link of the
    // installed file is still removable.
    let installed_target = match fs::canonicalize(installed) {
        Ok(target) => Some(Some(target)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    };
    links.into_iter().map(move |link| {
        let candidate = (|| {
            // Only a regular file: never follow or remove a link to elsewhere.
            let metadata = fs::symlink_metadata(&link)
                .ok()
                .filter(|metadata| metadata.is_file())
                .ok_or_else(|| "it is not a regular file".to_owned())?;
            let is_installed_target = match (&installed_target, fs::canonicalize(&link)) {
                (Some(Some(target)), Ok(path)) => *target == path,
                (Some(None), Ok(_)) => false,
                _ => true,
            };
            if is_installed_target {
                return Err("the installed path leads to it, or that could not be told".to_owned());
            }
            let known = |hash: &str| {
                installed_hash
                    .as_deref()
                    .is_some_and(|h| h.eq_ignore_ascii_case(hash))
                    || verified_sha256.iter().any(|v| v.eq_ignore_ascii_case(hash))
            };
            match crate::staging::hash_file(&link) {
                Ok(hash) if known(&hash) => Ok(metadata),
                Ok(_) => Err(
                    "its bytes match neither the installed binary nor a verified kept copy, \
                 so it may be the only copy of an old binary"
                        .to_owned(),
                ),
                Err(error) => Err(format!("it could not be read: {error}")),
            }
        })();
        (link, candidate)
    })
}

fn remove_stale_prev_with(
    ops: &dyn FileOps,
    installed: &Path,
    candidates: impl IntoIterator<Item = (PathBuf, Result<(), String>)>,
) -> io::Result<PrevCleanup> {
    let mut done = PrevCleanup::default();
    for (link, candidate) in candidates {
        match candidate {
            Ok(()) => match fs::remove_file(&link) {
                Ok(()) => {
                    tracing::info!("removed {}, a stale copy of a known binary", link.display());
                    done.removed.push(link);
                }
                Err(error) => {
                    warn_once(&link, &format!("it could not be removed: {error}"));
                    done.kept.push(link);
                }
            },
            Err(why) => {
                warn_once(&link, &why);
                done.kept.push(link);
            }
        }
    }
    if !done.removed.is_empty() {
        ops.sync_parent(installed)?;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {

    #[cfg(unix)]
    #[test]
    fn copied_binaries_never_restore_write_for_others_with_umask_zero() {
        use crate::journal::creation_tests::{assert_mode, with_umask_zero};
        with_umask_zero(
            "switch::tests::copied_binaries_never_restore_write_for_others_with_umask_zero",
            |root| {
                use std::os::unix::fs::PermissionsExt;
                let source = root.join("source");
                fs::write(&source, b"binary").unwrap();
                for mode in [0o777, 0o775] {
                    fs::set_permissions(&source, fs::Permissions::from_mode(mode)).unwrap();
                    let to = root.join(format!("data/{mode:o}/kept"));
                    let kept = copy_to_temp(&source, &to, root).unwrap();
                    assert_mode(&kept, 0o755);
                    let executable = copy_beside(&source, &to, ".new", true).unwrap();
                    assert_mode(&executable, 0o755);
                    let (file, private) = create_unique(&to, ".private").unwrap();
                    assert_mode(&private, 0o700);
                    drop(file);
                }
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn copied_binaries_never_restore_write_for_others_with_default_acl() {
        use crate::journal::creation_tests::{assert_acl_mode, with_default_acl};
        with_default_acl(
            "switch::tests::copied_binaries_never_restore_write_for_others_with_default_acl",
            |root| {
                use std::os::unix::fs::PermissionsExt;
                let source = root.join("source");
                fs::write(&source, b"binary").unwrap();
                for mode in [0o777, 0o775] {
                    fs::set_permissions(&source, fs::Permissions::from_mode(mode)).unwrap();
                    let to = root.join(format!("data/{mode:o}/kept"));
                    let kept = copy_to_temp(&source, &to, root).unwrap();
                    assert_acl_mode(&kept, 0o755);
                    let executable = copy_beside(&source, &to, ".new", true).unwrap();
                    assert_acl_mode(&executable, 0o755);
                    let (file, private) = create_unique(&to, ".private").unwrap();
                    assert_acl_mode(&private, 0o700);
                    drop(file);
                }
            },
        );
    }
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// A rename that fails: any whose destination is `to`, `times` times.
    struct Fail {
        to: PathBuf,
        times: usize,
        transient: bool,
    }

    /// The real file system with scripted rename failures and no waiting.
    #[derive(Default)]
    struct Ops {
        fail: Mutex<Vec<Fail>>,
        slept: Mutex<Vec<u64>>,
        renames: Mutex<VecDeque<(PathBuf, PathBuf)>>,
        fail_sync: Mutex<Option<PathBuf>>,
        operations: Mutex<Vec<(String, PathBuf)>>,
        /// Runs at the start of every rename (a writer outside the update).
        before_rename: Mutex<Option<Hook2>>,
        /// Runs at the start of every link.
        before_link: Mutex<Option<Hook1>>,
        /// Makes every link fail with this kind.
        link_error: Mutex<Option<io::ErrorKind>>,
    }

    type Hook1 = Box<dyn Fn(&Path) + Send + Sync>;
    type Hook2 = Box<dyn Fn(&Path, &Path) + Send + Sync>;

    impl Ops {
        fn failing(to: &Path, times: usize, transient: bool) -> Self {
            let ops = Self::default();
            ops.fail.lock().unwrap().push(Fail {
                to: to.to_path_buf(),
                times,
                transient,
            });
            ops
        }

        fn clear(&self) {
            self.fail.lock().unwrap().clear();
        }

        fn slept(&self) -> Vec<u64> {
            self.slept.lock().unwrap().clone()
        }
    }

    impl FileOps for Ops {
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(("rename".into(), to.into()));
            self.renames
                .lock()
                .unwrap()
                .push_back((from.to_path_buf(), to.to_path_buf()));
            if let Some(hook) = &*self.before_rename.lock().unwrap() {
                hook(from, to);
            }
            if let Some(fail) = self
                .fail
                .lock()
                .unwrap()
                .iter_mut()
                .find(|fail| fail.to == to && fail.times > 0)
            {
                fail.times -= 1;
                return Err(if fail.transient {
                    io::ErrorKind::PermissionDenied.into()
                } else {
                    io::Error::other("injected")
                });
            }
            fs::rename(from, to)
        }

        fn exists(&self, path: &Path) -> bool {
            RealFileOps.exists(path)
        }

        fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(("link".into(), to.into()));
            if let Some(hook) = &*self.before_link.lock().unwrap() {
                hook(from);
            }
            if let Some(kind) = *self.link_error.lock().unwrap() {
                return Err(kind.into());
            }
            fs::hard_link(from, to)
        }

        fn sync_parent(&self, path: &Path) -> io::Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(("sync".into(), path.into()));
            if self.fail_sync.lock().unwrap().as_deref() == Some(path) {
                return Err(io::Error::other("injected sync failure"));
            }
            crate::publication::sync_parent(path)
        }

        fn sleep(&self, wait: Duration) {
            self.slept.lock().unwrap().push(wait.as_millis() as u64);
        }
    }

    struct Dir {
        _temp: tempfile::TempDir,
        installed: PathBuf,
        new: PathBuf,
        aside: PathBuf,
    }

    fn dir() -> Dir {
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join("open-ferry");
        let new = temp.path().join("open-ferry.new.1");
        fs::write(&installed, "old").unwrap();
        fs::write(&new, "new").unwrap();
        Dir {
            aside: temp.path().join("open-ferry.old"),
            _temp: temp,
            installed,
            new,
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    // Not upstream's: the first rename fails and nothing has changed.
    #[test]
    fn the_first_rename_failing_changes_nothing() {
        let d = dir();
        let ops = Ops::failing(&d.aside, 1, false);
        let error = replace_running_exe_with(&ops, &d.installed, &d.new, &d.aside).unwrap_err();
        assert_eq!(error.step, "move the installed binary aside");
        assert_eq!(read(&d.installed), "old");
        assert_eq!(read(&d.new), "new");
        assert!(!d.aside.exists());
        assert!(ops.slept().is_empty(), "no retry for a permanent error");
    }

    // Not upstream's: the second rename fails twice for a moment and works.
    #[cfg(windows)]
    #[test]
    fn the_second_rename_failing_transiently_is_retried() {
        let d = dir();
        let ops = Ops::failing(&d.installed, 2, true);
        replace_running_exe_with(&ops, &d.installed, &d.new, &d.aside).unwrap();
        assert_eq!(read(&d.installed), "new");
        assert!(!d.new.exists());
        assert!(!d.aside.exists(), "a copy nothing runs is removed");
        assert_eq!(ops.slept(), [50, 100]);
    }

    // Not upstream's: the second rename fails for good and the old file
    // comes back.
    #[test]
    fn the_second_rename_failing_for_good_puts_the_original_back() {
        let d = dir();
        let ops = Ops::failing(&d.installed, 1, false);
        let error = replace_running_exe_with(&ops, &d.installed, &d.new, &d.aside).unwrap_err();
        assert_eq!(error.step, "put the new binary in place");
        assert_eq!(read(&d.installed), "old");
        assert_eq!(read(&d.new), "new");
        assert!(!d.aside.exists());
    }

    // Not upstream's: a transient error that never clears gives up after the
    // listed waits, and the original comes back.
    #[cfg(windows)]
    #[test]
    fn a_rename_that_stays_locked_waits_the_listed_times_then_gives_up() {
        let d = dir();
        // Fails the new file's rename 6 times, then lets the restore work.
        let ops = Ops::failing(&d.installed, RENAME_TRIES + 1, true);
        let error = replace_running_exe_with(&ops, &d.installed, &d.new, &d.aside);
        assert!(error.is_err());
        assert_eq!(ops.slept()[..RENAME_TRIES], RENAME_WAITS_MS);
        assert_eq!(read(&d.installed), "old");
    }

    // Not upstream's: both fail; the error names the three paths, the old
    // file stays in the aside, and a repeat restores it first.
    #[test]
    fn both_renames_failing_names_the_paths_and_a_retry_recovers() {
        let d = dir();
        let ops = Ops::failing(&d.installed, usize::MAX, false);
        let error = replace_running_exe_with(&ops, &d.installed, &d.new, &d.aside).unwrap_err();
        let text = error.to_string();
        for path in [&d.installed, &d.aside, &d.new] {
            assert!(text.contains(&path.display().to_string()), "{text}");
        }
        assert!(!d.installed.exists());
        assert_eq!(read(&d.aside), "old");
        assert_eq!(read(&d.new), "new");

        ops.clear();
        replace_running_exe_with(&ops, &d.installed, &d.new, &d.aside).unwrap();
        assert_eq!(read(&d.installed), "new");
    }

    // Not upstream's: power lost between the renames left only an `.old`.
    #[test]
    fn a_missing_installed_file_is_restored_before_anything_is_deleted() {
        let d = dir();
        let older = d.installed.with_file_name("open-ferry.old.1");
        fs::write(&older, "older").unwrap();
        fs::rename(&d.installed, &d.aside).unwrap();
        // The aside is the newest: make `.old.1` clearly older.
        let file = fs::OpenOptions::new().write(true).open(&older).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        drop(file);

        clean_up_after_switch(&d.installed);
        clean_up_after_switch_keeping(&d.installed, &[d.aside.as_path()]);
        assert!(d.aside.exists() && older.exists(), "nothing is deleted");

        let restored = restore_missing_installed(&RealFileOps, &d.installed, None).unwrap();
        assert_eq!(
            restored,
            Restore::Restored {
                from: d.aside.clone()
            }
        );
        assert_eq!(read(&d.installed), "old");
        assert_eq!(
            restore_missing_installed(&RealFileOps, &d.installed, None).unwrap(),
            Restore::NotNeeded
        );
    }

    // Not upstream's: a given aside is preferred, and none at all is an error.
    #[test]
    fn the_recorded_aside_is_restored_and_with_no_copy_the_replacement_fails() {
        let d = dir();
        fs::rename(&d.installed, &d.aside).unwrap();
        replace_running_exe_with(&RealFileOps, &d.installed, &d.new, &d.aside).unwrap();
        assert_eq!(read(&d.installed), "new");

        fs::remove_file(&d.installed).unwrap();
        let error =
            replace_running_exe_with(&RealFileOps, &d.installed, &d.new, &d.aside).unwrap_err();
        assert_eq!(error.step, "find the installed binary");
    }

    // Not upstream's: the cleanup removes the `.old` copies but the aside a
    // journal names.
    #[test]
    fn the_cleanup_skips_a_protected_aside() {
        let d = dir();
        let other = d.installed.with_file_name("open-ferry.old.3");
        let unrelated = d.installed.with_file_name("open-ferry.older");
        fs::write(&d.aside, "a").unwrap();
        fs::write(&other, "b").unwrap();
        fs::write(&unrelated, "c").unwrap();
        clean_up_after_switch_keeping(&d.installed, &[d.aside.as_path()]);
        assert!(d.aside.exists() && !other.exists() && unrelated.exists());
        clean_up_after_switch(&d.installed);
        assert!(!d.aside.exists() && unrelated.exists());
    }

    // Not upstream's: the aside is chosen from what exists.
    #[test]
    fn the_aside_is_the_first_free_name() {
        let d = dir();
        assert_eq!(choose_aside(&RealFileOps, &d.installed).unwrap(), d.aside);
        fs::write(&d.aside, "").unwrap();
        let second = d.installed.with_file_name("open-ferry.old.1");
        assert_eq!(choose_aside(&RealFileOps, &d.installed).unwrap(), second);
    }

    // Not upstream's: a caller that journals gets the aside before any
    // rename, and the same aside is what the commit uses.
    #[test]
    fn prepare_chooses_the_aside_and_renames_nothing() {
        let d = dir();
        let source = d.installed.with_file_name("release-binary");
        fs::write(&source, "release").unwrap();
        let plan = SwitchPlan {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            new_binary: source,
            installed: d.installed.clone(),
            keep_installed_at: None,
            keep_root: None,
            keep_policy: KeepPolicy::Immutable,
            preferred_aside: None,
            expected_installed_sha256: None,
        };
        fs::write(&d.aside, "stale").unwrap();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &plan, true).unwrap();
        assert!(ops.renames.lock().unwrap().is_empty());
        assert_eq!(prepared.aside.as_deref(), Some(d.aside.as_path()));
        assert_eq!(read(&d.installed), "old");
        assert_eq!(read(&prepared.new), "release");

        commit_with(&ops, &prepared).unwrap();
        assert_eq!(read(&d.installed), "release");
        assert!(!prepared.new.exists());

        // Without an aside (Unix) the new binary is renamed over the old.
        let source = d.installed.with_file_name("release-2");
        fs::write(&source, "again").unwrap();
        let plan = SwitchPlan {
            new_binary: source,
            ..plan
        };
        let prepared = prepare_with(&ops, &plan, false).unwrap();
        assert_eq!(prepared.aside, None);
        commit_with(&ops, &prepared).unwrap();
        assert_eq!(read(&d.installed), "again");
    }

    // Not upstream's: `prepare_from_open` copies the whole file whatever the
    // handle's offset was (the caller has read it to hash it).
    #[test]
    fn prepare_from_open_copies_the_whole_file_after_the_handle_was_read_to_the_end() {
        use std::io::Read;
        let d = dir();
        let source = d.installed.with_file_name("release-binary");
        fs::write(&source, "release").unwrap();
        let plan = SwitchPlan {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            new_binary: d.installed.with_file_name("never-opened"),
            installed: d.installed.clone(),
            keep_installed_at: None,
            keep_root: None,
            keep_policy: KeepPolicy::Immutable,
            preferred_aside: None,
            expected_installed_sha256: None,
        };
        let mut handle = fs::File::open(&source).unwrap();
        let mut sink = Vec::new();
        handle.read_to_end(&mut sink).unwrap();
        let prepared = ReplaceOnDisk.prepare_from_open(&plan, &mut handle).unwrap();
        assert_eq!(read(&prepared.new), "release");
    }

    // Not upstream's: a prepare after a lost power puts the old copy back
    // before it copies anything.
    #[test]
    fn prepare_restores_a_missing_installed_file_first() {
        let d = dir();
        fs::rename(&d.installed, &d.aside).unwrap();
        let plan = SwitchPlan {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            new_binary: d.new.clone(),
            installed: d.installed.clone(),
            keep_installed_at: None,
            keep_root: None,
            keep_policy: KeepPolicy::Immutable,
            preferred_aside: None,
            expected_installed_sha256: None,
        };
        let prepared = prepare_with(&RealFileOps, &plan, true).unwrap();
        assert_eq!(read(&d.installed), "old");
        assert_eq!(prepared.aside.as_deref(), Some(d.aside.as_path()));
    }

    // Not upstream's: a kept copy takes a free name without replacing, and
    // a taken name as before.
    #[test]
    fn the_kept_copy_is_exclusive_when_the_name_is_free() {
        let d = dir();
        let keep = d.installed.with_file_name("kept");
        copy_whole_exclusive(&d.installed, &keep).unwrap();
        assert_eq!(read(&keep), "old");
        let error = copy_whole_exclusive(&d.new, &keep).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&keep), "old");
        let leftovers: Vec<_> = fs::read_dir(d.installed.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("kept.partial."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    fn plan(d: &Dir, keep: Option<PathBuf>) -> SwitchPlan {
        SwitchPlan {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            new_binary: d.new.clone(),
            installed: d.installed.clone(),
            keep_installed_at: keep,
            keep_root: None,
            keep_policy: KeepPolicy::Immutable,
            preferred_aside: None,
            expected_installed_sha256: None,
        }
    }

    // Not upstream's: a kept copy is immutable. The same bytes are reused,
    // others fail the switch before anything is staged, and neither changes
    // the file.
    #[test]
    fn an_existing_kept_copy_is_reused_if_identical_and_refused_otherwise() {
        let d = dir();
        let keep = d.installed.with_file_name("kept");
        fs::write(&keep, "old").unwrap();
        let prepared = prepare_with(&RealFileOps, &plan(&d, Some(keep.clone())), false).unwrap();
        assert_eq!(read(&keep), "old");
        prepared.discard();

        fs::write(&keep, "something else").unwrap();
        let error = prepare_with(&RealFileOps, &plan(&d, Some(keep.clone())), false).unwrap_err();
        assert_eq!(error.step, "keep a copy of the installed binary");
        assert_eq!(error.error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&keep), "something else");
        assert_eq!(read(&d.installed), "old");
        let names: Vec<_> = fs::read_dir(d.installed.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.starts_with(&format!("open-ferry.new.{}", std::process::id()))
                    || name.contains(".partial.")
            })
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    // Not upstream's: a partial file a crashed run left is never opened,
    // overwritten or removed, by the copy or by the staging.
    #[test]
    fn a_crashed_runs_partial_files_survive_a_new_prepare() {
        let d = dir();
        let keep = d.installed.with_file_name("kept");
        let pid = std::process::id();
        let partial = d.installed.with_file_name(format!("kept.partial.{pid}"));
        let stale_new = d.installed.with_file_name(format!("open-ferry.new.{pid}"));
        fs::write(&partial, "crashed").unwrap();
        fs::write(&stale_new, "crashed too").unwrap();

        let prepared = prepare_with(&RealFileOps, &plan(&d, Some(keep.clone())), false).unwrap();
        assert_eq!(read(&keep), "old");
        assert_eq!(read(&partial), "crashed");
        assert_eq!(read(&stale_new), "crashed too");
        assert_ne!(prepared.new, stale_new);
        assert_eq!(read(&prepared.new), "new");

        // A failed copy removes only the file it made.
        let error = copy_whole_exclusive(&d.installed, &keep).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&partial), "crashed");
        let missing = d.installed.with_file_name("no-such-source");
        assert!(copy_whole_exclusive(&missing, &d.installed.with_file_name("k2")).is_err());
        assert_eq!(read(&partial), "crashed");
        assert_eq!(read(&stale_new), "crashed too");
    }

    // Not upstream's: the staged file is executable (set before the sync),
    // the kept copy keeps the source's mode, and the directory sync the
    // commit and the publication make works. (That the syncs reach the disk
    // can't be observed here.)
    #[cfg(unix)]
    #[test]
    fn the_staged_and_kept_files_have_their_modes() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir();
        let keep = d.installed.with_file_name("kept");
        fs::set_permissions(&d.installed, fs::Permissions::from_mode(0o700)).unwrap();
        let prepared = prepare_with(&RealFileOps, &plan(&d, Some(keep.clone())), false).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&prepared.new), 0o755);
        assert_eq!(mode(&keep), 0o700);
        commit_with(&RealFileOps, &prepared).unwrap();
        assert_eq!(read(&d.installed), "new");
        assert!(sync_parent(&d.installed).is_ok());
    }

    // Not upstream's: a copy is created private (0o700), so its bytes are never
    // group or world readable-writable while they are written.
    #[cfg(unix)]
    #[test]
    fn a_copy_is_created_with_a_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir();
        let (file, path) = create_unique(&d.installed, ".probe").unwrap();
        let mode = file.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", path.display());
    }

    // Not upstream's: kept bytes and their directory are durable before any
    // installed-file rename, and a failed kept sync prevents the commit.
    #[test]
    fn the_kept_copy_is_synced_before_the_installed_file_is_renamed() {
        let d = dir();
        let keep = d._temp.path().join("versions/0.1.0/open-ferry");
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &plan(&d, Some(keep.clone())), true).unwrap();
        commit_with(&ops, &prepared).unwrap();
        let operations = ops.operations.lock().unwrap();
        let synced = operations
            .iter()
            .position(|op| op == &("sync".into(), keep.clone()))
            .unwrap();
        let renamed = operations.iter().position(|op| op.0 == "rename").unwrap();
        assert!(synced < renamed);
        drop(operations);

        let d = dir();
        let keep = d._temp.path().join("versions/0.1.0/open-ferry");
        let ops = Ops::default();
        *ops.fail_sync.lock().unwrap() = Some(keep.clone());
        assert!(prepare_with(&ops, &plan(&d, Some(keep)), true).is_err());
        assert!(ops.renames.lock().unwrap().is_empty());
        assert_eq!(read(&d.installed), "old");
    }

    // Not upstream's: a retry of the same plan after a failed publication
    // sync finds the kept copy already there, and must sync it again before
    // any installed-file rename.
    #[test]
    fn a_kept_copy_left_by_a_failed_sync_is_synced_again_on_retry() {
        let d = dir();
        let keep = d._temp.path().join("versions/0.1.0/open-ferry");
        let same_plan = plan(&d, Some(keep.clone()));
        let ops = Ops::default();
        *ops.fail_sync.lock().unwrap() = Some(keep.clone());
        assert!(prepare_with(&ops, &same_plan, true).is_err());
        assert_eq!(read(&keep), "old", "the copy is there, unacknowledged");
        assert!(ops.renames.lock().unwrap().is_empty());

        *ops.fail_sync.lock().unwrap() = None;
        ops.operations.lock().unwrap().clear();
        let prepared = prepare_with(&ops, &same_plan, true).unwrap();
        commit_with(&ops, &prepared).unwrap();
        let operations = ops.operations.lock().unwrap();
        let synced = operations
            .iter()
            .position(|op| op == &("sync".into(), keep.clone()))
            .expect("the existing kept copy is synced again");
        let renamed = operations.iter().position(|op| op.0 == "rename").unwrap();
        assert!(synced < renamed);
        drop(operations);
        assert_eq!(read(&d.installed), "new");
    }

    // Not upstream's: with a root, a nested kept copy is made and re-synced
    // inside it, a retry of the same plan works, and a kept copy outside the
    // root is refused before anything is copied or renamed.
    #[test]
    fn a_rooted_kept_copy_retries_within_its_root_and_refuses_outside_it() {
        let d = dir();
        let root = d._temp.path().join("data");
        let keep = root.join("versions/0.1.0/open-ferry");
        let rooted = SwitchPlan {
            keep_root: Some(root.clone()),
            ..plan(&d, Some(keep.clone()))
        };
        let ops = Ops::default();
        *ops.fail_sync.lock().unwrap() = Some(keep.clone());
        assert!(prepare_with(&ops, &rooted, true).is_err());
        assert!(ops.renames.lock().unwrap().is_empty());

        *ops.fail_sync.lock().unwrap() = None;
        ops.operations.lock().unwrap().clear();
        let prepared = prepare_with(&ops, &rooted, true).unwrap();
        commit_with(&ops, &prepared).unwrap();
        let operations = ops.operations.lock().unwrap();
        let synced = operations
            .iter()
            .position(|op| op == &("sync".into(), keep.clone()))
            .expect("the existing kept copy is synced again");
        let renamed = operations.iter().position(|op| op.0 == "rename").unwrap();
        assert!(synced < renamed);
        drop(operations);
        assert_eq!(read(&keep), "old");
        assert_eq!(read(&d.installed), "new");

        let d = dir();
        let outside = SwitchPlan {
            keep_root: Some(d._temp.path().join("data")),
            ..plan(&d, Some(d._temp.path().join("elsewhere/0.1.0/open-ferry")))
        };
        let ops = Ops::default();
        let error = prepare_with(&ops, &outside, true).unwrap_err();
        assert_eq!(error.error.kind(), io::ErrorKind::InvalidInput);
        assert!(!d._temp.path().join("elsewhere").exists());
        assert!(ops.renames.lock().unwrap().is_empty());
    }

    // Not upstream's: a landed rename is a completed switch even if its
    // final directory sync cannot acknowledge durability.
    #[test]
    fn a_final_sync_failure_still_reports_the_completed_switch() {
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &plan(&d, None), true).unwrap();
        *ops.fail_sync.lock().unwrap() = Some(d.installed.clone());
        assert_eq!(
            commit_with(&ops, &prepared).unwrap(),
            SwitchOutcome::RestartNeeded
        );
        assert_eq!(read(&d.installed), "new");
    }

    fn hash(path: &Path) -> String {
        crate::staging::hash_file(path).unwrap()
    }

    /// The names in the directory holding `installed`.
    fn names(d: &Dir) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(d.installed.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn prev_links(d: &Dir) -> Vec<String> {
        names(d)
            .into_iter()
            .filter(|name| name.contains(".prev."))
            .collect()
    }

    /// A plan that expects the installed file to hold "old".
    fn bound_plan(d: &Dir) -> SwitchPlan {
        SwitchPlan {
            expected_installed_sha256: Some(hash(&d.installed)),
            ..plan(d, None)
        }
    }

    /// Replaces `path` with a different file (a new inode), as an installer
    /// outside the update would.
    fn replace_with_foreign(path: &Path) {
        fs::remove_file(path).unwrap();
        fs::write(path, "foreign").unwrap();
    }

    // Not upstream's (O3): the plan's expected hash is checked at prepare,
    // before anything is kept or copied.
    #[test]
    fn an_installed_file_other_than_the_classified_one_is_refused_at_prepare() {
        let d = dir();
        let keep = d.installed.with_file_name("kept");
        let wrong = SwitchPlan {
            expected_installed_sha256: Some(hash(&d.new)),
            ..plan(&d, Some(keep.clone()))
        };
        let error = prepare_with(&RealFileOps, &wrong, false).unwrap_err();
        assert!(error.is_changed_elsewhere(), "{error}");
        assert!(!keep.exists());
        assert_eq!(read(&d.installed), "old");
        assert_eq!(names(&d), ["open-ferry", "open-ferry.new.1"]);

        // The right hash is accepted, in either case, and recorded.
        let right = SwitchPlan {
            expected_installed_sha256: Some(hash(&d.installed).to_uppercase()),
            ..wrong
        };
        let prepared = prepare_with(&RealFileOps, &right, false).unwrap();
        assert_eq!(prepared.expected_installed_sha256, Some(hash(&d.installed)));
        // None measures what is installed now.
        let prepared = prepare_with(&RealFileOps, &plan(&d, None), false).unwrap();
        assert_eq!(prepared.expected_installed_sha256, Some(hash(&d.installed)));
    }

    // Not upstream's (O3): a missing installed file is placed without a
    // check, but one that appears before the commit is not replaced.
    #[test]
    fn a_missing_installed_file_is_placed_and_one_that_appears_is_refused() {
        let d = dir();
        fs::remove_file(&d.installed).unwrap();
        let prepared = prepare_with(&RealFileOps, &plan(&d, None), false).unwrap();
        assert_eq!(prepared.expected_installed_sha256, None);
        commit_with(&RealFileOps, &prepared).unwrap();
        assert_eq!(read(&d.installed), "new");

        fs::remove_file(&d.installed).unwrap();
        let prepared = prepare_with(&RealFileOps, &plan(&d, None), false).unwrap();
        fs::write(&d.installed, "foreign").unwrap();
        let error = commit_with(&RealFileOps, &prepared).unwrap_err();
        assert!(error.is_changed_elsewhere(), "{error}");
        assert_eq!(read(&d.installed), "foreign");
        assert_eq!(read(&prepared.new), "new");
    }

    // Not upstream's (O3, no-aside path): a file replaced between prepare
    // and the commit's first hash is refused, and nothing is linked or
    // renamed.
    #[test]
    fn a_binary_replaced_before_the_commit_is_refused_without_a_rename() {
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &bound_plan(&d), false).unwrap();
        fs::write(&d.installed, "foreign").unwrap();
        let error = commit_with(&ops, &prepared).unwrap_err();
        assert!(error.is_changed_elsewhere(), "{error}");
        assert_eq!(read(&d.installed), "foreign");
        assert_eq!(read(&prepared.new), "new");
        assert!(ops.renames.lock().unwrap().is_empty());
        assert!(prev_links(&d).is_empty());
    }

    // Not upstream's (O3, no-aside path): a file that replaces the installed
    // one after the first hash is caught by the second, just before the
    // rename.
    #[test]
    fn a_binary_replaced_before_the_final_read_is_refused_and_the_link_removed() {
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &bound_plan(&d), false).unwrap();
        let installed = d.installed.clone();
        *ops.before_link.lock().unwrap() =
            Some(Box::new(move |_| replace_with_foreign(&installed)));
        let error = commit_with(&ops, &prepared).unwrap_err();
        assert!(error.is_changed_elsewhere(), "{error}");
        assert_eq!(read(&d.installed), "foreign");
        assert_eq!(read(&prepared.new), "new");
        assert!(ops.renames.lock().unwrap().is_empty());
        assert!(prev_links(&d).is_empty());
    }

    // Not upstream's (O3, no-aside path): a write into the installed file
    // after the last read shares the link's inode; the hash of the link
    // after the rename finds it, and the file goes back.
    #[test]
    fn a_binary_changed_after_the_final_read_is_put_back() {
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &bound_plan(&d), false).unwrap();
        let installed = d.installed.clone();
        *ops.before_rename.lock().unwrap() = Some(Box::new(move |_, to| {
            if to == installed {
                fs::write(to, "foreign").unwrap();
            }
        }));
        let error = commit_with(&ops, &prepared).unwrap_err();
        assert!(error.is_changed_elsewhere(), "{error}");
        assert_eq!(read(&d.installed), "foreign", "put back, untouched");
        assert!(prev_links(&d).is_empty(), "{:?}", names(&d));
    }

    // Not upstream's (O3, no-aside path): a good commit removes its link,
    // and a crashed run's `.prev` file is neither used nor removed.
    #[test]
    fn the_link_is_removed_and_a_crashed_runs_link_is_left() {
        let d = dir();
        let stale_name = format!("open-ferry.prev.{}", std::process::id());
        let stale = d.installed.with_file_name(&stale_name);
        fs::write(&stale, "crashed").unwrap();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &bound_plan(&d), false).unwrap();
        commit_with(&ops, &prepared).unwrap();
        assert_eq!(read(&d.installed), "new");
        assert_eq!(read(&stale), "crashed");
        assert_eq!(prev_links(&d), [stale_name]);
        let linked = ops
            .operations
            .lock()
            .unwrap()
            .iter()
            .filter(|op| op.0 == "link")
            .count();
        assert_eq!(linked, 2, "the stale name was tried, then the next");
    }

    // Not upstream's (O3, no-aside path): a rename that fails removes the
    // link.
    #[test]
    fn a_failed_commit_rename_removes_the_link() {
        let d = dir();
        let ops = Ops::failing(&d.installed, 1, false);
        let prepared = prepare_with(&ops, &bound_plan(&d), false).unwrap();
        let error = commit_with(&ops, &prepared).unwrap_err();
        assert_eq!(error.step, "put the new binary in place");
        assert!(!error.is_changed_elsewhere());
        assert_eq!(read(&d.installed), "old");
        assert!(prev_links(&d).is_empty());
    }

    // Not upstream's (O3, no-aside path): where links aren't possible the
    // commit fails closed and changes nothing.
    #[test]
    fn a_file_system_without_links_fails_the_commit_closed() {
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &bound_plan(&d), false).unwrap();
        // The default policy is the journal's: links are required.
        assert!(prepared.require_links);
        *ops.link_error.lock().unwrap() = Some(io::ErrorKind::Unsupported);
        let error = commit_with(&ops, &prepared).unwrap_err();
        assert!(error.step.contains("link"), "{}", error.step);
        assert!(!error.is_changed_elsewhere());
        assert_eq!(read(&d.installed), "old");
        assert_eq!(read(&prepared.new), "new");
        assert!(ops.renames.lock().unwrap().is_empty());
    }

    /// A plan with no journal: it expects "old" and replaces a mismatched copy.
    fn unjournaled_plan(d: &Dir) -> SwitchPlan {
        SwitchPlan {
            keep_policy: KeepPolicy::ReplaceMismatched,
            ..bound_plan(d)
        }
    }

    // Not upstream's (U2-P2): with no journal a file system without links
    // still switches, as at 5b17f01, and the hash just before the rename
    // still runs.
    #[test]
    fn without_a_journal_a_file_system_without_links_still_switches() {
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &unjournaled_plan(&d), false).unwrap();
        assert!(!prepared.require_links);
        *ops.link_error.lock().unwrap() = Some(io::ErrorKind::Unsupported);
        assert_eq!(
            commit_with(&ops, &prepared).unwrap(),
            SwitchOutcome::RestartNeeded
        );
        assert_eq!(read(&d.installed), "new");
        assert!(prev_links(&d).is_empty(), "no .prev is created or left");

        // A file replaced before the first hash is refused, nothing renamed.
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &unjournaled_plan(&d), false).unwrap();
        *ops.link_error.lock().unwrap() = Some(io::ErrorKind::Unsupported);
        fs::write(&d.installed, "foreign").unwrap();
        assert!(
            commit_with(&ops, &prepared)
                .unwrap_err()
                .is_changed_elsewhere()
        );
        assert!(ops.renames.lock().unwrap().is_empty());

        // So is one replaced just before the rename (the link attempt).
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &unjournaled_plan(&d), false).unwrap();
        *ops.link_error.lock().unwrap() = Some(io::ErrorKind::Unsupported);
        let installed = d.installed.clone();
        *ops.before_link.lock().unwrap() =
            Some(Box::new(move |_| replace_with_foreign(&installed)));
        assert!(
            commit_with(&ops, &prepared)
                .unwrap_err()
                .is_changed_elsewhere()
        );
        assert_eq!(read(&d.installed), "foreign");
        assert!(ops.renames.lock().unwrap().is_empty());
    }

    // Not upstream's (U2-P2): with no journal only an unsupported link is
    // skipped; any other link error still stops the switch.
    #[test]
    fn without_a_journal_another_link_error_still_fails() {
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &unjournaled_plan(&d), false).unwrap();
        *ops.link_error.lock().unwrap() = Some(io::ErrorKind::OutOfMemory);
        let error = commit_with(&ops, &prepared).unwrap_err();
        assert!(error.step.contains("link"), "{}", error.step);
        assert_eq!(read(&d.installed), "old");
        assert!(ops.renames.lock().unwrap().is_empty());
    }

    // Not upstream's (U2-P5): the stale-link cleanup removes a link that holds
    // the installed file's bytes, or those of a verified kept copy, and
    // leaves one that matches neither, or whose name isn't ours.
    #[test]
    fn stale_prev_links_are_removed_only_when_their_bytes_are_known() {
        let d = dir();
        let beside = |name: &str| d.installed.with_file_name(name);
        let same_as_installed = beside("open-ferry.prev.1");
        let same_as_kept = beside("open-ferry.prev.1.7");
        let unknown = beside("open-ferry.prev.2");
        let not_ours = [
            beside("open-ferry.prev.x"),
            beside("open-ferry.prev."),
            beside("open-ferry.prev.1.2.3"),
            beside("open-ferry.previous.1"),
        ];
        fs::write(&same_as_installed, "old").unwrap();
        fs::write(&same_as_kept, "kept").unwrap();
        fs::write(&unknown, "only copy").unwrap();
        for path in &not_ours {
            fs::write(path, "old").unwrap();
        }
        let kept_hash = {
            let copy = beside("kept-copy");
            fs::write(&copy, "kept").unwrap();
            hash(&copy)
        };
        let ops = Ops::default();
        let done = cleanup_stale_prev_with(&ops, &d.installed, &[kept_hash]).unwrap();
        assert_eq!(
            done.removed,
            [same_as_installed.clone(), same_as_kept.clone()]
        );
        assert_eq!(done.kept.as_slice(), std::slice::from_ref(&unknown));
        assert!(!same_as_installed.exists() && !same_as_kept.exists());
        assert_eq!(read(&unknown), "only copy");
        for path in &not_ours {
            assert_eq!(read(path), "old");
        }
        assert!(
            ops.operations
                .lock()
                .unwrap()
                .contains(&("sync".into(), d.installed.clone()))
        );
        // A second run finds only the unknown one, and still keeps it.
        let again = cleanup_stale_prev_with(&ops, &d.installed, &[]).unwrap();
        assert!(again.removed.is_empty());
        assert_eq!(again.kept.as_slice(), std::slice::from_ref(&unknown));
        assert_eq!(read(&unknown), "only copy");
    }

    // Not upstream's (U2-P5): an installed path that is a symlink to a `.prev`
    // file hashes as that file, but the file is its only copy and stays; the
    // installed link keeps working.
    #[cfg(unix)]
    #[test]
    fn a_prev_file_the_installed_path_leads_to_is_never_removed() {
        let d = dir();
        let target = d.installed.with_file_name("open-ferry.prev.1");
        fs::remove_file(&d.installed).unwrap();
        fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink(&target, &d.installed).unwrap();
        // An ordinary hard link of the same bytes is still removable.
        let twin = d.installed.with_file_name("open-ferry.prev.2");
        fs::hard_link(&target, &twin).unwrap();
        let done =
            cleanup_stale_prev_with(&Ops::default(), &d.installed, &[hash(&target)]).unwrap();
        assert_eq!(done.removed.as_slice(), std::slice::from_ref(&twin));
        assert_eq!(done.kept.as_slice(), std::slice::from_ref(&target));
        assert_eq!(read(&target), "old");
        assert_eq!(read(&d.installed), "old");
        assert!(fs::symlink_metadata(&d.installed).unwrap().is_symlink());
    }

    // Not upstream's (U2-P5): with the installed file missing a link equal to
    // nothing verified is the only copy, and stays.
    #[test]
    fn a_stale_prev_link_is_kept_while_the_installed_file_is_missing() {
        let d = dir();
        let link = d.installed.with_file_name("open-ferry.prev.1");
        fs::write(&link, "old").unwrap();
        fs::remove_file(&d.installed).unwrap();
        let done = cleanup_stale_prev_with(&Ops::default(), &d.installed, &[]).unwrap();
        assert!(done.removed.is_empty());
        assert_eq!(read(&link), "old");
        let verified = [hash(&link)];
        let done = cleanup_stale_prev_with(&Ops::default(), &d.installed, &verified).unwrap();
        assert_eq!(done.removed.as_slice(), std::slice::from_ref(&link));
    }

    // No sleeps: length, timestamp and file type changes are forced directly.
    #[test]
    fn prev_cleanup_plan_keeps_changed_links() {
        for change in ["length", "modified", "directory", "missing"] {
            let d = dir();
            let link = d.installed.with_file_name("open-ferry.prev.1");
            fs::write(&link, "old").unwrap();
            let plan = plan_stale_prev(&d.installed, &[]);
            assert!(
                plan.links[0].1.is_ok(),
                "{change}: must be removable initially"
            );
            let modified = fs::metadata(&link).unwrap().modified().unwrap();
            match change {
                "length" => {
                    fs::write(&link, "foreign contents").unwrap();
                    fs::File::options()
                        .write(true)
                        .open(&link)
                        .unwrap()
                        .set_modified(modified)
                        .unwrap();
                }
                "modified" => {
                    fs::write(&link, "new").unwrap();
                    fs::File::options()
                        .write(true)
                        .open(&link)
                        .unwrap()
                        .set_modified(modified + Duration::from_secs(60))
                        .unwrap();
                }
                "directory" => {
                    fs::remove_file(&link).unwrap();
                    fs::create_dir(&link).unwrap();
                }
                "missing" => fs::remove_file(&link).unwrap(),
                _ => unreachable!(),
            }
            let ops = Ops::default();
            let done = apply_stale_prev_with(&ops, plan).unwrap();
            assert!(done.removed.is_empty(), "{change}");
            assert_eq!(
                done.kept.as_slice(),
                std::slice::from_ref(&link),
                "{change}"
            );
            assert!(
                ops.operations.lock().unwrap().is_empty(),
                "no directory sync: {change}"
            );
            match change {
                "length" => assert_eq!(read(&link), "foreign contents"),
                "modified" => assert_eq!(read(&link), "new"),
                "directory" => assert!(link.is_dir()),
                "missing" => assert!(!link.exists()),
                _ => unreachable!(),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn prev_cleanup_plan_keeps_a_replaced_inode_with_matching_size_and_time() {
        let d = dir();
        let link = d.installed.with_file_name("open-ferry.prev.1");
        fs::write(&link, "old").unwrap();
        let modified = fs::metadata(&link).unwrap().modified().unwrap();
        let plan = plan_stale_prev(&d.installed, &[]);
        // Retain the old inode so the replacement cannot reuse it.
        fs::rename(&link, link.with_extension("saved")).unwrap();
        fs::write(&link, "new").unwrap();
        fs::File::options()
            .write(true)
            .open(&link)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let done = apply_stale_prev(plan).unwrap();
        assert!(done.removed.is_empty());
        assert_eq!(done.kept.as_slice(), std::slice::from_ref(&link));
        assert_eq!(read(&link), "new");
    }

    #[test]
    fn prev_cleanup_plan_preserves_safety_and_sync_errors() {
        let d = dir();
        let link = d.installed.with_file_name("open-ferry.prev.1");
        let unknown = d.installed.with_file_name("open-ferry.prev.2");
        let directory = d.installed.with_file_name("open-ferry.prev.3");
        fs::hard_link(&d.installed, &link).unwrap();
        fs::write(&unknown, "only copy").unwrap();
        fs::create_dir(&directory).unwrap();
        let plan = plan_stale_prev(&d.installed, &[]);
        assert!(link.exists(), "planning must not delete");
        let ops = Ops::default();
        *ops.fail_sync.lock().unwrap() = Some(d.installed.clone());
        let error = apply_stale_prev_with(&ops, plan).unwrap_err();
        assert!(error.to_string().contains("injected sync failure"));
        assert!(!link.exists(), "the completed unlink is not undone");
        assert_eq!(read(&unknown), "only copy");
        assert!(directory.is_dir());
        let done = apply_stale_prev_with(&ops, plan_stale_prev(&d.installed, &[])).unwrap();
        assert!(done.removed.is_empty(), "kept files alone do not sync");
        assert_eq!(done.kept, [unknown, directory]);
    }

    // Not upstream's (O3, aside path): what is moved aside is hashed, and a
    // file that isn't the classified one goes back, without the new binary.
    #[test]
    fn an_aside_that_isnt_the_classified_binary_is_put_back() {
        let d = dir();
        let ops = Ops::default();
        let prepared = prepare_with(&ops, &bound_plan(&d), true).unwrap();
        fs::write(&d.installed, "foreign").unwrap();
        let error = commit_with(&ops, &prepared).unwrap_err();
        assert!(error.is_changed_elsewhere(), "{error}");
        assert_eq!(read(&d.installed), "foreign");
        assert!(!d.aside.exists());
        assert_eq!(read(&prepared.new), "new");

        // A good one is replaced as before.
        let d = dir();
        let prepared = prepare_with(&ops, &bound_plan(&d), true).unwrap();
        commit_with(&ops, &prepared).unwrap();
        assert_eq!(read(&d.installed), "new");
    }

    // Not upstream's (O3, aside path): if the foreign file can't be put back
    // the error says where it is, and is not reported as changed-elsewhere.
    #[test]
    fn an_aside_that_cant_be_put_back_is_named() {
        let d = dir();
        let prepared = prepare_with(&Ops::default(), &bound_plan(&d), true).unwrap();
        fs::write(&d.installed, "foreign").unwrap();
        let ops = Ops::failing(&d.installed, usize::MAX, false);
        let error = commit_with(&ops, &prepared).unwrap_err();
        assert!(!error.is_changed_elsewhere());
        let text = error.to_string();
        for path in [&d.installed, &d.aside] {
            assert!(text.contains(&path.display().to_string()), "{text}");
        }
        assert_eq!(read(&d.aside), "foreign");
    }

    // Not upstream's (O3): the marker is for that failure only.
    #[test]
    fn a_switch_error_says_changed_elsewhere_only_for_that_failure() {
        let plain = SwitchError {
            step: "x",
            error: io::Error::other("y"),
        };
        assert!(!plain.is_changed_elsewhere());
        let marked = SwitchError {
            step: "x",
            error: changed_elsewhere("z".into()),
        };
        assert!(marked.is_changed_elsewhere());
        assert_eq!(KeepPolicy::default(), KeepPolicy::Immutable);
    }

    // Not upstream's (Opus 2): with no journal a mismatched kept copy is
    // replaced, atomically and synced, and the switch goes on; with the
    // default policy it is refused and left alone.
    #[test]
    fn a_mismatched_kept_copy_is_replaced_when_the_policy_says_so() {
        let d = dir();
        let keep = d.installed.with_file_name("kept");
        fs::write(&keep, "from an outside reinstall").unwrap();
        let ops = Ops::default();
        let replacing = SwitchPlan {
            keep_policy: KeepPolicy::ReplaceMismatched,
            ..plan(&d, Some(keep.clone()))
        };
        let prepared = prepare_with(&ops, &replacing, false).unwrap();
        assert_eq!(read(&keep), "old");
        assert!(
            ops.operations
                .lock()
                .unwrap()
                .contains(&("sync".into(), keep.clone()))
        );
        commit_with(&ops, &prepared).unwrap();
        assert_eq!(read(&d.installed), "new");
        assert!(names(&d).iter().all(|name| !name.contains(".partial.")));

        fs::write(&keep, "mine").unwrap();
        fs::write(&d.installed, "old").unwrap();
        let error = prepare_with(&ops, &plan(&d, Some(keep.clone())), false).unwrap_err();
        assert_eq!(error.error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&keep), "mine");

        // A replacement whose source isn't what was classified is dropped.
        let stale = SwitchPlan {
            expected_installed_sha256: Some(hash(&d.new)),
            ..replacing
        };
        assert!(
            prepare_with(&ops, &stale, false)
                .unwrap_err()
                .is_changed_elsewhere()
        );
        assert_eq!(read(&keep), "mine");
    }

    // Not upstream's (Opus 4): a missing installed file is restored from the
    // aside the journal names, not the newest `.old`, and that aside is
    // protected from the cleanup while the installed file is there.
    #[test]
    fn the_preferred_aside_is_restored_first_and_then_protected() {
        let d = dir();
        let journal = d.installed.with_file_name("open-ferry.old");
        let newer = d.installed.with_file_name("open-ferry.old.1");
        fs::remove_file(&d.installed).unwrap();
        fs::write(&journal, "journal").unwrap();
        fs::write(&newer, "newer").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&journal).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        drop(file);
        let ops = Ops::default();
        let preferring = SwitchPlan {
            preferred_aside: Some(journal.clone()),
            ..plan(&d, None)
        };
        let prepared = prepare_with(&ops, &preferring, true).unwrap();
        assert_eq!(read(&d.installed), "journal");
        assert_eq!(
            ops.renames.lock().unwrap().front(),
            Some(&(journal.clone(), d.installed.clone()))
        );
        // Not touched before the restore; removed after it.
        assert!(!newer.exists());
        prepared.discard();

        // With the installed file there, the journal's aside survives.
        let running = d.installed.with_file_name("open-ferry.old.2");
        let other = d.installed.with_file_name("open-ferry.old.3");
        fs::write(&running, "still running").unwrap();
        fs::write(&other, "stale").unwrap();
        let preferring = SwitchPlan {
            preferred_aside: Some(running.clone()),
            ..preferring
        };
        let prepared = prepare_with(&ops, &preferring, true).unwrap();
        assert_eq!(read(&running), "still running");
        assert!(!other.exists());
        assert_ne!(prepared.aside.as_deref(), Some(running.as_path()));
        prepared.discard();

        // Without a preferred aside the newest `.old` is what comes back.
        fs::remove_file(&d.installed).unwrap();
        let none = SwitchPlan {
            preferred_aside: None,
            ..preferring
        };
        prepare_with(&ops, &none, true).unwrap();
        assert_eq!(read(&d.installed), "still running");
    }

    // Not upstream's (Opus 13): on Unix a permission error is real and is
    // not retried.
    #[cfg(unix)]
    #[test]
    fn a_permission_error_on_unix_is_not_retried() {
        let d = dir();
        let ops = Ops::failing(&d.aside, 1, true);
        let error = replace_running_exe_with(&ops, &d.installed, &d.new, &d.aside).unwrap_err();
        assert_eq!(error.step, "move the installed binary aside");
        assert!(ops.slept().is_empty());
        assert_eq!(read(&d.installed), "old");
    }

    // Not upstream's: which errors are worth another try.
    #[test]
    fn only_a_lock_like_error_is_transient() {
        // Access denied is a passing lock on Windows, and a real error on
        // Unix, where retrying it would only cost the waits.
        assert_eq!(
            is_transient(&io::ErrorKind::PermissionDenied.into()),
            cfg!(windows)
        );
        assert!(!is_transient(&io::Error::other("no")));
        assert!(!is_transient(&io::ErrorKind::NotFound.into()));
        if cfg!(windows) {
            assert!(is_transient(&io::Error::from_raw_os_error(32)));
            assert!(is_transient(&io::Error::from_raw_os_error(33)));
        }
    }
}
