//! The Windows endpoint: a named pipe whose DACL names only the service's account.
//! See the parent module for the rules.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};

use super::{EndpointError, Peer, ServiceKind};
use crate::process::SupervisorLock;
use crate::trust::windows::{SID_SYSTEM, SecurityDescriptor, current_user_sid, user_sid_of_pid};

const PIPE_PREFIX: &str = r"\\.\pipe\open-ferry-supervisor-";
const HASH_HEX_LEN: usize = 16;

/// A client retries a busy pipe this many times, 50 ms apart.
const BUSY_RETRIES: u32 = 20;

pub type ServerStream = NamedPipeServer;
pub type ClientStream = NamedPipeClient;

/// Whose endpoint to serve or to trust.
#[derive(Debug, Clone)]
pub struct Identity {
    kind: ServiceKind,
    sid: String,
}

impl Identity {
    /// A user service is the running user's SID; a system service is LocalSystem.
    pub fn current(kind: ServiceKind) -> io::Result<Self> {
        let sid = match kind {
            ServiceKind::User => current_user_sid()?,
            ServiceKind::System => SID_SYSTEM.to_string(),
        };
        Ok(Self { kind, sid })
    }

    /// An identity for a given SID string, for a client that runs as another account.
    pub fn for_sid(kind: ServiceKind, sid: impl Into<String>) -> Self {
        Self {
            kind,
            sid: sid.into(),
        }
    }

    pub fn kind(&self) -> ServiceKind {
        self.kind
    }

    /// The SID, as `S-1-5-...`, that must own the serving process.
    pub fn sid(&self) -> &str {
        &self.sid
    }

    /// The pipe's DACL: the user alone, or SYSTEM plus Administrators.
    pub(crate) fn pipe_sddl(&self) -> String {
        match self.kind {
            ServiceKind::User => format!("D:P(A;;GA;;;{})", self.sid),
            ServiceKind::System => "D:P(A;;GA;;;SY)(A;;GA;;;BA)".to_string(),
        }
    }
}

/// The pipe's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    name: String,
}

impl Location {
    /// The pipe for a data directory: `\\.\pipe\open-ferry-supervisor-<16 hex of sha256>`.
    ///
    /// The path is normalized first (verbatim prefix and trailing separators dropped, `/` as
    /// `\`, lower case), so two spellings of one directory share a pipe.
    pub fn for_data_dir(data_dir: &Path) -> Self {
        let text = data_dir.to_string_lossy();
        let text = text
            .strip_prefix(r"\\?\")
            .unwrap_or(&text)
            .replace('/', r"\");
        let normalized = text.trim_end_matches('\\').to_lowercase();
        let digest = Sha256::digest(normalized.as_bytes());
        let hex: String = digest
            .iter()
            .flat_map(|b| [hex_digit(b >> 4), hex_digit(b & 0xf)])
            .collect();
        Self {
            name: format!("{PIPE_PREFIX}{}", &hex[..HASH_HEX_LEN]),
        }
    }

    /// Always the data directory's pipe. A squatted name shows up at [`Server::bind`].
    pub fn resolve(_identity: &Identity, data_dir: &Path) -> Result<Self, EndpointError> {
        Ok(Self::for_data_dir(data_dir))
    }

    pub fn pipe_name(&self) -> &str {
        &self.name
    }

    /// The text for `supervisor.json`'s `endpoint` field: the pipe's name.
    pub fn to_record(&self) -> String {
        self.name.clone()
    }

    /// Reads back [`Location::to_record`]. Only a name of our own shape is accepted, so a record
    /// can't point a client at some other pipe.
    pub fn from_record(record: &str) -> Result<Self, EndpointError> {
        let tail = record.strip_prefix(PIPE_PREFIX).unwrap_or("");
        let shaped = tail.len() == HASH_HEX_LEN
            && tail
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !shaped {
            return Err(EndpointError::Unsafe(format!(
                "{record} is not a supervisor pipe name"
            )));
        }
        Ok(Self {
            name: record.to_string(),
        })
    }
}

fn hex_digit(nibble: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(nibble & 0xf)])
}

/// One accepted connection and who is on the other end. `peer.uid` is always `None` here;
/// `peer.pid` is the client process, from `GetNamedPipeClientProcessId`.
pub struct Connection {
    pub stream: ServerStream,
    pub peer: Peer,
}

/// The serving side. It keeps one pipe instance listening at all times.
pub struct Server {
    next: NamedPipeServer,
    name: String,
    descriptor: SecurityDescriptor,
}

fn create_instance(
    name: &str,
    descriptor: &SecurityDescriptor,
    first: bool,
) -> io::Result<NamedPipeServer> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .access_inbound(true)
        .access_outbound(true);
    // SAFETY: `attributes` is a valid SECURITY_ATTRIBUTES whose descriptor outlives the call.
    unsafe {
        options.create_with_security_attributes_raw(
            name,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
}

impl Server {
    /// Creates the first instance of the pipe. If the name already exists, whether made by a
    /// squatter or by a second supervisor, that is [`EndpointError::Taken`]: a startup error.
    /// Must run inside a tokio runtime.
    pub fn bind(
        location: &Location,
        identity: &Identity,
        _lock: &SupervisorLock,
    ) -> Result<Self, EndpointError> {
        let descriptor = SecurityDescriptor::from_sddl(&identity.pipe_sddl())?;
        match create_instance(&location.name, &descriptor, true) {
            Ok(next) => Ok(Self {
                next,
                name: location.name.clone(),
                descriptor,
            }),
            Err(e) if is_name_taken(&e) => Err(EndpointError::Taken(format!(
                "{} exists already (another program, or a second supervisor)",
                location.name
            ))),
            Err(e) => Err(EndpointError::Io(e)),
        }
    }

    /// Waits for a client. The DACL already admitted only the expected account, so this only
    /// reports the client's process id. A fresh instance is listening before this returns.
    pub async fn accept(&mut self) -> io::Result<Connection> {
        self.next.connect().await?;
        let fresh = create_instance(&self.name, &self.descriptor, false)?;
        let stream = std::mem::replace(&mut self.next, fresh);
        let pid = pipe_client_pid(&stream).ok();
        Ok(Connection {
            stream,
            peer: Peer { pid, uid: None },
        })
    }

    pub fn pipe_name(&self) -> &str {
        &self.name
    }
}

fn is_name_taken(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(code) if [ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_ALREADY_EXISTS].iter().any(|c| *c as i32 == code)
    )
}

/// Connects to the pipe without letting the server impersonate us (`SECURITY_ANONYMOUS`), then
/// requires the serving process to be owned by the identity's SID. Waits out a busy pipe for a
/// second or so.
pub async fn connect(
    location: &Location,
    identity: &Identity,
) -> Result<ClientStream, EndpointError> {
    let mut busy = 0;
    let client = loop {
        let mut options = ClientOptions::new();
        options
            .read(true)
            .write(true)
            .security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS);
        match options.open(&location.name) {
            Ok(client) => break client,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && busy < BUSY_RETRIES => {
                busy += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(EndpointError::Io(e)),
        }
    };
    let pid = pipe_server_pid(&client)?;
    let owner = user_sid_of_pid(pid).map_err(|e| {
        EndpointError::PeerRejected(format!("the serving process {pid} can't be examined: {e}"))
    })?;
    if owner != identity.sid {
        return Err(EndpointError::PeerRejected(format!(
            "the serving process {pid} belongs to {owner}, expected {}",
            identity.sid
        )));
    }
    Ok(client)
}

/// The process id of the client end of a pipe, from the server's handle.
pub fn pipe_client_pid(pipe: &impl AsRawHandle) -> io::Result<u32> {
    let mut pid = 0u32;
    // SAFETY: the handle is open for the borrow and `pid` is a valid out-pointer.
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// The process id of the server end of a pipe, from the client's handle.
pub fn pipe_server_pid(pipe: &impl AsRawHandle) -> io::Result<u32> {
    let mut pid = 0u32;
    // SAFETY: the handle is open for the borrow and `pid` is a valid out-pointer.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Which end of the pipe the process under test holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerEnd {
    /// The process being checked is the pipe's client (the handle is the server's).
    Client,
    /// The process being checked is the pipe's server (the handle is the client's).
    Server,
}

/// For the control pipe: checks that the process on the other end of `pipe` is `expected`.
/// Returns `PermissionDenied` on a mismatch.
pub fn verify_peer_pid(pipe: &impl AsRawHandle, peer: PeerEnd, expected: u32) -> io::Result<()> {
    let actual = match peer {
        PeerEnd::Client => pipe_client_pid(pipe)?,
        PeerEnd::Server => pipe_server_pid(pipe)?,
    };
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the pipe's peer is process {actual}, expected {expected}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};

    use crate::trust::windows::{Ace, LocalGuard, describe};

    fn user() -> Identity {
        Identity::current(ServiceKind::User).expect("identity")
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        lock: SupervisorLock,
        location: Location,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let lock = SupervisorLock::try_acquire(dir.path()).expect("lock");
        let location = Location::for_data_dir(dir.path());
        Fixture {
            _dir: dir,
            lock,
            location,
        }
    }

    #[test]
    fn the_pipe_name_is_the_prefix_and_sixteen_hex_digits_of_the_directory_hash() {
        // Not upstream's: v3 10.2's pipe name.
        let location =
            Location::for_data_dir(Path::new(r"C:\Users\Sam\AppData\Roaming\open-ferry"));
        let tail = location
            .pipe_name()
            .strip_prefix(PIPE_PREFIX)
            .expect("prefix");
        assert_eq!(tail.len(), 16);
        assert!(
            tail.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        // The digest of the normalized text, computed apart from the code under test.
        let expected = Sha256::digest(br"c:\users\sam\appdata\roaming\open-ferry");
        let expected_hex: String = expected.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(tail, &expected_hex[..16]);
    }

    #[test]
    fn two_spellings_of_one_directory_share_a_pipe_and_two_directories_do_not() {
        // Not upstream's: normalization of the hashed path.
        let a = Location::for_data_dir(Path::new(r"C:\Data\Open-Ferry"));
        assert_eq!(
            a,
            Location::for_data_dir(Path::new(r"\\?\c:\data\open-ferry\"))
        );
        assert_eq!(a, Location::for_data_dir(Path::new("C:/Data/Open-Ferry")));
        assert_ne!(a, Location::for_data_dir(Path::new(r"C:\Data\Other")));
    }

    #[test]
    fn a_record_must_be_one_of_our_pipe_names() {
        // Not upstream's: supervisor.json can't point a client at another pipe.
        let location = Location::for_data_dir(Path::new(r"C:\x"));
        assert_eq!(
            Location::from_record(&location.to_record()).expect("round trip"),
            location
        );
        assert!(Location::from_record(r"\\.\pipe\something-else").is_err());
        assert!(Location::from_record(r"\\.\pipe\open-ferry-supervisor-XYZ").is_err());
        assert!(
            Location::from_record(r"\\.\pipe\open-ferry-supervisor-0123456789abcdef\..\x").is_err()
        );
        assert!(
            Location::from_record(r"\\server\pipe\open-ferry-supervisor-0123456789abcdef").is_err()
        );
    }

    #[test]
    fn the_dacl_names_the_user_alone_or_system_and_administrators() {
        // Not upstream's: v3 10.2's DACL.
        let identity = Identity::for_sid(ServiceKind::User, "S-1-5-21-1-2-3-1001");
        assert_eq!(identity.pipe_sddl(), "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)");
        let system = Identity::current(ServiceKind::System).expect("system identity");
        assert_eq!(system.pipe_sddl(), "D:P(A;;GA;;;SY)(A;;GA;;;BA)");
        let parsed = SecurityDescriptor::from_sddl(&system.pipe_sddl())
            .expect("sddl")
            .describe()
            .expect("describe");
        let sids: Vec<String> = parsed
            .dacl
            .expect("dacl")
            .into_iter()
            .filter_map(|ace| match ace {
                Ace::Allow { sid, .. } => Some(sid),
                _ => None,
            })
            .collect();
        assert_eq!(sids, ["S-1-5-18", "S-1-5-32-544"]);
    }

    #[tokio::test]
    async fn a_client_and_server_talk_and_the_server_sees_the_client_process() {
        // Not upstream's: one round trip over the real pipe.
        let fx = fixture();
        let identity = user();
        let mut server = Server::bind(&fx.location, &identity, &fx.lock).expect("bind");
        let accept = tokio::spawn(async move {
            let mut connection = server.accept().await.expect("accept");
            let mut buf = [0u8; 4];
            connection.stream.read_exact(&mut buf).await.expect("read");
            connection.stream.write_all(&buf).await.expect("write");
            connection.peer
        });
        let mut client = connect(&fx.location, &identity).await.expect("connect");
        client.write_all(b"ping").await.expect("write");
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.expect("read");
        assert_eq!(&echoed, b"ping");
        let peer = accept.await.expect("task");
        assert_eq!(peer.pid, Some(std::process::id()));
        assert_eq!(
            pipe_server_pid(&client).expect("server pid"),
            std::process::id()
        );
        verify_peer_pid(&client, PeerEnd::Server, std::process::id()).expect("matching pid");
        let err = verify_peer_pid(&client, PeerEnd::Server, std::process::id().wrapping_add(1))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn a_second_connection_is_accepted_after_the_first() {
        // Not upstream's: a fresh instance is listening after every accept.
        let fx = fixture();
        let identity = user();
        let mut server = Server::bind(&fx.location, &identity, &fx.lock).expect("bind");
        let accept = tokio::spawn(async move {
            let _first = server.accept().await.expect("first");
            let _second = server.accept().await.expect("second");
        });
        let _a = connect(&fx.location, &identity)
            .await
            .expect("first client");
        let _b = connect(&fx.location, &identity)
            .await
            .expect("second client");
        accept.await.expect("task");
    }

    #[tokio::test]
    async fn a_squatted_name_is_a_startup_error_and_so_is_a_second_supervisor() {
        // Not upstream's: FILE_FLAG_FIRST_PIPE_INSTANCE makes a taken name fatal.
        let fx = fixture();
        let identity = user();
        let squatter = ServerOptions::new()
            .create(fx.location.pipe_name())
            .expect("squatter");
        match Server::bind(&fx.location, &identity, &fx.lock) {
            Err(EndpointError::Taken(why)) => assert!(why.contains("another program"), "{why}"),
            other => panic!("expected Taken, got {:?}", other.map(|_| ())),
        }
        drop(squatter);

        let fx = fixture();
        let _first = Server::bind(&fx.location, &identity, &fx.lock).expect("first");
        assert!(matches!(
            Server::bind(&fx.location, &identity, &fx.lock),
            Err(EndpointError::Taken(_))
        ));
    }

    #[tokio::test]
    async fn a_server_owned_by_another_account_is_refused() {
        // Not upstream's: the client checks the serving process's owner SID.
        let fx = fixture();
        let mut server = Server::bind(&fx.location, &user(), &fx.lock).expect("bind");
        let _accept = tokio::spawn(async move { server.accept().await.map(|_| ()) });
        let stranger = Identity::for_sid(ServiceKind::User, "S-1-5-21-1-2-3-9999");
        match connect(&fx.location, &stranger).await {
            Err(EndpointError::PeerRejected(why)) => {
                assert!(why.contains("S-1-5-21-1-2-3-9999"), "{why}")
            }
            other => panic!("expected PeerRejected, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn connecting_to_nothing_is_not_found() {
        // Not upstream's: no server, no pipe.
        let fx = fixture();
        match connect(&fx.location, &user()).await {
            Err(EndpointError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
            other => panic!("expected NotFound, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn the_live_pipe_carries_a_dacl_for_the_user_alone() {
        // Not upstream's: read the DACL back off the kernel object.
        let fx = fixture();
        let identity = user();
        let server = Server::bind(&fx.location, &identity, &fx.lock).expect("bind");
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut security = std::ptr::null_mut();
        // SAFETY: the handle is open and the out-pointers are valid for the call.
        let rc = unsafe {
            GetSecurityInfo(
                server.next.as_raw_handle(),
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut security,
            )
        };
        assert_eq!(rc, 0, "GetSecurityInfo");
        let _guard = LocalGuard(security);
        // SAFETY: the pointers were filled in by `GetSecurityInfo` and `_guard` keeps them alive.
        let parsed = unsafe { describe(owner, dacl, !dacl.is_null()) }.expect("describe");
        let sids: Vec<String> = parsed
            .dacl
            .expect("the pipe has a DACL")
            .into_iter()
            .filter_map(|ace| match ace {
                Ace::Allow { sid, .. } => Some(sid),
                _ => None,
            })
            .collect();
        assert_eq!(sids, [identity.sid().to_string()]);
    }
}
