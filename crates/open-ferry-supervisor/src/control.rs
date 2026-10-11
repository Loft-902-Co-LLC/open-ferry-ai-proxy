//! Not upstream's: the supervisor's side of a worker's control channel, the
//! counterpart of [`crate::worker`].
//!
//! The supervisor makes the channel before it starts the worker (a socketpair
//! made by the runner on Unix, a single-instance named pipe on Windows), checks
//! that whoever is on the other end is the process it started, and answers its
//! `hello` with a `welcome` or `incompatible`.

use std::ffi::OsString;
use std::io;
use std::path::Path;

use crate::protocol::{
    Budgets, Installation, ListenerTopology, Message, REQUIRED_CAPS, Role, VERSION, WindowsSocket,
    negotiate,
};
use crate::worker::{BoxedChannel, boxed_channel};

/// How many unknown messages the supervisor answers while it waits for `hello`.
const MAX_UNKNOWN_BEFORE_HELLO: u32 = 16;

/// The command-line arguments of a worker (L1). `control` is the Windows pipe
/// name; on Unix the runner adds `-worker-control <fd>` itself, so pass `None`.
pub fn worker_args(config: &Path, control: Option<&str>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-worker"),
        OsString::from("-config"),
        config.as_os_str().to_owned(),
    ];
    if let Some(control) = control {
        args.push(OsString::from("-worker-control"));
        args.push(OsString::from(control));
    }
    args
}

/// What the worker said in `hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerHello {
    /// The newest protocol version the worker speaks.
    pub max_version: u32,
    /// The oldest supervisor protocol version the worker accepts.
    pub min_supervisor: u32,
    pub version: String,
    pub pid: u32,
    pub caps: Vec<String>,
}

/// What `welcome` carries. `sockets` is empty on Unix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeParams {
    pub role: Role,
    pub installation: Installation,
    pub budgets: Budgets,
    pub sockets: Vec<WindowsSocket>,
    pub topology: Vec<ListenerTopology>,
    pub switch_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub hello: WorkerHello,
    /// The protocol version `welcome` selected.
    pub version: u32,
}

#[derive(Debug)]
pub enum HandshakeError {
    Io(io::Error),
    /// The channel ended before `hello`.
    Closed,
    /// The worker needs a newer supervisor; `incompatible` was sent.
    Incompatible {
        needs_supervisor: u32,
    },
    /// `hello` names a process other than the one that was started.
    PidMismatch {
        claimed: u32,
        expected: u32,
    },
    Protocol(String),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "control channel: {error}"),
            Self::Closed => write!(f, "the worker closed the channel before hello"),
            Self::Incompatible { needs_supervisor } => {
                write!(f, "the worker needs supervisor version {needs_supervisor}")
            }
            Self::PidMismatch { claimed, expected } => {
                write!(f, "hello names process {claimed}, expected {expected}")
            }
            Self::Protocol(reason) => write!(f, "control protocol error: {reason}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<io::Error> for HandshakeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reads `hello`, checks its pid, negotiates and answers. The caller bounds this
/// with `LOAD_TIMEOUT`. A message of an unknown type is answered `unsupported`.
pub async fn handshake(
    channel: &mut BoxedChannel,
    expected_pid: u32,
    welcome: WelcomeParams,
) -> Result<Negotiated, HandshakeError> {
    handshake_with_caps(channel, expected_pid, welcome, REQUIRED_CAPS).await
}

async fn handshake_with_caps(
    channel: &mut BoxedChannel,
    expected_pid: u32,
    welcome: WelcomeParams,
    required: &[&str],
) -> Result<Negotiated, HandshakeError> {
    let mut unknown = 0;
    let hello = loop {
        let Some(frame) = channel.receive::<Message>().await? else {
            return Err(HandshakeError::Closed);
        };
        match frame.message {
            Message::Hello {
                v,
                min_supervisor,
                version,
                pid,
                caps,
            } => {
                break WorkerHello {
                    max_version: v,
                    min_supervisor,
                    version,
                    pid,
                    caps,
                };
            }
            Message::Unknown => {
                unknown += 1;
                if unknown > MAX_UNKNOWN_BEFORE_HELLO {
                    return Err(HandshakeError::Protocol(
                        "too many unknown messages before hello".into(),
                    ));
                }
                channel
                    .send(&Message::Unsupported {
                        message_type: frame.message_type,
                    })
                    .await?;
            }
            _ => {
                return Err(HandshakeError::Protocol(format!(
                    "expected hello, got {}",
                    frame.message_type
                )));
            }
        }
    };
    if hello.pid != expected_pid {
        return Err(HandshakeError::PidMismatch {
            claimed: hello.pid,
            expected: expected_pid,
        });
    }
    let version = match negotiate(VERSION, hello.max_version, hello.min_supervisor) {
        Ok(version) => version,
        Err(answer) => {
            channel.send(&answer).await?;
            let needs_supervisor = match answer {
                Message::Incompatible { needs_supervisor } => needs_supervisor,
                _ => hello.min_supervisor,
            };
            return Err(HandshakeError::Incompatible { needs_supervisor });
        }
    };
    if required
        .iter()
        .any(|cap| !hello.caps.iter().any(|offered| offered == cap))
    {
        channel
            .send(&Message::Incompatible {
                needs_supervisor: VERSION,
            })
            .await?;
        return Err(HandshakeError::Protocol(format!(
            "the worker is missing required capabilities: {}",
            required
                .iter()
                .filter(|cap| !hello.caps.iter().any(|offered| offered == **cap))
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    channel
        .send(&Message::Welcome {
            v: version,
            role: welcome.role,
            installation: welcome.installation,
            budgets: welcome.budgets,
            sockets: welcome.sockets,
            topology: welcome.topology,
            switch_id: welcome.switch_id,
        })
        .await?;
    Ok(Negotiated { hello, version })
}

/// The channel to a worker the runner started on Unix.
#[cfg(unix)]
pub fn unix_channel(stream: std::os::unix::net::UnixStream) -> io::Result<BoxedChannel> {
    stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(stream)?;
    let (reader, writer) = tokio::io::split(stream);
    Ok(boxed_channel(reader, writer))
}

#[cfg(windows)]
pub use windows::{ControlPipe, parse_pipe_name};

#[cfg(windows)]
mod windows {
    use std::io;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    use super::boxed_channel;
    use crate::endpoint::{Identity, PeerEnd, verify_peer_pid};
    use crate::trust::windows::SecurityDescriptor;
    use crate::worker::BoxedChannel;
    use crate::worker::EofOnBrokenPipe;

    const NAME_PREFIX: &str = r"\\.\pipe\open-ferry-worker-";

    /// `<pid>-<32 lowercase hex>` after the prefix; returns the pid.
    pub fn parse_pipe_name(name: &str) -> io::Result<u32> {
        let bad = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the control pipe name is not an open-ferry worker pipe",
            )
        };
        let tail = name.strip_prefix(NAME_PREFIX).ok_or_else(bad)?;
        let (pid, random) = tail.split_once('-').ok_or_else(bad)?;
        let random_ok = random.len() == 32
            && random
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let pid_ok = !pid.is_empty()
            && pid.len() <= 10
            && pid.bytes().all(|b| b.is_ascii_digit())
            && !(pid.len() > 1 && pid.starts_with('0'));
        if !random_ok || !pid_ok {
            return Err(bad());
        }
        pid.parse().map_err(|_| bad())
    }

    /// A worker's control pipe, made before the worker is started.
    ///
    /// One instance, first-instance only, no remote clients, and a DACL that
    /// names only the service's account. A name that exists already is an error.
    /// Must be made inside a tokio runtime.
    pub struct ControlPipe {
        name: String,
        server: NamedPipeServer,
    }

    impl ControlPipe {
        pub fn create(identity: &Identity) -> io::Result<Self> {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
            let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let name = format!("{NAME_PREFIX}{}-{hex}", std::process::id());
            let descriptor = SecurityDescriptor::from_sddl(&identity.pipe_sddl())?;
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.as_ptr(),
                bInheritHandle: 0,
            };
            let mut options = ServerOptions::new();
            options
                .first_pipe_instance(true)
                .max_instances(1)
                .reject_remote_clients(true)
                .access_inbound(true)
                .access_outbound(true);
            // SAFETY: `attributes` is a valid SECURITY_ATTRIBUTES whose
            // descriptor outlives the call.
            let server = unsafe {
                options.create_with_security_attributes_raw(
                    &name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )
            }?;
            Ok(Self { name, server })
        }

        /// The value for `-worker-control`.
        pub fn name(&self) -> &str {
            &self.name
        }

        /// Waits for the worker to connect, then checks that the client is
        /// `worker_pid`. A connection from any other process is dropped. The
        /// caller bounds the wait (`LOAD_TIMEOUT`) and also watches the worker's
        /// exit, since a worker that dies before it connects never will.
        pub async fn accept(self, worker_pid: u32) -> io::Result<BoxedChannel> {
            loop {
                self.server.connect().await?;
                if verify_peer_pid(&self.server, PeerEnd::Client, worker_pid).is_ok() {
                    break;
                }
                self.server.disconnect()?;
            }
            let (reader, writer) = tokio::io::split(self.server);
            Ok(boxed_channel(EofOnBrokenPipe(reader), writer))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Channel;
    use tokio::io::AsyncWriteExt;

    fn params() -> WelcomeParams {
        WelcomeParams {
            role: Role::Standby,
            installation: Installation {
                installed: "installed".into(),
                data_dir: "data".into(),
                receipt: serde_json::Value::Null,
            },
            budgets: Budgets {
                load_ms: 1,
                handover_ms: 2,
                ready_ms: 3,
                resume_ms: 4,
                watch_ms: 5,
                drain_ms: 6,
                stop_ms: 7,
            },
            sockets: Vec::new(),
            topology: Vec::new(),
            switch_id: Some("s".into()),
        }
    }

    // Not upstream's: a type the supervisor does not know is answered
    // unsupported while it waits for hello, and the hello that follows is used.
    #[tokio::test]
    async fn an_unknown_message_before_hello_is_answered_unsupported() {
        let (near, far) = tokio::io::duplex(4096);
        let (near_read, near_write) = tokio::io::split(near);
        let mut supervisor = boxed_channel(near_read, near_write);
        let (mut far_read, mut far_write) = tokio::io::split(far);
        far_write
            .write_all(
                b"{\"type\":\"future_thing\"}
{\"type\":\"hello\",\"v\":3,\"min_supervisor\":1,\"version\":\"x\",\"pid\":9,\"caps\":[]}
",
            )
            .await
            .unwrap();
        let negotiated = handshake(&mut supervisor, 9, params()).await.unwrap();
        assert_eq!(negotiated.version, VERSION);
        assert_eq!(negotiated.hello.max_version, 3);
        drop(supervisor);
        let mut worker = Channel::new(&mut far_read, tokio::io::sink());
        let first = worker.receive::<Message>().await.unwrap().unwrap();
        assert_eq!(
            first.message,
            Message::Unsupported {
                message_type: "future_thing".into()
            }
        );
        let second = worker.receive::<Message>().await.unwrap().unwrap();
        assert!(matches!(second.message, Message::Welcome { v: 1, .. }));
    }

    // Not upstream's: anything but hello first is a protocol error, not a welcome.
    #[tokio::test]
    async fn a_message_other_than_hello_first_is_refused() {
        let (near, far) = tokio::io::duplex(4096);
        let (near_read, near_write) = tokio::io::split(near);
        let mut supervisor = boxed_channel(near_read, near_write);
        let (_far_read, mut far_write) = tokio::io::split(far);
        far_write
            .write_all(
                b"{\"type\":\"ready\"}
",
            )
            .await
            .unwrap();
        assert!(matches!(
            handshake(&mut supervisor, 9, params()).await,
            Err(HandshakeError::Protocol(_))
        ));
    }
    #[tokio::test]
    async fn a_missing_required_capability_gets_incompatible_not_welcome() {
        let (near, far) = tokio::io::duplex(4096);
        let (r, w) = tokio::io::split(near);
        let mut supervisor = boxed_channel(r, w);
        let (r, w) = tokio::io::split(far);
        let mut worker = boxed_channel(r, w);
        worker
            .send(&Message::Hello {
                v: 1,
                min_supervisor: 1,
                version: "test".into(),
                pid: 9,
                caps: vec![],
            })
            .await
            .unwrap();
        assert!(matches!(
            handshake_with_caps(&mut supervisor, 9, params(), &["required-test-cap"]).await,
            Err(HandshakeError::Protocol(reason))
                if reason == "the worker is missing required capabilities: required-test-cap"
        ));
        assert!(matches!(
            worker.receive::<Message>().await.unwrap().unwrap().message,
            Message::Incompatible { .. }
        ));
    }
}
