//! Not upstream's: what the supervisor asks of the `open-ferry` binary that
//! hosts it (L5). The supervisor crate never parses the config (v3 2.6), so
//! the host answers these. `open-ferry` implements the trait; tests fake it.

use std::io;
use std::net::TcpListener;
use std::time::Duration;

use crate::protocol::ListenerTopology;

/// The effective auto-update mode (v3 5.3: the config file and the
/// environment override), as far as a switch's checkpoints care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSnapshot {
    /// The effective mode is `off`.
    pub off: bool,
    /// Changes whenever the effective mode value changes (a config edit, a
    /// reload), so a checkpoint can tell "off now" from "off, then on, then
    /// off again".
    pub mode_gen: u64,
}

pub trait Host: Send + Sync + 'static {
    /// The main socket and the optional separate management socket, each with
    /// its address and TLS flag. Fixed for the supervisor's life; `main` first.
    fn topology(&self) -> Vec<ListenerTopology>;

    /// Binds the sockets `topology` names, with P's exclusive bind, in the same
    /// order. Called once, before the first active worker starts.
    fn bind(&self) -> io::Result<Vec<(String, TcpListener)>>;

    /// The effective mode now. Read again each time it is needed.
    fn effective_mode(&self) -> ModeSnapshot;

    /// This binary's version string.
    fn own_version(&self) -> String;

    /// The sha256 of this binary's own file, `None` if it could not be read.
    fn own_sha256(&self) -> Option<String>;

    /// Still waiting for the update lock at startup (T6): called when the wait
    /// begins and then every warning interval, with the time waited so far.
    /// A Windows service uses it to keep its start pending.
    fn waiting_for_update_lock(&self, waited: Duration);
}
