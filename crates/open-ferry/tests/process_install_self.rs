//! Not upstream's: the real binary's `install-self` against supervisors run
//! as processes (the integration step of the 2c-2a plan).
//!
//! The binary that runs `install-self` is a copy of the test crate's
//! `open-ferry` (with the harness's version trailer) staged beside the
//! fixture's installed file, as the install scripts stage it. It finds its
//! data directory the way a user's does, from `LOCALAPPDATA` (Windows) or
//! `XDG_DATA_HOME`, both set on the child to the fixture's root; the fixture
//! made by `Fx::new_for_this_user_layout` has its data directory at
//! `<root>/open-ferry`, which is that directory. Each test checks that before
//! it runs anything, so the child never sees the developer's own.
//!
//! What a test waits for is observable state (the child's output, the
//! supervisor's log, its status), never a sleep.
//!
//! The harness is the supervisor crate's `under_test`, included by path (see
//! `process_supervise.rs`).
#![cfg(feature = "blue-green-test")]

#[path = "support/privilege.rs"]
mod privilege;

#[path = "../../open-ferry-supervisor/tests/under_test/mod.rs"]
mod under_test;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use open_ferry_update::data_dir::DataDir;
use open_ferry_update::install_lock::InstallLock;
use under_test::*;

/// What the supervisor says while it waits for the update lock.
const UPDATE_WAIT: &str = "waiting for the update lock";

const SUPERVISED: i32 = 3;
const WAITED_OUT: i32 = 5;

fn installer() -> &'static str {
    if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    }
}

/// The fixture, made so that a child with `LOCALAPPDATA`, `XDG_DATA_HOME` and
/// `HOME` at its root has the fixture's data directory as the user's.
fn fixture() -> Fx {
    let fx = Fx::new_for_this_user_layout();
    let vars = |name: &str| match name {
        "LOCALAPPDATA" | "XDG_DATA_HOME" | "HOME" => Some(fx.root().as_os_str().to_owned()),
        _ => None,
    };
    let found = DataDir::locate(vars, cfg!(windows)).expect("a data directory");
    assert_eq!(found.root(), fx.data(), "the child would look elsewhere");
    assert!(
        fx.data().starts_with(fx.root()),
        "the data directory is outside the fixture"
    );
    fx
}

/// The binary that installs: a copy of the real open-ferry beside the
/// installed file. Not in the fixture's `versions/` (`Fx::stage` does that).
fn stage_installer(fx: &Fx) -> PathBuf {
    let staged = fx.root().join("bin").join(format!("staged-{NAME}"));
    write_executable(&staged, &artifact_bytes(B));
    staged
}

/// A running `install-self`, killed when dropped.
struct Install {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
    began: Instant,
}

impl Install {
    /// Starts `install-self` from `staged` onto the fixture's installed file,
    /// waiting at most `wait` seconds for the locks.
    fn start(fx: &Fx, staged: &Path, wait: u64) -> Install {
        let mut command = Command::new(staged);
        command
            .arg("install-self")
            .arg("-from")
            .arg(staged)
            .arg("-to")
            .arg(fx.installed())
            .args(["-installer", installer(), "-wait", &wait.to_string()])
            .current_dir(fx.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        isolate(&mut command);
        command
            .env("LOCALAPPDATA", fx.root())
            .env("XDG_DATA_HOME", fx.root())
            // install-self reads HOME to bound the directories it syncs; the
            // developer's own must not be the boundary of a fixture.
            .env("HOME", fx.root());
        let began = Instant::now();
        let mut child = command.spawn().expect("start install-self");
        let (send, lines) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        for stream in [
            Box::new(stdout) as Box<dyn std::io::Read + Send>,
            Box::new(stderr),
        ] {
            let send = send.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if send.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        Install {
            child,
            lines,
            seen: Vec::new(),
            began,
        }
    }

    /// Waits, bounded, for a line containing `text`.
    fn wait_line(&mut self, text: &str) {
        let until = Instant::now() + LONG;
        loop {
            if self.seen.iter().any(|line| line.contains(text)) {
                return;
            }
            let left = until.saturating_duration_since(Instant::now());
            match self
                .lines
                .recv_timeout(left.min(Duration::from_millis(200)))
            {
                Ok(line) => self.seen.push(line),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = self.child.try_wait().unwrap() {
                        panic!(
                            "install-self ended ({status}) before saying {text:?}\n{}",
                            self.output()
                        );
                    }
                    assert!(
                        Instant::now() < until,
                        "install-self didn't say {text:?}\n{}",
                        self.output()
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!(
                        "install-self closed its output without {text:?}\n{}",
                        self.output()
                    )
                }
            }
        }
    }

    fn output(&self) -> String {
        self.seen.join("\n")
    }

    /// Waits, bounded, for the exit. The exit code, the output, and how long
    /// it ran.
    fn finish(mut self) -> (i32, String, Duration) {
        let until = Instant::now() + LONG;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= until {
                panic!("install-self didn't end\n{}", self.output());
            }
            std::thread::sleep(POLL);
        };
        let took = self.began.elapsed();
        // The readers end with the pipes.
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) {
            self.seen.push(line);
        }
        (status.code().unwrap_or(-1), self.output(), took)
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A receipt of an earlier install, which nothing may touch.
const SEEDED_RECEIPT: &[u8] = b"{\"seeded\": \"by the test\"}\n";

fn seed_receipt(fx: &Fx) {
    std::fs::create_dir_all(fx.data()).unwrap();
    std::fs::write(fx.data().join("install-receipt.json"), SEEDED_RECEIPT).unwrap();
}

/// Nothing was changed: the installed file is `A`'s, the receipt is the
/// `seeded` one byte for byte (or, with none, none was written), no aside
/// file was made, and the staged file is where it was.
#[track_caller]
fn assert_unchanged(fx: &Fx, staged: &Path, seeded: bool, output: &str) {
    assert_eq!(
        std::fs::read(fx.installed()).unwrap(),
        artifact_bytes(A),
        "the installed file changed\n{output}\n{}",
        fx.dump()
    );
    let receipt = fx.data().join("install-receipt.json");
    if seeded {
        assert_eq!(
            std::fs::read(&receipt).ok().as_deref(),
            Some(SEEDED_RECEIPT),
            "the receipt changed\n{output}"
        );
    } else {
        assert!(!receipt.exists(), "a receipt was written\n{output}");
    }
    assert!(staged.is_file(), "the staged file is gone\n{output}");
    assert_eq!(
        std::fs::read(staged).unwrap(),
        artifact_bytes(B),
        "the staged file changed"
    );
    assert_eq!(aside_files(fx), Vec::<String>::new(), "{output}");
}

fn aside_files(fx: &Fx) -> Vec<String> {
    std::fs::read_dir(fx.root().join("bin"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".old-"))
        .collect()
}

/// A supervisor of this data directory that serves: install-self is refused
/// with exit 3, and nothing is changed or stopped.
#[test]
fn a_live_supervisor_refuses_the_install_with_exit_3_and_nothing_changes() {
    if privilege::privileged() {
        eprintln!(
            "skipped a_live_supervisor_refuses_the_install_with_exit_3_and_nothing_changes: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
        );
        return;
    }
    let fx = fixture();
    let staged = stage_installer(&fx);
    seed_receipt(&fx);
    let mut sup = fx.start(&Tm::default());

    let (code, output, _) = Install::start(&fx, &staged, 5).finish();
    assert_eq!(code, SUPERVISED, "{output}\n{}", fx.dump());
    assert_unchanged(&fx, &staged, true, &output);
    assert!(
        sup.try_exit().is_none(),
        "the supervisor ended\n{}",
        fx.dump()
    );
    assert!(
        fx.status().is_some_and(|status| status["state"] == "idle"),
        "the supervisor isn't serving\n{}",
        fx.dump()
    );
}

/// The supervisor lock is held, as a supervisor in its startup wait holds it,
/// with no record and no endpoint: install-self finds it taken, exits 3 and
/// changes nothing. Deterministic: the test holds the lock itself (through the
/// supervisor crate's public `SupervisorLock`, the way test 3 holds the
/// install lock), so nothing races for it. Breaking detection's check of the
/// supervisor lock fails this test.
#[test]
fn a_held_supervisor_lock_with_no_supervisor_serving_refuses_the_install_with_exit_3() {
    if privilege::privileged() {
        eprintln!(
            "skipped a_held_supervisor_lock_with_no_supervisor_serving_refuses_the_install_with_exit_3: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
        );
        return;
    }
    let fx = fixture();
    let staged = stage_installer(&fx);
    seed_receipt(&fx);
    let lock = open_ferry_supervisor::process::SupervisorLock::try_acquire(&fx.data())
        .expect("the supervisor lock is free");

    let (code, output, _) = Install::start(&fx, &staged, 5).finish();
    assert_eq!(code, SUPERVISED, "{output}\n{}", fx.dump());
    assert_unchanged(&fx, &staged, true, &output);
    drop(lock);
}

/// A supervisor starting while an install waits for the update lock: the test
/// holds the update lock (a supervisor waits for it, holding the supervisor
/// lock) until install-self says it is waiting, then lets it go. Either of
/// the two then takes the update lock first, and either way the install is
/// refused with exit 3 (by the supervisor lock if install-self wins, by the
/// live supervisor if the supervisor does). This shows the refusal whichever
/// wins, not which one it was: the held supervisor lock alone is the test
/// above.
#[test]
fn a_supervisor_starting_while_the_install_waits_refuses_it_whoever_gets_the_update_lock() {
    if privilege::privileged() {
        eprintln!(
            "skipped a_supervisor_starting_while_the_install_waits_refuses_it_whoever_gets_the_update_lock: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
        );
        return;
    }
    let fx = fixture();
    let staged = stage_installer(&fx);
    let update = DataDir::at(fx.data())
        .try_lock()
        .unwrap()
        .expect("the update lock is free");
    let mut sup = fx.spawn(&Tm::default());
    fx.wait_for("the supervisor's wait for the update lock", || {
        if let Some(status) = sup.try_exit() {
            panic!("the supervisor exited early ({status})\n{}", fx.dump());
        }
        fx.child_log().contains(UPDATE_WAIT).then_some(())
    });

    let mut install = Install::start(&fx, &staged, 60);
    install.wait_line("Waiting for another open-ferry");
    drop(update);
    let (code, output, _) = install.finish();
    assert_eq!(code, SUPERVISED, "{output}\n{}", fx.dump());
    assert_unchanged(&fx, &staged, false, &output);
    assert!(
        sup.try_exit().is_none(),
        "the supervisor ended\n{}",
        fx.dump()
    );
}

/// The binary's install lock is held (as another data directory's supervisor
/// or update holds it), and this data directory has no supervisor: install-self
/// waits `-wait` seconds and gives up with exit 5, having changed nothing.
///
/// The run is bounded: at least the wait, and at most the wait plus 10 s,
/// which covers the process start, the one detection that is cut at the wait's
/// deadline, and a loaded machine, and is far under the 30 s default wait a
/// wrong deadline would show.
#[test]
fn an_install_lock_held_elsewhere_is_waited_out_with_exit_5_in_the_wait() {
    if privilege::privileged() {
        eprintln!(
            "skipped an_install_lock_held_elsewhere_is_waited_out_with_exit_5_in_the_wait: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
        );
        return;
    }
    let fx = fixture();
    let staged = stage_installer(&fx);
    let held = InstallLock::try_acquire(&fx.installed()).unwrap();

    let wait = 2;
    let (code, output, took) = Install::start(&fx, &staged, wait).finish();
    assert_eq!(code, WAITED_OUT, "{output}");
    assert!(
        took >= Duration::from_secs(wait),
        "gave up after {took:?}, before the wait\n{output}"
    );
    assert!(
        took < Duration::from_secs(wait + 10),
        "gave up after {took:?}, long after the wait\n{output}"
    );
    assert_unchanged(&fx, &staged, false, &output);
    drop(held);
}

/// No supervisor: the install happens. The installed file has the staged
/// bytes, the receipt names it, nothing of this install is left beside it,
/// and a supervisor started afterwards runs the new bytes.
#[test]
fn with_no_supervisor_the_install_replaces_the_binary_and_a_later_supervisor_runs_it() {
    if privilege::privileged() {
        eprintln!(
            "skipped with_no_supervisor_the_install_replaces_the_binary_and_a_later_supervisor_runs_it: this test needs an unprivileged account; the real CLI refuses user-writable scratch when privileged"
        );
        return;
    }
    let fx = fixture();
    fx.stage(B, "B");
    let staged = stage_installer(&fx);

    let released = now_ms();
    let (code, output, _) = Install::start(&fx, &staged, 5).finish();
    assert_eq!(code, 0, "{output}\n{}", fx.dump());
    assert_eq!(
        std::fs::read(fx.installed()).unwrap(),
        artifact_bytes(B),
        "{output}"
    );
    assert!(!staged.exists(), "the staged file is still there\n{output}");
    assert_eq!(aside_files(&fx), Vec::<String>::new(), "{output}");

    let receipt: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fx.data().join("install-receipt.json")).expect("a receipt"),
    )
    .unwrap();
    assert_eq!(receipt["installer"], installer(), "{receipt}");
    let named = PathBuf::from(receipt["binary"].as_str().expect("a binary"));
    assert!(
        same_file::is_same_file(&named, fx.installed()).unwrap(),
        "the receipt names {named:?}, not the installed file"
    );

    // A supervisor that starts now runs the replaced bytes.
    let mut sup = fx.start(&Tm::default());
    assert!(sup.try_exit().is_none());
    assert_eq!(
        open_ferry_update::staging::hash_file(&fx.kept(B)).unwrap(),
        open_ferry_update::staging::hash_file(&fx.installed()).unwrap(),
        "the kept copy is not the installed file's bytes"
    );
    // The supervisor can report its active worker before that worker's own
    // event is written.
    let active: Vec<Ev> = fx.wait_for("the active worker's event", || {
        let active: Vec<Ev> = fx
            .events()
            .into_iter()
            .filter(|event| event.event == "active")
            .collect();
        (!active.is_empty()).then_some(active)
    });
    assert!(
        active.iter().all(|event| event.marker == "B"),
        "a worker ran the old binary: {active:?}\n{}",
        fx.dump()
    );
    let first = fx
        .driver_log()
        .into_iter()
        .find(|line| line["k"] == "spawn")
        .expect("a spawn line");
    assert!(
        first["at"].as_u64().unwrap() >= released,
        "the first worker started before the install\n{}",
        fx.dump()
    );
}
