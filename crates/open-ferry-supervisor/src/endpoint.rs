//! The supervisor's local endpoint: a Unix socket in a private directory, or a named pipe.
//! Never a TCP port, on any system.
//!
//! The server side is [`Server`], bound by the supervisor while it holds its
//! [`SupervisorLock`](crate::process::SupervisorLock). The client side is [`connect`], used by
//! the CLI and the server's update routes. Both check who is on the other end:
//!
//! - **Unix.** The whole directory chain is validated with `openat(O_NOFOLLOW)` (only the
//!   runtime root is resolved, once, with `realpath`). The final directory must be mode 0700
//!   and owned by the expected uid, the socket mode 0600 and a socket owned by that uid. The
//!   peer's uid comes from `SO_PEERCRED` (Linux) or `getpeereid` (macOS).
//! - **Windows.** The pipe is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`,
//!   `PIPE_REJECT_REMOTE_CLIENTS` and a DACL naming the user's SID (or SYSTEM and
//!   Administrators for a system service). A taken name is a startup error. The client opens it
//!   with `SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS` and requires the serving process to be
//!   owned by the expected SID.
//!
//! Locations come from [`Location::resolve`]. [`EndpointError::SwitchingDisabled`] means no
//! location passes the policy, and the supervisor then runs without an endpoint; it does not
//! fail the service. [`EndpointError::Unsafe`] and [`EndpointError::Taken`] are startup errors.

use std::fmt;
use std::io;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{ClientStream, Connection, Identity, Location, Server, ServerStream, connect};
#[cfg(windows)]
pub use windows::{
    ClientStream, Connection, Identity, Location, PeerEnd, Server, ServerStream, connect,
    pipe_client_pid, pipe_server_pid, verify_peer_pid,
};

/// Whose service this is. It decides the expected owner of the endpoint and of the paths the
/// service runs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    /// A service of one user: the user's uid or SID owns everything.
    User,
    /// A system service: root, or LocalSystem with Administrators.
    System,
}

/// Who is on the other end of a connection, as far as the system could tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Peer {
    pub pid: Option<u32>,
    pub uid: Option<u32>,
}

#[derive(Debug)]
pub enum EndpointError {
    /// No location passes the path policy (an unusual filesystem, the macOS temp directory).
    /// Not a startup failure: run without an endpoint and without switching.
    SwitchingDisabled(String),
    /// The endpoint path is taken or not safe: a wrong owner, mode or type, or a link.
    /// A startup failure for the server; a refusal for the client.
    Unsafe(String),
    /// Another server holds the endpoint: a live socket, or a pipe name already created.
    Taken(String),
    /// The server on the other end is not the expected account.
    PeerRejected(String),
    Io(io::Error),
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EndpointError::SwitchingDisabled(why) => write!(f, "switching is disabled: {why}"),
            EndpointError::Unsafe(why) => {
                write!(f, "the endpoint path is taken or not safe: {why}")
            }
            EndpointError::Taken(why) => write!(f, "the endpoint name is taken: {why}"),
            EndpointError::PeerRejected(why) => {
                write!(f, "the endpoint's server was refused: {why}")
            }
            EndpointError::Io(e) => write!(f, "endpoint I/O error: {e}"),
        }
    }
}

impl std::error::Error for EndpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EndpointError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for EndpointError {
    fn from(e: io::Error) -> Self {
        EndpointError::Io(e)
    }
}
