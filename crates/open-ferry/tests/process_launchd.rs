//! Not upstream's: opt-in coverage of the actual per-user LaunchAgent on a macOS runner.
#![cfg(all(target_os = "macos", feature = "blue-green-test"))]

#[path = "../../open-ferry-supervisor/tests/under_test/mod.rs"]
mod under_test;

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use open_ferry_supervisor::process::{SupervisorLock, unix::process_alive};
use open_ferry_update::data_dir::DataDir;
use open_ferry_update::receipt::Receipt;
use under_test::*;

const LABEL: &str = "io.github.loft-902-co-llc.open-ferry";

fn service(fx: &Fx, action: &str) -> Output {
    let mut command = Command::new(fx.installed());
    command.args(["service", action]);
    if action == "install" {
        command.arg("-config").arg(fx.root().join("config.yaml"));
    }
    isolate(&mut command);
    command
        .env("HOME", fx.root())
        .env("XDG_DATA_HOME", fx.root())
        .current_dir(fx.root())
        .output()
        .expect("service command")
}

struct Agent(String);

impl Drop for Agent {
    fn drop(&mut self) {
        // Also clean up after a failed assertion, before the fixture's files are removed.
        let _ = Command::new("launchctl")
            .args(["bootout", &self.0])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[test]
#[ignore = "needs an unused real launchd GUI domain; CI probes it before opting in"]
fn a_user_launchagent_serves_and_uninstalls_within_its_exit_timeout() {
    assert_eq!(std::env::var("OFP_TEST_LAUNCHD").as_deref(), Ok("1"));
    // SAFETY: geteuid has no preconditions.
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    assert!(
        Command::new("launchctl")
            .args(["print", &domain])
            .output()
            .unwrap()
            .status
            .success()
    );
    let name = format!("{domain}/{LABEL}");
    assert!(
        !Command::new("launchctl")
            .args(["print", &name])
            .output()
            .unwrap()
            .status
            .success(),
        "another service already owns the label"
    );

    let fx = Fx::new_for_this_user_layout();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::write(
        fx.root().join("config.yaml"),
        config_text(fx.root(), port, "off"),
    )
    .unwrap();
    std::fs::create_dir_all(fx.root().join("Library/Logs")).unwrap();
    Receipt::new(
        "install.sh",
        env!("CARGO_PKG_VERSION"),
        &fx.installed().display().to_string(),
        "aarch64-apple-darwin",
        chrono::Utc::now(),
    )
    .write_atomic(&DataDir::at(fx.data()).receipt_file())
    .unwrap();
    let _agent = Agent(name.clone());
    let installed = service(&fx, "install");
    assert!(
        installed.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    let plist = std::fs::read_to_string(
        fx.root()
            .join(format!("Library/LaunchAgents/{LABEL}.plist")),
    )
    .unwrap();
    assert!(
        plist.contains("-supervise"),
        "the service must use its real supervisor"
    );
    assert!(plist.contains("<key>ExitTimeOut</key>\n\t<integer>60</integer>"));
    fx.wait_for("launchd's worker to serve", || http_get(port, "/"));
    let data = DataDir::at(fx.data());
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data.root().join("supervisor.json")).unwrap())
            .unwrap();
    let pid = record["pid"].as_u64().unwrap() as u32;
    let started = open_ferry_supervisor::process::unix::start_time(pid)
        .unwrap()
        .unwrap();
    // A worker serves before it logs that it is active, so its first event can
    // trail the response above.
    let workers = fx.wait_for("a supervised worker to report events", || {
        let workers: Vec<_> = fx
            .events()
            .into_iter()
            .filter_map(|event| {
                open_ferry_supervisor::process::unix::start_time(event.pid)
                    .ok()
                    .flatten()
                    .map(|started| (event.pid, started))
            })
            .collect();
        (!workers.is_empty()).then_some(workers)
    });

    let began = Instant::now();
    let removed = service(&fx, "uninstall");
    assert!(
        removed.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&removed.stdout),
        String::from_utf8_lossy(&removed.stderr)
    );
    // bootout can return before the job exits. Its processes and lifetime lock
    // must be gone within the same ExitTimeOut, measured from the command's start.
    loop {
        let supervisor_alive = process_alive(pid, started);
        let worker_alive = workers
            .iter()
            .any(|&(pid, started)| process_alive(pid, started));
        if !supervisor_alive && !worker_alive {
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "uninstall exceeded launchd's ExitTimeOut: supervisor={supervisor_alive}, worker={worker_alive}\n{}",
            std::fs::read_to_string(fx.root().join("Library/Logs/open-ferry.log"))
                .unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        began.elapsed() < Duration::from_secs(60),
        "uninstall exceeded launchd's ExitTimeOut"
    );
    SupervisorLock::try_acquire(data.root()).expect("supervisor lifetime lock released");
    eprintln!(
        "LaunchAgent served and released its supervisor, workers and lock in {:.3}s after uninstall began",
        began.elapsed().as_secs_f64()
    );
    assert!(
        !Command::new("launchctl")
            .args(["print", &name])
            .output()
            .unwrap()
            .status
            .success()
    );
}
