//! The Unix endpoint. See the parent module for the rules; the walk itself is in
//! `trust::unix`, shared with the data-directory check.
#![allow(clippy::unnecessary_cast)]

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use super::{EndpointError, Peer, ServiceKind};
use crate::process::SupervisorLock;
use crate::trust::unix::{
    TrustRules, WalkError, check_fd_acl, cstring, file_type, fstat, fstatat_nofollow, open_dir_at,
    walk,
};

/// The socket's file name inside the endpoint directory.
const SOCKET_FILE: &str = "supervisor.sock";

/// `sun_path` is 104 bytes on macOS and 108 on Linux, and holds a trailing NUL.
const MAX_SOCKET_PATH: usize = 103;

pub type ServerStream = UnixStream;
pub type ClientStream = UnixStream;

/// Whose endpoint to serve or to trust.
#[derive(Debug, Clone)]
pub struct Identity {
    kind: ServiceKind,
    uid: u32,
}

impl Identity {
    /// The identity of the running process: a user service expects the effective uid, a system
    /// service expects root.
    pub fn current(kind: ServiceKind) -> io::Result<Self> {
        let uid = match kind {
            ServiceKind::User => {
                // SAFETY: `geteuid` has no preconditions.
                unsafe { libc::geteuid() }
            }
            ServiceKind::System => 0,
        };
        Ok(Self { kind, uid })
    }

    /// An identity for a given uid, for a client that runs as someone else than the service.
    pub fn for_uid(kind: ServiceKind, uid: u32) -> Self {
        Self { kind, uid }
    }

    pub fn kind(&self) -> ServiceKind {
        self.kind
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    fn rules(&self) -> TrustRules {
        TrustRules::for_identity(self)
    }
}

/// Where the socket lives: `<root>/<name>/supervisor.sock`. The root is a runtime directory the
/// system provides; it is resolved once with `realpath`, and nothing below it may be a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    root: PathBuf,
    name: OsString,
}

impl Location {
    /// A location chosen by the caller. `root` must be absolute and `name` one path component.
    /// The policy applies all the same when the location is bound or connected to.
    pub fn new(root: impl Into<PathBuf>, name: impl Into<OsString>) -> Result<Self, EndpointError> {
        let root = root.into();
        let name = name.into();
        // The raw name must be its own sole component: `Path::components` forgives a trailing
        // `/`, but `openat` would follow a symlink named `link/`.
        let mut parts = Path::new(&name).components();
        let single = matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(only)), None) if only == name.as_os_str()
        );
        if !single || !root.is_absolute() {
            return Err(EndpointError::Unsafe(format!(
                "{} / {} is not an absolute root and a single directory name",
                root.display(),
                name.to_string_lossy()
            )));
        }
        Ok(Self { root, name })
    }

    /// The first location, in the design's order, whose path chain validates. The data
    /// directory plays no part on Unix.
    pub fn resolve(identity: &Identity, _data_dir: &Path) -> Result<Self, EndpointError> {
        let mut skipped = Vec::new();
        for location in candidates(identity) {
            match open_endpoint_dir(&location, identity, false) {
                Ok(_) => return Ok(location),
                Err(Fail::Skip(why)) => skipped.push(why),
                Err(Fail::Unsafe(why)) => return Err(EndpointError::Unsafe(why)),
                Err(Fail::Io(e)) => return Err(EndpointError::Io(e)),
            }
        }
        Err(EndpointError::SwitchingDisabled(format!(
            "no endpoint location passes the path policy ({})",
            skipped.join("; ")
        )))
    }

    /// The text for `supervisor.json`'s `endpoint` field: the socket's path.
    pub fn to_record(&self) -> String {
        self.socket_path().to_string_lossy().into_owned()
    }

    /// Reads back what [`Location::to_record`] wrote. The path is not trusted by this call;
    /// validation happens at [`connect`].
    pub fn from_record(record: &str) -> Result<Self, EndpointError> {
        let socket = Path::new(record);
        let shaped = socket.file_name() == Some(OsStr::new(SOCKET_FILE));
        let dir = socket.parent().filter(|_| shaped);
        let name = dir.and_then(Path::file_name);
        let root = dir.and_then(Path::parent);
        match (root, name) {
            (Some(root), Some(name)) => Self::new(root, name),
            _ => Err(EndpointError::Unsafe(format!(
                "{record} is not a supervisor socket path"
            ))),
        }
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(&self.name)
    }

    pub fn socket_path(&self) -> PathBuf {
        self.dir().join(SOCKET_FILE)
    }
}

/// Locations in the order the design tries them.
fn candidates(identity: &Identity) -> Vec<Location> {
    let roots = match identity.kind {
        ServiceKind::System => system_roots(),
        ServiceKind::User => user_roots(identity.uid),
    };
    roots
        .into_iter()
        .filter_map(|(root, name)| Location::new(root, name).ok())
        .collect()
}

#[cfg(target_os = "macos")]
fn system_roots() -> Vec<(PathBuf, String)> {
    vec![(PathBuf::from("/var/run"), "open-ferry".to_string())]
}

#[cfg(not(target_os = "macos"))]
fn system_roots() -> Vec<(PathBuf, String)> {
    vec![(PathBuf::from("/run"), "open-ferry".to_string())]
}

#[cfg(target_os = "macos")]
fn user_roots(_uid: u32) -> Vec<(PathBuf, String)> {
    darwin_user_temp_dir()
        .map(|dir| vec![(dir, "open-ferry".to_string())])
        .unwrap_or_default()
}

#[cfg(not(target_os = "macos"))]
fn user_roots(uid: u32) -> Vec<(PathBuf, String)> {
    vec![
        (
            PathBuf::from(format!("/run/user/{uid}")),
            "open-ferry".to_string(),
        ),
        (PathBuf::from("/tmp"), format!("open-ferry-{uid}")),
    ]
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

/// Why a location can't be used.
#[derive(Debug)]
enum Fail {
    /// The location is not usable here and the next one may be (missing root, a path chain the
    /// policy refuses, a path too long for `sun_path`).
    Skip(String),
    /// The location exists and is wrong: a squatter, a link, the wrong owner or mode.
    Unsafe(String),
    Io(io::Error),
}

impl Fail {
    fn for_server(self) -> EndpointError {
        match self {
            Fail::Skip(why) => EndpointError::SwitchingDisabled(why),
            Fail::Unsafe(why) => EndpointError::Unsafe(why),
            Fail::Io(e) => EndpointError::Io(e),
        }
    }

    fn for_client(self) -> EndpointError {
        match self {
            Fail::Skip(why) | Fail::Unsafe(why) => EndpointError::Unsafe(why),
            Fail::Io(e) => EndpointError::Io(e),
        }
    }
}

/// An endpoint directory that passed the checks, held open by descriptor.
struct Opened {
    dir: OwnedFd,
    /// The socket's path below the resolved root.
    socket: PathBuf,
}

/// Validates the chain to `location` and the endpoint directory itself. With `create`, a
/// missing directory is made (0700). Without it, a missing directory is `Ok(None)`.
fn open_endpoint_dir(
    location: &Location,
    identity: &Identity,
    create: bool,
) -> Result<Option<Opened>, Fail> {
    let rules = identity.rules();
    let root = std::fs::canonicalize(&location.root).map_err(|e| {
        Fail::Skip(format!(
            "{} can't be resolved: {e}",
            location.root.display()
        ))
    })?;
    let socket = root.join(&location.name).join(SOCKET_FILE);
    if socket.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(Fail::Skip(format!(
            "{} is longer than a socket path may be ({MAX_SOCKET_PATH} bytes)",
            socket.display()
        )));
    }
    let root_fd = walk(&rules, &root, true).map_err(|e| match e {
        WalkError::Missing(path) => Fail::Skip(format!("{} does not exist", path.display())),
        WalkError::Untrusted(path, why) => Fail::Skip(format!("{}: {why}", path.display())),
        WalkError::Io(_, e) => Fail::Io(e),
    })?;

    let name = location.name.as_os_str();
    let shown = root.join(name);
    let mut created = false;
    match fstatat_nofollow(root_fd.as_raw_fd(), name) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if !create {
                return Ok(None);
            }
            mkdirat(root_fd.as_raw_fd(), name, 0o700).map_err(Fail::Io)?;
            created = true;
        }
        Err(e) => return Err(Fail::Io(e)),
    }
    let dir = match open_dir_at(root_fd.as_raw_fd(), name) {
        Ok(fd) => fd,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
            return Err(Fail::Unsafe(format!(
                "{} is a symlink or not a directory",
                shown.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Fail::Unsafe(format!(
                "{} vanished while it was checked",
                shown.display()
            )));
        }
        Err(e) => return Err(Fail::Io(e)),
    };
    let st = fstat(dir.as_raw_fd()).map_err(Fail::Io)?;
    if st.st_uid as u32 != identity.uid {
        return Err(Fail::Unsafe(format!(
            "{} is owned by uid {}, expected {}",
            shown.display(),
            st.st_uid,
            identity.uid
        )));
    }
    let mut mode = st.st_mode as u32 & 0o7777;
    if mode != 0o700 && created {
        // SAFETY: `dir` is an open descriptor.
        if unsafe { libc::fchmod(dir.as_raw_fd(), 0o700) } != 0 {
            return Err(Fail::Io(io::Error::last_os_error()));
        }
        mode = 0o700;
    }
    if mode != 0o700 {
        return Err(Fail::Unsafe(format!(
            "{} has mode {mode:04o}, expected 0700",
            shown.display()
        )));
    }
    check_fd_acl(dir.as_raw_fd(), st.st_uid as u32)
        .map_err(|why| Fail::Unsafe(format!("{}: {why}", shown.display())))?;
    Ok(Some(Opened { dir, socket }))
}

fn mkdirat(dir: RawFd, name: &OsStr, mode: libc::mode_t) -> io::Result<()> {
    let name = cstring(name)?;
    // SAFETY: `name` is NUL-terminated and outlives the call.
    if unsafe { libc::mkdirat(dir, name.as_ptr(), mode) } != 0 {
        let e = io::Error::last_os_error();
        // Lost a race with another creator: the checks that follow judge what is there.
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    Ok(())
}

fn unlinkat(dir: RawFd, name: &str) -> io::Result<()> {
    let name = cstring(OsStr::new(name))?;
    // SAFETY: `name` is NUL-terminated and outlives the call.
    if unsafe { libc::unlinkat(dir, name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What a probe connect to a socket path found.
enum Probe {
    /// Something is listening (or its backlog is full).
    Live,
    /// Nothing listens: a stale file.
    Refused,
}

/// A non-blocking connect attempt, so that a listener with a full backlog can't hang startup.
fn probe_connect(path: &Path) -> io::Result<Probe> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: an all-zero `sockaddr_un` is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    // SAFETY: plain socket creation; the result is wrapped at once.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a freshly created descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: `fd` is open; these only set flags on it.
    let flagged = unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) == 0
            && libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) == 0
    };
    if !flagged {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `addr` is a valid `sockaddr_un` and the length passed is its size.
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        return Ok(Probe::Live);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ECONNREFUSED) => Ok(Probe::Refused),
        Some(libc::EAGAIN) | Some(libc::EINPROGRESS) => Ok(Probe::Live),
        _ => Err(e),
    }
}

/// Which file system object a socket entry is: device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketId {
    dev: u64,
    ino: u64,
}

impl SocketId {
    fn of(st: &libc::stat) -> Self {
        Self {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        }
    }
}

/// Removes the socket entry only while it is still the object `expected`, so a replacement
/// someone else bound in its place is left alone. The directory is private, so the window
/// between the check and the unlink belongs to the same uid at most.
fn unlink_socket_if(dir: RawFd, expected: SocketId) -> io::Result<()> {
    let st = fstatat_nofollow(dir, OsStr::new(SOCKET_FILE))?;
    if file_type(&st) == libc::S_IFSOCK as u32 && SocketId::of(&st) == expected {
        unlinkat(dir, SOCKET_FILE)
    } else {
        Ok(())
    }
}

/// The lock file that serializes every supervisor of one endpoint, whatever data directory its
/// `SupervisorLock` guards.
const NAMESPACE_LOCK_FILE: &str = "supervisor.lock";

/// Takes the endpoint's namespace lock: a regular 0600 file owned by the expected uid in the
/// private endpoint directory, opened without following links, held with `flock` for as long as
/// the returned descriptor lives. Another holder is [`EndpointError::Taken`].
fn lock_namespace(dir: &OwnedFd, identity: &Identity) -> Result<OwnedFd, EndpointError> {
    let name = cstring(OsStr::new(NAMESPACE_LOCK_FILE))?;
    // SAFETY: `name` is NUL-terminated and outlives the call; the result is wrapped at once.
    let raw = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR
                | libc::O_CREAT
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_NOCTTY
                | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if raw < 0 {
        let e = io::Error::last_os_error();
        return Err(if e.raw_os_error() == Some(libc::ELOOP) {
            EndpointError::Unsafe(format!("{NAMESPACE_LOCK_FILE} is a symlink"))
        } else {
            EndpointError::Io(e)
        });
    }
    // SAFETY: `raw` is a freshly opened descriptor that nothing else owns.
    let file = unsafe { OwnedFd::from_raw_fd(raw) };
    let st = fstat(file.as_raw_fd())?;
    if file_type(&st) != libc::S_IFREG as u32 || st.st_uid as u32 != identity.uid {
        return Err(EndpointError::Unsafe(format!(
            "{NAMESPACE_LOCK_FILE} is not a regular file owned by uid {}",
            identity.uid
        )));
    }
    // SAFETY: `file` is an open descriptor of a file we own, so fchmod cannot touch another.
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
        return Err(EndpointError::Io(io::Error::last_os_error()));
    }
    // SAFETY: `file` is open.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return Err(if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            EndpointError::Taken(
                "another supervisor holds this endpoint's namespace lock".to_string(),
            )
        } else {
            EndpointError::Io(e)
        });
    }
    Ok(file)
}

/// Checks the socket entry in the endpoint directory. `strict_mode` also requires 0600.
fn check_socket_entry(
    opened: &Opened,
    identity: &Identity,
    strict_mode: bool,
) -> Result<Option<SocketId>, Fail> {
    let st = match fstatat_nofollow(opened.dir.as_raw_fd(), OsStr::new(SOCKET_FILE)) {
        Ok(st) => st,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Fail::Io(e)),
    };
    let shown = opened.socket.display();
    if file_type(&st) != libc::S_IFSOCK as u32 {
        return Err(Fail::Unsafe(format!("{shown} exists and is not a socket")));
    }
    if st.st_uid as u32 != identity.uid {
        return Err(Fail::Unsafe(format!(
            "{shown} is owned by uid {}, expected {}",
            st.st_uid, identity.uid
        )));
    }
    let mode = st.st_mode as u32 & 0o777;
    if strict_mode && mode != 0o600 {
        return Err(Fail::Unsafe(format!(
            "{shown} has mode {mode:04o}, expected 0600"
        )));
    }
    Ok(Some(SocketId::of(&st)))
}

/// The serving side. Dropping it removes the socket, if it is still the one this server bound,
/// and then releases the namespace lock.
pub struct Server {
    listener: UnixListener,
    dir: OwnedFd,
    socket: PathBuf,
    id: SocketId,
    uid: u32,
    rejected: u64,
    /// Declared last so it is released after the socket is removed.
    _namespace: OwnedFd,
}

/// One accepted connection and who is on the other end.
pub struct Connection {
    pub stream: ServerStream,
    pub peer: Peer,
}

impl Server {
    /// Creates the directory if needed, validates it, removes a stale socket, binds, and
    /// fixes the socket's mode to 0600. Must run inside a tokio runtime.
    ///
    /// A socket left by a crash is removed only because `lock` proves this process is the one
    /// supervisor of its data directory, because this process also holds the endpoint's
    /// namespace lock (several data directories can share one per-uid endpoint), only after a
    /// connect attempt is refused, only when it is a socket owned by the expected uid, and only
    /// while it is still the same inode that was checked. A live socket is
    /// [`EndpointError::Taken`]. The namespace lock is kept for the life of the [`Server`].
    pub fn bind(
        location: &Location,
        identity: &Identity,
        _lock: &SupervisorLock,
    ) -> Result<Self, EndpointError> {
        let opened = open_endpoint_dir(location, identity, true)
            .map_err(Fail::for_server)?
            .ok_or_else(|| EndpointError::Io(io::Error::from(io::ErrorKind::NotFound)))?;
        let namespace = lock_namespace(&opened.dir, identity)?;

        if let Some(stale) =
            check_socket_entry(&opened, identity, false).map_err(Fail::for_server)?
        {
            match probe_connect(&opened.socket) {
                Ok(Probe::Live) => {
                    return Err(EndpointError::Taken(format!(
                        "a supervisor is already serving {}",
                        opened.socket.display()
                    )));
                }
                Ok(Probe::Refused) => unlink_socket_if(opened.dir.as_raw_fd(), stale)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(EndpointError::Io(e)),
            }
        }

        let std_listener = match std::os::unix::net::UnixListener::bind(&opened.socket) {
            Ok(listener) => listener,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                return Err(EndpointError::Taken(format!(
                    "{} was taken while binding",
                    opened.socket.display()
                )));
            }
            Err(e) => return Err(EndpointError::Io(e)),
        };
        // From here the socket file is ours; drop it again if it does not end up right. What
        // we bound is recorded by inode, so only that object is ever removed.
        let bound = fstatat_nofollow(opened.dir.as_raw_fd(), OsStr::new(SOCKET_FILE))
            .map_err(EndpointError::Io)?;
        let id = SocketId::of(&bound);
        let finish = || -> Result<(), EndpointError> {
            let name = cstring(OsStr::new(SOCKET_FILE))?;
            // SAFETY: `name` is NUL-terminated; the directory is open and private (0700), so
            // nobody else can swap the entry between the bind and this call.
            if unsafe { libc::fchmodat(opened.dir.as_raw_fd(), name.as_ptr(), 0o600, 0) } != 0 {
                return Err(EndpointError::Io(io::Error::last_os_error()));
            }
            let checked = check_socket_entry(&opened, identity, true)
                .map_err(Fail::for_server)?
                .ok_or_else(|| {
                    EndpointError::Unsafe("the socket vanished after bind".to_string())
                })?;
            if checked != id {
                return Err(EndpointError::Unsafe(
                    "the socket was replaced right after bind".to_string(),
                ));
            }
            std_listener.set_nonblocking(true)?;
            Ok(())
        };
        if let Err(e) = finish() {
            let _ = unlink_socket_if(opened.dir.as_raw_fd(), id);
            return Err(e);
        }
        let listener = match UnixListener::from_std(std_listener) {
            Ok(listener) => listener,
            Err(e) => {
                let _ = unlink_socket_if(opened.dir.as_raw_fd(), id);
                return Err(EndpointError::Io(e));
            }
        };
        Ok(Self {
            listener,
            dir: opened.dir,
            socket: opened.socket,
            id,
            uid: identity.uid,
            rejected: 0,
            _namespace: namespace,
        })
    }

    /// Waits for a connection from the expected uid. A peer with another uid, or one whose
    /// credentials can't be read, is dropped and counted, and the wait goes on.
    pub async fn accept(&mut self) -> io::Result<Connection> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let cred = match stream.peer_cred() {
                Ok(cred) if cred.uid() == self.uid => cred,
                _ => {
                    self.rejected += 1;
                    continue;
                }
            };
            return Ok(Connection {
                stream,
                peer: Peer {
                    pid: cred.pid().and_then(|pid| u32::try_from(pid).ok()),
                    uid: Some(cred.uid()),
                },
            });
        }
    }

    /// How many connections [`Server::accept`] dropped for a wrong or unreadable uid.
    pub fn rejected_peers(&self) -> u64 {
        self.rejected
    }

    /// The socket's path below the resolved runtime root.
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Remove only the socket this server bound, not a replacement; a failure here is not
        // an error.
        let _ = unlink_socket_if(self.dir.as_raw_fd(), self.id);
    }
}

/// Connects to the endpoint after validating the path chain, the directory, and the socket,
/// and checks that the server's uid is the expected one.
pub async fn connect(
    location: &Location,
    identity: &Identity,
) -> Result<ClientStream, EndpointError> {
    let opened = open_endpoint_dir(location, identity, false)
        .map_err(Fail::for_client)?
        .ok_or_else(|| EndpointError::Io(io::Error::from(io::ErrorKind::NotFound)))?;
    check_socket_entry(&opened, identity, true)
        .map_err(Fail::for_client)?
        .ok_or_else(|| EndpointError::Io(io::Error::from(io::ErrorKind::NotFound)))?;
    let stream = UnixStream::connect(&opened.socket).await?;
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == identity.uid => Ok(stream),
        Ok(cred) => Err(EndpointError::PeerRejected(format!(
            "the server runs as uid {}, expected {}",
            cred.uid(),
            identity.uid
        ))),
        Err(e) => Err(EndpointError::PeerRejected(format!(
            "the server's credentials can't be read: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_refuses_an_acl_grant_even_on_a_0700_endpoint_directory() {
        // Not upstream's: endpoint directories need the ACL check too, not only their root.
        let data = tempfile::tempdir().expect("data");
        let lock = SupervisorLock::try_acquire(data.path()).expect("lock");
        let identity = Identity::current(ServiceKind::User).expect("identity");
        let location =
            Location::new("/tmp", format!("ofp-acl-{}", std::process::id())).expect("location");
        let server = Server::bind(&location, &identity, &lock).expect("safe endpoint");
        drop(server);
        assert!(
            std::process::Command::new("chmod")
                .args(["+a", "everyone allow add_file"])
                .arg(location.dir())
                .status()
                .expect("chmod")
                .success()
        );
        let error = Server::bind(&location, &identity, &lock)
            .err()
            .expect("refused");
        assert!(
            matches!(error, EndpointError::Unsafe(ref why) if why.contains("extended ACL")),
            "{error}"
        );
        std::fs::remove_dir_all(location.dir()).expect("cleanup");
    }

    #[test]
    fn a_location_needs_an_absolute_root_and_one_name() {
        // Not upstream's: the shape of a location.
        assert!(Location::new("/tmp", "open-ferry-1").is_ok());
        assert!(Location::new("relative", "x").is_err());
        assert!(Location::new("/tmp", "a/b").is_err());
        assert!(Location::new("/tmp", "..").is_err());
        assert!(Location::new("/tmp", "").is_err());
        // A trailing separator or a dot would make `openat` follow a link of that name.
        assert!(Location::new("/tmp", "link/").is_err());
        assert!(Location::new("/tmp", "link/.").is_err());
        assert!(Location::new("/tmp", ".").is_err());
        assert!(Location::new("/tmp", "./x").is_err());
    }

    #[test]
    fn a_record_round_trips_and_a_foreign_path_is_refused() {
        // Not upstream's: supervisor.json's endpoint field.
        let location = Location::new("/run/user/1000", "open-ferry").expect("location");
        assert_eq!(
            location.to_record(),
            "/run/user/1000/open-ferry/supervisor.sock"
        );
        assert_eq!(
            Location::from_record(&location.to_record()).expect("parse"),
            location
        );
        assert!(Location::from_record("/run/user/1000/open-ferry/other.sock").is_err());
        assert!(Location::from_record("supervisor.sock").is_err());
        assert!(Location::from_record("/supervisor.sock").is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn candidates_follow_the_designs_order() {
        // Not upstream's: v3 10.1 locations on Linux.
        let user = candidates(&Identity::for_uid(ServiceKind::User, 1234));
        let records: Vec<String> = user.iter().map(Location::to_record).collect();
        assert_eq!(
            records,
            [
                "/run/user/1234/open-ferry/supervisor.sock",
                "/tmp/open-ferry-1234/supervisor.sock"
            ]
        );
        let system = candidates(&Identity::for_uid(ServiceKind::System, 0));
        assert_eq!(
            system.iter().map(Location::to_record).collect::<Vec<_>>(),
            ["/run/open-ferry/supervisor.sock"]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_candidates_use_the_system_provided_temp_directory_and_var_run() {
        // Not upstream's: exercise confstr and validate the actual user's endpoint ancestry.
        let identity = Identity::current(ServiceKind::User).expect("identity");
        let temp = darwin_user_temp_dir().expect("Darwin user temp directory");
        let location =
            Location::resolve(&identity, Path::new("/unused")).expect("safe runtime root");
        assert_eq!(location.root, temp);
        assert_eq!(location.name, "open-ferry");
        let system = candidates(&Identity::for_uid(ServiceKind::System, 0));
        // Only the candidate string is checked: machine-wide endpoints are refused on macOS.
        assert_eq!(system[0].to_record(), "/var/run/open-ferry/supervisor.sock");
    }

    #[test]
    fn a_path_too_long_for_sun_path_skips_the_location() {
        // Not upstream's: the length is checked at start.
        let long = "x".repeat(120);
        let location = Location::new("/tmp", long).expect("location");
        let identity = Identity::current(ServiceKind::User).expect("identity");
        match open_endpoint_dir(&location, &identity, false) {
            Err(Fail::Skip(why)) => assert!(why.contains("longer"), "{why}"),
            other => panic!("expected a skip, got {:?}", other.map(|_| ())),
        }
    }
}
