//! The real machine install, only in the lead's disposable Linux container.
#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, chown};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[path = "support/parent_death.rs"]
mod parent_death;

use open_ferry_supervisor::client::{self, StopOutcome};
use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
use open_ferry_supervisor::timings::STOP_BUDGET;

const ROOT: &str = "/opt/ofp-system-test";
const DATA: &str = "/var/lib/open-ferry";
const CONFIG: &str = "/etc/open-ferry/config.yaml";
const SOURCE: &str = "/opt/ofp-system-test/source/open-ferry";
const INSTALLED: &str = "/opt/ofp-system-test/bin/open-ferry";
const HOME: &str = "/opt/ofp-system-test/home";
const POLL: Duration = Duration::from_millis(50);
const COMMAND_BUDGET: Duration = Duration::from_secs(60);

fn guarded() -> bool {
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    let enabled =
        std::env::var_os("OFP_SYSTEM_PROCESS_TEST").as_deref() == Some(std::ffi::OsStr::new("1"));
    let absent = |path| {
        fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    };
    if root
        && enabled
        && [
            "/.dockerenv",
            "/run/.containerenv",
            DATA,
            "/etc/open-ferry",
            ROOT,
        ]
        .into_iter()
        .all(absent)
    {
        true
    } else {
        eprintln!(
            "skip process_system: needs root, OFP_SYSTEM_PROCESS_TEST=1, no container markers, and no existing machine test/install paths"
        );
        false
    }
}

fn directory(path: &str, step: &str) {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
        .unwrap_or_else(|error| panic!("{step}: create {path}: {error}"));
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("{step}: chmod {path}: {error}"));
}

fn command(binary: &str, args: &[&str], home: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .current_dir(ROOT)
        .stdin(Stdio::null())
        // Each child and its descendants can be killed without touching others.
        .process_group(0);
    parent_death::on_test_exit(&mut command);
    command
}

struct Running(Child);

impl Running {
    fn wait_until(&mut self, deadline: Instant, step: &str) -> ExitStatus {
        loop {
            if let Some(status) = self
                .0
                .try_wait()
                .unwrap_or_else(|error| panic!("{step}: wait: {error}"))
            {
                return status;
            }
            assert!(Instant::now() < deadline, "{step}: child timed out");
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            // SAFETY: the child created its own process group; a negative pid
            // targets that group, including any supervised worker we started.
            unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL) };
            let deadline = Instant::now() + Duration::from_secs(5);
            while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(POLL);
            }
        }
    }
}

struct ResultText {
    status: ExitStatus,
    text: String,
}

fn run(binary: &str, args: &[&str], home: &str, step: &str) -> ResultText {
    // Files avoid pipe backpressure; init's generated credentials are never
    // included in assertion output or printed by the test.
    let stdout = Path::new(ROOT).join("command.stdout");
    let stderr = Path::new(ROOT).join("command.stderr");
    let mut command = command(binary, args, home);
    command
        .stdout(File::create(&stdout).expect("create command stdout"))
        .stderr(File::create(&stderr).expect("create command stderr"));
    let mut child = Running(
        command
            .spawn()
            .unwrap_or_else(|error| panic!("{step}: spawn: {error}")),
    );
    let status = child.wait_until(Instant::now() + COMMAND_BUDGET, step);
    let text = if args.first() == Some(&"init") && status.success() {
        String::new()
    } else {
        format!(
            "{}{}",
            fs::read_to_string(stdout).expect("read command stdout"),
            fs::read_to_string(stderr).expect("read command stderr")
        )
    };
    ResultText { status, text }
}

fn refused(result: &ResultText, fragment: &str, step: &str) {
    assert_eq!(result.status.code(), Some(1), "{step}: {}", result.text);
    assert!(result.text.contains(fragment), "{step}: {}", result.text);
}

struct RestoreMode;

impl Drop for RestoreMode {
    fn drop(&mut self) {
        let _ = fs::set_permissions(DATA, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn root_machine_install_serves_stops_and_refuses_untrusted_records() {
    if !guarded() {
        return;
    }
    directory(ROOT, "setup");
    directory(HOME, "setup");
    directory("/opt/ofp-system-test/source", "step 2");
    directory("/opt/ofp-system-test/bin", "step 2");
    fs::copy(env!("CARGO_BIN_EXE_open-ferry"), SOURCE).expect("step 2: stage real binary");
    fs::set_permissions(SOURCE, fs::Permissions::from_mode(0o755))
        .expect("step 2: executable trusted source");
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("step 1: reserve loopback port")
        .local_addr()
        .expect("step 1: loopback address")
        .port()
        .to_string();
    assert_ne!(
        port, "8317",
        "step 1: never contact the developer's proxy port"
    );
    let init = ["init", "-system", "-port", &port];
    // Linux needs install-self to provision the machine directory first.
    refused(
        &run(SOURCE, &init, HOME, "step 1 before provisioning"),
        "/var/lib/open-ferry doesn't exist yet",
        "step 1 before provisioning",
    );
    let install = [
        "install-self",
        "-system",
        "-from",
        SOURCE,
        "-to",
        INSTALLED,
        "-installer",
        "install.sh",
        "-wait",
        "10",
    ];
    let result = run(SOURCE, &install, HOME, "step 2");
    assert!(result.status.success(), "step 2: {}", result.text);
    assert!(Path::new(INSTALLED).is_file(), "step 2: installed binary");
    // install-self moves the staged image; stage another for steps 4 and 7.
    fs::copy(env!("CARGO_BIN_EXE_open-ferry"), SOURCE).expect("step 2: restage real binary");
    fs::set_permissions(SOURCE, fs::Permissions::from_mode(0o755))
        .expect("step 2: restaged executable");
    let metadata = fs::metadata(DATA).expect("step 2: machine directory exists");
    assert!(metadata.is_dir(), "step 2: machine root is a directory");
    assert_eq!(metadata.uid(), 0, "step 2: machine root owned by root");
    assert_eq!(
        metadata.mode() & 0o022,
        0,
        "step 2: machine root cannot be written by group/others"
    );
    let result = run(INSTALLED, &init, HOME, "step 1 after provisioning");
    assert!(result.status.success(), "step 1: init succeeds");
    let config = fs::read_to_string(CONFIG).expect("step 1: machine config exists");
    assert!(
        config
            .lines()
            .any(|line| line.trim() == "auth-dir: \"/var/lib/open-ferry/auth\""),
        "step 1: machine auth-dir line"
    );
    let mode = ["update", "-mode", "off", "-config", CONFIG];
    let result = run(
        INSTALLED,
        &mode,
        HOME,
        "step 3 disable background networking",
    );
    assert!(result.status.success(), "step 3: {}", result.text);
    let mut start = command(
        INSTALLED,
        &[
            "-supervise",
            "-system",
            "-data-dir",
            DATA,
            "-config",
            CONFIG,
        ],
        HOME,
    );
    let log = File::create(Path::new(ROOT).join("supervisor.log")).expect("step 3: supervisor log");
    start
        .stdout(log.try_clone().expect("step 3: clone log"))
        .stderr(log);
    let mut supervisor = Running(start.spawn().expect("step 3: start supervisor"));
    let deadline = Instant::now() + COMMAND_BUDGET;
    let address: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("step 3: address");
    loop {
        assert!(
            supervisor
                .0
                .try_wait()
                .expect("step 3: child status")
                .is_none(),
            "step 3: supervisor exited before serving; see supervisor.log"
        );
        if TcpStream::connect_timeout(&address, POLL).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "step 3: supervisor did not serve within 60 seconds; see supervisor.log"
        );
        std::thread::sleep(POLL);
    }
    let result = run(SOURCE, &install, HOME, "step 4");
    assert_eq!(result.status.code(), Some(3), "step 4: {}", result.text);
    for fragment in [
        "Stop the service as root",
        "open-ferry -supervise -system",
        "sudo systemctl stop",
    ] {
        assert!(
            result.text.contains(fragment),
            "step 4: missing system hint {fragment}: {}",
            result.text
        );
    }
    // Check/update fetch before detecting a supervisor; mode off is the
    // specified network-free fallback and succeeds even while it runs.
    let result = run(INSTALLED, &mode, HOME, "step 5");
    assert!(result.status.success(), "step 5: {}", result.text);
    assert!(
        result
            .text
            .contains("self-update.mode is off in /etc/open-ferry/config.yaml"),
        "step 5: {}",
        result.text
    );
    let identity = Identity::current(ServiceKind::System).expect("step 6: system identity");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("step 6: runtime");
    let deadline = Instant::now() + STOP_BUDGET;
    let outcome = runtime.block_on(client::stop(Path::new(DATA), &identity, deadline));
    assert_eq!(outcome, StopOutcome::Stopped, "step 6: system client stop");
    assert_eq!(
        supervisor.wait_until(deadline, "step 6").code(),
        Some(0),
        "step 6: supervisor exits successfully within STOP_BUDGET"
    );
    let foreign_home = "/opt/ofp-system-test/foreign-home";
    directory(foreign_home, "step 7");
    chown(foreign_home, Some(1000), None).expect("step 7: uid 1000 home");
    // The source is not the machine receipt's binary, so lookup must use HOME.
    refused(
        &run(SOURCE, &["update", "-check"], foreign_home, "step 7"),
        "Run it without sudo",
        "step 7",
    );
    directory("/var/lib/open-ferry/versions", "step 8");
    let foreign_file = "/var/lib/open-ferry/versions/foreign";
    fs::write(foreign_file, b"untrusted record").expect("step 8: foreign file");
    chown(foreign_file, Some(1000), None).expect("step 8: foreign owner");
    let result = run(INSTALLED, &["update", "-check"], HOME, "step 8");
    refused(&result, "owned by uid 1000", "step 8");
    assert!(
        result.text.contains(foreign_file),
        "step 8: names foreign file: {}",
        result.text
    );
    fs::remove_file(foreign_file).expect("step 8: remove foreign file");
    let restore = RestoreMode;
    fs::set_permissions(DATA, fs::Permissions::from_mode(0o775))
        .expect("step 9: group-writable machine root");
    let result = run(INSTALLED, &["update", "-check"], HOME, "step 9");
    refused(
        &result,
        "Stop the open-ferry service, then as root fix what is named",
        "step 9",
    );
    assert!(
        result.text.contains("its group can write to it"),
        "step 9: group-write refusal: {}",
        result.text
    );
    assert!(
        result.text.contains(DATA),
        "step 9: refusal names machine root: {}",
        result.text
    );
    drop(restore);
    assert_eq!(
        fs::metadata(DATA)
            .expect("step 9: restored machine root")
            .mode()
            & 0o777,
        0o755,
        "step 9: restore 0755"
    );
}
