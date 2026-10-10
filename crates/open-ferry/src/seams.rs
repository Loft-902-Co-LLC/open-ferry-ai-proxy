//! Not upstream's: install the core's test barriers before starting a worker.
//! Async core barriers offload this synchronous file poll and await it;
//! the synchronous persist barrier runs inside the refresh's blocking save.

use std::io;
use std::path::Path;

/// Installs the file barriers once, rooted only in the supplied test data dir.
#[allow(dead_code, reason = "W3 installs this in the supervised worker")]
pub fn install_barriers(data_dir: &Path) -> io::Result<()> {
    let root = data_dir.to_path_buf();
    open_ferry_core::test_hooks::install(Box::new(move |name| {
        open_ferry_update::test_seams::hit(&root, name)
    }))
}

#[cfg(test)]
mod tests;
