//! Whose update records a command uses: its account's, or the machine's.
//!
//! An account's records are in its data directory (`DataDir::for_this_user`,
//! from `LOCALAPPDATA`, `XDG_DATA_HOME` or `HOME`). A machine install's are
//! in the machine data directory: `/var/lib/open-ferry` on Linux, and
//! `open-ferry` in the ProgramData known folder on Windows, made only by
//! `install-self -system` ([`provision`]). macOS has none in this version.
//!
//! A privileged process (Unix: an effective uid of 0; Windows: an elevated
//! token, or SYSTEM) runs the binaries the records name and changes what they
//! point at, so it uses a data directory only when nobody but root or the
//! administrators can change it or anything in it ([`authorize`]). Otherwise
//! it refuses, saying why and what to do; it never takes such a directory for
//! an empty one. [`lookup`] finds the records for a binary, and whatever
//! writes, locks or stops through them calls [`Home::require_privilege`]
//! first.
//!
//! Paths are strings handled by [`Platform`], and everything this module asks
//! of the machine goes through a [`Probe`], so every branch is tested on any
//! system.
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

#[cfg(windows)]
use open_ferry_supervisor::endpoint::Identity;
use open_ferry_supervisor::endpoint::ServiceKind;
use open_ferry_supervisor::trust;
use open_ferry_update::DataDir;
use open_ferry_update::data_dir::RECEIPT_FILE;
use open_ferry_update::receipt::{Receipt, ReceiptError};

use crate::os_service::{self, Platform, System as _};

/// The machine data directory on Linux.
const LINUX_MACHINE_DIR: &str = "/var/lib/open-ferry";
/// The machine config on Linux, where `install.sh --system` writes it.
const LINUX_MACHINE_CONFIG: &str = "/etc/open-ferry/config.yaml";
/// The machine data directory's name in ProgramData.
const MACHINE_DIR_NAME: &str = "open-ferry";
/// The auth directory's name in the machine data directory.
const AUTH_DIR: &str = "auth";
/// The machine config's name in the protected machine data directory on Windows.
const CONFIG_FILE: &str = "config.yaml";
/// LocalSystem's SID.
#[cfg(windows)]
const SYSTEM_SID: &str = "S-1-5-18";

/// The files that tell a container, for [`Probe::in_container`].
const CONTAINER_MARKERS: [&str; 3] = [
    "/.dockerenv",
    "/run/.containerenv",
    "/var/run/secrets/kubernetes.io",
];

/// The most entries [`authorize`] looks at below a data directory.
pub(crate) const MAX_ENTRIES: usize = 10_000;
/// The most levels [`authorize`] goes below a data directory.
pub(crate) const MAX_DEPTH: usize = 16;

/// Whose records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Those of the account the command runs as.
    Account,
    /// The machine's.
    Machine,
}

/// The update records a command uses, as [`lookup`] or [`provision`] found
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Home {
    /// Whose they are.
    pub(crate) scope: Scope,
    /// Where they are: the path [`authorize`] checked, for a privileged
    /// process.
    pub(crate) data: DataDir,
    /// The supervisor to expect: a user one for an account's records, the
    /// system one for the machine's.
    pub(crate) kind: ServiceKind,
    /// Whether this process is privileged.
    pub(crate) privileged: bool,
    /// What to tell an unprivileged process that can't read the machine data
    /// directory, runs a binary only root or the administrators can change,
    /// and has no install of it in its own records.
    pub(crate) machine_hint: Option<String>,
    platform: Platform,
}

impl Home {
    fn new(platform: Platform, scope: Scope, root: String, privileged: bool) -> Home {
        Home {
            scope,
            data: DataDir::at(root),
            kind: match scope {
                Scope::Account => ServiceKind::User,
                Scope::Machine => ServiceKind::System,
            },
            privileged,
            machine_hint: None,
            platform,
        }
    }

    /// Refuses unless this process may change the records: anyone its
    /// account's (a privileged process's own were authorized), only a
    /// privileged process the machine's. Whatever writes, locks or stops
    /// through them calls it first; reading needs no privilege.
    pub(crate) fn require_privilege(&self) -> Result<(), Refusal> {
        if self.scope == Scope::Machine && !self.privileged {
            return Err(needs_privilege(
                self.platform,
                "open-ferry is installed for the whole machine, so changing it",
            ));
        }
        Ok(())
    }
}

/// Why the records can't be used, and what to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// What is wrong.
    pub(crate) reason: String,
    /// What to do, as a sentence, when there is something to do.
    pub(crate) remedy: Option<String>,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.remedy {
            Some(remedy) => write!(f, "{}. {remedy}", self.reason),
            None => f.write_str(&self.reason),
        }
    }
}

/// What is at a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// Nothing.
    Missing,
    /// A directory this process can list.
    Directory,
    /// A regular file.
    File,
    /// A link, or anything else that is neither a directory nor a regular
    /// file.
    Other,
    /// Something that can't be looked at or listed: why.
    Inaccessible(String),
}

/// What this module asks of the machine; [`Host`]'s answers are real.
pub(crate) trait Probe {
    /// The platform whose path rules apply.
    fn platform(&self) -> Platform;
    /// Whether this process is privileged: Unix an effective uid of 0;
    /// Windows an elevated token, or SYSTEM.
    fn privileged(&self) -> Result<bool, String>;
    /// The account's data directory (`DataDir::for_this_user`).
    fn account_dir(&self) -> Result<String, String>;
    /// The ProgramData known folder (Windows).
    fn program_data(&self) -> Result<String, String>;
    /// What is at `path`, links not followed.
    fn state(&self, path: &str) -> State;
    /// The names in the directory `dir`: at most `limit` of them.
    fn entries(&self, dir: &str, limit: usize) -> io::Result<Vec<String>>;
    /// Refuses the directory `path` unless it and every directory above it
    /// pass the system service's trust check; a link anywhere refuses.
    fn trusted_dir(&self, path: &str) -> Result<(), String>;
    /// Refuses `path` unless it is a regular file, not a link, that passes
    /// the system service's trust check with every directory above it.
    fn trusted_file(&self, path: &str) -> Result<(), String>;
    /// Opens `path` read-only without following a link or waiting on a special
    /// file. Permission denied distinguishes an unreadable receipt from a
    /// readable one the trust check refuses; success does not imply trust.
    fn readable_file(&self, path: &str) -> io::Result<()>;
    /// The install receipt at `path`, if there is one.
    fn receipt(&self, path: &str) -> Result<Option<Receipt>, ReceiptError>;
    /// `path` with links resolved.
    fn canonical(&self, path: &str) -> io::Result<String>;
    /// Makes the directory `path`, private to root or SYSTEM and the
    /// administrators; its parent must exist. An existing directory is left
    /// as it is: on Windows that is an `AlreadyExists` error.
    fn create_machine_dir(&mut self, path: &str) -> io::Result<()>;
    /// Whether this is a container, as files only root could have made say:
    /// `/.dockerenv` (Docker), `/run/.containerenv` (Podman) or
    /// `/var/run/secrets/kubernetes.io` (a Kubernetes pod with its service
    /// account). The environment isn't asked, since whoever starts a
    /// privileged process may have chosen it. Never on Windows.
    fn in_container(&self) -> bool {
        false
    }
}

/// This machine.
pub(crate) struct Host;

impl Probe for Host {
    /// Other systems are taken for macOS: no machine installs, and no
    /// privileged use of records, since the trust check is untested there.
    fn platform(&self) -> Platform {
        Platform::HOST.unwrap_or(Platform::MacOs)
    }

    fn privileged(&self) -> Result<bool, String> {
        privileged_process()
    }

    fn account_dir(&self) -> Result<String, String> {
        let data = DataDir::for_this_user().map_err(|error| error.to_string())?;
        data.root()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("{} isn't valid Unicode", data.root().display()))
    }

    fn program_data(&self) -> Result<String, String> {
        program_data()
    }

    fn state(&self, path: &str) -> State {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return State::Missing,
            Err(error) => return State::Inaccessible(error.to_string()),
        };
        let kind = metadata.file_type();
        if kind.is_symlink() {
            State::Other
        } else if kind.is_dir() {
            match fs::read_dir(path) {
                Ok(_) => State::Directory,
                Err(error) => State::Inaccessible(error.to_string()),
            }
        } else if kind.is_file() {
            State::File
        } else {
            State::Other
        }
    }

    fn entries(&self, dir: &str, limit: usize) -> io::Result<Vec<String>> {
        fs::read_dir(dir)?
            .take(limit)
            .map(|entry| {
                entry?.file_name().into_string().map_err(|name| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} isn't valid Unicode", name.display()),
                    )
                })
            })
            .collect()
    }

    fn trusted_dir(&self, path: &str) -> Result<(), String> {
        os_service::Host.trusted_dir(path)
    }

    fn trusted_file(&self, path: &str) -> Result<(), String> {
        os_service::Host.trusted_file(path)
    }

    fn readable_file(&self, path: &str) -> io::Result<()> {
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
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(io::Error::other("not a regular file"));
            }
        }
        if !metadata.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        Ok(())
    }

    fn receipt(&self, path: &str) -> Result<Option<Receipt>, ReceiptError> {
        Receipt::load(Path::new(path))
    }

    fn canonical(&self, path: &str) -> io::Result<String> {
        fs::canonicalize(path)?
            .into_os_string()
            .into_string()
            .map_err(|path| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} isn't valid Unicode", path.display()),
                )
            })
    }

    fn create_machine_dir(&mut self, path: &str) -> io::Result<()> {
        create_machine_dir(Path::new(path))
    }

    fn in_container(&self) -> bool {
        cfg!(unix)
            && CONTAINER_MARKERS
                .iter()
                .any(|path| fs::symlink_metadata(path).is_ok())
    }
}

/// Finds the records for the installed `binary`.
///
/// They are the machine's when the machine data directory exists, can be
/// read, passes the trust check, and holds a receipt (passing it too) that
/// names `binary`, links resolved; a privileged process also needs
/// [`authorize`] to pass for it. Otherwise they are the account's, which a
/// privileged process needs [`authorize`] to pass for; the account's data
/// directory is used as it is given. A machine data directory that can't be
/// read or used refuses a privileged process, unless the account's records
/// pass [`authorize`] and their receipt names `binary`. An unprivileged
/// process may get the machine's records for reading, never the account's
/// for lack of privilege.
pub(crate) fn lookup(probe: &dyn Probe, binary: &str) -> Result<Home, Refusal> {
    let platform = probe.platform();
    let privileged = privileged(probe)?;
    if privileged && platform == Platform::MacOs {
        return Err(macos_refusal());
    }
    let machine = find_machine(probe, binary);
    if let Machine::Ours(root) = machine {
        if privileged {
            check(probe, &root, Whose::Machine)?;
        }
        return Ok(Home::new(platform, Scope::Machine, root, privileged));
    }
    let account = match probe.account_dir() {
        Ok(account) => account,
        Err(reason) => {
            return Err(match machine {
                Machine::Unreadable(refusal) | Machine::Unusable(refusal) if privileged => refusal,
                _ => Refusal {
                    reason,
                    remedy: None,
                },
            });
        }
    };
    let receipt = platform.join(&account, RECEIPT_FILE);
    let mut home = Home::new(platform, Scope::Account, account.clone(), privileged);
    match machine {
        Machine::Unreadable(refusal) | Machine::Unusable(refusal) if privileged => {
            // Only an install of the account's own stands beside a machine
            // data directory that can't be read or used, and its receipt is
            // read only once the account's records pass.
            if check(probe, &account, Whose::Account).is_err()
                || names(probe, &receipt, binary) != Ok(true)
            {
                return Err(refusal);
            }
        }
        Machine::Unreadable(_) => {
            if names(probe, &receipt, binary) != Ok(true) && probe.trusted_file(binary).is_ok() {
                home.machine_hint = Some(machine_hint(platform));
            }
        }
        _ => check(probe, &account, Whose::Account)?,
    }
    Ok(home)
}

/// Makes the machine data directory, or finds the one an earlier or a
/// concurrent install made, and [`authorize`]s it: one somebody else could
/// change is refused, never repaired or adopted. For `install-self -system`
/// and Windows `init -system`; needs privilege.
pub(crate) fn provision(probe: &mut dyn Probe) -> Result<Home, Refusal> {
    let platform = probe.platform();
    if platform == Platform::MacOs {
        return Err(macos_refusal());
    }
    if !privileged(&*probe)? {
        return Err(needs_privilege(
            platform,
            "installing open-ferry for the whole machine",
        ));
    }
    let root = known_machine_root(&*probe)?;
    match probe.create_machine_dir(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(Refusal {
                reason: format!("failed to create {root}: {error}"),
                remedy: None,
            });
        }
    }
    check(&*probe, &root, Whose::Machine)?;
    Ok(Home::new(platform, Scope::Machine, root, true))
}

/// Refuses a privileged process the data directory `root` unless nobody but
/// root or the administrators can change it or anything in it.
///
/// `root`, or while it doesn't exist the nearest directory above it that
/// does, must pass the trust check with every directory above it; and every
/// entry below it, to [`MAX_DEPTH`] levels and [`MAX_ENTRIES`] entries, must
/// be a directory or a regular file that passes it. A link, anything else,
/// anything that can't be looked at, or more than that refuses the whole
/// directory. `root` must be a full path without `.` or `..`, and is used as
/// it is: a link anywhere in it refuses. Every check walks from the top, so
/// only root or the administrators could change anything between two of them.
/// On macOS every privileged process is refused: the trust check doesn't
/// read ACLs yet. Does nothing for an unprivileged process.
pub(crate) fn authorize(probe: &dyn Probe, root: &str) -> Result<(), Refusal> {
    let platform = probe.platform();
    let machine = machine_root(probe).ok().flatten();
    let whose = match machine {
        Some(machine)
            if platform.is_within(root, &machine) && platform.is_within(&machine, root) =>
        {
            Whose::Machine
        }
        _ => Whose::Account,
    };
    check(probe, root, whose)
}

/// The machine data directory: `/var/lib/open-ferry` on Linux, `open-ferry`
/// in the ProgramData known folder on Windows, none on macOS.
pub(crate) fn machine_root(probe: &dyn Probe) -> Result<Option<String>, String> {
    let platform = probe.platform();
    match platform {
        Platform::Linux => Ok(Some(LINUX_MACHINE_DIR.to_owned())),
        Platform::Windows => {
            let data = probe.program_data()?;
            if !platform.is_absolute(&data) {
                return Err(format!("the ProgramData folder, {data}, isn't a full path"));
            }
            Ok(Some(
                platform.join(&platform.clean(&data), MACHINE_DIR_NAME),
            ))
        }
        Platform::MacOs => Ok(None),
    }
}

/// The auth directory of a machine install's config: `auth` in the machine
/// data directory, which must exist and pass [`authorize`]. Needs privilege;
/// refused on macOS.
pub(crate) fn machine_auth_dir(probe: &dyn Probe) -> Result<String, Refusal> {
    let platform = probe.platform();
    if platform == Platform::MacOs {
        return Err(macos_refusal());
    }
    if !privileged(probe)? {
        return Err(needs_privilege(
            platform,
            "writing a config for the whole machine",
        ));
    }
    let root = known_machine_root(probe)?;
    if probe.state(&root) == State::Missing {
        return Err(Refusal {
            reason: format!("{root} doesn't exist yet"),
            remedy: Some(format!(
                "Install open-ferry for the whole machine with {} first.",
                machine_install(platform)
            )),
        });
    }
    check(probe, &root, Whose::Machine)?;
    Ok(platform.join(&root, AUTH_DIR))
}

/// A machine install's config: `/etc/open-ferry/config.yaml` on Linux,
/// `config.yaml` in the protected machine data directory on Windows, where the install
/// scripts put it; none on macOS.
pub(crate) fn machine_config(probe: &dyn Probe) -> Result<Option<String>, String> {
    let platform = probe.platform();
    match platform {
        Platform::Linux => Ok(Some(LINUX_MACHINE_CONFIG.to_owned())),
        Platform::Windows => Ok(machine_root(probe)?.map(|dir| platform.join(&dir, CONFIG_FILE))),
        Platform::MacOs => Ok(None),
    }
}

/// Whether this process runs as the system service's account: root (an
/// effective uid of 0).
#[cfg(unix)]
pub(crate) fn runs_as_system() -> Result<bool, String> {
    privileged_process()
}

/// Whether this process runs as the system service's account: LocalSystem,
/// not an elevated administrator.
#[cfg(windows)]
pub(crate) fn runs_as_system() -> Result<bool, String> {
    let identity = Identity::current(ServiceKind::User)
        .map_err(|error| format!("failed to find this process's account: {error}"))?;
    Ok(identity.sid() == SYSTEM_SID)
}

/// Whose directory [`check`] looks at, for the remedy it gives.
#[derive(Clone, Copy)]
enum Whose {
    Account,
    Machine,
}

/// What the machine data directory holds for a binary.
enum Machine {
    /// There is none.
    Absent,
    /// A receipt that names the binary: the directory.
    Ours(String),
    /// No receipt, or one that names another binary.
    Other,
    /// It or its receipt can't be read: the refusal of a privileged
    /// process, which can't tell whose it is.
    Unreadable(Refusal),
    /// It can't be found, the trust check refuses it or its receipt, or the
    /// receipt isn't valid.
    Unusable(Refusal),
}

fn find_machine(probe: &dyn Probe, binary: &str) -> Machine {
    let platform = probe.platform();
    let root = match machine_root(probe) {
        Ok(Some(root)) => root,
        Ok(None) => return Machine::Absent,
        Err(reason) => {
            return Machine::Unusable(Refusal {
                reason,
                remedy: None,
            });
        }
    };
    let refusal = |reason: String| Refusal {
        reason,
        remedy: Some(remedy(platform, Whose::Machine, &root)),
    };
    let unusable = |reason: String| Machine::Unusable(refusal(reason));
    let unreadable = |reason: String| Machine::Unreadable(refusal(reason));
    match probe.state(&root) {
        State::Missing => return Machine::Absent,
        State::Directory => {}
        State::File | State::Other => return unusable(format!("{root} is not a real directory")),
        State::Inaccessible(error) => {
            return unreadable(format!("{root} can't be looked at: {error}"));
        }
    }
    if let Err(reason) = probe.trusted_dir(&root) {
        return unusable(reason);
    }
    let receipt = platform.join(&root, RECEIPT_FILE);
    match probe.state(&receipt) {
        State::Missing => return Machine::Other,
        State::File => {}
        State::Directory | State::Other => {
            return unusable(format!("{receipt} is not a regular file"));
        }
        State::Inaccessible(error) => {
            return unreadable(format!("{receipt} can't be looked at: {error}"));
        }
    }
    if let Err(reason) = probe.trusted_file(&receipt) {
        return match probe.readable_file(&receipt) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => unreadable(reason),
            _ => unusable(reason),
        };
    }
    match names(probe, &receipt, binary) {
        Ok(true) => Machine::Ours(root),
        Ok(false) => Machine::Other,
        Err(ReceiptError::Unreadable(error)) => {
            unreadable(format!("{receipt} can't be read: {error}"))
        }
        Err(ReceiptError::Malformed(error)) => {
            unusable(format!("{receipt} isn't a valid install receipt: {error}"))
        }
    }
}

/// Whether the receipt at `path` names `binary`, links resolved: no
/// receipt names nothing.
fn names(probe: &dyn Probe, path: &str, binary: &str) -> Result<bool, ReceiptError> {
    let Some(receipt) = probe.receipt(path)? else {
        return Ok(false);
    };
    Ok(
        match (probe.canonical(&receipt.binary), probe.canonical(binary)) {
            (Ok(named), Ok(binary)) => named == binary,
            _ => false,
        },
    )
}

/// Checks a file a process is about to take settings from, such as its
/// `.env` or its config, before anything in it is read: `Ok(true)` to read
/// it, `Ok(false)` when nothing is there, so there is nothing to read. A
/// privileged process reads only a file, named by a full path without `.` or
/// `..`, that passes the trust check with every directory above it; on macOS
/// it refuses any, since the check doesn't read ACLs yet. An unprivileged
/// process, or one in a container ([`Probe::in_container`]), reads the file
/// as it is.
pub(crate) fn authorize_file(probe: &dyn Probe, path: &Path) -> Result<bool, Refusal> {
    let platform = probe.platform();
    let refuse = |reason: String| Refusal {
        reason: format!("Refusing to load {}: {reason}", path.display()),
        remedy: Some(file_remedy(platform)),
    };
    let privileged = probe.privileged().map_err(|error| {
        refuse(format!(
            "failed to tell whether this process is privileged: {error}"
        ))
    })?;
    if !privileged || probe.in_container() {
        return Ok(true);
    }
    let text = path
        .to_str()
        .ok_or_else(|| refuse("the path isn't valid Unicode".to_owned()))?;
    let full = platform.is_absolute(text)
        && !text
            .split(|c| platform.is_separator(c))
            .any(|part| part == "." || part == "..");
    if !full {
        return Err(refuse(
            "it isn't a full path without `.` or `..`".to_owned(),
        ));
    }
    match probe.state(text) {
        State::Missing => Ok(false),
        _ if platform == Platform::MacOs => {
            let macos = macos_refusal();
            Err(Refusal {
                reason: format!("Refusing to load {}: {}", path.display(), macos.reason),
                remedy: macos.remedy,
            })
        }
        _ => probe.trusted_file(text).map(|()| true).map_err(refuse),
    }
}

/// Refuses an auth directory among the machine's records: the machine data
/// directory `machine_root`, a directory holding it, or one inside it other
/// than its `auth` directory and those below that. Credential files are
/// written to the auth directory, the management API's among them, so there
/// they could pass for records a privileged process trusts. Both are full
/// paths.
pub(crate) fn check_auth_dir_placement(
    platform: Platform,
    machine_root: &str,
    auth_dir: &str,
) -> Result<(), Refusal> {
    let root = platform.clean(machine_root);
    let auth = platform.join(&root, AUTH_DIR);
    let among = platform.is_within(&root, auth_dir)
        || (platform.is_within(auth_dir, &root) && !platform.is_within(auth_dir, &auth));
    if !among {
        return Ok(());
    }
    Err(Refusal {
        reason: format!(
            "The auth directory {} would put credential files among the machine's records in {root}",
            platform.clean(auth_dir)
        ),
        remedy: Some(format!(
            "Set auth-dir to {auth}, or to a directory outside {root}."
        )),
    })
}

/// Checks a resolved auth directory before a privileged process uses it.
/// Containers have the same placement rules as any other process.
pub(crate) fn authorize_auth_dir(probe: &dyn Probe, auth_dir: &Path) -> Result<(), Refusal> {
    if !privileged(probe)? {
        return Ok(());
    }
    let root = machine_root(probe).map_err(|reason| Refusal {
        reason,
        remedy: None,
    })?;
    let refuse = |reason| Refusal {
        reason: format!("Refusing auth directory {}: {reason}", auth_dir.display()),
        remedy: Some(format!(
            "Make it and every directory above it root's (administrators or SYSTEM on Windows), not writable by others, with no links{}.",
            root.as_ref()
                .map(|root| format!(
                    ", or set auth-dir to {}",
                    probe.platform().join(root, AUTH_DIR)
                ))
                .unwrap_or_default()
        )),
    };
    let text = auth_dir
        .to_str()
        .ok_or_else(|| refuse("the auth directory's path isn't valid Unicode".to_owned()))?;
    let platform = probe.platform();
    if !probe.in_container()
        && text
            .split(|c| platform.is_separator(c))
            .any(|part| part == "." || part == "..")
    {
        return Err(refuse("the path contains `.` or `..`".to_owned()));
    }
    let auth_dir = if platform.is_absolute(text) {
        text.to_owned()
    } else {
        let cwd = std::env::current_dir()
            .map_err(|error| refuse(format!("failed to get the working directory: {error}")))?;
        let cwd = cwd
            .to_str()
            .ok_or_else(|| refuse("the working directory's path isn't valid Unicode".to_owned()))?;
        platform.absolute(cwd, text).map_err(refuse)?
    };
    if let Some(root) = &root {
        check_auth_dir_placement(platform, root, &auth_dir)?;
    }
    if !probe.in_container() {
        let existing = trusted_existing_dir(probe, &auth_dir).map_err(refuse)?;
        let canonical = probe
            .canonical(&existing)
            .map_err(|error| refuse(format!("can't resolve {existing}: {error}")))?;
        let resolved = resolved_path(platform, &auth_dir, &existing, &canonical);
        if let Some(root) = &root {
            let resolved_root = resolved_machine_root(probe, root).map_err(refuse)?;
            check_auth_dir_placement(platform, &resolved_root, &resolved)?;
        }
    }
    Ok(())
}

/// Resolves the machine root's existing part for the placement comparison.
fn resolved_machine_root(probe: &dyn Probe, root: &str) -> Result<String, String> {
    let platform = probe.platform();
    let mut existing = platform.clean(root);
    loop {
        match probe.state(&existing) {
            State::Directory => break,
            State::Missing => match platform.parent(&existing) {
                Some(parent) => existing = parent,
                // A missing volume cannot hold machine records yet.
                None => return Ok(platform.clean(root)),
            },
            _ => return Err(format!("{existing} is not an accessible real directory")),
        }
    }
    let canonical = probe
        .canonical(&existing)
        .map_err(|error| format!("can't resolve {existing}: {error}"))?;
    Ok(resolved_path(platform, root, &existing, &canonical))
}

fn resolved_path(platform: Platform, path: &str, existing: &str, canonical: &str) -> String {
    let canonical = if platform == Platform::Windows {
        if let Some(unc) = canonical.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else {
            canonical
                .strip_prefix(r"\\?\")
                .unwrap_or(canonical)
                .to_owned()
        }
    } else {
        canonical.to_owned()
    };
    let cleaned = platform.clean(path);
    let suffix = cleaned.strip_prefix(existing).unwrap_or_default();
    platform.join(
        &canonical,
        suffix.trim_start_matches(|c| platform.is_separator(c)),
    )
}

/// Checks a directory or its nearest existing ancestor before creating it.
fn trusted_existing_dir(probe: &dyn Probe, path: &str) -> Result<String, String> {
    let mut existing = probe.platform().clean(path);
    loop {
        match probe.state(&existing) {
            State::Directory => break,
            State::Missing => {
                existing = probe
                    .platform()
                    .parent(&existing)
                    .ok_or_else(|| format!("nothing of {path} exists"))?
            }
            _ => return Err(format!("{existing} is not an accessible real directory")),
        }
    }
    probe.trusted_dir(&existing)?;
    Ok(existing)
}

/// The binary's policy for each Claude Code invocation.
pub(crate) fn claude_spawn_guard() -> Option<open_ferry_providers::claude_cli::SpawnGuard> {
    if matches!(Host.privileged(), Ok(false))
        || (Host.privileged() == Ok(true) && Host.in_container())
    {
        return None;
    }
    Some(open_ferry_providers::claude_cli::SpawnGuard::new(
        |entry, program, root| {
            let home = std::env::var_os("HOME").filter(|value| !value.is_empty());
            let profile = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty());
            let inherited = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty());
            authorize_claude_spawn(
                &Host,
                entry,
                program,
                root,
                home.as_deref().map(Path::new),
                inherited.as_deref().map(Path::new),
                profile.as_deref().map(Path::new),
            )
        },
    ))
}

fn authorize_claude_spawn(
    probe: &dyn Probe,
    entry: &open_ferry_providers::claude_cli::Entry,
    program: &Path,
    root: &Path,
    home: Option<&Path>,
    inherited_config: Option<&Path>,
    profile: Option<&Path>,
) -> Result<(), String> {
    if !probe.privileged()? || probe.in_container() {
        return Ok(());
    }
    if probe.platform() == Platform::MacOs {
        return Err(macos_refusal().to_string());
    }
    let text = |path: &Path| -> Result<String, String> {
        let path = path.to_str().ok_or("the path isn't valid Unicode")?;
        if !probe.platform().is_absolute(path)
            || path
                .split(|c| probe.platform().is_separator(c))
                .any(|part| part == "." || part == "..")
        {
            return Err(format!(
                "{path} isn't a full path without `.` or `..`; set full paths for command and config-dir"
            ));
        }
        Ok(path.to_owned())
    };
    let remedy = |reason| {
        format!(
            "{reason}; make the program and its directories, config-dir and working directories root's (administrators or SYSTEM on Windows), not writable by others, with no links"
        )
    };
    probe.trusted_file(&text(program)?).map_err(remedy)?;
    let account = match entry.config_dir_path() {
        Some(path) => path,
        None => {
            let home = home.ok_or(
                "Claude Code has no home directory; set config-dir to a trusted full path",
            )?;
            Path::new(&probe.platform().join(&text(home)?, ".claude")).to_path_buf()
        }
    };
    trusted_existing_dir(probe, &text(&account)?).map_err(remedy)?;
    // Without an entry override Claude Code also honours this inherited path.
    if entry.config_dir.is_empty()
        && let Some(path) = inherited_config
    {
        trusted_existing_dir(probe, &text(path)?).map_err(remedy)?;
    }
    // Windows also has USERPROFILE; check its default account directory
    // when the entry and the environment supply no explicit config directory.
    if probe.platform() == Platform::Windows
        && entry.config_dir.is_empty()
        && inherited_config.is_none()
    {
        let profile = profile.ok_or(
            "Claude Code has no Windows profile directory; set config-dir to a trusted full path",
        )?;
        let account = probe.platform().join(&text(profile)?, ".claude");
        trusted_existing_dir(probe, &account).map_err(remedy)?;
    }
    if root.starts_with(std::env::temp_dir()) {
        return Err("Claude Code's working root is in the temporary directory; set a trusted LOCALAPPDATA (Windows) or XDG_RUNTIME_DIR/XDG_CACHE_HOME/HOME (Unix), or use a trusted data directory".to_owned());
    }
    let root = text(root)?;
    let dir = probe.platform().join(&root, &entry.dir_name());
    let work = probe.platform().join(&dir, "work");
    for path in [&root, &dir, &work] {
        trusted_existing_dir(probe, path).map_err(remedy)?;
    }
    Ok(())
}

/// [`authorize`], given whose directory `root` is.
fn check(probe: &dyn Probe, root: &str, whose: Whose) -> Result<(), Refusal> {
    if !privileged(probe)? {
        return Ok(());
    }
    let platform = probe.platform();
    if platform == Platform::MacOs {
        return Err(macos_refusal());
    }
    let full = platform.is_absolute(root)
        && !root
            .split(|c| platform.is_separator(c))
            .any(|part| part == "." || part == "..");
    if !full {
        return Err(Refusal {
            reason: format!("{root} is not a full path without `.` or `..`"),
            remedy: Some(remedy(platform, whose, root)),
        });
    }
    let root = platform.clean(root);
    let refuse = |reason: String| Refusal {
        reason,
        remedy: Some(remedy(platform, whose, &root)),
    };
    let mut existing = root.clone();
    loop {
        match probe.state(&existing) {
            State::Directory => break,
            State::Missing => match platform.parent(&existing) {
                Some(parent) => existing = parent,
                None => return Err(refuse(format!("nothing of {root} exists"))),
            },
            State::File | State::Other => {
                return Err(refuse(format!("{existing} is not a real directory")));
            }
            State::Inaccessible(error) => {
                return Err(refuse(format!("{existing} can't be looked at: {error}")));
            }
        }
    }
    probe.trusted_dir(&existing).map_err(refuse)?;
    if existing == root {
        check_tree(probe, &root).map_err(refuse)?;
    }
    Ok(())
}

/// Refuses the existing directory `root` unless everything below it is a
/// directory or a regular file the trust check passes, within the limits.
/// Request log directories are checked themselves, but their data isn't walked.
fn check_tree(probe: &dyn Probe, root: &str) -> Result<(), String> {
    let platform = probe.platform();
    let mut seen = 0;
    let mut pending = vec![(root.to_owned(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let names = probe
            .entries(&dir, MAX_ENTRIES - seen + 1)
            .map_err(|error| format!("{dir} can't be listed: {error}"))?;
        seen += names.len();
        if seen > MAX_ENTRIES {
            return Err(format!("{root} holds more than {MAX_ENTRIES} entries"));
        }
        for name in names {
            let path = platform.join(&dir, &name);
            match probe.state(&path) {
                // Gone since it was listed.
                State::Missing => {}
                State::Directory => {
                    probe.trusted_dir(&path)?;
                    let log_name = if platform == Platform::Windows {
                        name.eq_ignore_ascii_case("logs")
                    } else {
                        name == "logs"
                    };
                    let logs = log_name
                        && (dir == root
                            || platform.is_within(&dir, &platform.join(root, AUTH_DIR)));
                    if !logs {
                        if depth >= MAX_DEPTH {
                            return Err(format!(
                                "{path} is more than {MAX_DEPTH} levels below {root}"
                            ));
                        }
                        pending.push((path, depth + 1));
                    }
                }
                State::File => probe.trusted_file(&path)?,
                State::Other => return Err(format!("{path} is a link or a special file")),
                State::Inaccessible(error) => {
                    return Err(format!("{path} can't be looked at: {error}"));
                }
            }
        }
    }
    Ok(())
}

/// Whether this process is privileged, a failure to tell refusing.
fn privileged(probe: &dyn Probe) -> Result<bool, Refusal> {
    probe.privileged().map_err(|error| Refusal {
        reason: format!("failed to tell whether this process is privileged: {error}"),
        remedy: None,
    })
}

/// The machine data directory, where there must be one.
fn known_machine_root(probe: &dyn Probe) -> Result<String, Refusal> {
    match machine_root(probe) {
        Ok(Some(root)) => Ok(root),
        Ok(None) => Err(macos_refusal()),
        Err(reason) => Err(Refusal {
            reason,
            remedy: None,
        }),
    }
}

/// The refusal of every privileged process on macOS.
fn macos_refusal() -> Refusal {
    Refusal {
        reason: "open-ferry can't yet check who may change its files on macOS, so it doesn't use them as root".to_owned(),
        remedy: Some(
            "Run it without sudo; installs for the whole machine come to macOS in a later version."
                .to_owned(),
        ),
    }
}

/// The refusal of an unprivileged process `doing` what needs privilege.
fn needs_privilege(platform: Platform, doing: &str) -> Refusal {
    let (who, remedy) = match platform {
        Platform::Windows => (
            "an administrator",
            "Run it in a terminal opened as administrator.",
        ),
        Platform::Linux | Platform::MacOs => ("root", "Run it with sudo."),
    };
    Refusal {
        reason: format!("{doing} needs {who}"),
        remedy: Some(remedy.to_owned()),
    }
}

/// What to do about a data directory a privileged process can't use. The
/// machine's holds the machine's credentials, so nothing here deletes it.
fn remedy(platform: Platform, whose: Whose, root: &str) -> String {
    match (whose, platform) {
        (Whose::Account, Platform::Windows) => format!(
            "Run it in a terminal not opened as administrator, or install open-ferry for the whole machine with {}.",
            machine_install(platform)
        ),
        (Whose::Account, Platform::Linux | Platform::MacOs) => format!(
            "Run it without sudo, or install open-ferry for the whole machine with {}.",
            machine_install(platform)
        ),
        (Whose::Machine, Platform::Windows) => format!(
            "Stop the open-ferry service, then as an administrator fix what is named, or move {root} aside and install again with {}; keep its auth directory private, since it holds the machine's credentials.",
            machine_install(platform)
        ),
        (Whose::Machine, Platform::Linux | Platform::MacOs) => format!(
            "Stop the open-ferry service, then as root fix what is named, or move {root} aside and install again with {}; keep its auth directory private, since it holds the machine's credentials.",
            machine_install(platform)
        ),
    }
}

/// What to do about a settings file [`authorize_file`] refuses.
fn file_remedy(platform: Platform) -> String {
    match platform {
        Platform::Windows => "As an administrator, make it and every folder above it writable only by administrators, or run open-ferry in a terminal not opened as administrator.".to_owned(),
        Platform::Linux | Platform::MacOs => "As root, make the file and every directory above it root's and writable only by root, or run open-ferry as a user other than root. This also applies in a container open-ferry doesn't recognise (LXC, systemd-nspawn, or a Kubernetes pod without a service-account token).".to_owned(),
    }
}

/// How to install open-ferry for the whole machine.
fn machine_install(platform: Platform) -> &'static str {
    match platform {
        Platform::Windows => "`install.ps1 -System`",
        Platform::Linux | Platform::MacOs => "`install.sh --system`",
    }
}

/// What to tell an unprivileged process whose binary seems to be the
/// machine's.
fn machine_hint(platform: Platform) -> String {
    match platform {
        Platform::Windows => "This open-ferry looks installed for the whole machine: run it in a terminal opened as administrator.",
        Platform::Linux | Platform::MacOs => {
            "This open-ferry looks installed for the whole machine: run it with sudo."
        }
    }
    .to_owned()
}

/// Whether this process is privileged: an effective uid of 0.
#[cfg(unix)]
fn privileged_process() -> Result<bool, String> {
    // SAFETY: `geteuid` only reads this process's credentials.
    Ok(unsafe { libc::geteuid() } == 0)
}

/// Whether this process is privileged: its token is elevated, or it runs as
/// SYSTEM.
#[cfg(windows)]
fn privileged_process() -> Result<bool, String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the current-process pseudo-handle is always valid and `token` is a valid
    // out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(format!(
            "failed to open this process's token: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: `token` was just opened and nothing else owns it.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut size = 0u32;
    // SAFETY: `elevation` is a writable TOKEN_ELEVATION of the size passed.
    let read = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    };
    if read == 0 {
        return Err(format!(
            "failed to read whether this process is elevated: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(elevation.TokenIsElevated != 0 || runs_as_system()?)
}

/// The ProgramData known folder, from the shell, never the environment.
#[cfg(windows)]
fn program_data() -> Result<String, String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };

    let mut path: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: the folder ID is a valid GUID, a null token asks for this user's view, and
    // `path` is a valid out-pointer.
    let result = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramData,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut path,
        )
    };
    let text = if result >= 0 && !path.is_null() {
        let mut len = 0;
        // SAFETY: on success `path` is a NUL-terminated UTF-16 string.
        while unsafe { *path.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` code units were just read from `path`.
        Some(String::from_utf16(unsafe {
            std::slice::from_raw_parts(path, len)
        }))
    } else {
        None
    };
    // SAFETY: the shell allocated `path`, or left it null, and it is freed once, as the API
    // requires even after a failure.
    unsafe { CoTaskMemFree(path.cast()) };
    match text {
        Some(Ok(text)) => Ok(text),
        Some(Err(_)) => Err("the ProgramData folder's path isn't valid Unicode".to_owned()),
        None => Err(format!(
            "failed to find the ProgramData folder (HRESULT {result:#010x})"
        )),
    }
}

/// There is no ProgramData off Windows.
#[cfg(not(windows))]
fn program_data() -> Result<String, String> {
    Err("there is no ProgramData folder on this system".to_owned())
}

/// Makes the machine data directory: mode 0700, owned by root.
#[cfg(unix)]
fn create_machine_dir(path: &Path) -> io::Result<()> {
    trust::create_private_dir(path)
}

/// Makes the machine data directory, with a protected DACL that grants only
/// SYSTEM and the administrators; an existing one is an `AlreadyExists`
/// error.
#[cfg(windows)]
fn create_machine_dir(path: &Path) -> io::Result<()> {
    trust::create_system_data_dir(path)
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod receipt_access_tests {
    use super::*;
    use crate::update::scope_probe::{Fake, Node};

    #[test]
    fn an_unreadable_receipt_keeps_the_machine_hint_but_an_untrusted_one_does_not() {
        for platform in [Platform::Linux, Platform::Windows] {
            let mut probe = Fake::new(platform, false);
            let root = machine_root(&probe).unwrap().unwrap();
            let receipt = platform.join(&root, RECEIPT_FILE);
            probe.node(&receipt, Node::Unreadable);
            assert!(matches!(
                find_machine(&probe, "binary"),
                Machine::Unreadable(_)
            ));
            assert!(lookup(&probe, "binary").unwrap().machine_hint.is_some());
            assert!(probe.reads.borrow().iter().all(|path| path != &receipt));

            probe.file(&receipt).untrust(&receipt);
            assert!(matches!(
                find_machine(&probe, "binary"),
                Machine::Unusable(_)
            ));
            assert!(lookup(&probe, "binary").unwrap().machine_hint.is_none());
            probe.privileged = Ok(true);
            assert!(lookup(&probe, "binary").is_err());
        }
    }

    #[test]
    fn the_host_permission_probe_accepts_only_regular_files_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("receipt");
        fs::write(&file, b"unchanged").unwrap();
        Host.readable_file(file.to_str().unwrap()).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"unchanged");
        assert!(Host.readable_file(dir.path().to_str().unwrap()).is_err());
        assert!(
            Host.readable_file(dir.path().join("missing").to_str().unwrap())
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_host_permission_probe_refuses_links_and_does_not_wait_on_a_fifo() {
        use std::ffi::CString;
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("receipt");
        fs::write(&file, b"unchanged").unwrap();
        let link = dir.path().join("link");
        symlink(&file, &link).unwrap();
        assert!(Host.readable_file(link.to_str().unwrap()).is_err());
        let fifo = dir.path().join("fifo");
        let path = CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `path` is NUL-terminated and names a path in this test's scratch directory.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(Host.readable_file(fifo.to_str().unwrap()).is_err());
    }
}
