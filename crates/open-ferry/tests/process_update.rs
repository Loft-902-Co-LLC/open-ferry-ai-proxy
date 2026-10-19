//! The real CLI switches a real supervisor while requests keep arriving.
#![cfg(feature = "blue-green-test")]

#[cfg(target_os = "linux")]
#[path = "support/parent_death.rs"]
mod parent_death;
#[path = "support/privilege.rs"]
mod privilege;
#[path = "../../open-ferry-supervisor/tests/under_test/mod.rs"]
mod under_test;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use open_ferry_supervisor::protocol::{EndpointReply, EndpointRequest, VERSION};
use open_ferry_update::{DataDir, State};
use serde_json::{Value, json};
use under_test::*;

const BEFORE: &str = open_ferry_update::CURRENT_VERSION;
const AFTER: &str = "99.0.0";

fn command(fx: &Fx) -> Command {
    let mut command = Command::new(fx.installed());
    isolate(&mut command);
    command
        .current_dir(fx.root())
        .stdin(Stdio::null())
        .env("XDG_DATA_HOME", fx.root())
        .env("XDG_CONFIG_HOME", fx.root())
        .env("LOCALAPPDATA", fx.root())
        .env("APPDATA", fx.root())
        .env("PROGRAMDATA", fx.root().join("machine"))
        .env("HOME", fx.root());
    command
}

fn log(fx: &Fx) -> String {
    fs::read_to_string(fx.root().join("supervisor.log")).unwrap_or_default()
}

fn start(fx: &Fx) -> (Sup, u16) {
    let deadline = Instant::now() + LONG;
    for attempt in 0..8 {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert_ne!(port, 8317, "never contact the developer's proxy");
        fs::write(
            fx.root().join("config.yaml"),
            config_text(fx.root(), port, "off"),
        )
        .unwrap();
        let output = File::create(fx.root().join("supervisor.log")).unwrap();
        let mut command = command(fx);
        command
            .arg("-supervise")
            .arg("-config")
            .arg(fx.root().join("config.yaml"))
            .arg("-test-data-dir")
            .arg(fx.data())
            .stdout(output.try_clone().unwrap())
            .stderr(output);
        if let Some(endpoint) = endpoint_flag(fx.root()) {
            command.arg("-test-endpoint").arg(endpoint);
        }
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        #[cfg(target_os = "linux")]
        parent_death::on_test_exit(&mut command);
        let mut supervisor = Sup::adopt(command.spawn().unwrap(), fx.root());
        loop {
            let text = log(fx);
            let conflict = text.lines().any(|line| {
                line.contains("failed to bind the main listener")
                    && ["(os error 98)", "(os error 48)", "(os error 10048)"]
                        .iter()
                        .any(|code| line.contains(code))
            });
            if conflict {
                assert!(attempt < 7, "{text}");
                break;
            }
            assert!(supervisor.try_exit().is_none(), "supervisor exited: {text}");
            let named = fs::read_to_string(fx.data().join("supervisor.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|record| record["pid"].as_u64());
            if named == Some(u64::from(supervisor.pid()))
                && http_get(port, "/").is_some_and(|(status, marker)| {
                    status == 200 && marker.as_deref() == Some("before")
                })
            {
                return (supervisor, port);
            }
            assert!(
                Instant::now() < deadline,
                "supervisor did not serve: {text}"
            );
            std::thread::sleep(POLL);
        }
    }
    unreachable!("all bind attempts failed")
}

fn status(fx: &Fx) -> Value {
    match endpoint_ask(fx.root(), EndpointRequest::Status { v: VERSION }) {
        Some(EndpointReply::Status { snapshot, .. }) => snapshot,
        other => panic!("no supervisor status: {other:?}\n{}", log(fx)),
    }
}

struct CliChild(Child);

impl Drop for CliChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn cli(fx: &Fx, label: &str, flags: &[&str]) -> (i32, Value) {
    let out = fx.root().join(format!("{label}.stdout"));
    let err = fx.root().join(format!("{label}.stderr"));
    let child = command(fx)
        .arg("update")
        .args(flags)
        .arg("-json")
        .arg("-config")
        .arg(fx.root().join("config.yaml"))
        .stdout(File::create(&out).unwrap())
        .stderr(File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let mut child = CliChild(child);
    let deadline = Instant::now() + LONG;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "{label} timed out\n{}", log(fx));
        std::thread::sleep(POLL);
    };
    let text = fs::read_to_string(out).unwrap();
    let errors = fs::read_to_string(err).unwrap();
    let value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{label}: {error}: {text}\n{errors}\n{}", log(fx)));
    (status.code().unwrap_or(-1), value)
}

fn request(port: u16) -> Result<String, String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|error| format!("connect: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .map_err(|error| format!("write: {error}"))?;
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read: {error}"))?;
    let text = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    let (headers, body) = text
        .split_once("\r\n\r\n")
        .ok_or("incomplete HTTP response")?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1));
    if status != Some("200") {
        return Err(format!("HTTP status: {status:?}"));
    }
    let header = |name: &str| {
        headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    };
    let size = header("content-length").ok_or("missing Content-Length")?;
    let expected: usize = size.parse().map_err(|_| "invalid Content-Length")?;
    if body.len() != expected {
        return Err("incomplete response body".into());
    }
    match header("x-open-ferry-test-marker") {
        Some(marker @ ("before" | "after")) => Ok(marker.into()),
        other => Err(format!("unexpected worker: {other:?}")),
    }
}

struct Traffic {
    stop: Arc<AtomicBool>,
    before: Arc<AtomicU64>,
    after: Arc<AtomicU64>,
    task: Option<std::thread::JoinHandle<Result<(), String>>>,
}

impl Traffic {
    fn start(port: u16) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let before = Arc::new(AtomicU64::new(0));
        let after = Arc::new(AtomicU64::new(0));
        let (end, a, b) = (stop.clone(), before.clone(), after.clone());
        let task = std::thread::spawn(move || {
            while !end.load(Ordering::SeqCst) {
                match request(port)?.as_str() {
                    "before" => {
                        a.fetch_add(1, Ordering::SeqCst);
                    }
                    "after" => {
                        b.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => unreachable!(),
                }
            }
            Ok(())
        });
        Self {
            stop,
            before,
            after,
            task: Some(task),
        }
    }

    fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.task
            .take()
            .unwrap()
            .join()
            .expect("request thread panicked")
            .expect("no request may fail or be refused");
        eprintln!(
            "uninterrupted requests: {} before, {} after",
            self.before.load(Ordering::SeqCst),
            self.after.load(Ordering::SeqCst)
        );
    }
}

impl Drop for Traffic {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

#[test]
fn the_cli_updates_and_rolls_back_with_no_failed_or_refused_request() {
    if privilege::privileged() {
        eprintln!(
            "skip process_update: the account-install CLI test requires an unprivileged account"
        );
        return;
    }
    if open_ferry_update::install::detect_container(std::path::Path::exists, |name| {
        std::env::var_os(name)
    }) {
        eprintln!("skip process_update: container installs do not update themselves");
        return;
    }
    let fx = Fx::new_for_this_user_layout();
    write_executable(&fx.installed(), &artifact_bytes(BEFORE));
    fs::write(fx.installed().with_file_name("test-marker"), "before").unwrap();
    let data = DataDir::at(fx.data());
    fx.stage(BEFORE, "before");
    let next = fx.stage(AFTER, "after");
    State {
        latest: Some(AFTER.into()),
        staged: Some(AFTER.into()),
        staged_sha256: Some(next.sha),
        ..Default::default()
    }
    .save(&data.state_file())
    .unwrap();
    fs::write(
        data.receipt_file(),
        serde_json::to_vec(&json!({
            "format":1, "installer": if cfg!(windows) {"install.ps1"} else {"install.sh"},
            "version":BEFORE, "binary":fx.installed(), "target":open_ferry_update::TARGET,
            "installed_at":"2026-10-18T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let (mut supervisor, port) = start(&fx);
    fx.wait_for("the initial worker to load", || {
        fx.events()
            .iter()
            .any(|event| event.marker == "before" && event.event == "active")
            .then_some(())
    });
    fx.wait_for("startup to release the update lock", || {
        data.try_lock_existing_dir().ok().flatten().map(|_| ())
    });
    let traffic = Traffic::start(port);
    fx.wait_for("requests before update", || {
        (traffic.before.load(Ordering::SeqCst) >= 5).then_some(())
    });

    let (code, update) = cli(&fx, "update", &["-yes"]);
    assert_eq!(code, 0, "{update}\n{}", log(&fx));
    assert_eq!(update["result"], "updated");
    assert_eq!(update["switch"]["blue_green"], true);
    assert_eq!(update["switch"]["to"], AFTER);
    fx.wait_for("requests from the new version", || {
        (traffic.after.load(Ordering::SeqCst) >= 5).then_some(())
    });
    assert_eq!(status(&fx)["active_version"], AFTER);

    let before_rollback = traffic.before.load(Ordering::SeqCst);
    let (code, rollback) = cli(&fx, "rollback", &["-rollback", "-yes"]);
    assert_eq!(code, 0, "{rollback}\n{}", log(&fx));
    assert_eq!(rollback["result"], "rolled-back");
    assert_eq!(rollback["switch"]["to"], BEFORE);
    fx.wait_for("requests after rollback", || {
        (traffic.before.load(Ordering::SeqCst) >= before_rollback + 5).then_some(())
    });
    assert_eq!(status(&fx)["active_version"], BEFORE);

    // This build has no release key; its live supervisor view must survive
    // the independent release-check error, without contacting any provider.
    let (_, check) = cli(&fx, "check", &["-check"]);
    assert_eq!(check["switch_view"]["supervisor"]["active_version"], BEFORE);
    assert_eq!(check["switch_view"]["last"]["result"], "rolled-back");
    let (code, cancel) = cli(&fx, "cancel", &["-cancel"]);
    assert_eq!((code, cancel["result"].as_str()), (1, Some("too-late")));
    assert_eq!(
        State::load(&data.state_file()).rolled_back.as_deref(),
        Some(AFTER)
    );
    traffic.finish();
    assert!(matches!(
        endpoint_ask(fx.root(), EndpointRequest::Stop { v: VERSION }),
        Some(EndpointReply::Stopping { .. })
    ));
    assert!(fx.wait_exit(&mut supervisor).success(), "{}", log(&fx));
}
