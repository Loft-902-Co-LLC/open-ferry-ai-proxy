use std::cell::{Cell, RefCell};
use std::rc::Rc;

use open_ferry_update::install_lock::InstallLock;

use super::*;

// --- The rig ----------------------------------------------------------------

/// A bin directory with the staged binary `from` (which stands for the file
/// that runs), the destination `dest`, and a data directory that doesn't
/// exist yet.
struct Rig {
    _dir: tempfile::TempDir,
    bin: PathBuf,
    data: DataDir,
    from: PathBuf,
    dest: PathBuf,
}

/// A temporary directory under the resolved temp directory, so its path is
/// the one install-self resolves it to: on Windows, `TEMP` can hold an 8.3
/// short name (`RUNNER~1` on GitHub's runners), which resolving makes long.
fn tempdir() -> tempfile::TempDir {
    let temp = unverbatim(fs::canonicalize(std::env::temp_dir()).unwrap());
    tempfile::tempdir_in(temp).unwrap()
}

fn new_rig() -> Rig {
    let dir = tempdir();
    let bin = dir.path().join("bin dir");
    fs::create_dir(&bin).unwrap();
    let from = bin.join(".open-ferry.new");
    fs::write(&from, b"new").unwrap();
    let dest = bin.join("open-ferry");
    let data = DataDir::at(dir.path().join("data"));
    Rig {
        _dir: dir,
        bin,
        data,
        from,
        dest,
    }
}

impl Rig {
    fn with_old(self) -> Self {
        fs::write(&self.dest, b"old").unwrap();
        self
    }

    fn options(&self, wait: Duration) -> Options {
        Options {
            from: self.from.clone(),
            to: self.dest.clone(),
            installer: "install.sh".into(),
            wait,
        }
    }

    /// The text of `path`, or none.
    fn text(path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn dest_text(&self) -> Option<String> {
        Self::text(&self.dest)
    }

    fn staged_text(&self) -> Option<String> {
        Self::text(&self.from)
    }

    /// The rollback material in the bin directory.
    fn olds(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.bin)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("open-ferry.old-")
            })
            .collect()
    }

    fn receipt(&self) -> Option<Receipt> {
        Receipt::load(&self.data.receipt_file()).unwrap()
    }
}

/// A clock that only moves when it is slept on or told to.
struct Virtual {
    base: Instant,
    elapsed: Rc<Cell<Duration>>,
    sleeps: Rc<RefCell<Vec<Duration>>>,
    on_sleep: Option<Box<dyn FnMut(usize)>>,
}

impl Virtual {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            elapsed: Rc::default(),
            sleeps: Rc::default(),
            on_sleep: None,
        }
    }

    fn slept(&self) -> Vec<Duration> {
        self.sleeps.borrow().clone()
    }
}

impl Clock for Virtual {
    fn now(&self) -> Instant {
        self.base + self.elapsed.get()
    }

    fn sleep(&mut self, duration: Duration) {
        self.elapsed.set(self.elapsed.get() + duration);
        let count = {
            let mut sleeps = self.sleeps.borrow_mut();
            sleeps.push(duration);
            sleeps.len()
        };
        if let Some(on_sleep) = &mut self.on_sleep {
            on_sleep(count);
        }
    }
}

/// The real files, with chosen operations failing. An operation is named by
/// its kind and what its paths are: `dest` (`open-ferry`), `old` (the
/// rollback material) or `from` (the staged file).
struct Scripted {
    fail: Vec<(&'static str, &'static str, &'static str)>,
    /// Operations that fail only the nth time they are made (from 1).
    fail_nth: Vec<(&'static str, &'static str, &'static str, usize)>,
    log: Vec<String>,
    /// Every rename, with its paths in full.
    renames: Vec<(PathBuf, PathBuf)>,
    /// The directories made and the directories synced, in full.
    made: Vec<PathBuf>,
    synced: Vec<PathBuf>,
}

impl Scripted {
    fn new() -> Self {
        Self::failing(&[])
    }

    fn failing(fail: &[(&'static str, &'static str, &'static str)]) -> Self {
        Self {
            fail: fail.to_vec(),
            fail_nth: Vec::new(),
            log: Vec::new(),
            renames: Vec::new(),
            made: Vec::new(),
            synced: Vec::new(),
        }
    }

    fn failing_nth(fail_nth: &[(&'static str, &'static str, &'static str, usize)]) -> Self {
        Self {
            fail_nth: fail_nth.to_vec(),
            ..Self::new()
        }
    }

    /// The position in the log of the first `entry`.
    fn at(&self, entry: &str) -> usize {
        self.log
            .iter()
            .position(|line| line == entry)
            .unwrap_or_else(|| panic!("{entry:?} not in {:?}", self.log))
    }

    /// Operations are named by what they act on: `dest`, `old`, `from`, `bin`
    /// (the directory of them all) or `other`.
    fn label(path: &Path) -> &'static str {
        // A root has no name.
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        if name.starts_with("open-ferry.old-") {
            "old"
        } else if name == "open-ferry" {
            "dest"
        } else if name == ".open-ferry.new" {
            "from"
        } else if name == "bin dir" {
            "bin"
        } else {
            "other"
        }
    }

    /// Records the operation, and whether it is to fail.
    fn step(&mut self, op: &'static str, a: &Path, b: Option<&Path>) -> io::Result<()> {
        let (a, b) = (Self::label(a), b.map_or("", Self::label));
        let entry = format!("{op} {a} {b}").trim_end().to_owned();
        self.log.push(entry.clone());
        let count = self.log.iter().filter(|line| **line == entry).count();
        if self.fail.contains(&(op, a, b)) || self.fail_nth.contains(&(op, a, b, count)) {
            return Err(io::Error::other(format!("injected failure: {op} {a} {b}")));
        }
        Ok(())
    }

    fn did(&self, entry: &str) -> bool {
        self.log.iter().any(|line| line == entry)
    }
}

impl Files for Scripted {
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        self.renames.push((from.to_owned(), to.to_owned()));
        self.step("rename", from, Some(to))?;
        RealFiles.rename(from, to)
    }

    fn sync_file(&mut self, path: &Path) -> io::Result<()> {
        self.step("sync_file", path, None)?;
        RealFiles.sync_file(path)
    }

    fn sync_dir(&mut self, dir: &Path) -> io::Result<()> {
        self.synced.push(dir.to_owned());
        self.step("sync_dir", dir, None)?;
        RealFiles.sync_dir(dir)
    }

    fn create_dir(&mut self, dir: &Path) -> io::Result<()> {
        self.made.push(dir.to_owned());
        self.step("create_dir", dir, None)?;
        RealFiles.create_dir(dir)
    }

    fn exists(&mut self, path: &Path) -> io::Result<bool> {
        self.step("exists", path, None)?;
        RealFiles.exists(path)
    }

    fn resolve(&mut self, path: &Path) -> io::Result<PathBuf> {
        self.step("resolve", path, None)?;
        RealFiles.resolve(path)
    }

    fn look(&mut self, path: &Path) -> io::Result<Option<fs::Metadata>> {
        self.step("look", path, None)?;
        RealFiles.look(path)
    }

    fn hard_link(&mut self, original: &Path, link: &Path) -> io::Result<()> {
        self.step("hard_link", original, Some(link))?;
        RealFiles.hard_link(original, link)
    }

    fn copy(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        if let Err(error) = self.step("copy", from, Some(to)) {
            // As a copy that ran out of room does.
            fs::write(to, b"par")?;
            return Err(error);
        }
        RealFiles.copy(from, to)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        self.step("remove_file", path, None)?;
        RealFiles.remove_file(path)
    }
}

struct Out {
    exit: Exit,
    out: String,
    err: String,
}

/// What a detector finds, as a function of the call.
fn clear(dest: &Path) -> Detection {
    Detection::Clear(InstallLock::try_acquire(dest).unwrap())
}

/// Runs the installation with everything given.
fn execute(
    rig: &Rig,
    windows: bool,
    files: &mut dyn Files,
    publish: &mut Publish<'_>,
    detect: &mut Detect<'_>,
    clock: &mut dyn Clock,
    wait: Duration,
) -> Out {
    let mut env = Env {
        current_exe: rig.from.clone(),
        data: rig.data.clone(),
        windows,
        home: None,
        unique: "1f-2a-0".into(),
        detect,
        files,
        clock,
        publish,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let exit = install(
        &rig.options(wait),
        &mut env,
        &mut Say::new(&mut out, &mut err),
    );
    Out {
        exit,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

/// An install with the real receipt writer and a detector that finds it clear.
fn run_with(rig: &Rig, windows: bool, files: &mut dyn Files) -> Out {
    let dest = rig.dest.clone();
    execute(
        rig,
        windows,
        files,
        &mut |receipt, path| receipt.write_atomic(path),
        &mut move |_, _, _| clear(&dest),
        &mut Virtual::new(),
        Duration::from_secs(1),
    )
}

/// An install whose receipt write is `result`, and which looks at the files
/// (dest, from and the rollback material) as it happens.
fn run_publishing(
    rig: &Rig,
    windows: bool,
    files: &mut dyn Files,
    mut result: impl FnMut() -> Result<(), WriteError>,
    mut during: impl FnMut(),
) -> Out {
    let dest = rig.dest.clone();
    execute(
        rig,
        windows,
        files,
        &mut |_, _| {
            during();
            result()
        },
        &mut move |_, _, _| clear(&dest),
        &mut Virtual::new(),
        Duration::from_secs(1),
    )
}

fn not_published() -> Result<(), WriteError> {
    Err(WriteError::NotPublished(io::Error::other("disk full")))
}

const BOTH: [bool; 2] = [false, true];

// --- Success ----------------------------------------------------------------

#[test]
fn a_fresh_install_places_the_binary_and_publishes_the_receipt() {
    for windows in BOTH {
        let rig = new_rig();
        let out = run_with(&rig, windows, &mut Scripted::new());
        assert_eq!(out.exit, Exit::Installed, "{}", out.err);
        assert_eq!(rig.dest_text().as_deref(), Some("new"));
        assert_eq!(rig.staged_text(), None, "the staged file moved");
        assert!(rig.olds().is_empty());
        let receipt = rig.receipt().expect("a receipt");
        assert_eq!(receipt.installer, "install.sh");
        assert_eq!(receipt.version, open_ferry_update::CURRENT_VERSION);
        assert_eq!(receipt.target, open_ferry_update::TARGET);
        assert_eq!(
            Path::new(&receipt.binary),
            canonical_dest(&rig.dest).unwrap().as_path()
        );
        assert!(!receipt.binary.starts_with(r"\\?\"), "{}", receipt.binary);
        assert!(
            receipt.installed_at.ends_with('Z'),
            "{}",
            receipt.installed_at
        );
    }
}

#[test]
fn the_data_directory_is_made_when_it_is_missing() {
    let rig = new_rig();
    assert!(!rig.data.root().exists());
    let out = run_with(&rig, false, &mut Scripted::new());
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert!(rig.data.root().is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(rig.data.root()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

#[test]
fn an_install_over_an_old_binary_replaces_it_and_removes_the_rollback_material() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        let out = run_with(&rig, windows, &mut Scripted::new());
        assert_eq!(out.exit, Exit::Installed, "{}", out.err);
        assert_eq!(rig.dest_text().as_deref(), Some("new"));
        assert!(rig.olds().is_empty(), "{:?}", rig.olds());
        assert!(rig.receipt().is_some());
        assert!(!out.out.contains("still running"), "{}", out.out);
    }
}

#[test]
fn unix_keeps_a_hard_link_to_the_old_binary_until_the_receipt_is_published() {
    let rig = new_rig().with_old();
    let mut files = Scripted::new();
    let mut seen = Vec::new();
    let out = run_publishing(
        &rig,
        false,
        &mut files,
        || Ok(()),
        || {
            seen.push((
                rig.olds().first().and_then(|old| Rig::text(old)),
                rig.dest_text(),
            ));
        },
    );
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(seen, [(Some("old".to_owned()), Some("new".to_owned()))]);
    assert!(files.did("hard_link dest old"));
    assert!(!files.did("copy dest old"));
    assert!(!files.did("rename dest old"), "{:?}", files.log);
    assert!(rig.olds().is_empty());
}

#[test]
fn unix_copies_the_old_binary_when_it_cannot_link_it() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("hard_link", "dest", "old")]);
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert!(files.did("copy dest old"));
    assert_eq!(rig.dest_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty());
}

#[test]
fn windows_moves_the_old_binary_aside_and_never_links_or_copies_it() {
    let rig = new_rig().with_old();
    let mut files = Scripted::new();
    let mut seen = Vec::new();
    let out = run_publishing(
        &rig,
        true,
        &mut files,
        || Ok(()),
        || {
            seen.push((
                rig.olds().first().and_then(|old| Rig::text(old)),
                rig.dest_text(),
            ));
        },
    );
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(seen, [(Some("old".to_owned()), Some("new".to_owned()))]);
    assert!(files.did("rename dest old"));
    assert!(files.did("rename from dest"));
    assert!(!files.did("hard_link dest old") && !files.did("copy dest old"));
    assert!(rig.olds().is_empty());
}

#[test]
fn a_binary_that_cannot_be_deleted_is_left_for_a_later_install_and_said() {
    // Windows cannot delete a running image.
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("remove_file", "old", "")]);
    let out = run_with(&rig, true, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert!(
        out.out
            .contains("An older open-ferry is still running: restart it to run this one."),
        "{}",
        out.out
    );
    assert_eq!(rig.olds().len(), 1);
    assert_eq!(rig.dest_text().as_deref(), Some("new"));
    assert!(rig.receipt().is_some());

    // On Unix it is a warning, not a running program.
    let rig = rig_with_old_again();
    let mut files = Scripted::failing(&[("remove_file", "old", "")]);
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Installed);
    assert!(out.err.contains("can't remove"), "{}", out.err);
    assert!(!out.out.contains("still running"));
}

fn rig_with_old_again() -> Rig {
    new_rig().with_old()
}

#[test]
fn rollback_material_left_by_earlier_installs_is_swept_but_nothing_else() {
    let rig = new_rig().with_old();
    let stale = rig.bin.join("open-ferry.old-1f-2a-9");
    let notes = rig.bin.join("open-ferry.old-notes");
    let other = rig.bin.join("other.old-1f");
    for path in [&stale, &notes, &other] {
        fs::write(path, b"x").unwrap();
    }
    let out = run_with(&rig, true, &mut Scripted::new());
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert!(!stale.exists());
    assert!(notes.exists() && other.exists());

    // With nothing installed there is nothing to sweep around: whatever an
    // earlier failed restore left is the user's only copy.
    let rig = new_rig();
    fs::write(rig.bin.join("open-ferry.old-1f-2a-9"), b"keep").unwrap();
    let out = run_with(&rig, true, &mut Scripted::new());
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(rig.olds().len(), 1);
}

#[test]
fn a_swept_binary_that_is_still_running_is_said_once() {
    let rig = new_rig().with_old();
    fs::write(rig.bin.join("open-ferry.old-1f-2a-9"), b"x").unwrap();
    // Both the stale one and this install's own cannot be removed.
    let mut files = Scripted::failing(&[("remove_file", "old", "")]);
    let out = run_with(&rig, true, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(out.out.matches("still running").count(), 1, "{}", out.out);
}

#[test]
fn a_receipt_published_but_not_durable_keeps_the_install_and_warns() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        let out = run_publishing(
            &rig,
            windows,
            &mut Scripted::new(),
            || {
                Err(WriteError::PublishedNotDurable(io::Error::other(
                    "sync failed",
                )))
            },
            || {},
        );
        assert_eq!(out.exit, Exit::Installed, "{}", out.err);
        assert!(out.err.contains("may lose the receipt"), "{}", out.err);
        assert!(out.err.contains("sync failed"), "{}", out.err);
        assert_eq!(rig.dest_text().as_deref(), Some("new"));
        assert!(rig.olds().is_empty());
    }
}

// --- Rollback: the table, row by row ----------------------------------------

/// Row 1, making rollback material, on Unix.
#[test]
fn unix_a_failure_to_make_rollback_material_changes_nothing() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("hard_link", "dest", "old"), ("copy", "dest", "old")]);
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Failed);
    assert!(out.err.contains("Nothing was changed"), "{}", out.err);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty(), "the partial copy is removed");
    assert!(rig.receipt().is_none());
    assert!(!files.did("rename from dest"));
}

#[test]
fn unix_a_partial_copy_that_cannot_be_removed_is_named() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[
        ("hard_link", "dest", "old"),
        ("copy", "dest", "old"),
        ("remove_file", "old", ""),
    ]);
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Failed);
    assert!(out.err.contains("a partial copy"), "{}", out.err);
    assert!(out.err.contains("open-ferry.old-1f-2a-0"), "{}", out.err);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
}

/// Row 1 on Windows: the move aside fails and the destination is untouched.
#[test]
fn windows_a_failed_move_aside_changes_nothing() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("rename", "dest", "old")]);
    let out = run_with(&rig, true, &mut files);
    assert_eq!(out.exit, Exit::Failed);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty());
    assert!(rig.receipt().is_none());
}

/// Row 2, placing the new binary, on Unix.
#[test]
fn unix_a_failed_placement_removes_the_rollback_link() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("rename", "from", "dest")]);
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Failed);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty());
    assert!(rig.receipt().is_none());
}

#[test]
fn unix_a_rollback_link_that_cannot_be_removed_after_a_failed_placement_is_named() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("rename", "from", "dest"), ("remove_file", "old", "")]);
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Failed);
    assert!(out.err.contains("open-ferry.old-1f-2a-0"), "{}", out.err);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
}

/// Row 2 on Windows: the old binary moves back.
#[test]
fn windows_a_failed_placement_moves_the_old_binary_back() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("rename", "from", "dest")]);
    let out = run_with(&rig, true, &mut files);
    assert_eq!(out.exit, Exit::Failed);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty());
    assert!(files.did("rename old dest"));
}

#[test]
fn windows_a_failed_placement_and_a_failed_move_back_exit_6_naming_every_file() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("rename", "from", "dest"), ("rename", "old", "dest")]);
    let out = run_with(&rig, true, &mut files);
    assert_eq!(out.exit, Exit::PartlyChanged);
    assert!(rig.dest_text().is_none());
    assert_eq!(rig.olds().len(), 1);
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    let old = &rig.olds()[0];
    for path in [&rig.dest, old, &rig.from] {
        assert!(
            out.err.contains(&path.display().to_string()),
            "{path:?} missing from:\n{}",
            out.err
        );
    }
    assert!(out.err.contains("move"), "{}", out.err);
}

#[test]
fn a_fresh_install_whose_placement_fails_changes_nothing() {
    for windows in BOTH {
        let rig = new_rig();
        let mut files = Scripted::failing(&[("rename", "from", "dest")]);
        let out = run_with(&rig, windows, &mut files);
        assert_eq!(out.exit, Exit::Failed);
        assert_eq!(rig.dest_text(), None);
        assert_eq!(rig.staged_text().as_deref(), Some("new"));
        assert!(rig.receipt().is_none());
    }
}

/// Row 3, receipt not published with an old binary, on Unix.
#[test]
fn unix_an_unpublished_receipt_restores_the_old_binary() {
    let rig = new_rig().with_old();
    let mut files = Scripted::new();
    let out = run_publishing(&rig, false, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::Failed);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert!(rig.olds().is_empty());
    assert!(rig.receipt().is_none());
    assert!(files.did("rename old dest"));
    assert!(out.err.contains("disk full"), "{}", out.err);
}

#[test]
fn unix_an_unpublished_receipt_and_a_failed_restore_exit_6_naming_every_file() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("rename", "old", "dest")]);
    let out = run_publishing(&rig, false, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::PartlyChanged);
    assert_eq!(rig.dest_text().as_deref(), Some("new"));
    assert_eq!(rig.olds().len(), 1);
    assert_eq!(Rig::text(&rig.olds()[0]).as_deref(), Some("old"));
    for path in [&rig.dest, &rig.olds()[0], &rig.from] {
        assert!(out.err.contains(&path.display().to_string()), "{}", out.err);
    }
    assert!(
        out.err.contains("now holds the new open-ferry"),
        "{}",
        out.err
    );
}

/// Row 3 on Windows: the running new image goes back to `from` first.
#[test]
fn windows_an_unpublished_receipt_moves_the_new_binary_back_and_restores_the_old() {
    let rig = new_rig().with_old();
    let mut files = Scripted::new();
    let out = run_publishing(&rig, true, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::Failed);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty());
    assert!(rig.receipt().is_none());
    let back = files
        .log
        .iter()
        .position(|l| l == "rename dest from")
        .unwrap();
    let restore = files
        .log
        .iter()
        .rposition(|l| l == "rename old dest")
        .unwrap();
    assert!(back < restore, "{:?}", files.log);
}

#[test]
fn windows_a_failed_move_back_of_the_new_binary_exits_6() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("rename", "dest", "from")]);
    let out = run_publishing(&rig, true, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::PartlyChanged);
    assert_eq!(rig.dest_text().as_deref(), Some("new"));
    assert_eq!(rig.olds().len(), 1);
    assert_eq!(rig.staged_text(), None);
    for path in [&rig.dest, &rig.olds()[0], &rig.from] {
        assert!(out.err.contains(&path.display().to_string()), "{}", out.err);
    }
}

#[test]
fn windows_a_failed_restore_of_the_old_binary_exits_6() {
    let rig = new_rig().with_old();
    // The first `rename old dest` of this run is the restore: none came before.
    let mut files = Scripted::failing(&[("rename", "old", "dest")]);
    let out = run_publishing(&rig, true, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::PartlyChanged);
    assert_eq!(rig.dest_text(), None);
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert_eq!(rig.olds().len(), 1);
    assert!(out.err.contains("now holds nothing"), "{}", out.err);
    assert!(out.err.contains("To restore the old one"), "{}", out.err);
}

/// Row 4, a fresh install whose receipt is not published.
#[test]
fn a_fresh_install_with_an_unpublished_receipt_moves_the_binary_back() {
    for windows in BOTH {
        let rig = new_rig();
        let mut files = Scripted::new();
        let out = run_publishing(&rig, windows, &mut files, not_published, || {});
        assert_eq!(out.exit, Exit::Failed, "{}", out.err);
        assert_eq!(rig.dest_text(), None, "absence is restored");
        assert_eq!(rig.staged_text().as_deref(), Some("new"));
        assert!(rig.receipt().is_none());
    }
}

#[test]
fn a_fresh_install_with_an_unpublished_receipt_and_a_failed_move_back_exits_6() {
    for windows in BOTH {
        let rig = new_rig();
        let mut files = Scripted::failing(&[("rename", "dest", "from")]);
        let out = run_publishing(&rig, windows, &mut files, not_published, || {});
        assert_eq!(out.exit, Exit::PartlyChanged);
        assert_eq!(rig.dest_text().as_deref(), Some("new"));
        assert!(out.err.contains(&rig.dest.display().to_string()));
        assert!(out.err.contains(&rig.from.display().to_string()));
        assert!(out.err.contains("delete"), "{}", out.err);
    }
}

#[test]
fn a_receipt_the_real_writer_cannot_publish_restores_the_old_binary() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        // A directory where the receipt goes.
        fs::create_dir_all(rig.data.receipt_file()).unwrap();
        let out = run_with(&rig, windows, &mut Scripted::new());
        assert_eq!(out.exit, Exit::Failed, "{}", out.err);
        assert!(out.err.contains("install receipt"), "{}", out.err);
        assert_eq!(rig.dest_text().as_deref(), Some("old"));
        assert!(rig.olds().is_empty());
        assert!(rig.data.receipt_file().is_dir());
    }
}

// --- The wait and detection --------------------------------------------------

/// A detector that finds `outcome` each time, counting its calls and noting
/// the deadlines it was given.
struct Scripted3 {
    calls: Rc<RefCell<Vec<Instant>>>,
}

fn call_counter() -> Scripted3 {
    Scripted3 {
        calls: Rc::default(),
    }
}

#[test]
fn a_supervisor_exits_3_with_the_stop_instructions_and_changes_nothing() {
    let rig = new_rig().with_old();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, _| {
            Detection::Supervised(Running {
                pid: Some(4242),
                version: Some("0.1.0".into()),
            })
        },
        &mut Virtual::new(),
        Duration::from_secs(5),
    );
    assert_eq!(out.exit, Exit::Supervised);
    assert!(
        out.err.contains("process 4242, version 0.1.0"),
        "{}",
        out.err
    );
    assert!(
        out.err
            .contains("Stop the service, run the installer again, and start the service again"),
        "{}",
        out.err
    );
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
}

#[test]
fn a_supervisor_that_gave_no_details_is_still_refused() {
    let rig = new_rig();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, _| Detection::Supervised(Running::default()),
        &mut Virtual::new(),
        Duration::ZERO,
    );
    assert_eq!(out.exit, Exit::Supervised);
    assert!(out.err.contains("A supervisor") || out.err.contains("supervisor is running"));
}

#[test]
fn not_being_able_to_tell_exits_4_with_the_reason() {
    let rig = new_rig().with_old();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, _| Detection::Refuse("the install lock is a link".into()),
        &mut Virtual::new(),
        Duration::from_secs(5),
    );
    assert_eq!(out.exit, Exit::Unknown);
    assert!(
        out.err.contains("the install lock is a link"),
        "{}",
        out.err
    );
    assert!(out.err.contains("Nothing was changed"), "{}", out.err);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
}

#[test]
fn a_busy_detection_releases_the_update_lock_and_tries_again_until_clear() {
    let rig = new_rig();
    let mut clock = Virtual::new();
    let mut calls = 0;
    let data = rig.data.clone();
    let free_while_waiting = Rc::new(Cell::new(0));
    let seen = free_while_waiting.clone();
    // The update lock is free while it waits.
    clock.on_sleep = Some(Box::new(move |_| {
        if data.try_lock_existing_dir().unwrap().is_some() {
            seen.set(seen.get() + 1);
        }
    }));
    let dest = rig.dest.clone();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |receipt, path| receipt.write_atomic(path),
        &mut |_, _, _| {
            calls += 1;
            if calls < 3 {
                Detection::Busy("another install holds the lock".into())
            } else {
                clear(&dest)
            }
        },
        &mut clock,
        Duration::from_secs(10),
    );
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(calls, 3);
    assert_eq!(free_while_waiting.get(), 2);
    assert_eq!(clock.slept(), [RETRY_EVERY, RETRY_EVERY]);
    assert_eq!(
        out.out
            .matches("Waiting for another open-ferry update or install")
            .count(),
        1,
        "{}",
        out.out
    );
}

#[test]
fn a_wait_that_runs_out_on_a_busy_detection_exits_5_saying_what_it_waited_for() {
    let rig = new_rig().with_old();
    let mut clock = Virtual::new();
    let mut calls = 0;
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, _| {
            calls += 1;
            Detection::Busy(
                "a supervisor of another data directory holds /x/.open-ferry.lock".into(),
            )
        },
        &mut clock,
        Duration::from_secs(1),
    );
    assert_eq!(out.exit, Exit::WaitedOut);
    assert_eq!(calls, 4, "every 250 ms until the second is used");
    assert_eq!(
        clock.slept().iter().sum::<Duration>(),
        Duration::from_secs(1)
    );
    assert!(clock.slept().iter().all(|pause| *pause <= RETRY_EVERY));
    assert!(
        out.err.contains("gave up after waiting 1 seconds"),
        "{}",
        out.err
    );
    assert!(out.err.contains("/x/.open-ferry.lock"), "{}", out.err);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
}

#[test]
fn wait_0_makes_one_attempt_and_never_pauses() {
    let rig = new_rig().with_old();
    let mut clock = Virtual::new();
    let mut calls = 0;
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, _| {
            calls += 1;
            Detection::Busy("busy".into())
        },
        &mut clock,
        Duration::ZERO,
    );
    assert_eq!(out.exit, Exit::WaitedOut);
    assert_eq!(calls, 1);
    assert!(clock.slept().is_empty());
    assert!(!out.out.contains("Waiting"), "{}", out.out);
}

#[test]
fn wait_0_still_installs_when_nothing_is_in_the_way() {
    let rig = new_rig();
    let dest = rig.dest.clone();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |receipt, path| receipt.write_atomic(path),
        &mut move |_, _, _| clear(&dest),
        &mut Virtual::new(),
        Duration::ZERO,
    );
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
}

#[test]
fn a_detector_that_stalls_until_its_deadline_cannot_push_the_run_past_the_wait() {
    for wait in [1, 2, 10] {
        let rig = new_rig();
        let mut clock = Virtual::new();
        let base = clock.base;
        let elapsed = clock.elapsed.clone();
        let counter = call_counter();
        let calls = counter.calls.clone();
        let out = execute(
            &rig,
            false,
            &mut Scripted::new(),
            &mut |_, _| panic!("nothing is published"),
            &mut |_, _, deadline| {
                let now = base + elapsed.get();
                calls.borrow_mut().push(deadline);
                assert!(deadline <= now + DETECT_BUDGET, "more than the budget");
                // It answers only when its deadline comes.
                elapsed.set(deadline - base);
                Detection::Busy("stalled".into())
            },
            &mut clock,
            Duration::from_secs(wait),
        );
        assert_eq!(out.exit, Exit::WaitedOut, "{}", out.err);
        assert!(
            clock.elapsed.get() <= Duration::from_secs(wait),
            "wait {wait}: took {:?}",
            clock.elapsed.get()
        );
        for deadline in calls.borrow().iter() {
            assert!(*deadline <= base + Duration::from_secs(wait));
        }
    }
}

#[test]
fn detection_gets_the_wait_deadline_or_three_seconds_whichever_is_less() {
    let now = Instant::now();
    let second = Duration::from_secs(1);
    // Never past the wait, never more than the budget, and no floor: with
    // `-wait 0` the deadline has passed already.
    assert_eq!(detect_deadline(now, now + 10 * second), now + DETECT_BUDGET);
    assert_eq!(detect_deadline(now, now + 2 * second), now + 2 * second);
    assert_eq!(detect_deadline(now, now), now);
}

#[test]
fn an_update_lock_wait_followed_by_a_stalled_detector_never_passes_the_wait_deadline() {
    let rig = new_rig();
    // Held for the first second, then free: the detector then stalls.
    let held = Rc::new(RefCell::new(Some(rig.data.try_lock().unwrap().unwrap())));
    let mut clock = Virtual::new();
    let base = clock.base;
    let elapsed = clock.elapsed.clone();
    let release = held.clone();
    let deadlines = Rc::new(RefCell::new(Vec::new()));
    let seen = deadlines.clone();
    clock.on_sleep = Some(Box::new(move |count| {
        if count == 4 {
            release.borrow_mut().take();
        }
    }));
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, deadline| {
            seen.borrow_mut().push(deadline);
            // It answers only when its deadline comes.
            elapsed.set(deadline - base);
            Detection::Busy("stalled".into())
        },
        &mut clock,
        Duration::from_secs(2),
    );
    assert_eq!(out.exit, Exit::WaitedOut, "{}", out.err);
    let wait_deadline = base + Duration::from_secs(2);
    assert!(!deadlines.borrow().is_empty(), "the detector was asked");
    for deadline in deadlines.borrow().iter() {
        assert!(*deadline <= wait_deadline, "past the wait deadline");
    }
    assert!(clock.elapsed.get() <= Duration::from_secs(2));
}

#[test]
fn wait_0_with_a_free_install_lock_installs_and_with_a_held_one_gives_the_waited_out_exit() {
    let rig = new_rig().with_old();
    // Free: installs, and detection was given a deadline that had passed.
    let mut clock = Virtual::new();
    let base = clock.base;
    let given = Rc::new(RefCell::new(Vec::new()));
    let seen = given.clone();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |receipt, path| receipt.write_atomic(path),
        &mut |_, installed, deadline| {
            seen.borrow_mut().push(deadline);
            lock_detector(installed)
        },
        &mut clock,
        Duration::ZERO,
    );
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(*given.borrow(), [base], "one attempt, due at once");

    // Held: one attempt, no pause, no overrun, and it says what may hold it.
    let rig = new_rig().with_old();
    let _held = InstallLock::try_acquire(&rig.dest).unwrap();
    let mut clock = Virtual::new();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, installed, _| lock_detector(installed),
        &mut clock,
        Duration::ZERO,
    );
    assert_eq!(out.exit, Exit::WaitedOut, "{}", out.err);
    assert!(clock.slept().is_empty());
    assert_eq!(clock.elapsed.get(), Duration::ZERO);
    assert!(
        out.err.contains("supervisor may be what holds"),
        "{}",
        out.err
    );
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
}

#[test]
fn a_held_update_lock_exits_5_and_detection_is_never_asked() {
    let rig = new_rig().with_old();
    let held = rig.data.try_lock().unwrap().expect("the test's lock");
    let mut clock = Virtual::new();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, _| panic!("detection runs only under the update lock"),
        &mut clock,
        Duration::from_secs(1),
    );
    assert_eq!(out.exit, Exit::WaitedOut);
    assert!(out.err.contains("holds the update lock"), "{}", out.err);
    assert!(clock.slept().iter().all(|pause| *pause <= RETRY_EVERY));
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    drop(held);
}

#[test]
fn an_update_lock_released_during_the_wait_lets_the_install_go_on() {
    let rig = new_rig();
    let held = Rc::new(RefCell::new(Some(rig.data.try_lock().unwrap().unwrap())));
    let mut clock = Virtual::new();
    let release = held.clone();
    clock.on_sleep = Some(Box::new(move |count| {
        if count == 2 {
            release.borrow_mut().take();
        }
    }));
    let dest = rig.dest.clone();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |receipt, path| receipt.write_atomic(path),
        &mut move |_, _, _| clear(&dest),
        &mut clock,
        Duration::from_secs(5),
    );
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(clock.slept().len(), 2);
}

/// What detection finds when the install lock is held by another: busy.
fn lock_detector(installed: &Path) -> Detection {
    match InstallLock::try_acquire(installed) {
        Ok(lock) => Detection::Clear(lock),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Detection::Busy(format!("{error}"))
        }
        Err(error) => Detection::Refuse(error.to_string()),
    }
}

#[test]
fn a_held_install_lock_exits_5_and_a_released_one_lets_the_install_go_on() {
    let rig = new_rig().with_old();
    let held = Rc::new(RefCell::new(Some(
        InstallLock::try_acquire(&rig.dest).unwrap(),
    )));
    let mut clock = Virtual::new();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, installed, _| lock_detector(installed),
        &mut clock,
        Duration::from_secs(1),
    );
    assert_eq!(out.exit, Exit::WaitedOut);
    assert!(out.err.contains("install lock"), "{}", out.err);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));

    let mut clock = Virtual::new();
    let release = held.clone();
    clock.on_sleep = Some(Box::new(move |count| {
        if count == 2 {
            release.borrow_mut().take();
        }
    }));
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |receipt, path| receipt.write_atomic(path),
        &mut |_, installed, _| lock_detector(installed),
        &mut clock,
        Duration::from_secs(5),
    );
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(rig.dest_text().as_deref(), Some("new"));
}

#[test]
fn both_locks_are_held_until_the_receipt_is_published() {
    let rig = new_rig().with_old();
    let data = rig.data.clone();
    let dest = rig.dest.clone();
    let mut during = None;
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |receipt, path| {
            during = Some((
                data.try_lock_existing_dir().unwrap().is_none(),
                InstallLock::try_acquire(&dest).is_err(),
            ));
            receipt.write_atomic(path)
        },
        &mut {
            let dest = rig.dest.clone();
            move |_, _, _| clear(&dest)
        },
        &mut Virtual::new(),
        Duration::from_secs(1),
    );
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(during, Some((true, true)));
    // And both are free afterwards.
    assert!(rig.data.try_lock_existing_dir().unwrap().is_some());
    assert!(InstallLock::try_acquire(&rig.dest).is_ok());
}

#[test]
fn the_real_clock_waits_about_the_wait_for_a_held_update_lock() {
    let rig = new_rig();
    let _held = rig.data.try_lock().unwrap().unwrap();
    let started = Instant::now();
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, _| panic!("detection runs only under the update lock"),
        &mut RealClock,
        Duration::from_secs(1),
    );
    let took = started.elapsed();
    assert_eq!(out.exit, Exit::WaitedOut);
    assert!(took >= Duration::from_millis(900), "{took:?}");
    assert!(took < Duration::from_secs(4), "{took:?}");
}

// --- The checks and the arguments -------------------------------------------

#[test]
fn the_checks_pass_for_the_binary_that_runs_and_a_regular_destination() {
    let rig = new_rig().with_old();
    assert_eq!(check_paths(&rig.from, &rig.dest, &rig.from), Ok(()));
    let fresh = new_rig();
    assert_eq!(check_paths(&fresh.from, &fresh.dest, &fresh.from), Ok(()));
}

#[test]
fn a_source_that_is_missing_or_not_a_regular_file_is_refused_with_exit_2() {
    let rig = new_rig();
    let missing = rig.bin.join("absent");
    let error = check_paths(&missing, &rig.dest, &missing).unwrap_err();
    assert!(error.contains("can't read -from"), "{error}");
    let error = check_paths(&rig.bin, &rig.dest, &rig.bin).unwrap_err();
    assert!(error.contains("isn't a regular file"), "{error}");
    let out = execute(
        &Rig {
            from: missing,
            ..rig
        },
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, _| panic!("nothing is detected"),
        &mut Virtual::new(),
        Duration::ZERO,
    );
    assert_eq!(out.exit, Exit::Usage);
}

#[test]
fn a_source_that_is_not_the_binary_that_runs_is_refused() {
    let rig = new_rig();
    let other = rig.bin.join("other");
    fs::write(&other, b"new").unwrap();
    let error = check_paths(&rig.from, &rig.dest, &other).unwrap_err();
    assert!(error.contains("isn't the binary that runs"), "{error}");
    // The same bytes under another name are not the same file.
    let copy = rig.bin.join("copy");
    fs::copy(&rig.from, &copy).unwrap();
    assert!(check_paths(&copy, &rig.dest, &rig.from).is_err());
    // A hard link to it is.
    let link = rig.bin.join("link");
    fs::hard_link(&rig.from, &link).unwrap();
    assert_eq!(check_paths(&link, &rig.dest, &rig.from), Ok(()));
}

/// A link to `target` at `link`, or none where the system doesn't allow it.
fn make_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).is_ok();
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(target, link).is_ok();
}

#[test]
fn a_source_that_is_a_link_is_refused() {
    let rig = new_rig();
    let link = rig.bin.join("linked");
    if !make_link(&rig.from, &link) {
        eprintln!("can't make a symlink here: skipped");
        return;
    }
    let error = check_paths(&link, &rig.dest, &link).unwrap_err();
    assert!(error.contains("a link or reparse point"), "{error}");
}

#[test]
fn a_destination_that_is_a_link_is_refused_so_the_file_it_names_is_installed_to() {
    let rig = new_rig().with_old();
    let real = rig.bin.join("real-open-ferry");
    fs::rename(&rig.dest, &real).unwrap();
    if !make_link(&real, &rig.dest) {
        eprintln!("can't make a symlink here: skipped");
        return;
    }
    let error = check_paths(&rig.from, &rig.dest, &rig.from).unwrap_err();
    assert!(error.contains("install to the file it names"), "{error}");
    let out = run_with(&rig, false, &mut Scripted::new());
    assert_eq!(out.exit, Exit::Usage);
    assert_eq!(Rig::text(&real).as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
}

#[cfg(windows)]
#[test]
fn a_reparse_point_is_a_link() {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    assert!(has_reparse_attribute(
        FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT
    ));
    assert!(!has_reparse_attribute(FILE_ATTRIBUTE_DIRECTORY));

    // A junction at the destination: a real reparse point, with no
    // privilege needed.
    let rig = new_rig();
    let target = rig.bin.join("target dir");
    fs::create_dir(&target).unwrap();
    let junction = rig.bin.join("junction");
    let made = std::process::Command::new(std::env::var_os("COMSPEC").unwrap())
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let error = check_paths(&rig.from, &junction, &rig.from).unwrap_err();
    assert!(error.contains("a link or reparse point"), "{error}");
}

#[test]
fn a_destination_that_is_a_directory_or_has_no_directory_is_refused() {
    let rig = new_rig();
    let error = check_paths(&rig.from, &rig.bin, &rig.from).unwrap_err();
    assert!(error.contains("isn't a regular file"), "{error}");
    let nowhere = rig.bin.join("absent dir").join("open-ferry");
    let error = check_paths(&rig.from, &nowhere, &rig.from).unwrap_err();
    assert!(error.contains("doesn't exist"), "{error}");
}

#[test]
fn a_destination_that_is_the_source_is_refused() {
    let rig = new_rig();
    let error = check_paths(&rig.from, &rig.from, &rig.from).unwrap_err();
    assert!(error.contains("the same file"), "{error}");
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

#[test]
fn the_arguments_are_read() {
    let options = parse(args(&[
        "-from",
        "/a/.new",
        "-to",
        "/a/open-ferry",
        "-installer",
        "install.sh",
    ]))
    .unwrap();
    assert_eq!(options.from, Path::new("/a/.new"));
    assert_eq!(options.to, Path::new("/a/open-ferry"));
    assert_eq!(options.installer, "install.sh");
    assert_eq!(options.wait, DEFAULT_WAIT);

    let options = parse(args(&[
        "--installer",
        "install.ps1",
        "-wait",
        "0",
        "--to",
        "t",
        "-from",
        "f",
    ]))
    .unwrap();
    assert_eq!(options.installer, "install.ps1");
    assert_eq!(options.wait, Duration::ZERO);
    assert_eq!(
        parse(args(&[
            "-from",
            "f",
            "-to",
            "t",
            "-installer",
            "install.sh",
            "-wait",
            "86400",
        ]))
        .unwrap()
        .wait,
        Duration::from_secs(86_400)
    );
}

#[test]
fn bad_arguments_are_usage_errors() {
    let good = ["-from", "f", "-to", "t", "-installer", "install.sh"];
    let cases: [(Vec<&str>, &str); 10] = [
        (vec![], "-from is needed"),
        (vec!["-from", "f"], "-to is needed"),
        (vec!["-from", "f", "-to", "t"], "-installer is needed"),
        (vec!["-from"], "-from needs a value"),
        (vec!["-from", ""], "-from needs a value"),
        (vec!["-bogus", "x"], "unknown argument: -bogus"),
        (vec!["---from", "x"], "unknown argument: ---from"),
        (vec!["from", "x"], "unknown argument: from"),
        (
            vec!["-installer", "install.exe"],
            "install.sh or install.ps1",
        ),
        (vec!["-wait", "-1"], "-wait is a number of seconds"),
    ];
    for (given, message) in cases {
        let error = parse(args(&given)).unwrap_err();
        assert!(error.contains(message), "{given:?}: {error}");
    }
    for wait in ["x", "1.5", "86401"] {
        let mut given = good.to_vec();
        given.extend(["-wait", wait]);
        let error = parse(args(&given)).unwrap_err();
        assert!(
            error.contains("-wait is a number of seconds"),
            "{wait}: {error}"
        );
    }
}

#[test]
fn the_usage_names_the_arguments() {
    let usage = usage("open-ferry");
    for text in ["install-self", "-from", "-to", "-installer", "-wait"] {
        assert!(usage.contains(text), "{usage}");
    }
}

#[test]
fn the_exit_codes_are_the_documented_ones() {
    assert_eq!(
        [
            Exit::Installed,
            Exit::Failed,
            Exit::Usage,
            Exit::Supervised,
            Exit::Unknown,
            Exit::WaitedOut,
            Exit::PartlyChanged,
        ]
        .map(|exit| exit as u8),
        [0, 1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn a_verbatim_prefix_is_stripped_from_a_resolved_path() {
    assert_eq!(strip_verbatim(r"\\?\C:\Users\me\bin"), r"C:\Users\me\bin");
    assert_eq!(
        strip_verbatim(r"\\?\UNC\server\share\bin"),
        r"\\server\share\bin"
    );
    assert_eq!(strip_verbatim(r"\\?\Volume{1}\bin"), r"\\?\Volume{1}\bin");
    assert_eq!(strip_verbatim("/home/me/bin"), "/home/me/bin");
    assert_eq!(strip_verbatim(r"C:\bin"), r"C:\bin");
}

#[test]
fn the_destination_is_named_by_its_resolved_directory_and_its_own_name() {
    let rig = new_rig();
    let resolved = canonical_dest(&rig.dest).unwrap();
    assert_eq!(resolved.file_name().unwrap(), "open-ferry");
    assert!(resolved.is_absolute());
    assert!(!resolved.to_string_lossy().starts_with(r"\\?\"));
    assert!(same_file::is_same_file(resolved.parent().unwrap(), &rig.bin).unwrap());
}

#[test]
fn rollback_material_is_named_beside_the_destination() {
    let old = old_path(Path::new("/b/open-ferry"), "1f-2a-0");
    assert_eq!(old, Path::new("/b/open-ferry.old-1f-2a-0"));
    assert_eq!(unique_suffix().split('-').count(), 3);
    assert_ne!(unique_suffix(), unique_suffix());
}

// --- The destination is resolved once ----------------------------------------

#[test]
fn detection_and_the_replacement_use_the_one_resolved_destination() {
    let rig = new_rig().with_old();
    // A path that isn't its own resolution.
    let given = rig.bin.join("..").join("bin dir").join("open-ferry");
    let rig = Rig { dest: given, ..rig };
    let resolved = canonical_dest(&rig.dest).unwrap();
    assert_ne!(resolved, rig.dest);

    let detected = Rc::new(RefCell::new(Vec::new()));
    let seen = detected.clone();
    let mut files = Scripted::new();
    let out = execute(
        &rig,
        false,
        &mut files,
        &mut |receipt, path| receipt.write_atomic(path),
        &mut |_, installed, _| {
            seen.borrow_mut().push(installed.to_owned());
            lock_detector(installed)
        },
        &mut Virtual::new(),
        Duration::from_secs(1),
    );
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(*detected.borrow(), std::slice::from_ref(&resolved));
    assert!(
        files
            .renames
            .contains(&(rig.from.clone(), resolved.clone()))
    );
    for (from, to) in &files.renames {
        for path in [from, to] {
            assert!(
                !path.components().any(|part| part.as_os_str() == ".."),
                "{path:?} isn't resolved"
            );
        }
    }
    assert_eq!(Path::new(&rig.receipt().unwrap().binary), resolved);
    assert_eq!(rig.dest_text().as_deref(), Some("new"));
}

#[test]
fn a_destination_that_cannot_be_resolved_is_a_usage_error_before_any_lock() {
    let rig = new_rig();
    let nowhere = rig.bin.join("absent dir").join("open-ferry");
    let rig = Rig {
        dest: nowhere,
        ..rig
    };
    let out = execute(
        &rig,
        false,
        &mut Scripted::new(),
        &mut |_, _| panic!("nothing is published"),
        &mut |_, _, _| panic!("nothing is detected"),
        &mut Virtual::new(),
        Duration::ZERO,
    );
    assert_eq!(out.exit, Exit::Usage);
    assert!(!rig.data.root().exists(), "no lock, no data directory");
}

// --- Durability ---------------------------------------------------------------

/// A receipt writer that must not be reached.
fn unreachable_publish() -> Result<(), WriteError> {
    panic!("the receipt was published");
}

#[test]
fn the_staged_file_is_flushed_before_anything_is_moved() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        let mut files = Scripted::new();
        let out = run_with(&rig, windows, &mut files);
        assert_eq!(out.exit, Exit::Installed, "{}", out.err);
        assert!(files.at("sync_file from") < files.at("rename from dest"));
        if windows {
            assert!(files.at("sync_file from") < files.at("rename dest old"));
        } else {
            assert!(files.at("sync_file from") < files.at("hard_link dest old"));
        }
    }
}

#[test]
fn a_failed_flush_of_the_staged_file_changes_nothing() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        let mut files = Scripted::failing(&[("sync_file", "from", "")]);
        let out = run_publishing(&rig, windows, &mut files, unreachable_publish, || {});
        assert_eq!(out.exit, Exit::Failed);
        assert!(out.err.contains("Nothing was changed"), "{}", out.err);
        assert!(out.err.contains("injected failure"), "{}", out.err);
        assert_eq!(rig.dest_text().as_deref(), Some("old"));
        assert_eq!(rig.staged_text().as_deref(), Some("new"));
        assert!(rig.olds().is_empty());
        assert!(!files.did("rename from dest"));
    }
}

#[test]
fn every_move_is_followed_by_a_sync_of_the_binary_directory() {
    // Unix: the link, the placement. Windows: the move aside, the placement.
    for windows in BOTH {
        let rig = new_rig().with_old();
        let mut files = Scripted::new();
        let out = run_with(&rig, windows, &mut files);
        assert_eq!(out.exit, Exit::Installed, "{}", out.err);
        let first = if windows {
            "rename dest old"
        } else {
            "hard_link dest old"
        };
        let syncs: Vec<usize> = files
            .log
            .iter()
            .enumerate()
            .filter(|(_, line)| *line == "sync_dir bin")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(syncs.len(), 2, "{:?}", files.log);
        assert!(files.at(first) < syncs[0] && syncs[0] < files.at("rename from dest"));
        assert!(files.at("rename from dest") < syncs[1]);
    }
}

#[test]
fn unix_a_copy_is_synced_as_well_as_its_directory() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("hard_link", "dest", "old")]);
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert!(files.at("copy dest old") < files.at("sync_file old"));
    assert!(files.at("sync_file old") < files.at("sync_dir bin"));
}

#[test]
fn unix_a_failed_sync_of_the_rollback_link_changes_nothing() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing_nth(&[("sync_dir", "bin", "", 1)]);
    let out = run_publishing(&rig, false, &mut files, unreachable_publish, || {});
    assert_eq!(out.exit, Exit::Failed);
    assert!(out.err.contains("Nothing was changed"), "{}", out.err);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty(), "the unconfirmed link is removed");
    assert!(!files.did("rename from dest"));
}

#[test]
fn unix_a_failed_sync_of_a_rollback_copy_changes_nothing() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("hard_link", "dest", "old"), ("sync_file", "old", "")]);
    let out = run_publishing(&rig, false, &mut files, unreachable_publish, || {});
    assert_eq!(out.exit, Exit::Failed);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty(), "the unconfirmed copy is removed");
}

#[test]
fn windows_a_failed_sync_of_the_move_aside_moves_the_old_binary_back() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing_nth(&[("sync_dir", "bin", "", 1)]);
    let out = run_publishing(&rig, true, &mut files, unreachable_publish, || {});
    assert_eq!(out.exit, Exit::Failed, "{}", out.err);
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty());
    assert!(files.did("rename old dest"));
    assert!(!files.did("rename from dest"));
}

#[test]
fn a_failed_sync_after_the_placement_is_a_placement_failure_and_rolls_back() {
    // The second sync of the binary directory is the placement's, on both.
    for windows in BOTH {
        let rig = new_rig().with_old();
        let mut files = Scripted::failing_nth(&[("sync_dir", "bin", "", 2)]);
        let out = run_publishing(&rig, windows, &mut files, unreachable_publish, || {});
        assert_eq!(out.exit, Exit::Failed, "windows {windows}: {}", out.err);
        assert!(
            out.err.contains("didn't confirm"),
            "windows {windows}: {}",
            out.err
        );
        assert_eq!(rig.dest_text().as_deref(), Some("old"));
        assert!(rig.olds().is_empty());
        assert!(rig.receipt().is_none());
        if windows {
            assert_eq!(rig.staged_text().as_deref(), Some("new"));
        }
    }
}

#[test]
fn a_fresh_install_whose_placement_cannot_be_confirmed_moves_the_binary_back() {
    for windows in BOTH {
        let rig = new_rig();
        // The only sync of the binary directory before the receipt.
        let mut files = Scripted::failing_nth(&[("sync_dir", "bin", "", 1)]);
        let out = run_publishing(&rig, windows, &mut files, unreachable_publish, || {});
        assert_eq!(out.exit, Exit::Failed, "{}", out.err);
        assert_eq!(rig.dest_text(), None);
        assert_eq!(rig.staged_text().as_deref(), Some("new"));
        assert!(rig.receipt().is_none());
    }
}

#[test]
fn a_failed_sync_during_the_undo_exits_6_naming_the_files() {
    // Unix: the placement's sync fails, then the sync after the restore does.
    let rig = new_rig().with_old();
    let mut files =
        Scripted::failing_nth(&[("sync_dir", "bin", "", 2), ("sync_dir", "bin", "", 3)]);
    let out = run_publishing(&rig, false, &mut files, unreachable_publish, || {});
    assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
    for path in [&rig.dest, &rig.from] {
        assert!(out.err.contains(&path.display().to_string()), "{}", out.err);
    }
    assert!(out.err.contains("didn't confirm"), "{}", out.err);

    // Windows: the receipt fails, the new binary goes back, and then the sync
    // of that move fails.
    let rig = new_rig().with_old();
    let mut files = Scripted::failing_nth(&[("sync_dir", "bin", "", 3)]);
    let out = run_publishing(&rig, true, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
    for path in [&rig.dest, &rig.from] {
        assert!(out.err.contains(&path.display().to_string()), "{}", out.err);
    }
    assert!(out.err.contains("To restore the old one"), "{}", out.err);

    // Windows: the sync of the old binary's move back fails.
    let rig = new_rig().with_old();
    let mut files = Scripted::failing_nth(&[("sync_dir", "bin", "", 4)]);
    let out = run_publishing(&rig, true, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
    assert!(
        out.err.contains("check that it is still there"),
        "{}",
        out.err
    );
}

#[test]
fn a_failed_sync_of_an_undone_failed_placement_exits_6() {
    // Windows: the placement fails, and the old binary's move back is not
    // confirmed.
    let rig = new_rig().with_old();
    let mut files =
        Scripted::failing_nth(&[("rename", "from", "dest", 1), ("sync_dir", "bin", "", 2)]);
    let out = run_publishing(&rig, true, &mut files, unreachable_publish, || {});
    assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
    assert!(out.err.contains("moving"), "{}", out.err);
}

#[test]
fn the_receipt_is_published_only_after_the_placement_is_confirmed() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        let mut files = Scripted::failing_nth(&[("sync_dir", "bin", "", 2)]);
        // `unreachable_publish` would panic if the receipt were written first.
        let out = run_publishing(&rig, windows, &mut files, unreachable_publish, || {});
        assert_eq!(out.exit, Exit::Failed);
        assert!(rig.receipt().is_none());
    }
}

// --- Earlier installs' material ------------------------------------------------

#[test]
fn an_earlier_installs_material_is_kept_while_this_install_fails_and_removed_when_it_succeeds() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        // The original, left by an install that exited 6.
        let kept = rig.bin.join("open-ferry.old-1f-2a-9");
        fs::write(&kept, b"original").unwrap();

        // A retry that fails to place, and one that fails to publish: the
        // material stays.
        let mut files = Scripted::failing(&[("rename", "from", "dest")]);
        let out = run_with(&rig, windows, &mut files);
        assert_eq!(out.exit, Exit::Failed, "{}", out.err);
        assert_eq!(Rig::text(&kept).as_deref(), Some("original"));

        let mut files = Scripted::new();
        let out = run_publishing(&rig, windows, &mut files, not_published, || {
            assert_eq!(Rig::text(&kept).as_deref(), Some("original"));
        });
        assert_eq!(out.exit, Exit::Failed, "{}", out.err);
        assert_eq!(Rig::text(&kept).as_deref(), Some("original"));

        // A retry that exits 6 keeps it too.
        let stuck = new_rig().with_old();
        let kept_stuck = stuck.bin.join("open-ferry.old-1f-2a-9");
        fs::write(&kept_stuck, b"original").unwrap();
        let mut files = Scripted::failing(&[("rename", "old", "dest")]);
        let out = run_publishing(&stuck, windows, &mut files, not_published, || {});
        assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
        assert_eq!(Rig::text(&kept_stuck).as_deref(), Some("original"));

        // A retry that succeeds sweeps it, after the receipt. (A Unix undo
        // consumed the staged file.)
        fs::write(&rig.from, b"new").unwrap();
        let mut files = Scripted::new();
        let out = run_publishing(
            &rig,
            windows,
            &mut files,
            || Ok(()),
            || assert_eq!(Rig::text(&kept).as_deref(), Some("original")),
        );
        assert_eq!(out.exit, Exit::Installed, "{}", out.err);
        assert!(!kept.exists(), "swept once the install committed");
        assert_eq!(rig.dest_text().as_deref(), Some("new"));
    }
}

#[test]
fn the_sweep_after_the_commit_says_an_older_binary_is_still_running() {
    let rig = new_rig().with_old();
    fs::write(rig.bin.join("open-ferry.old-1f-2a-9"), b"x").unwrap();
    let mut files = Scripted::failing(&[("remove_file", "old", "")]);
    let out = run_with(&rig, true, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert!(out.out.contains("still running"), "{}", out.out);
}

// --- The data directory chain ---------------------------------------------------

#[test]
fn the_parent_of_every_directory_made_for_the_data_directory_is_synced() {
    let rig = new_rig();
    let tmp = rig._dir.path().to_owned();
    let data = tmp.join("a").join("b").join("data");
    let rig = Rig {
        data: DataDir::at(data.clone()),
        ..rig
    };
    let mut files = Scripted::new();
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert_eq!(
        files.made,
        [tmp.join("a"), tmp.join("a").join("b"), data.clone()]
    );
    assert_eq!(
        files.synced[..3],
        [tmp.clone(), tmp.join("a"), tmp.join("a").join("b")]
    );
}

#[test]
fn a_data_directory_that_exists_is_not_made() {
    let rig = new_rig();
    fs::create_dir_all(rig.data.root()).unwrap();
    let mut files = Scripted::new();
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert!(files.made.is_empty(), "{:?}", files.made);
}

#[test]
fn a_failed_sync_of_the_data_directory_chain_changes_nothing() {
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("sync_dir", "other", "")]);
    let out = run_publishing(&rig, false, &mut files, unreachable_publish, || {});
    assert_eq!(out.exit, Exit::Failed);
    assert!(
        out.err.contains("can't make the data directory"),
        "{}",
        out.err
    );
    assert_eq!(rig.dest_text().as_deref(), Some("old"));
    assert_eq!(rig.staged_text().as_deref(), Some("new"));
    assert!(rig.olds().is_empty());
}

#[test]
fn a_retry_syncs_the_data_directorys_parent_even_though_it_made_nothing() {
    let rig = new_rig();
    let tmp = rig._dir.path().to_owned();
    // The first run makes the directory, and the sync of its parent fails.
    let mut files = Scripted::failing_nth(&[("sync_dir", "other", "", 1)]);
    let out = run_publishing(&rig, false, &mut files, unreachable_publish, || {});
    assert_eq!(out.exit, Exit::Failed, "{}", out.err);
    assert_eq!(files.made, [rig.data.root().to_owned()]);
    assert!(rig.data.root().is_dir());

    // The second finds the directory there; it syncs the parent before it
    // places the binary or publishes the receipt.
    let mut files = Scripted::new();
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert!(files.made.is_empty(), "{:?}", files.made);
    assert_eq!(files.synced.first(), Some(&tmp));
    assert!(files.at("sync_dir other") < files.at("rename from dest"));
}

#[test]
fn the_destination_directorys_parent_is_synced_after_the_placement_and_before_the_receipt() {
    // The data directory is made first (its parent is synced once), then the
    // placement's parent sync is the second sync of that directory, and
    // `bin dir` shares it.
    for windows in BOTH {
        let rig = new_rig().with_old();
        let mut files = Scripted::failing_nth(&[("sync_dir", "other", "", 2)]);
        let out = run_publishing(&rig, windows, &mut files, unreachable_publish, || {});
        assert_eq!(out.exit, Exit::Failed, "windows {windows}: {}", out.err);
        assert!(
            out.err.contains("didn't confirm"),
            "windows {windows}: {}",
            out.err
        );
        let syncs: Vec<usize> = files
            .log
            .iter()
            .enumerate()
            .filter(|(_, line)| *line == "sync_dir other")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(syncs.len(), 2, "{:?}", files.log);
        assert!(files.at("rename from dest") < syncs[1]);
        // A placement failure: rolled back, no receipt.
        assert_eq!(rig.dest_text().as_deref(), Some("old"));
        assert!(rig.olds().is_empty());
        assert!(rig.receipt().is_none());
    }
}

#[test]
fn a_fresh_install_whose_undo_move_is_unconfirmed_does_not_say_to_delete_dest() {
    for windows in BOTH {
        let rig = new_rig();
        // The placement's sync (1) passes, the receipt fails, and the sync of
        // the move back to `from` (2) fails.
        let mut files = Scripted::failing_nth(&[("sync_dir", "bin", "", 2)]);
        let out = run_publishing(&rig, windows, &mut files, not_published, || {});
        assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
        assert!(out.err.contains("didn't confirm"), "{}", out.err);
        assert!(
            !out.err.contains("To undo the install, delete"),
            "{}",
            out.err
        );
        assert!(
            out.err
                .contains(&format!("The new one is back at {}", rig.from.display())),
            "{}",
            out.err
        );
        assert_eq!(rig.staged_text().as_deref(), Some("new"));
    }
}

#[test]
fn a_fresh_install_whose_undo_move_failed_still_says_to_delete_dest() {
    let rig = new_rig();
    let mut files = Scripted::failing(&[("rename", "dest", "from")]);
    let out = run_publishing(&rig, false, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
    assert!(
        out.err.contains("To undo the install, delete"),
        "{}",
        out.err
    );
}

#[test]
fn windows_an_unconfirmed_move_of_the_new_binary_back_does_not_say_replacing_it() {
    let rig = new_rig().with_old();
    // Move aside (1) and placement (2) pass; the move back to `from` (3) is
    // unconfirmed: `dest` is empty.
    let mut files = Scripted::failing_nth(&[("sync_dir", "bin", "", 3)]);
    let out = run_publishing(&rig, true, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
    assert!(out.err.contains("To restore the old one"), "{}", out.err);
    assert!(!out.err.contains("replacing the new one"), "{}", out.err);

    // A rename that failed leaves the new binary at `dest`.
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("rename", "dest", "from")]);
    let out = run_publishing(&rig, true, &mut files, not_published, || {});
    assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
    assert!(out.err.contains("replacing the new one"), "{}", out.err);
}

#[test]
fn windows_the_undo_of_a_failed_placement_tells_a_failed_move_from_an_unconfirmed_one() {
    // The old binary's move back never happened.
    let rig = new_rig().with_old();
    let mut files = Scripted::failing(&[("rename", "from", "dest"), ("rename", "old", "dest")]);
    let out = run_publishing(&rig, true, &mut files, unreachable_publish, || {});
    assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
    assert!(out.err.contains("To restore the old one"), "{}", out.err);
    assert!(
        !out.err.contains("check that it is still there"),
        "{}",
        out.err
    );
    assert!(
        out.err
            .contains(&format!("{} now holds nothing", rig.dest.display())),
        "{}",
        out.err
    );

    // It happened, and the disk didn't confirm it.
    let rig = new_rig().with_old();
    let mut files =
        Scripted::failing_nth(&[("rename", "from", "dest", 1), ("sync_dir", "bin", "", 2)]);
    let out = run_publishing(&rig, true, &mut files, unreachable_publish, || {});
    assert_eq!(out.exit, Exit::PartlyChanged, "{}", out.err);
    assert!(
        out.err.contains("check that it is still there"),
        "{}",
        out.err
    );
    assert!(!out.err.contains("To restore the old one"), "{}", out.err);
    assert!(
        out.err.contains(&format!(
            "{} now holds the open-ferry that was installed",
            rig.dest.display()
        )),
        "{}",
        out.err
    );
}

#[cfg(unix)]
#[test]
fn unix_the_staged_file_is_flushed_through_a_read_only_handle() {
    // A write-open would be refused here, as it is for a running image.
    let dir = tempdir();
    let file = dir.path().join("staged");
    fs::write(&file, b"bytes").unwrap();
    let mut permissions = fs::metadata(&file).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&file, permissions).unwrap();
    RealFiles.sync_file(&file).unwrap();
}

#[cfg(not(unix))]
#[test]
fn windows_a_sharing_violation_is_accepted_only_for_this_processs_own_image() {
    let exe = std::env::current_exe().unwrap();
    assert!(is_own_image(&exe));
    let rig = new_rig();
    assert!(!is_own_image(&rig.from));
    // The running image can't be write-opened (error 32); it is accepted.
    RealFiles.sync_file(&exe).unwrap();
    // Another file that is in use is not: hold one open without sharing.
    let held = rig.bin.join("held");
    fs::write(&held, b"x").unwrap();
    let _lock = {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&held)
            .unwrap()
    };
    let error = RealFiles.sync_file(&held).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(32), "{error}");
}

// --- Round 3: the root, the home directory chain, directories that can't sync ---

#[test]
fn a_directory_in_a_root_has_a_holder_to_sync_only_when_it_has_a_parent() {
    // A destination directly in the root: its directory is the root, which
    // has nothing above it to sync, and in particular not `.`.
    let root_dest = Path::new("/open-ferry");
    assert_eq!(parent_of(root_dest), Path::new("/"));
    assert_eq!(holder_of(parent_of(root_dest)), None);
    #[cfg(windows)]
    {
        let dest = Path::new(r"C:\open-ferry.exe");
        assert_eq!(holder_of(parent_of(dest)), None);
    }
    // One level down has a holder, and a relative path's is the cwd.
    assert_eq!(holder_of(Path::new("/bin")), Some(Path::new("/")));
    assert_eq!(holder_of(Path::new("bin")), Some(Path::new(".")));
}

#[test]
fn the_data_directory_chain_at_a_root_syncs_nothing_and_not_the_working_directory() {
    let mut files = Scripted::new();
    create_private_dir(Path::new("/"), None, &mut files).unwrap();
    assert!(files.synced.is_empty(), "{:?}", files.synced);
    assert!(files.made.is_empty());
}

/// `path` resolved, as the code under test names directories it syncs.
fn rz(path: &Path) -> PathBuf {
    RealFiles.resolve(path).unwrap()
}

/// A home directory with `.local/share` below it, and the data directory in
/// it: the default layout.
fn home_rig(made: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempdir();
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let mut dir = home.clone();
    for part in made {
        dir = dir.join(part);
        fs::create_dir(&dir).unwrap();
    }
    (tmp, home)
}

#[test]
fn a_retry_syncs_every_ancestor_up_to_home_even_the_outer_one_a_failed_run_made() {
    let (_tmp, home) = home_rig(&[]);
    let data = home.join(".local").join("share").join("open-ferry");
    // Run 1 makes `.local`, and the sync of `home` (its parent) fails.
    let mut files = Scripted::failing_nth(&[("sync_dir", "other", "", 1)]);
    assert!(create_private_dir(&data, Some(&home), &mut files).is_err());
    assert_eq!(files.made, [home.join(".local")]);
    assert!(home.join(".local").is_dir());

    // Run 2 finds `.local` and makes the rest, and still syncs `home`.
    let mut files = Scripted::new();
    create_private_dir(&data, Some(&home), &mut files).unwrap();
    assert_eq!(
        files.made,
        [home.join(".local").join("share"), data.clone()]
    );
    // What is synced is the resolved directory (a temp dir may be a link).
    assert!(files.synced.contains(&rz(&home)), "{:?}", files.synced);
    assert!(
        files.synced.contains(&rz(&home.join(".local"))),
        "{:?}",
        files.synced
    );
    assert!(
        files
            .synced
            .contains(&rz(&home.join(".local").join("share"))),
        "{:?}",
        files.synced
    );
}

#[test]
fn with_everything_there_every_run_syncs_the_data_directory_and_each_ancestor_below_home() {
    let (_tmp, home) = home_rig(&[".local", "share", "open-ferry"]);
    let share = home.join(".local").join("share");
    let data = share.join("open-ferry");
    let mut files = Scripted::new();
    create_private_dir(&data, Some(&home), &mut files).unwrap();
    assert!(files.made.is_empty());
    assert_eq!(
        files.synced,
        [rz(&share), rz(&home.join(".local")), rz(&home)]
    );
}

#[test]
fn a_data_directory_outside_home_keeps_the_directories_made_and_its_own_parent() {
    let tmp = tempdir();
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let xdg = tmp.path().join("xdg");
    fs::create_dir(&xdg).unwrap();

    // Made: the data directory's parent, once.
    let data = xdg.join("open-ferry");
    let mut files = Scripted::new();
    create_private_dir(&data, Some(&home), &mut files).unwrap();
    assert_eq!(files.made, std::slice::from_ref(&data));
    assert_eq!(files.synced, [rz(&xdg)]);

    // Found there: its own parent, and nothing above it.
    let mut files = Scripted::new();
    create_private_dir(&data, Some(&home), &mut files).unwrap();
    assert!(files.made.is_empty());
    assert_eq!(files.synced, [rz(&xdg)]);

    // No home known: the same.
    let mut files = Scripted::new();
    create_private_dir(&data, None, &mut files).unwrap();
    assert_eq!(files.synced, [rz(&xdg)]);
}

#[cfg(unix)]
#[test]
fn unix_the_real_files_sync_a_directory_through_the_updaters_rule() {
    // The EINVAL rule itself is tested where it lives, in open-ferry-update's
    // publication module; a real directory syncs through it, and one that
    // can't be opened is an error.
    let dir = tempdir();
    RealFiles.sync_dir(dir.path()).unwrap();
    assert!(RealFiles.sync_dir(&dir.path().join("missing")).is_err());
}

// --- Round 4: home decided on resolved paths, an unreadable destination ---

#[test]
fn a_destination_that_cannot_be_looked_at_is_not_taken_for_a_fresh_install() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        // The look at `dest` fails; a rename that would work is never tried.
        let mut files = Scripted::failing_nth(&[("exists", "dest", "", 1)]);
        let out = run_publishing(&rig, windows, &mut files, unreachable_publish, || {});
        assert_eq!(out.exit, Exit::Failed, "windows {windows}: {}", out.err);
        assert!(
            out.err.contains("can't tell whether") && out.err.contains("Nothing was changed."),
            "{}",
            out.err
        );
        assert!(files.renames.is_empty(), "{:?}", files.renames);
        assert!(!files.did("hard_link dest old"));
        assert_eq!(rig.dest_text().as_deref(), Some("old"));
        assert_eq!(rig.staged_text().as_deref(), Some("new"));
        assert!(rig.olds().is_empty());
        assert!(rig.receipt().is_none());
    }
}

#[test]
fn a_destination_that_is_not_there_is_a_fresh_install() {
    let rig = new_rig();
    let mut files = Scripted::new();
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    assert!(files.did("exists dest"));
    assert!(rig.olds().is_empty());
}

#[test]
fn the_default_layout_syncs_home_and_every_ancestor_of_the_data_directory_below_it() {
    let (_tmp, home) = home_rig(&[]);
    let data = home.join(".local").join("share").join("open-ferry");
    let mut files = Scripted::new();
    create_private_dir(&data, Some(&home), &mut files).unwrap();
    // Made outermost first, each parent synced, and nothing synced twice.
    assert_eq!(
        files.made,
        [
            home.join(".local"),
            home.join(".local").join("share"),
            data.clone()
        ]
    );
    assert_eq!(
        files.synced,
        [
            rz(&home),
            rz(&home.join(".local")),
            rz(&home.join(".local").join("share"))
        ]
    );
}

#[test]
fn a_data_directory_reached_by_dot_dot_out_of_home_is_outside_home() {
    // XDG_DATA_HOME=$HOME/../shared: it starts with home as text.
    let tmp = tempdir();
    let home = tmp.path().join("home");
    let shared = tmp.path().join("shared");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&shared).unwrap();
    let data = home.join("..").join("shared").join("open-ferry");
    fs::create_dir(&data).unwrap();
    let mut files = Scripted::new();
    create_private_dir(&data, Some(&home), &mut files).unwrap();
    // Only the data directory's own parent: nothing above home.
    assert_eq!(files.synced, [rz(&shared)]);
    assert!(!files.synced.contains(&rz(tmp.path())));

    // Made there: the made directory's parent, and no walk.
    let data = home.join("..").join("shared").join("fresh");
    let mut files = Scripted::new();
    create_private_dir(&data, Some(&home), &mut files).unwrap();
    assert_eq!(files.synced, [rz(&shared)]);
}

#[cfg(unix)]
#[test]
fn unix_a_symlinked_home_is_home_and_its_real_directory_is_synced_on_a_retry() {
    let tmp = tempdir();
    let real = tmp.path().join("real");
    fs::create_dir(&real).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let real = fs::canonicalize(&real).unwrap();
    let data = link.join(".local").join("share").join("open-ferry");

    // Run 1 makes `.local` and the sync of its parent, the real home, fails.
    let mut files = Scripted::failing_nth(&[("sync_dir", "other", "", 1)]);
    assert!(create_private_dir(&data, Some(&link), &mut files).is_err());
    assert_eq!(files.synced, std::slice::from_ref(&real));

    // Run 2 syncs the real home again, and every directory it names is a
    // resolved one: no path through the link.
    let mut files = Scripted::new();
    create_private_dir(&data, Some(&link), &mut files).unwrap();
    assert!(files.synced.contains(&real), "{:?}", files.synced);
    assert!(
        files.synced.iter().all(|dir| !dir.starts_with(&link)),
        "{:?}",
        files.synced
    );
    assert_eq!(files.synced.last(), Some(&real));
    // And nothing above the real home.
    assert!(
        files.synced.iter().all(|dir| dir.starts_with(&real)),
        "{:?}",
        files.synced
    );

    // A home that can't be resolved is no boundary: the outside rule.
    let mut files = Scripted::new();
    create_private_dir(&data, Some(&tmp.path().join("gone")), &mut files).unwrap();
    assert_eq!(
        files.synced,
        [real.join(".local").join("share")],
        "{:?}",
        files.synced
    );
}

// --- Round 5: resolution without loss, a data directory that won't resolve,
// --- the first look at -to ---

#[test]
fn a_first_look_at_the_destination_that_fails_is_exit_1_not_a_usage_error() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        let mut files = Scripted::failing(&[("look", "dest", "")]);
        let out = run_publishing(&rig, windows, &mut files, unreachable_publish, || {});
        assert_eq!(out.exit, Exit::Failed, "windows {windows}: {}", out.err);
        assert!(
            out.err.contains("can't tell whether")
                && out.err.contains("Nothing was changed.")
                && !out.err.contains("can't read -to"),
            "{}",
            out.err
        );
        // Nothing was touched, not even the data directory.
        assert!(files.renames.is_empty() && files.made.is_empty());
        assert_eq!(rig.dest_text().as_deref(), Some("old"));
        assert_eq!(rig.staged_text().as_deref(), Some("new"));
        assert!(rig.olds().is_empty() && !rig.data.root().exists());
    }
}

#[test]
fn a_destination_that_is_not_found_is_still_checked_as_before() {
    // NotFound is a fresh install, and a bad argument is still exit 2.
    let rig = new_rig();
    let mut files = Scripted::new();
    let out = run_with(&rig, false, &mut files);
    assert_eq!(out.exit, Exit::Installed, "{}", out.err);
    let rig = new_rig();
    fs::create_dir(&rig.dest).unwrap();
    let out = run_with(&rig, false, &mut Scripted::new());
    assert_eq!(out.exit, Exit::Usage, "{}", out.err);
}

#[test]
fn a_data_directory_that_exists_and_will_not_resolve_fails_the_install() {
    for windows in BOTH {
        let rig = new_rig().with_old();
        fs::create_dir_all(rig.data.root()).unwrap();
        let mut files = Scripted::failing(&[("resolve", "other", "")]);
        let out = run_publishing(&rig, windows, &mut files, unreachable_publish, || {});
        assert_eq!(out.exit, Exit::Failed, "windows {windows}: {}", out.err);
        assert!(
            out.err.contains("can't resolve")
                && out.err.contains(&rig.data.root().display().to_string())
                && out.err.contains("injected failure: resolve")
                && out.err.contains("were not changed"),
            "{}",
            out.err
        );
        // No sync through a lexical path, and the install changed nothing.
        assert!(files.synced.is_empty(), "{:?}", files.synced);
        assert!(files.renames.is_empty());
        assert_eq!(rig.dest_text().as_deref(), Some("old"));
        assert!(rig.olds().is_empty() && rig.receipt().is_none());
    }
}

#[test]
fn a_home_that_will_not_resolve_selects_the_outside_home_rule() {
    let (_tmp, home) = home_rig(&[]);
    let xdg = home.join("data");
    fs::create_dir(&xdg).unwrap();
    // Every resolution of home fails (the data directory's succeeds): only
    // the own parent is synced, nothing is an error.
    struct HomeGone(Scripted, PathBuf);
    impl Files for HomeGone {
        fn rename(&mut self, a: &Path, b: &Path) -> io::Result<()> {
            self.0.rename(a, b)
        }
        fn hard_link(&mut self, a: &Path, b: &Path) -> io::Result<()> {
            self.0.hard_link(a, b)
        }
        fn copy(&mut self, a: &Path, b: &Path) -> io::Result<()> {
            self.0.copy(a, b)
        }
        fn remove_file(&mut self, a: &Path) -> io::Result<()> {
            self.0.remove_file(a)
        }
        fn sync_file(&mut self, a: &Path) -> io::Result<()> {
            self.0.sync_file(a)
        }
        fn sync_dir(&mut self, a: &Path) -> io::Result<()> {
            self.0.sync_dir(a)
        }
        fn create_dir(&mut self, a: &Path) -> io::Result<()> {
            self.0.create_dir(a)
        }
        fn exists(&mut self, a: &Path) -> io::Result<bool> {
            self.0.exists(a)
        }
        fn look(&mut self, a: &Path) -> io::Result<Option<fs::Metadata>> {
            self.0.look(a)
        }
        fn resolve(&mut self, a: &Path) -> io::Result<PathBuf> {
            if a == self.1 {
                return Err(io::Error::other("home is gone"));
            }
            self.0.resolve(a)
        }
    }
    let mut files = HomeGone(Scripted::new(), home.clone());
    create_private_dir(&xdg.join("open-ferry"), Some(&home), &mut files).unwrap();
    assert_eq!(files.0.synced, [rz(&xdg)]);
}

#[cfg(unix)]
#[test]
fn unix_a_data_directory_with_a_non_utf8_name_syncs_its_own_holders_not_a_lookalike() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let (_tmp, home) = home_rig(&[]);
    // The name with the byte 0xff, and a sibling with U+FFFD, which is what a
    // lossy conversion of it reads as.
    let odd = home.join(OsStr::from_bytes(b"share\xff"));
    let lookalike = home.join("share\u{fffd}");
    if fs::create_dir(&odd).is_err() {
        return; // A filesystem that refuses such names has nothing to test.
    }
    fs::create_dir(&lookalike).unwrap();
    fs::create_dir(lookalike.join("open-ferry")).unwrap();
    let data = odd.join("open-ferry");
    fs::create_dir(&data).unwrap();

    let mut files = Scripted::new();
    create_private_dir(&data, Some(&home), &mut files).unwrap();
    // The data directory's holder (the odd one) and home: exactly.
    assert_eq!(
        files.synced,
        [
            fs::canonicalize(&odd).unwrap(),
            fs::canonicalize(&home).unwrap()
        ]
    );
    assert!(
        files.synced.iter().all(|dir| !dir.starts_with(&lookalike)),
        "{:?}",
        files.synced
    );

    // Made fresh under the odd name: the same.
    let fresh = odd.join("fresh");
    let mut files = Scripted::new();
    create_private_dir(&fresh, Some(&home), &mut files).unwrap();
    assert_eq!(
        files.synced,
        [
            fs::canonicalize(&odd).unwrap(),
            fs::canonicalize(&home).unwrap()
        ]
    );
}

// --- Round 6: a destination that isn't valid Unicode ---

/// A file name in `dir` that is not valid Unicode: an unpaired surrogate on
/// Windows, the byte 0xff on Unix.
fn non_unicode_dest(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0x6f, 0x66, 0xD800])
    };
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(b"of\xff").to_owned()
    };
    dir.join(name)
}

#[test]
fn a_destination_that_is_not_valid_unicode_is_refused_before_anything_is_touched() {
    for windows in BOTH {
        let mut rig = new_rig();
        rig.dest = non_unicode_dest(&rig.bin);
        assert!(rig.dest.to_str().is_none());
        let mut files = Scripted::new();
        let out = run_with(&rig, windows, &mut files);
        assert_eq!(out.exit, Exit::Usage, "windows {windows}: {}", out.err);
        assert!(
            out.err.contains("isn't valid Unicode text")
                && out.err.contains("Nothing was changed."),
            "{}",
            out.err
        );
        // Not even the data directory.
        assert!(files.made.is_empty() && files.renames.is_empty());
        assert!(!rig.data.root().exists());
        assert_eq!(rig.staged_text().as_deref(), Some("new"));
        assert!(rig.olds().is_empty() && rig.receipt().is_none());
    }
}
