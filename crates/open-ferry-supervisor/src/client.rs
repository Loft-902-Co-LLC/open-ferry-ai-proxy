//! What the update CLI, the install scripts' `install-self` and `service
//! uninstall` ask of a supervisor that may be running: whether one runs
//! (design 10.3 and 10.4), and that it stop.
//!
//! [`detect`] decides whether the installed binary may be replaced. It runs
//! only under the caller's update lock, and answers [`Detection::Clear`] only
//! with the binary's install lock taken: a supervisor holds that lock for its
//! whole life, whichever data directory it uses, so one taken shows that none
//! runs from the binary. Never from a failure. [`stop`] takes no lock. Each
//! takes one deadline that bounds all it does, its file reads included.

use std::path::Path;
use std::time::Instant;

use open_ferry_update::install_lock::InstallLock;

use crate::endpoint::Identity;

/// What [`detect`] found.
#[derive(Debug)]
pub enum Detection {
    /// No supervisor runs from the binary: its install lock, which the
    /// caller keeps through the replacement and the receipt.
    Clear(InstallLock),
    /// A supervisor of the data directory runs: it answered, or it holds the
    /// data directory's lifetime lock while it starts.
    Supervised(Running),
    /// The install lock is held, and no supervisor of the data directory
    /// answered: another install, or a supervisor of another data directory,
    /// as the reason says. Worth trying again.
    Busy(String),
    /// Whether one runs can't be told, for the reason given: nothing may be
    /// replaced.
    Refuse(String),
}

/// What a supervisor says of itself, as far as it said anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Running {
    pub pid: Option<u32>,
    pub version: Option<String>,
}

/// How [`stop`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// The supervisor said it was stopping, and its lifetime lock came free.
    Stopped,
    /// No supervisor held the data directory's lifetime lock.
    NotRunning,
    /// The deadline came first; what was last seen.
    TimedOut(String),
    /// The stop couldn't be asked for, for the reason given.
    Refused(String),
}

/// Whether a supervisor runs from the installed binary `installed`, for a
/// replacement of it by a caller holding `data_dir`'s update lock, who keeps
/// both locks through the replacement and its receipt. Never waits on a
/// lock, and returns by `deadline`.
pub async fn detect(
    data_dir: &Path,
    identity: &Identity,
    installed: &Path,
    deadline: Instant,
) -> Detection {
    let _ = (data_dir, identity, installed, deadline);
    unimplemented!("detection comes with 2c-2")
}

/// Asks the supervisor of `data_dir` to stop, and waits until `deadline`
/// for its lifetime lock to come free. Takes no lock of its own.
pub async fn stop(data_dir: &Path, identity: &Identity, deadline: Instant) -> StopOutcome {
    let _ = (data_dir, identity, deadline);
    unimplemented!("the stop comes with 2c-2")
}
