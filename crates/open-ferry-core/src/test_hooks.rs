//! Not upstream's: named test barriers, only in blue-green-test builds.
//!
//! The installed hook touches test files only. Async waits move only that
//! hook to a blocking thread, so aborting the awaiting task leaves no step
//! continuation on that thread. `persist.before` fires for saves carried by
//! `Operation::run_blocking`, or saves without a runtime. Task-local admission
//! alone does not fire it: inline service sync/upserts can reconcile and save.
//! Refresh offloads its admitted save; management credential edits do too.
//!
//! The barrier is before the per-id persist lock, with no Manager state lock
//! held. Refresh holds `refresh_locks`, a PendingSave and the mutation-gate
//! read guard: a concurrent `Manager::load()` waits, just as behind a slow
//! write. Get and status remain usable, but a queued reload blocks later
//! mutation readers. Workers run reloads off the service loop, and never
//! reload during a resume with admitted work alive. At the stop deadline the
//! worker uses `process::exit`, not runtime drop, which can wait for a held
//! blocking barrier thread.

use std::io;
use std::sync::{Arc, OnceLock};

type Hook = dyn Fn(&str) -> io::Result<()> + Send + Sync;
static HOOK: OnceLock<Arc<Hook>> = OnceLock::new();

/// Installs the process's hook once. A second install is refused.
pub fn install(hook: Box<Hook>) -> io::Result<()> {
    HOOK.set(Arc::from(hook)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "test barriers already installed",
        )
    })
}

pub(crate) fn is_installed() -> bool {
    HOOK.get().is_some()
}

/// Calls the hook on a blocking thread when a runtime is present, or inline
/// without a runtime. No installed hook means no work. Dropping a runtime
/// await never runs the caller's continuation on the blocking thread.
#[allow(
    clippy::disallowed_methods,
    reason = "only the inert test hook runs on the blocking pool"
)]
pub async fn barrier(name: &str) -> io::Result<()> {
    let Some(hook) = HOOK.get().cloned() else {
        return Ok(());
    };
    if tokio::runtime::Handle::try_current().is_err() {
        return hook(name);
    }
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || hook(&name))
        .await
        .map_err(|error| io::Error::other(format!("test barrier task failed: {error}")))?
}

/// Calls the hook synchronously: **blocks its thread**. Only for the
/// synchronous persist API; async refresh offloads that entire save with
/// its admitted Operation before calling it. No hook means no work.
pub fn barrier_blocking(name: &str) -> io::Result<()> {
    HOOK.get().map_or(Ok(()), |hook| hook(name))
}
