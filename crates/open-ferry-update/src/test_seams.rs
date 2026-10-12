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
        // A barrier nobody will release any more is let go: its test, and
        // the directory with it, is over. A worker that was aborted while
        // it waited here leaves this thread behind, and a runtime waits for
        // it when it is dropped. The step that was held fails, though: it
        // wasn't released, and must not go on as if it had been.
        let release = dir.join(format!("{name}.release"));
        while !release.exists() && dir.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !release.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("barrier abandoned at {name}"),
            ));
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

#[cfg(all(test, feature = "blue-green-test"))]
mod tests {
    use super::hit;

    // Not upstream's: a barrier whose directory is gone is let go, and the
    // step it held fails; it is not passed as if released.
    #[test]
    fn a_barrier_whose_directory_vanished_is_not_passed() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".blue-green-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("step.hold"), []).unwrap();
        let path = root.path().to_path_buf();
        let held = std::thread::spawn(move || hit(&path, "step"));
        while !dir.join("step.arrived").exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::fs::remove_dir_all(&dir).unwrap();
        let error = held.join().unwrap().expect_err("the barrier was passed");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(error.to_string().contains("abandoned"), "{error}");
    }

    #[test]
    fn a_released_barrier_is_passed() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".blue-green-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("step.hold"), []).unwrap();
        std::fs::write(dir.join("step.release"), []).unwrap();
        hit(root.path(), "step").unwrap();
    }
}
