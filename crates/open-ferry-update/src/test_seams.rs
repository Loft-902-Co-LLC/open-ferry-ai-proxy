//! Named disk fault barriers, compiled out unless explicitly enabled.

use std::io;
use std::path::Path;

/// At a named barrier, publish arrival and wait for its release file.
/// A named failure file injects an IO error. All files live in the supplied
/// test directory's `.blue-green-test` child; there is no environment hook.
#[cfg(feature = "blue-green-test")]
pub fn hit(root: &Path, name: &str) -> io::Result<()> {
    let dir = root.join(".blue-green-test");
    let name = name.replace(':', "_");
    if dir.join(format!("{name}.fail")).exists() {
        // Not upstream's: U3-6. A receipt, one byte per injected failure, so a
        // test can show the named seam fired. It lives in the same test
        // directory, so tests on different temp roots never share a count.
        use std::io::Write as _;
        if let Ok(mut receipt) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{name}.failed")))
        {
            let _ = receipt.write_all(b"x");
        }
        return Err(io::Error::other(format!("injected failure at {name}")));
    }
    if dir.join(format!("{name}.hold")).exists() {
        std::fs::write(dir.join(format!("{name}.arrived")), [])?;
        while !dir.join(format!("{name}.release")).exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Ok(())
}

/// Without the test feature, no fault names or files are inspected.
#[cfg(not(feature = "blue-green-test"))]
#[inline(always)]
pub fn hit(_root: &Path, _name: &str) -> io::Result<()> {
    Ok(())
}
