//! Whether this install may replace its own binary.
//!
//! Only a binary `install.sh` or `install.ps1` put there updates itself:
//! the install receipt must name the running binary, the binary must be
//! called `open-ferry`, its directory must be writable, and open-ferry
//! mustn't run in a container. Anything else (a package manager's copy, a
//! container, another copy of the binary, `open-ferry migrate`'s drop-in
//! copy under CLIProxyAPI's name, a directory open-ferry can't write) is
//! told that a release is out and how to get it, and nothing is replaced.
//!
//! A container is recognized by `/.dockerenv` (Docker), `/run/.containerenv`
//! (Podman), a non-empty `container` variable (systemd-nspawn, Podman,
//! LXC's init) or `KUBERNETES_SERVICE_HOST` (a Kubernetes pod).

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::receipt::{Receipt, ReceiptError};

/// What updates need of the machine, behind a trait so tests can say.
pub trait System: Send + Sync {
    /// The running binary.
    fn current_exe(&self) -> io::Result<PathBuf>;
    /// `path` with links resolved.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    /// Whether open-ferry runs in a container.
    fn in_container(&self) -> bool;
    /// Whether open-ferry can write in `dir`.
    fn dir_writable(&self, dir: &Path) -> bool;
}

/// This machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealSystem {
    /// Whether [`System::dir_writable`] writes a file to find out, rather
    /// than going by the directory's permissions, which can be wrong.
    pub write_probe: bool,
}

impl System for RealSystem {
    fn current_exe(&self) -> io::Result<PathBuf> {
        std::env::current_exe()
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }

    fn in_container(&self) -> bool {
        detect_container(Path::exists, |name| std::env::var_os(name))
    }

    fn dir_writable(&self, dir: &Path) -> bool {
        if !self.write_probe {
            return fs::metadata(dir).is_ok_and(|metadata| !metadata.permissions().readonly());
        }
        let probe = dir.join(format!(".open-ferry-write-test.{}", std::process::id()));
        match create_write_probe(&probe) {
            Ok(file) => {
                drop(file);
                let _ = fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }
}

fn create_write_probe(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Whether the files `exists` finds and the variables `var` gives say
/// this is a container.
pub fn detect_container(
    exists: impl Fn(&Path) -> bool,
    var: impl Fn(&str) -> Option<OsString>,
) -> bool {
    let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    exists(Path::new("/.dockerenv"))
        || exists(Path::new("/run/.containerenv"))
        || set("container")
        || set("KUBERNETES_SERVICE_HOST")
}

/// Whether the install updates itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// It does: the binary to replace.
    SelfUpdating {
        /// The running binary, with links resolved.
        binary: PathBuf,
    },
    /// It is only told of releases, for this reason.
    NotifyOnly(NotSelfUpdating),
}

impl Install {
    /// Whether the install updates itself.
    pub fn can_update_itself(&self) -> bool {
        matches!(self, Self::SelfUpdating { .. })
    }

    /// Why it doesn't, when it doesn't.
    pub fn why_not(&self) -> Option<&NotSelfUpdating> {
        match self {
            Self::SelfUpdating { .. } => None,
            Self::NotifyOnly(why) => Some(why),
        }
    }
}

/// Why an install doesn't update itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotSelfUpdating {
    /// open-ferry runs in a container.
    Container,
    /// No install receipt: not installed by the install scripts.
    NoReceipt,
    /// The receipt can't be read.
    BadReceipt(String),
    /// The running binary can't be found.
    UnknownBinary(String),
    /// The binary isn't named `open-ferry`.
    OtherName(String),
    /// The receipt names another binary.
    OtherBinary {
        /// The binary the receipt names.
        installed: String,
        /// The running one.
        running: String,
    },
    /// The binary's directory can't be written.
    ReadOnly(String),
}

impl NotSelfUpdating {
    /// A short code for the status.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::NoReceipt => "no-receipt",
            Self::BadReceipt(_) => "bad-receipt",
            Self::UnknownBinary(_) => "unknown-binary",
            Self::OtherName(_) => "other-name",
            Self::OtherBinary { .. } => "other-binary",
            Self::ReadOnly(_) => "read-only",
        }
    }
}

impl fmt::Display for NotSelfUpdating {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Container => f.write_str(
                "open-ferry runs in a container; update the image (docker pull) instead",
            ),
            Self::NoReceipt => f.write_str(
                "this binary wasn't installed by install.sh or install.ps1 (there is no install receipt); update it the way it was installed",
            ),
            Self::BadReceipt(message) => write!(f, "{message}; run the installer again"),
            Self::UnknownBinary(message) => {
                write!(f, "the running binary can't be found: {message}")
            }
            Self::OtherName(name) => write!(
                f,
                "this binary is named {name}, not open-ferry (a copy put in another program's place); update it the way it was put there"
            ),
            Self::OtherBinary { installed, running } => write!(
                f,
                "the install receipt names {installed}, not this binary ({running}); update this copy the way it was installed"
            ),
            Self::ReadOnly(dir) => write!(f, "open-ferry can't write to {dir}"),
        }
    }
}

/// Whether the running binary updates itself, given its receipt.
pub fn assess(system: &dyn System, receipt: Result<Option<Receipt>, ReceiptError>) -> Install {
    assess_with(system, receipt, || system.current_exe())
}

/// Whether the install updates itself, taking `binary` as the running
/// binary instead of [`System::current_exe`]. A supervised worker runs from
/// `versions/<v>/`, so its own path would never match the receipt; the
/// installed path stands for it, and every other check applies to that.
///
/// Not upstream's: nothing here is a port.
pub fn assess_for(
    system: &dyn System,
    receipt: Result<Option<Receipt>, ReceiptError>,
    binary: &Path,
) -> Install {
    assess_with(system, receipt, || Ok(binary.to_path_buf()))
}

fn assess_with(
    system: &dyn System,
    receipt: Result<Option<Receipt>, ReceiptError>,
    running: impl FnOnce() -> io::Result<PathBuf>,
) -> Install {
    let not = Install::NotifyOnly;
    if system.in_container() {
        return not(NotSelfUpdating::Container);
    }
    let receipt = match receipt {
        Ok(Some(receipt)) => receipt,
        Ok(None) => return not(NotSelfUpdating::NoReceipt),
        Err(error) => return not(NotSelfUpdating::BadReceipt(error.to_string())),
    };
    let running = match running().and_then(|exe| system.canonicalize(&exe)) {
        Ok(running) => running,
        Err(error) => return not(NotSelfUpdating::UnknownBinary(error.to_string())),
    };
    let stem = running
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !stem.eq_ignore_ascii_case("open-ferry") {
        let name = running
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        return not(NotSelfUpdating::OtherName(name));
    }
    let installed = system.canonicalize(Path::new(&receipt.binary));
    if installed.as_ref().ok() != Some(&running) {
        return not(NotSelfUpdating::OtherBinary {
            installed: receipt.binary,
            running: running.display().to_string(),
        });
    }
    let dir = running.parent().map(Path::to_path_buf).unwrap_or_default();
    if !system.dir_writable(&dir) {
        return not(NotSelfUpdating::ReadOnly(dir.display().to_string()));
    }
    // The file itself, so a link to it stays a link.
    Install::SelfUpdating { binary: running }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_write_probe_is_private_at_creation_with_umask_zero() {
        use crate::journal::creation_tests::{assert_mode, with_umask_zero};
        with_umask_zero(
            "install::tests::the_write_probe_is_private_at_creation_with_umask_zero",
            |root| {
                let probe = root.join("probe");
                let _file = create_write_probe(&probe).unwrap();
                assert_mode(&probe, 0o600);
                assert!(RealSystem { write_probe: true }.dir_writable(root));
                assert!(
                    !root
                        .join(format!(".open-ferry-write-test.{}", std::process::id()))
                        .exists()
                );
            },
        );
    }

    /// A machine whose running binary is somewhere that matches nothing.
    struct Fake {
        exe: PathBuf,
        container: bool,
        writable: bool,
    }

    impl System for Fake {
        fn current_exe(&self) -> io::Result<PathBuf> {
            Ok(self.exe.clone())
        }
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            if path.file_name().is_some_and(|name| name == "gone") {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            Ok(path.to_path_buf())
        }
        fn in_container(&self) -> bool {
            self.container
        }
        fn dir_writable(&self, _dir: &Path) -> bool {
            self.writable
        }
    }

    fn receipt(binary: &str) -> Result<Option<Receipt>, ReceiptError> {
        Ok(Some(Receipt {
            format: 1,
            binary: binary.into(),
            ..Receipt::default()
        }))
    }

    fn fake(exe: &str) -> Fake {
        Fake {
            exe: PathBuf::from(exe),
            container: false,
            writable: true,
        }
    }

    // Not upstream's: a worker runs from versions/<v>/, so it is judged by
    // the installed path, not by `current_exe`.
    #[test]
    fn assess_for_judges_the_given_binary_not_the_running_one() {
        let worker = fake("/data/versions/0.2.0/open-ferry");
        let installed = Path::new("/bin/open-ferry");
        assert_eq!(
            assess(&worker, receipt("/bin/open-ferry")),
            Install::NotifyOnly(NotSelfUpdating::OtherBinary {
                installed: "/bin/open-ferry".into(),
                running: "/data/versions/0.2.0/open-ferry".into(),
            })
        );
        assert_eq!(
            assess_for(&worker, receipt("/bin/open-ferry"), installed),
            Install::SelfUpdating {
                binary: installed.to_path_buf()
            }
        );
    }

    // Not upstream's: every other check applies to the given binary.
    #[test]
    fn assess_for_applies_the_other_checks() {
        let installed = Path::new("/bin/open-ferry");
        let not = |system: Fake, receipt, binary: &Path| {
            assess_for(&system, receipt, binary)
                .why_not()
                .map(NotSelfUpdating::code)
        };
        let mut container = fake("/x");
        container.container = true;
        assert_eq!(
            not(container, receipt("/bin/open-ferry"), installed),
            Some("container")
        );
        assert_eq!(not(fake("/x"), Ok(None), installed), Some("no-receipt"));
        assert_eq!(
            not(
                fake("/x"),
                Err(ReceiptError::Malformed("bad".into())),
                installed
            ),
            Some("bad-receipt")
        );
        assert_eq!(
            not(
                fake("/x"),
                receipt("/bin/open-ferry"),
                Path::new("/bin/gone")
            ),
            Some("unknown-binary")
        );
        assert_eq!(
            not(
                fake("/x"),
                receipt("/bin/other"),
                Path::new("/bin/ferry-copy")
            ),
            Some("other-name")
        );
        assert_eq!(
            not(fake("/x"), receipt("/bin/other"), installed),
            Some("other-binary")
        );
        let mut read_only = fake("/x");
        read_only.writable = false;
        assert_eq!(
            not(read_only, receipt("/bin/open-ferry"), installed),
            Some("read-only")
        );
    }
}
