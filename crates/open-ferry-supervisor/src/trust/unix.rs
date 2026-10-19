//! The Unix ancestry check: every component from `/` is opened with
//! `openat(O_NOFOLLOW | O_DIRECTORY)` and examined with `fstat` on the descriptor, so what is
//! checked is what is later used and a symlink swap cannot slip in between.
//!
//! The endpoint module reuses [`walk`] with its own final-directory rules.
#![allow(clippy::unnecessary_cast)]

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use super::TrustError;
use crate::endpoint::ServiceKind;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod macos;

/// What a trust check tolerates. Build one with [`TrustRules::system`] or [`TrustRules::user`].
#[derive(Clone, Debug)]
pub struct TrustRules {
    kind: ServiceKind,
    uid: u32,
    accounts: Arc<dyn AccountDb>,
    exempt_through: Option<PathBuf>,
}

impl TrustRules {
    /// A system service. Every directory must be owned by root or `service_uid` and be
    /// writable by neither group nor others. No sticky-directory exception.
    pub fn system(service_uid: u32) -> Self {
        Self::new(ServiceKind::System, service_uid)
    }

    /// A user service. Like the system rules, except that group write is tolerated on a
    /// directory whose group is the user's private group (its name equals the user's name and
    /// no other account is a member of it). See `private_group` in this module.
    pub fn user(uid: u32) -> Self {
        Self::new(ServiceKind::User, uid)
    }

    /// The rules for a service identity, the same on every system.
    pub fn for_identity(identity: &crate::endpoint::Identity) -> Self {
        Self::new(identity.kind(), identity.uid())
    }

    fn new(kind: ServiceKind, uid: u32) -> Self {
        Self {
            kind,
            uid,
            accounts: Arc::new(SystemAccounts::host()),
            exempt_through: None,
        }
    }

    /// Not upstream's: lets a test put its directories under a temp dir whose own ancestors
    /// (`/tmp` and friends) the rules would refuse.
    #[cfg(test)]
    pub(crate) fn for_test(
        mut self,
        accounts: Arc<dyn AccountDb>,
        exempt_through: Option<PathBuf>,
    ) -> Self {
        self.accounts = accounts;
        self.exempt_through = exempt_through;
        self
    }

    fn is_exempt(&self, shown: &Path) -> bool {
        match &self.exempt_through {
            Some(top) => top.starts_with(shown),
            None => false,
        }
    }
}

/// Checks each directory in `paths`, the directory itself and every ancestor up to `/`.
/// A symlink anywhere in a chain is refused, so pass canonical paths.
///
/// Linux reads the POSIX access ACL before allowing a private group to write. macOS reads
/// each object's extended ACL, including when its mode bits show no group write, and refuses
/// mutation grants to anyone other than the owner or root. ACL read errors fail closed.
pub fn check_dirs(paths: &[&Path], rules: &TrustRules) -> Result<(), TrustError> {
    for path in paths {
        walk(rules, path, false).map_err(WalkError::into_trust)?;
    }
    Ok(())
}

/// Checks that `path` is a regular file (not a link) owned by root or the service uid that
/// neither group nor others can write, and that its directory chain passes [`check_dirs`].
pub fn check_file(path: &Path, rules: &TrustRules) -> Result<(), TrustError> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(TrustError::new(path, "it has no parent directory"));
    };
    let dir = walk(rules, parent, false).map_err(WalkError::into_trust)?;
    // One descriptor for the type, owner, mode and ACL, so they describe the same object.
    let cname =
        cstring(name).map_err(|e| TrustError::new(path, format!("it can't be examined: {e}")))?;
    // SAFETY: `cname` is NUL-terminated; O_NONBLOCK keeps a FIFO from blocking the open.
    let raw = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            cname.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    let file = match owned(raw) {
        Ok(file) => file,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(TrustError::new(path, "it is not a regular file"));
        }
        Err(e) => return Err(TrustError::new(path, format!("it can't be examined: {e}"))),
    };
    let st = fstat(file.as_raw_fd())
        .map_err(|e| TrustError::new(path, format!("it can't be examined: {e}")))?;
    if file_type(&st) != libc::S_IFREG as u32 {
        return Err(TrustError::new(path, "it is not a regular file"));
    }
    check_fd_acl(file.as_raw_fd(), st.st_uid as u32)
        .map_err(|reason| TrustError::new(path, reason))?;
    check_dir_stat(rules, DirStat::of_fd(&st, file.as_raw_fd()), false)
        .map_err(|reason| TrustError::new(path, reason))
}

/// Darwin ACLs grant access independently of mode bits, including on a 0700 endpoint.
pub(crate) fn check_fd_acl(fd: RawFd, owner: u32) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return macos::check(fd, owner);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (fd, owner);
        Ok(())
    }
}

/// Why a walk stopped.
#[derive(Debug)]
pub(crate) enum WalkError {
    /// A component does not exist.
    Missing(PathBuf),
    /// A component is a symlink, is not a directory, or breaks the ownership or mode rules.
    Untrusted(PathBuf, String),
    Io(PathBuf, io::Error),
}

impl WalkError {
    pub(crate) fn into_trust(self) -> TrustError {
        match self {
            WalkError::Missing(path) => TrustError::new(&path, "it does not exist"),
            WalkError::Untrusted(path, reason) => TrustError::new(&path, reason),
            WalkError::Io(path, e) => TrustError::new(&path, format!("it can't be examined: {e}")),
        }
    }
}

/// The POSIX access ACL of an object, as far as the group-write exception cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AclState {
    /// No extended ACL: the mode bits are the whole story.
    Plain,
    /// Named users or groups have entries, and the group bits are only the ACL mask.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Extended,
    /// The ACL could not be read, or this system's ACLs are not understood.
    Unreadable,
}

/// The part of `fstat` the directory rules look at. `acl` is only consulted when the group can
/// write, so a caller may leave it `Plain` when that bit is clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirStat {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub acl: AclState,
}

impl DirStat {
    /// Reads the ACL of `fd` only when the group-write bit makes it matter.
    fn of_fd(st: &libc::stat, fd: RawFd) -> Self {
        let mode = st.st_mode as u32;
        let acl = if mode & 0o020 != 0 {
            access_acl(fd)
        } else {
            AclState::Plain
        };
        Self {
            uid: st.st_uid as u32,
            gid: st.st_gid as u32,
            mode,
            acl,
        }
    }
}

/// Whether the object behind `fd` has a POSIX access ACL beyond its mode bits. The Linux kernel
/// stores one in the `system.posix_acl_access` attribute only when it is extended; no attribute
/// (`ENODATA`) or no ACL support (`ENOTSUP`) means none. Default ACLs are not looked at: they
/// only shape what is created inside a directory.
#[cfg(target_os = "linux")]
fn access_acl(fd: RawFd) -> AclState {
    // SAFETY: the name is NUL-terminated and a null buffer of size 0 only asks for the size.
    let rc = unsafe {
        libc::fgetxattr(
            fd,
            c"system.posix_acl_access".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if rc >= 0 {
        return AclState::Extended;
    }
    match io::Error::last_os_error().raw_os_error() {
        // No attribute, or a filesystem with no ACLs at all: nothing beyond the mode bits.
        Some(libc::ENODATA) | Some(libc::ENOTSUP) => AclState::Plain,
        _ => AclState::Unreadable,
    }
}

#[cfg(target_os = "macos")]
fn access_acl(_fd: RawFd) -> AclState {
    // Darwin's independent extended ACL was checked by check_fd_acl. A real macOS
    // account lookup still fails closed for the private-group exception.
    AclState::Plain
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn access_acl(_fd: RawFd) -> AclState {
    AclState::Unreadable
}

/// The ownership and mode rule for an ancestor, or for a directory the service runs from.
/// `sticky_ok` allows the one exception, a root-owned sticky directory such as `/tmp`, and is
/// passed only for the directory that directly holds the endpoint directory of a user service.
pub(crate) fn check_dir_stat(
    rules: &TrustRules,
    st: DirStat,
    sticky_ok: bool,
) -> Result<(), String> {
    if st.uid != 0 && st.uid != rules.uid {
        return Err(format!(
            "it is owned by uid {}, not root or uid {}",
            st.uid, rules.uid
        ));
    }
    let mode = st.mode & 0o7777;
    if sticky_ok && rules.kind == ServiceKind::User && st.uid == 0 && mode & 0o1000 != 0 {
        return Ok(());
    }
    if mode & 0o002 != 0 {
        return Err("anyone can write to it".to_string());
    }
    if mode & 0o020 != 0 {
        match rules.kind {
            ServiceKind::System => return Err("its group can write to it".to_string()),
            ServiceKind::User => {
                match st.acl {
                    AclState::Plain => {}
                    AclState::Extended => {
                        return Err("its group can write to it, and it has an extended ACL that may name other accounts".to_string())
                    }
                    AclState::Unreadable => {
                        return Err("its group can write to it, and its ACL can't be read".to_string())
                    }
                }
                private_group(rules.accounts.as_ref(), rules.uid, st.gid)
                    .map_err(|why| format!("its group can write to it, and {why}"))?;
            }
        }
    }
    Ok(())
}

/// Whether `gid` is `uid`'s private group, so that group write on a path is no wider than the
/// user's own write. Anything that can't be established fails closed.
pub(crate) fn private_group(db: &dyn AccountDb, uid: u32, gid: u32) -> Result<(), String> {
    let user = db
        .user_name(uid)
        .ok_or_else(|| format!("the name of uid {uid} can't be looked up"))?;
    let group = db
        .group_name(gid)
        .ok_or_else(|| format!("the name of gid {gid} can't be looked up"))?;
    if group != user {
        return Err(format!("group {group} is not the private group of {user}"));
    }
    let primary_gid = db
        .user_primary_gid(uid)
        .ok_or_else(|| format!("the primary group of {user} can't be looked up"))?;
    if primary_gid != gid {
        return Err(format!("group {group} is not the primary group of {user}"));
    }
    let listed = db
        .group_members(gid)
        .ok_or_else(|| format!("the members of group {group} can't be fully listed"))?;
    if let Some(other) = listed.iter().find(|m| **m != user) {
        return Err(format!("{other} is a member of group {group}"));
    }
    let primary = db.primary_members(gid).ok_or_else(|| {
        format!("the accounts whose primary group is {group} can't be fully enumerated")
    })?;
    if let Some(other) = primary.iter().find(|m| **m != user) {
        return Err(format!("{other} has {group} as its primary group"));
    }
    Ok(())
}

/// Opens `path` from `/` one component at a time without following links, and checks each
/// component. Returns the descriptor of the final directory. `last_may_be_sticky` applies the
/// sticky exception to the final directory of the walk.
pub(crate) fn walk(
    rules: &TrustRules,
    path: &Path,
    last_may_be_sticky: bool,
) -> Result<OwnedFd, WalkError> {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(WalkError::Untrusted(
            path.to_path_buf(),
            "the path is not absolute".to_string(),
        ));
    }
    let mut names: Vec<OsString> = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::CurDir => {}
            _ => {
                return Err(WalkError::Untrusted(
                    path.to_path_buf(),
                    "the path contains `..` or a prefix".to_string(),
                ));
            }
        }
    }

    let mut shown = PathBuf::from("/");
    let mut current = open_root().map_err(|e| WalkError::Io(shown.clone(), e))?;
    check_component(
        rules,
        &current,
        &shown,
        names.is_empty() && last_may_be_sticky,
    )?;
    for (index, name) in names.iter().enumerate() {
        shown.push(name);
        let next = match open_dir_at(current.as_raw_fd(), name) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(WalkError::Missing(shown)),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
                return Err(WalkError::Untrusted(
                    shown,
                    "it is a symlink or not a directory".to_string(),
                ));
            }
            Err(e) => return Err(WalkError::Io(shown, e)),
        };
        let is_last = index + 1 == names.len();
        check_component(rules, &next, &shown, is_last && last_may_be_sticky)?;
        current = next;
    }
    Ok(current)
}

fn check_component(
    rules: &TrustRules,
    fd: &OwnedFd,
    shown: &Path,
    sticky_ok: bool,
) -> Result<(), WalkError> {
    if rules.is_exempt(shown) {
        return Ok(());
    }
    let st = fstat(fd.as_raw_fd()).map_err(|e| WalkError::Io(shown.to_path_buf(), e))?;
    check_fd_acl(fd.as_raw_fd(), st.st_uid as u32)
        .map_err(|reason| WalkError::Untrusted(shown.to_path_buf(), reason))?;
    let sticky_ok = sticky_ok && is_sticky_root(shown);
    check_dir_stat(rules, DirStat::of_fd(&st, fd.as_raw_fd()), sticky_ok)
        .map_err(|reason| WalkError::Untrusted(shown.to_path_buf(), reason))
}

/// The only directories the sticky exception may apply to: `/tmp` (`/private/tmp` once macOS
/// has resolved its link), or on macOS the user's `confstr` temp directory.
fn is_sticky_root(shown: &Path) -> bool {
    if shown == Path::new("/tmp") {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        if shown == Path::new("/private/tmp") {
            return true;
        }
        if let Some(dir) = darwin_user_temp_dir() {
            return std::fs::canonicalize(dir).is_ok_and(|dir| dir == shown);
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn darwin_user_temp_dir() -> Option<PathBuf> {
    let mut buf = vec![0 as libc::c_char; libc::PATH_MAX as usize];
    // SAFETY: `buf` is valid for `buf.len()` bytes for the duration of the call.
    let needed =
        unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if needed == 0 || needed > buf.len() {
        return None;
    }
    // SAFETY: `confstr` wrote a NUL-terminated string that fit in `buf`.
    let text = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    Some(PathBuf::from(OsStr::from_bytes(text.to_bytes())))
}

/// Makes the last component of `path` a private directory (0700, owned by the effective uid)
/// when it does not exist yet; the parent must already exist. An existing directory is left
/// exactly as it is (its mode may be 0755), and only has to be a real directory rather than a
/// link. Neither case is a trust verdict: the caller runs [`check_dirs`] afterwards.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let name = cstring(path.as_os_str())?;
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let made = unsafe { libc::mkdir(name.as_ptr(), 0o700) } == 0;
    if !made {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    // SAFETY: `name` is NUL-terminated and outlives the call; the descriptor is wrapped at once.
    let fd = owned(unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    if !made {
        return Ok(());
    }
    // SAFETY: `fd` is an open descriptor of the directory just made, so this changes nothing else.
    if unsafe { libc::fchmod(fd.as_raw_fd(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let st = fstat(fd.as_raw_fd())?;
    // SAFETY: `geteuid` has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if st.st_uid as u32 != euid || st.st_mode as u32 & 0o7777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} was not created as a 0700 directory owned by uid {euid}",
                path.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn file_type(st: &libc::stat) -> u32 {
    st.st_mode as u32 & libc::S_IFMT as u32
}

pub(crate) fn cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn open_root() -> io::Result<OwnedFd> {
    // SAFETY: the path is a valid NUL-terminated string; the descriptor is wrapped at once.
    let fd = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    owned(fd)
}

pub(crate) fn open_dir_at(dir: RawFd, name: &OsStr) -> io::Result<OwnedFd> {
    let name = cstring(name)?;
    // SAFETY: `name` is NUL-terminated and outlives the call; the descriptor is wrapped at once.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned(fd)
}

fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(crate) fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: an all-zero `stat` is a valid value, and `fstat` fills it in.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid out-pointer for the duration of the call.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

pub(crate) fn fstatat_nofollow(dir: RawFd, name: &OsStr) -> io::Result<libc::stat> {
    let name = cstring(name)?;
    // SAFETY: an all-zero `stat` is a valid value, and `fstatat` fills it in.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is NUL-terminated and `st` is a valid out-pointer for the call.
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

/// What the private-group rule needs to know about accounts. `None` means the answer can't be
/// established completely, and the rule then refuses.
pub(crate) trait AccountDb: Send + Sync + std::fmt::Debug {
    fn user_name(&self, uid: u32) -> Option<String>;
    /// The primary group the user's own account record names.
    fn user_primary_gid(&self, uid: u32) -> Option<u32>;
    fn group_name(&self, gid: u32) -> Option<String>;
    /// Names in the group's member list.
    fn group_members(&self, gid: u32) -> Option<Vec<String>>;
    /// Names of every account whose primary group is `gid`.
    fn primary_members(&self, gid: u32) -> Option<Vec<String>>;
}

/// The account database as `/etc/passwd` and `/etc/group` hold it, read directly (no NSS call)
/// and only when `/etc/nsswitch.conf` names sources that root controls (see
/// [`nsswitch_local_only`]). A directory service, a missing or malformed file, or a NIS `+`/`-`
/// line leaves the answer unknown. Only Linux is trusted with this; elsewhere every answer is
/// unknown and the private-group exception stays off.
///
/// Residual: `systemd` is accepted as a source because its records (userdb drop-ins, dynamic
/// users) are written by root only, but a userdb drop-in's group membership is not enumerated
/// here, so a root-written membership of this kind goes unseen.
#[derive(Debug)]
pub(crate) struct SystemAccounts {
    root: PathBuf,
}

impl SystemAccounts {
    pub(crate) fn host() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }

    /// Reads `etc/...` below `root` instead of the host's files, so tests use fixtures.
    #[cfg(test)]
    pub(crate) fn rooted(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    fn read(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(relative)).ok()
    }

    fn passwd(&self) -> Option<Vec<PasswdEntry>> {
        if !cfg!(target_os = "linux") || !nsswitch_local_only(&self.read("etc/nsswitch.conf")?) {
            return None;
        }
        parse_passwd(&self.read("etc/passwd")?)
    }

    fn group(&self) -> Option<Vec<GroupEntry>> {
        if !cfg!(target_os = "linux") || !nsswitch_local_only(&self.read("etc/nsswitch.conf")?) {
            return None;
        }
        parse_group(&self.read("etc/group")?)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PasswdEntry {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GroupEntry {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

/// Parses `/etc/passwd`. Blank lines and `#` comments are skipped; anything else that is not
/// `name:pw:uid:gid:...` (including NIS `+` and `-` lines) makes the whole file unusable.
pub(crate) fn parse_passwd(text: &str) -> Option<Vec<PasswdEntry>> {
    let mut entries = Vec::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, uid, gid, ..] = fields.as_slice() else {
            return None;
        };
        if name.is_empty() || name.starts_with(['+', '-']) {
            return None;
        }
        entries.push(PasswdEntry {
            name: (*name).to_string(),
            uid: uid.parse().ok()?,
            gid: gid.parse().ok()?,
        });
    }
    Some(entries)
}

/// Parses `/etc/group` the same way: `name:pw:gid:member,member`.
pub(crate) fn parse_group(text: &str) -> Option<Vec<GroupEntry>> {
    let mut entries = Vec::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, gid, rest @ ..] = fields.as_slice() else {
            return None;
        };
        if name.is_empty() || name.starts_with(['+', '-']) || rest.len() > 1 {
            return None;
        }
        let members = rest
            .first()
            .map(|list| {
                list.split(',')
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        entries.push(GroupEntry {
            name: (*name).to_string(),
            gid: gid.parse().ok()?,
            members,
        });
    }
    Some(entries)
}

/// True only when `nsswitch.conf` text gives `passwd` and `group` sources that root alone
/// controls: `files`, `systemd`, and `compat` (whose NIS `+`/`-` lines the parsers above refuse
/// anyway). `[ACTION=...]` tokens are skipped. Every other source (`sss`, `ldap`, `nis`,
/// `winbind`, `mymachines`, `db`, `altfiles`, ...), an empty or missing line, and a
/// `passwd_compat`/`group_compat`/`initgroups_compat` redirect leave the answer unknown. An
/// `initgroups` line is judged the same way when present and may be absent. The file is only read;
/// no NSS lookup is made, and the result does not depend on which libc is in use.
pub(crate) fn nsswitch_local_only(text: &str) -> bool {
    let mut seen = [false, false];
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let Some((db, sources)) = line.split_once(':') else {
            continue;
        };
        // `initgroups` may be absent (glibc then falls back to `group`), but when it is
        // there its sources get the same judgment, since it can grant supplementary gids.
        let slot = match db.trim() {
            "passwd" => Some(0),
            "group" => Some(1),
            "initgroups" => None,
            "passwd_compat" | "group_compat" | "initgroups_compat" => return false,
            _ => continue,
        };
        let mut real_sources = 0;
        let mut in_action = false;
        for token in sources.split_whitespace() {
            if in_action || token.starts_with('[') {
                in_action = !token.ends_with(']');
                continue;
            }
            if !matches!(token, "files" | "systemd" | "compat") {
                return false;
            }
            real_sources += 1;
        }
        if real_sources == 0 || in_action {
            return false;
        }
        if let Some(slot) = slot {
            seen[slot] = true;
        }
    }
    seen[0] && seen[1]
}

impl AccountDb for SystemAccounts {
    fn user_name(&self, uid: u32) -> Option<String> {
        self.passwd()?
            .into_iter()
            .find(|e| e.uid == uid)
            .map(|e| e.name)
    }

    fn user_primary_gid(&self, uid: u32) -> Option<u32> {
        self.passwd()?
            .into_iter()
            .find(|e| e.uid == uid)
            .map(|e| e.gid)
    }

    fn group_name(&self, gid: u32) -> Option<String> {
        self.group()?
            .into_iter()
            .find(|e| e.gid == gid)
            .map(|e| e.name)
    }

    fn group_members(&self, gid: u32) -> Option<Vec<String>> {
        let groups = self.group()?;
        let mut found = false;
        let mut members = Vec::new();
        for entry in groups.into_iter().filter(|e| e.gid == gid) {
            found = true;
            members.extend(entry.members);
        }
        found.then_some(members)
    }

    fn primary_members(&self, gid: u32) -> Option<Vec<String>> {
        Some(
            self.passwd()?
                .into_iter()
                .filter(|e| e.gid == gid)
                .map(|e| e.name)
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[derive(Debug, Default)]
    struct Fake {
        user: Option<&'static str>,
        group: Option<&'static str>,
        members: Option<Vec<&'static str>>,
        primary: Option<Vec<&'static str>>,
        primary_gid: Option<u32>,
    }

    impl Fake {
        fn private() -> Self {
            Self {
                primary_gid: Some(1000),
                user: Some("sam"),
                group: Some("sam"),
                members: Some(vec![]),
                primary: Some(vec!["sam"]),
            }
        }

        /// The private group of whoever runs the test, for directories the test really makes:
        /// they carry the effective gid, which is not 1000 everywhere (it is 0 in some
        /// containers). The policy under test is unchanged: the group must still be the
        /// account's primary group.
        fn private_here() -> Self {
            Self {
                primary_gid: Some(egid()),
                ..Self::private()
            }
        }
    }

    fn egid() -> u32 {
        // SAFETY: `getegid` has no preconditions.
        unsafe { libc::getegid() }
    }

    impl AccountDb for Fake {
        fn user_name(&self, _uid: u32) -> Option<String> {
            self.user.map(str::to_string)
        }
        fn user_primary_gid(&self, _uid: u32) -> Option<u32> {
            self.primary_gid
        }
        fn group_name(&self, _gid: u32) -> Option<String> {
            self.group.map(str::to_string)
        }
        fn group_members(&self, _gid: u32) -> Option<Vec<String>> {
            self.members
                .clone()
                .map(|m| m.into_iter().map(str::to_string).collect())
        }
        fn primary_members(&self, _gid: u32) -> Option<Vec<String>> {
            self.primary
                .clone()
                .map(|m| m.into_iter().map(str::to_string).collect())
        }
    }

    fn rules(kind: ServiceKind, uid: u32, db: Fake, exempt: Option<PathBuf>) -> TrustRules {
        let base = match kind {
            ServiceKind::System => TrustRules::system(uid),
            ServiceKind::User => TrustRules::user(uid),
        };
        base.for_test(Arc::new(db), exempt)
    }

    fn stat(uid: u32, mode: u32) -> DirStat {
        DirStat {
            uid,
            gid: 1000,
            mode,
            acl: AclState::Plain,
        }
    }

    #[test]
    fn an_extended_or_unreadable_acl_refuses_the_private_group_exception() {
        // Not upstream's: with an ACL the group bits are only the mask, and named accounts
        // may write.
        let user = rules(ServiceKind::User, 1000, Fake::private(), None);
        let mut st = stat(1000, 0o40775);
        assert!(check_dir_stat(&user, st, false).is_ok());
        st.acl = AclState::Extended;
        assert!(
            check_dir_stat(&user, st, false)
                .unwrap_err()
                .contains("extended ACL")
        );
        st.acl = AclState::Unreadable;
        assert!(
            check_dir_stat(&user, st, false)
                .unwrap_err()
                .contains("can't be read")
        );
        // Without group write the ACL state doesn't matter to this rule.
        let mut quiet = stat(1000, 0o40755);
        quiet.acl = AclState::Extended;
        assert!(check_dir_stat(&user, quiet, false).is_ok());
    }

    #[test]
    fn root_and_the_service_uid_may_own_a_directory() {
        // Not upstream's: the owner rule for ancestors of the service's directories.
        let r = rules(ServiceKind::System, 500, Fake::default(), None);
        assert!(check_dir_stat(&r, stat(0, 0o40755), false).is_ok());
        assert!(check_dir_stat(&r, stat(500, 0o40755), false).is_ok());
        let err = check_dir_stat(&r, stat(501, 0o40755), false).unwrap_err();
        assert!(err.contains("owned by uid 501"), "{err}");
    }

    #[test]
    fn others_write_is_refused_and_group_write_depends_on_the_kind() {
        // Not upstream's: a system service tolerates no group write; a user service only the
        // private group's.
        let system = rules(ServiceKind::System, 0, Fake::private(), None);
        assert!(
            check_dir_stat(&system, stat(0, 0o40757), false)
                .unwrap_err()
                .contains("anyone")
        );
        assert!(
            check_dir_stat(&system, stat(0, 0o40775), false)
                .unwrap_err()
                .contains("group")
        );
        let user = rules(ServiceKind::User, 1000, Fake::private(), None);
        assert!(check_dir_stat(&user, stat(1000, 0o40775), false).is_ok());
        assert!(
            check_dir_stat(&user, stat(1000, 0o40757), false)
                .unwrap_err()
                .contains("anyone")
        );
    }

    #[test]
    fn the_sticky_exception_is_for_user_services_and_root_owned_sticky_directories() {
        // Not upstream's: `/tmp` (root, 1777) is accepted only as the endpoint directory's
        // parent of a user service.
        let user = rules(ServiceKind::User, 1000, Fake::default(), None);
        assert!(check_dir_stat(&user, stat(0, 0o41777), true).is_ok());
        assert!(check_dir_stat(&user, stat(0, 0o41777), false).is_err());
        assert!(
            check_dir_stat(&user, stat(0, 0o40777), true).is_err(),
            "world-writable without the sticky bit"
        );
        assert!(
            check_dir_stat(&user, stat(1000, 0o41777), true).is_err(),
            "sticky but not root-owned"
        );
        let system = rules(ServiceKind::System, 0, Fake::default(), None);
        assert!(check_dir_stat(&system, stat(0, 0o41777), true).is_err());
    }

    #[test]
    fn the_sticky_exception_names_tmp_and_nothing_else() {
        // Not upstream's: review feedback, no arbitrary root-owned sticky directory.
        assert!(is_sticky_root(Path::new("/tmp")));
        assert!(!is_sticky_root(Path::new("/var/tmp")));
        assert!(!is_sticky_root(Path::new("/tmp/sub")));
        assert!(!is_sticky_root(Path::new("/dev/shm")));
        assert!(!is_sticky_root(Path::new("/")));
    }

    #[test]
    fn a_private_group_needs_the_users_name_and_no_other_member() {
        // Not upstream's: lead answer 3, the private-group exception.
        let ok = Fake::private();
        assert!(private_group(&ok, 1000, 1000).is_ok());

        let shared = Fake {
            group: Some("users"),
            ..Fake::private()
        };
        assert!(
            private_group(&shared, 1000, 100)
                .unwrap_err()
                .contains("not the private group")
        );

        let listed = Fake {
            members: Some(vec!["sam", "kim"]),
            ..Fake::private()
        };
        assert!(
            private_group(&listed, 1000, 1000)
                .unwrap_err()
                .contains("kim is a member")
        );

        let primary = Fake {
            primary: Some(vec!["sam", "kim"]),
            ..Fake::private()
        };
        assert!(
            private_group(&primary, 1000, 1000)
                .unwrap_err()
                .contains("kim has")
        );
    }

    #[test]
    fn a_private_group_fails_closed_when_membership_is_incomplete() {
        // Not upstream's: review 3 M8, an unlistable or unenumerable group refuses.
        let no_members = Fake {
            members: None,
            ..Fake::private()
        };
        assert!(
            private_group(&no_members, 1000, 1000)
                .unwrap_err()
                .contains("fully listed")
        );
        let no_primary = Fake {
            primary: None,
            ..Fake::private()
        };
        assert!(
            private_group(&no_primary, 1000, 1000)
                .unwrap_err()
                .contains("fully enumerated")
        );
        let no_user = Fake {
            user: None,
            ..Fake::private()
        };
        assert!(private_group(&no_user, 1000, 1000).is_err());
        let no_group = Fake {
            group: None,
            ..Fake::private()
        };
        assert!(private_group(&no_group, 1000, 1000).is_err());
        let no_primary_gid = Fake {
            primary_gid: None,
            ..Fake::private()
        };
        assert!(
            private_group(&no_primary_gid, 1000, 1000)
                .unwrap_err()
                .contains("primary group")
        );
    }

    #[test]
    fn the_group_must_be_the_users_primary_group() {
        // Not upstream's: a same-named group that is not the account's primary group is out.
        let elsewhere = Fake {
            primary_gid: Some(2000),
            ..Fake::private()
        };
        assert!(
            private_group(&elsewhere, 1000, 1000)
                .unwrap_err()
                .contains("not the primary group")
        );
    }

    #[test]
    fn the_local_files_are_parsed_strictly() {
        // Not upstream's: only /etc/passwd and /etc/group are read, and a line that can't be
        // understood makes the whole file unusable.
        let passwd =
            "# c\nroot:x:0:0:root:/root:/bin/sh\n\nsam:x:1000:1000:Sam,,,:/home/sam:/bin/sh\n";
        let entries = parse_passwd(passwd).expect("passwd");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1],
            PasswdEntry {
                name: "sam".into(),
                uid: 1000,
                gid: 1000
            }
        );
        for bad in [
            "+::::::\n",
            "-sam\n",
            "sam:x:notanumber:1000\n",
            "sam:x:1000\n",
            ":x:1:1\n",
        ] {
            assert!(parse_passwd(bad).is_none(), "{bad:?}");
        }

        let group = "sam:x:1000:\nstaff:x:50:sam,kim\nempty:x:60:,\n";
        let groups = parse_group(group).expect("group");
        assert_eq!(
            groups[0],
            GroupEntry {
                name: "sam".into(),
                gid: 1000,
                members: vec![]
            }
        );
        assert_eq!(groups[1].members, ["sam", "kim"]);
        assert!(groups[2].members.is_empty());
        for bad in ["+:::\n", "sam:x:abc:\n", "sam:x\n", "sam:x:1:a:b\n"] {
            assert!(parse_group(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn only_root_controlled_nsswitch_sources_let_the_local_account_files_stand_for_everyone() {
        // Not upstream's: a remote directory could add primary-group members the files lack.
        let ok = "# comment\npasswd:  files # trailing\ngroup: files\nhosts: files dns\n";
        assert!(nsswitch_local_only(ok));
        assert!(nsswitch_local_only("passwd: files files\ngroup:\tfiles\n"));
        assert!(nsswitch_local_only(
            "passwd: files systemd\ngroup: files systemd\n"
        ));
        assert!(nsswitch_local_only(
            "passwd: compat systemd\ngroup: compat systemd\n"
        ));
        assert!(nsswitch_local_only(
            "passwd: files [SUCCESS=merge] systemd\ngroup: files [NOTFOUND = return]\n"
        ));
        // initgroups: absent is fine, a local one is fine, a remote or ambiguous one is not.
        assert!(nsswitch_local_only(
            "passwd: files\ngroup: files\ninitgroups: files\n"
        ));
        assert!(nsswitch_local_only(
            "passwd: files\ngroup: files\ninitgroups: files [SUCCESS=continue] systemd\n"
        ));
        for bad in [
            "passwd: files\ngroup: files\ninitgroups: ldap\n",
            "passwd: files\ngroup: files\ninitgroups: files sss\n",
            "passwd: files\ngroup: files\ninitgroups:\n",
            "passwd: files\ngroup: files\ninitgroups: files\ninitgroups: nis\n",
            "passwd: files\ngroup: files\ninitgroups_compat: files\n",
            "passwd: files sss\ngroup: files\n",
            "passwd: files\ngroup: files ldap\n",
            "passwd: files nis\ngroup: files\n",
            "passwd: files winbind\ngroup: files\n",
            "passwd: files mymachines\ngroup: files\n",
            "passwd: db files\ngroup: files\n",
            "passwd: altfiles files\ngroup: files\n",
            "passwd: files [NOTFOUND=return] sss\ngroup: files\n",
            "passwd: [NOTFOUND=return]\ngroup: files\n",
            "passwd: files [NOTFOUND=return\ngroup: files\n",
            "passwd: compat\ngroup: compat\npasswd_compat: sss\n",
            "passwd: files\n",
            "group: files\n",
            "passwd:\ngroup: files\n",
            "passwd: files\ngroup: files\ngroup: files nis\n",
            "hosts: files\n",
            "",
        ] {
            assert!(!nsswitch_local_only(bad), "{bad:?}");
        }
    }

    /// A fake root with `etc/nsswitch.conf`, `etc/passwd` and `etc/group`; never the host's.
    fn account_root(nsswitch: Option<&str>, passwd: &str, group: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("root");
        let etc = root.path().join("etc");
        std::fs::create_dir(&etc).expect("etc");
        if let Some(text) = nsswitch {
            std::fs::write(etc.join("nsswitch.conf"), text).expect("nsswitch");
        }
        std::fs::write(etc.join("passwd"), passwd).expect("passwd");
        std::fs::write(etc.join("group"), group).expect("group");
        root
    }

    #[test]
    fn the_system_account_database_reads_fixture_files_and_obeys_nsswitch() {
        // Not upstream's: a rooted fixture, so the host's files are never read.
        let passwd = "sam:x:1000:1000::/home/sam:/bin/sh\n";
        let group = "sam:x:1000:\n";
        let files = account_root(Some("passwd: files\ngroup: files\n"), passwd, group);
        let db = SystemAccounts::rooted(files.path());
        let linux = cfg!(target_os = "linux");
        assert_eq!(db.user_name(1000), linux.then(|| "sam".to_string()));
        assert_eq!(db.group_members(1000), linux.then(Vec::new));
        assert_eq!(
            db.primary_members(1000),
            linux.then(|| vec!["sam".to_string()])
        );

        let remote = account_root(Some("passwd: files sss\ngroup: files sss\n"), passwd, group);
        let db = SystemAccounts::rooted(remote.path());
        assert!(db.user_name(1000).is_none());
        assert!(db.group_name(1000).is_none());
        assert!(db.primary_members(1000).is_none());

        let remote_initgroups = account_root(
            Some("passwd: files\ngroup: files\ninitgroups: ldap\n"),
            passwd,
            group,
        );
        let db = SystemAccounts::rooted(remote_initgroups.path());
        assert!(db.user_name(1000).is_none());
        assert!(db.group_members(1000).is_none());

        let local_initgroups = account_root(
            Some("passwd: files\ngroup: files\ninitgroups: files\n"),
            passwd,
            group,
        );
        let db = SystemAccounts::rooted(local_initgroups.path());
        assert_eq!(db.user_name(1000), linux.then(|| "sam".to_string()));

        let missing = account_root(None, passwd, group);
        assert!(
            SystemAccounts::rooted(missing.path())
                .user_name(1000)
                .is_none()
        );

        let nis = account_root(Some("passwd: compat\ngroup: compat\n"), "+::::::\n", group);
        assert!(SystemAccounts::rooted(nis.path()).user_name(1000).is_none());
    }

    #[test]
    fn the_system_account_database_only_answers_on_linux() {
        // Not upstream's: other systems keep the private-group exception off.
        if !cfg!(target_os = "linux") {
            let root = account_root(
                Some("passwd: files\ngroup: files\n"),
                "sam:x:1:1\n",
                "sam:x:1:\n",
            );
            assert!(SystemAccounts::rooted(root.path()).user_name(1).is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_nsswitch_disables_the_private_group_exception_on_musl_too() {
        // Not upstream's: Alpine may omit nsswitch.conf. Keep the exception off unless
        // the complete local-only membership policy can be established.
        let root = account_root(
            None,
            "sam:x:1000:1000::/home/sam:/bin/sh\n",
            "sam:x:1000:\n",
        );
        let db = SystemAccounts::rooted(root.path());
        assert!(db.user_name(1000).is_none());
        assert!(db.user_primary_gid(1000).is_none());
        assert!(db.group_name(1000).is_none());
        assert!(db.group_members(1000).is_none());
        assert!(db.primary_members(1000).is_none());
        assert!(private_group(&db, 1000, 1000).is_err());
    }

    fn euid() -> u32 {
        // SAFETY: `geteuid` has no preconditions.
        unsafe { libc::geteuid() }
    }

    /// A temp directory that the rules treat as the top of the trusted tree.
    struct Tree {
        root: tempfile::TempDir,
    }

    impl Tree {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().expect("temp dir"),
            }
        }
        fn path(&self) -> PathBuf {
            std::fs::canonicalize(self.root.path()).expect("canonical temp dir")
        }
        fn rules(&self, kind: ServiceKind, db: Fake) -> TrustRules {
            rules(kind, euid(), db, Some(self.path()))
        }
        fn dir(&self, name: &str, mode: u32) -> PathBuf {
            let path = self.path().join(name);
            std::fs::create_dir_all(&path).expect("create dir");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            path
        }
    }

    #[test]
    fn create_private_dir_makes_a_new_directory_0700_and_leaves_an_existing_one_alone() {
        // Not upstream's: a new directory is private whatever the umask; an existing 0755 one
        // (a stock user data directory) is not touched.
        let tree = Tree::new();
        let fresh = tree.path().join("fresh");
        create_private_dir(&fresh).expect("create");
        let mode = std::fs::metadata(&fresh)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o700);

        let existing = tree.dir("existing", 0o755);
        create_private_dir(&existing).expect("existing is fine");
        let mode = std::fs::metadata(&existing)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o755);

        create_private_dir(&fresh).expect("a second call is fine");
    }

    #[test]
    fn create_private_dir_refuses_a_link_a_file_and_a_missing_parent() {
        // Not upstream's: nothing is followed or replaced.
        let tree = Tree::new();
        let target = tree.dir("target", 0o700);
        let link = tree.path().join("link");
        symlink(&target, &link).expect("symlink");
        assert!(create_private_dir(&link).is_err());

        let file = tree.path().join("file");
        std::fs::write(&file, b"x").expect("file");
        assert!(create_private_dir(&file).is_err());

        assert!(create_private_dir(&tree.path().join("no/such/parent")).is_err());
    }

    #[test]
    fn a_private_chain_passes_and_a_group_writable_one_depends_on_the_rules() {
        // Not upstream's: the walk over a real directory chain.
        let tree = Tree::new();
        let data = tree.dir("data", 0o755);
        let versions = tree.dir("data/versions", 0o755);
        let system = tree.rules(ServiceKind::System, Fake::default());
        check_dirs(&[&data, &versions], &system).expect("clean chain");

        let shared = tree.dir("data/shared", 0o775);
        let err = check_dirs(&[&shared], &system).unwrap_err();
        assert!(err.reason.contains("group"), "{err}");
        let user = tree.rules(ServiceKind::User, Fake::private_here());
        check_dirs(&[&shared], &user).expect("private group may write");
        let user_shared = tree.rules(
            ServiceKind::User,
            Fake {
                group: Some("users"),
                ..Fake::private_here()
            },
        );
        assert!(check_dirs(&[&shared], &user_shared).is_err());
    }

    #[test]
    fn a_world_writable_directory_is_refused_even_deep_in_the_chain() {
        // Not upstream's: any component counts, not only the last.
        let tree = Tree::new();
        tree.dir("a", 0o777);
        let deep = tree.dir("a/b", 0o755);
        let err = check_dirs(
            &[&deep],
            &tree.rules(ServiceKind::User, Fake::private_here()),
        )
        .unwrap_err();
        assert!(err.path.ends_with("a"), "{err}");
        assert!(err.reason.contains("anyone"), "{err}");
    }

    #[test]
    fn a_symlink_in_the_chain_is_refused() {
        // Not upstream's: O_NOFOLLOW on every component.
        let tree = Tree::new();
        let real = tree.dir("real", 0o755);
        tree.dir("real/inner", 0o755);
        let link = tree.path().join("link");
        symlink(&real, &link).expect("symlink");
        let err = check_dirs(
            &[&link.join("inner")],
            &tree.rules(ServiceKind::System, Fake::default()),
        )
        .unwrap_err();
        assert!(err.reason.contains("symlink"), "{err}");
    }

    #[test]
    fn a_missing_directory_and_a_relative_path_are_refused() {
        // Not upstream's: both fail closed with a reason.
        let tree = Tree::new();
        let rules = tree.rules(ServiceKind::System, Fake::default());
        let err = check_dirs(&[&tree.path().join("nope")], &rules).unwrap_err();
        assert!(err.reason.contains("does not exist"), "{err}");
        assert!(check_dirs(&[Path::new("relative/dir")], &rules).is_err());
        assert!(check_dirs(&[&tree.path().join("a/../b")], &rules).is_err());
    }

    #[test]
    fn a_directory_owned_by_someone_else_is_refused() {
        // Not upstream's: the owner must be root or the service's uid.
        let tree = Tree::new();
        let dir = tree.dir("owned", 0o755);
        let rules_uid = if euid() == 0 {
            let c = cstring(dir.as_os_str()).expect("path");
            // SAFETY: valid NUL-terminated path; `u32::MAX` leaves the group unchanged.
            assert_eq!(
                unsafe { libc::chown(c.as_ptr(), 4242, u32::MAX) },
                0,
                "chown as root"
            );
            0
        } else {
            euid() + 1
        };
        let rules = rules(
            ServiceKind::System,
            rules_uid,
            Fake::default(),
            Some(tree.path()),
        );
        let err = check_dirs(&[&dir], &rules).unwrap_err();
        assert!(err.reason.contains("owned by uid"), "{err}");
    }

    #[test]
    fn a_file_must_be_regular_and_not_writable_by_others() {
        // Not upstream's: the binary itself is checked, not only its directory.
        let tree = Tree::new();
        let dir = tree.dir("bin", 0o755);
        let rules = tree.rules(ServiceKind::System, Fake::default());
        let file = dir.join("open-ferry");
        std::fs::write(&file, b"x").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        check_file(&file, &rules).expect("clean file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o757)).expect("chmod");
        assert!(
            check_file(&file, &rules)
                .unwrap_err()
                .reason
                .contains("anyone")
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o775)).expect("chmod");
        assert!(
            check_file(&file, &rules)
                .unwrap_err()
                .reason
                .contains("group")
        );

        let link = dir.join("link");
        symlink(&file, &link).expect("symlink");
        assert!(
            check_file(&link, &rules)
                .unwrap_err()
                .reason
                .contains("regular file")
        );
        assert!(check_file(&dir.join("missing"), &rules).is_err());
    }

    /// Sets an access ACL that gives `other_uid` read and write, with the mask at rwx, by
    /// writing the kernel's binary xattr format directly. False when the filesystem refuses
    /// ACLs, so a test can skip.
    #[cfg(target_os = "linux")]
    fn give_another_uid_write(path: &Path, other_uid: u32) -> bool {
        const UNDEFINED: u32 = u32::MAX;
        let entries: [(u16, u16, u32); 5] = [
            (0x01, 0o7, UNDEFINED), // user::rwx
            (0x02, 0o6, other_uid), // user:<other>:rw-
            (0x04, 0o7, UNDEFINED), // group::rwx
            (0x10, 0o7, UNDEFINED), // mask::rwx
            (0x20, 0o5, UNDEFINED), // other::r-x
        ];
        let mut value = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in entries {
            value.extend_from_slice(&tag.to_le_bytes());
            value.extend_from_slice(&perm.to_le_bytes());
            value.extend_from_slice(&id.to_le_bytes());
        }
        let c = cstring(path.as_os_str()).expect("path");
        // SAFETY: the path and name are NUL-terminated and `value` is valid for its length.
        let rc = unsafe {
            libc::setxattr(
                c.as_ptr(),
                c"system.posix_acl_access".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        rc == 0
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_named_other_user_in_an_acl_defeats_a_genuinely_private_group() {
        // Not upstream's: the group bits are only the ACL mask, so a private group with a
        // named other user's write entry is refused, for a directory and for a file.
        let tree = Tree::new();
        let user = tree.rules(ServiceKind::User, Fake::private_here());
        let dir = tree.dir("acl-dir", 0o775);
        check_dirs(&[&dir], &user).expect("private group passes without an ACL");
        let file = dir.join("acl-file");
        std::fs::write(&file, b"x").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o775)).expect("chmod");
        check_file(&file, &user).expect("private group passes without an ACL");

        if !give_another_uid_write(&dir, 4242) || !give_another_uid_write(&file, 4242) {
            eprintln!("skipped: this filesystem does not take POSIX ACLs");
            return;
        }
        let err = check_dirs(&[&dir], &user).unwrap_err();
        assert!(err.reason.contains("extended ACL"), "{err}");
        let err = check_file(&file, &user).unwrap_err();
        assert!(err.reason.contains("extended ACL"), "{err}");
    }

    #[cfg(target_os = "macos")]
    fn macos_acl(path: &Path, text: &str) {
        assert!(
            std::process::Command::new("chmod")
                .args(["+a", text])
                .arg(path)
                .status()
                .expect("chmod +a")
                .success()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_refuses_each_mutation_grant_without_a_group_write_bit() {
        // Not upstream's: Darwin ACLs add rights independently of the mode, including
        // permissions to replace a child or change who can write it later.
        for permission in [
            "add_file",
            "add_subdirectory",
            "delete_child",
            "delete",
            "writeattr",
            "writeextattr",
            "writesecurity",
            "chown",
        ] {
            let tree = Tree::new();
            let dir = tree.dir("acl-dir", 0o755);
            let rules = tree.rules(ServiceKind::User, Fake::private_here());
            check_dirs(&[&dir], &rules).expect("plain directory");
            macos_acl(&dir, &format!("everyone allow {permission}"));
            let error = check_dirs(&[&dir], &rules).unwrap_err();
            assert!(
                error.reason.contains("extended ACL"),
                "{permission}: {error}"
            );
            let deep = tree.dir("acl-dir/child", 0o700);
            assert_eq!(check_dirs(&[&deep], &rules).unwrap_err().path, dir);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_refuses_file_and_group_acl_grants_and_inherit_only_grants() {
        // Not upstream's: files, groups and rights inherited by future children count too.
        let tree = Tree::new();
        let dir = tree.dir("acl-dir", 0o755);
        let rules = tree.rules(ServiceKind::User, Fake::private_here());
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        macos_acl(&file, "everyone allow write");
        assert!(
            check_file(&file, &rules)
                .unwrap_err()
                .reason
                .contains("extended ACL")
        );
        macos_acl(&dir, "group:staff allow add_file");
        assert!(check_dirs(&[&dir], &rules).is_err());

        let inherited = tree.dir("inherited", 0o700);
        macos_acl(
            &inherited,
            "everyone allow file_inherit,directory_inherit,only_inherit,write",
        );
        assert!(check_dirs(&[&inherited], &rules).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allows_read_grants_deny_entries_and_owner_or_root_mutation() {
        // Not upstream's: common read-only and deny ACLs do not disable switching.
        let tree = Tree::new();
        let dir = tree.dir("acl-dir", 0o755);
        let rules = tree.rules(ServiceKind::User, Fake::private_here());
        macos_acl(
            &dir,
            "everyone allow read,readattr,readextattr,readsecurity",
        );
        macos_acl(&dir, "everyone deny write");
        macos_acl(&dir, &format!("user:{} allow write,writesecurity", euid()));
        macos_acl(&dir, "user:root allow write");
        check_dirs(&[&dir], &rules).expect("ACL only grants mutations to owner and root");
    }
}
