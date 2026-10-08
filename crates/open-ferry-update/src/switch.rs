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
//! A switch doesn't restart anything: the running server goes on with the
//! old binary until it is restarted.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
    pub keep_installed_at: Option<PathBuf>,
}

/// What a switch leaves to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchOutcome {
    /// The binary is replaced; open-ferry runs it once restarted.
    RestartNeeded,
}

/// Why a switch failed; the installed binary is as it was.
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

/// Puts a version in place of the installed binary.
pub trait Switch: Send + Sync {
    /// Makes the switch `plan` describes.
    fn switch(&self, plan: &SwitchPlan) -> Result<SwitchOutcome, SwitchError>;
}

/// Replaces the binary on disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReplaceOnDisk;

fn step(step: &'static str) -> impl FnOnce(io::Error) -> SwitchError {
    move |error| SwitchError { step, error }
}

/// Copies `from` to `to` through a temporary file, so `to` is whole or
/// absent.
fn copy_whole(from: &Path, to: &Path) -> io::Result<()> {
    let dir = to.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let temp = sibling(to, &format!(".partial.{}", std::process::id()));
    fs::copy(from, &temp)?;
    fs::rename(&temp, to).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// `path` with `suffix` added to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Makes `path` executable by everyone, as the installers do.
#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

impl Switch for ReplaceOnDisk {
    fn switch(&self, plan: &SwitchPlan) -> Result<SwitchOutcome, SwitchError> {
        if let Some(keep) = &plan.keep_installed_at {
            copy_whole(&plan.installed, keep)
                .map_err(step("keep a copy of the installed binary"))?;
        }
        let new = sibling(&plan.installed, &format!(".new.{}", std::process::id()));
        let staged = fs::copy(&plan.new_binary, &new)
            .and_then(|_| make_executable(&new))
            .map_err(step("copy the new binary beside the installed one"));
        if let Err(error) = staged {
            let _ = fs::remove_file(&new);
            return Err(error);
        }
        let placed = if cfg!(windows) {
            replace_running_exe(&plan.installed, &new)
        } else {
            fs::rename(&new, &plan.installed).map_err(step("put the new binary in place"))
        };
        if placed.is_err() {
            let _ = fs::remove_file(&new);
        }
        placed.map(|()| SwitchOutcome::RestartNeeded)
    }
}

/// Windows: renames the installed binary aside, then the new one into its
/// place, renaming the old one back if that fails.
fn replace_running_exe(installed: &Path, new: &Path) -> Result<(), SwitchError> {
    remove_old_copies(installed);
    let aside = (0..100)
        .map(|n| match n {
            0 => sibling(installed, ".old"),
            n => sibling(installed, &format!(".old.{n}")),
        })
        .find(|path| !path.exists())
        .ok_or_else(|| SwitchError {
            step: "find a name to move the installed binary aside to",
            error: io::Error::other("too many .old copies"),
        })?;
    fs::rename(installed, &aside).map_err(step("move the installed binary aside"))?;
    if let Err(error) = fs::rename(new, installed) {
        let _ = fs::rename(&aside, installed);
        return Err(SwitchError {
            step: "put the new binary in place",
            error,
        });
    }
    // Removed now if nothing runs it; else at the next switch.
    let _ = fs::remove_file(&aside);
    Ok(())
}

/// Removes the `.old` copies earlier switches left beside `installed`
/// that are no longer in use.
fn remove_old_copies(installed: &Path) {
    let (Some(dir), Some(name)) = (installed.parent(), installed.file_name()) else {
        return;
    };
    let prefix = format!("{}.old", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let suffix = file_name.strip_prefix(&prefix);
        let is_old = suffix.is_some_and(|rest| {
            rest.is_empty()
                || rest
                    .strip_prefix('.')
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        });
        if is_old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Removes the `.old` copies a switch left beside `installed` (Windows
/// keeps one while the old binary still runs).
pub fn clean_up_after_switch(installed: &Path) {
    remove_old_copies(installed);
}
