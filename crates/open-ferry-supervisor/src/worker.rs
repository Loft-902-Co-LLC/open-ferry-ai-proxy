//! Not upstream's: the worker's side of the control channel. A worker started
//! by the supervisor calls [`take`] once at single-threaded startup, then
//! [`Pending::attach`] inside its runtime. The supervisor's side is
//! [`crate::control`].
//!
//! On Unix the control channel is a socketpair end the runner left at a known
//! descriptor, with the listeners before it. On Windows it is a named pipe the
//! supervisor made before starting the worker, and the listeners arrive as
//! protocol info in `welcome`.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::net::TcpListener;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::protocol::{
    Budgets, Channel, Installation, ListenerTopology, Message, Role, WindowsSocket,
};

pub type BoxedRead = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxedWrite = Box<dyn AsyncWrite + Send + Unpin>;
pub type BoxedChannel = Channel<BoxedRead, BoxedWrite>;

pub(crate) fn boxed_channel<R, W>(reader: R, writer: W) -> BoxedChannel
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    Channel::new(
        Box::new(reader) as BoxedRead,
        Box::new(writer) as BoxedWrite,
    )
}

/// A pipe whose other end has closed reads as `ERROR_BROKEN_PIPE`, which is this
/// platform's end of file. The channel's rule is that EOF is a clean close, so
/// the read half says so.
#[cfg(windows)]
pub(crate) struct EofOnBrokenPipe<R>(pub(crate) R);

#[cfg(windows)]
impl<R: AsyncRead + Unpin> AsyncRead for EofOnBrokenPipe<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match std::pin::Pin::new(&mut self.0).poll_read(cx, buf) {
            std::task::Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// How many unknown messages a worker answers while it waits for `welcome`.
const MAX_UNKNOWN_BEFORE_WELCOME: u32 = 16;

/// What a worker tells the supervisor about itself. The process id is the
/// worker's own and is added by [`Pending::attach`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// The newest protocol version the worker speaks.
    pub max_version: u32,
    /// The oldest supervisor protocol version the worker can work under.
    pub min_supervisor: u32,
    /// The worker binary's version string.
    pub version: String,
    pub caps: Vec<String>,
}

/// The supervisor's `welcome`, less the Windows socket list that `attach` turns
/// into listeners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub role: Role,
    pub installation: Installation,
    pub budgets: Budgets,
    pub topology: Vec<ListenerTopology>,
    pub switch_id: Option<String>,
}

pub struct Attached {
    pub channel: BoxedChannel,
    pub welcome: Welcome,
    /// The protocol version the supervisor selected.
    pub version: u32,
    /// `main`, and `management` if there is one. Non-blocking, not inheritable.
    pub listeners: Vec<(String, TcpListener)>,
}

#[derive(Debug)]
pub enum AttachError {
    /// The supervisor is too old for this worker: it needs `needs_supervisor`.
    Incompatible {
        needs_supervisor: u32,
    },
    /// The channel ended before `welcome`.
    Closed,
    /// The peer is not the supervisor that started us, or sent something that is
    /// not a valid welcome.
    Protocol(String),
    Io(io::Error),
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incompatible { needs_supervisor } => {
                write!(
                    f,
                    "the supervisor is too old: needs version {needs_supervisor}"
                )
            }
            Self::Closed => write!(f, "the control channel closed before welcome"),
            Self::Protocol(reason) => write!(f, "control protocol error: {reason}"),
            Self::Io(error) => write!(f, "control channel: {error}"),
        }
    }
}

impl std::error::Error for AttachError {}

impl From<io::Error> for AttachError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A worker's control channel and sockets, taken but not yet greeted.
pub struct Pending {
    #[cfg(unix)]
    listeners: Vec<TcpListener>,
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    name: String,
    #[cfg(windows)]
    server_pid: u32,
}

/// Takes what the supervisor handed this worker. `control` is the value after
/// `-worker-control`.
///
/// Call it at startup before any runtime or thread exists: on Unix it removes
/// `LISTEN_FDS` from the environment.
pub fn take(control: &OsStr) -> io::Result<Pending> {
    take_imp(control)
}

#[cfg(unix)]
fn take_imp(control: &OsStr) -> io::Result<Pending> {
    let given: usize = control
        .to_str()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "malformed control fd"))?;
    // SAFETY: the documented contract of `take`: single-threaded startup of a
    // worker the supervisor started.
    let (listeners, stream) = unsafe { crate::sockets::unix::take_worker_sockets() }?;
    if given != 3 + listeners.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the control fd is not the one after the listeners",
        ));
    }
    Ok(Pending { listeners, stream })
}

#[cfg(windows)]
fn take_imp(control: &OsStr) -> io::Result<Pending> {
    let name = control
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "malformed control pipe"))?;
    let server_pid = crate::control::parse_pipe_name(name)?;
    Ok(Pending {
        name: name.to_owned(),
        server_pid,
    })
}

impl Pending {
    /// Connects if the channel needs it, greets the supervisor and reads its
    /// answer. The supervisor's `incompatible` is an error here, and a message
    /// this version does not know is answered with `unsupported`.
    pub async fn attach(self, hello: Hello) -> Result<Attached, AttachError> {
        let (mut channel, mut listeners) = self.open().await?;
        channel
            .send(&Message::Hello {
                v: hello.max_version,
                min_supervisor: hello.min_supervisor,
                version: hello.version.clone(),
                pid: std::process::id(),
                caps: hello.caps.clone(),
            })
            .await?;
        let mut unknown = 0;
        let (welcome, version, sockets) = loop {
            let Some(frame) = channel.receive::<Message>().await? else {
                return Err(AttachError::Closed);
            };
            match frame.message {
                Message::Welcome {
                    v,
                    role,
                    installation,
                    budgets,
                    sockets,
                    topology,
                    switch_id,
                } => {
                    if v == 0 || v > hello.max_version || v < hello.min_supervisor {
                        return Err(AttachError::Protocol(format!(
                            "welcome selects version {v}, outside what hello offered"
                        )));
                    }
                    break (
                        Welcome {
                            role,
                            installation,
                            budgets,
                            topology,
                            switch_id,
                        },
                        v,
                        sockets,
                    );
                }
                Message::Incompatible { needs_supervisor } => {
                    return Err(AttachError::Incompatible { needs_supervisor });
                }
                Message::Unknown => {
                    unknown += 1;
                    if unknown > MAX_UNKNOWN_BEFORE_WELCOME {
                        return Err(AttachError::Protocol(
                            "too many unknown messages before welcome".into(),
                        ));
                    }
                    channel
                        .send(&Message::Unsupported {
                            message_type: frame.message_type,
                        })
                        .await?;
                }
                _ => {
                    return Err(AttachError::Protocol(format!(
                        "expected welcome, got {}",
                        frame.message_type
                    )));
                }
            }
        };
        windows_listeners(&sockets, &mut listeners)?;
        if listeners.is_empty() || listeners[0].0 != "main" {
            return Err(AttachError::Protocol(
                "no main listener was handed over".into(),
            ));
        }
        Ok(Attached {
            channel,
            welcome,
            version,
            listeners,
        })
    }

    #[cfg(unix)]
    async fn open(self) -> Result<(BoxedChannel, Vec<(String, TcpListener)>), AttachError> {
        let stream = tokio::net::UnixStream::from_std(self.stream)?;
        let (reader, writer) = tokio::io::split(stream);
        let names = ["main", "management"];
        let listeners = self
            .listeners
            .into_iter()
            .zip(names)
            .map(|(listener, name)| (name.to_owned(), listener))
            .collect();
        Ok((boxed_channel(reader, writer), listeners))
    }

    #[cfg(windows)]
    async fn open(self) -> Result<(BoxedChannel, Vec<(String, TcpListener)>), AttachError> {
        use crate::endpoint::{PeerEnd, verify_peer_pid};
        use tokio::net::windows::named_pipe::ClientOptions;
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
        use windows_sys::Win32::Storage::FileSystem::{SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT};

        let mut busy = 0;
        let client = loop {
            let mut options = ClientOptions::new();
            options
                .read(true)
                .write(true)
                .security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS);
            match options.open(&self.name) {
                Ok(client) => break client,
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && busy < 20 => {
                    busy += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        verify_peer_pid(&client, PeerEnd::Server, self.server_pid).map_err(|error| {
            AttachError::Protocol(format!(
                "the control pipe's server is not the supervisor: {error}"
            ))
        })?;
        let (reader, writer) = tokio::io::split(client);
        Ok((boxed_channel(EofOnBrokenPipe(reader), writer), Vec::new()))
    }
}

#[cfg(unix)]
fn windows_listeners(
    _sockets: &[WindowsSocket],
    _into: &mut Vec<(String, TcpListener)>,
) -> Result<(), AttachError> {
    Ok(())
}

#[cfg(windows)]
fn windows_listeners(
    sockets: &[WindowsSocket],
    into: &mut Vec<(String, TcpListener)>,
) -> Result<(), AttachError> {
    for socket in sockets {
        if !matches!(socket.name.as_str(), "main" | "management")
            || into.iter().any(|(name, _)| *name == socket.name)
        {
            return Err(AttachError::Protocol(format!(
                "unexpected socket {:?} in welcome",
                socket.name
            )));
        }
        let listener = crate::sockets::windows::listener_from_info(&socket.info)?;
        listener.set_nonblocking(true)?;
        into.push((socket.name.clone(), listener));
    }
    // `main` first, as on Unix.
    into.sort_by_key(|(name, _)| name != "main");
    Ok(())
}
