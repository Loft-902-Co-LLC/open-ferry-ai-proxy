//! The real supervisor and worker stage a signed release and switch themselves.
#![cfg(feature = "blue-green-test")]

#[path = "support/privilege.rs"]
mod privilege;
#[path = "../../open-ferry-update/src/tests/support.rs"]
#[allow(dead_code)]
mod releases;
#[path = "../../open-ferry-supervisor/tests/under_test/mod.rs"]
mod under_test;

// The signed-release fixture uses the update crate's own modules.
use open_ferry_update::{data_dir, fetch, install, keys, release, runner, switch, updater};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use under_test::*;

#[cfg(target_os = "linux")]
#[path = "support/parent_death.rs"]
mod parent_death;

fn launch(root: &Path, port: u16, base: &str) -> Sup {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(NAME);
    write_executable(&exe, &artifact_bytes(env!("CARGO_PKG_VERSION")));
    let data = data_dir::DataDir::at(root.join("data"));
    releases::write_receipt(&data, &exe);
    let mut config = config_text(root, port, "auto");
    config.push_str("remote-management:\n  secret-key: auto-test-password\n");
    std::fs::write(root.join("config.yaml"), config).unwrap();
    let log = std::fs::File::create(root.join("supervisor.log")).unwrap();
    let mut command = Command::new(&exe);
    isolate(&mut command);
    command
        .arg("-supervise")
        .arg("-config")
        .arg(root.join("config.yaml"))
        .arg("-test-data-dir")
        .arg(data.root());
    if let Some(endpoint) = endpoint_flag(root) {
        command.arg("-test-endpoint").arg(endpoint);
    }
    command
        .env("OPEN_FERRY_UPDATE_BASE_URL", base)
        .env("TOKIO_WORKER_THREADS", "1")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(target_os = "linux")]
    parent_death::on_test_exit(&mut command);
    Sup::adopt(command.spawn().unwrap(), root)
}

fn dump(root: &Path) -> String {
    std::fs::read_to_string(root.join("supervisor.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.contains("[access_log.rs:"))
        .collect::<Vec<_>>()
        .join("\n")
}

struct Waits<'a> {
    root: &'a Path,
    port: u16,
    server: &'a releases::ReleaseServer,
}

impl Waits<'_> {
    fn observed(&self) -> serde_json::Value {
        let data = data_dir::DataDir::at(self.root.join("data"));
        let state = open_ferry_update::state::State::load(&data.state_file());
        let view = journal(self.root);
        let dashboard = match try_update_request(self.port, "GET", "") {
            Ok((status, answer)) => serde_json::json!({
                "status": status,
                "body": answer.split_once("\r\n\r\n").map(|(_, body)| body),
            }),
            Err(error) => serde_json::json!({"error": error.to_string()}),
        };
        serde_json::json!({
            "state": {
                "has_failed(99.0.0)": state.has_failed("99.0.0"),
                "last_check": state.last_check,
                "last_result": state.last_result,
                "history": state.history,
            },
            "journal": {
                "active": view["active"],
                "switch": view["switch"],
                "outcomes": view["outcomes"],
            },
            "dashboard": dashboard,
            "server.requests()": self.server.requests(),
        })
    }

    async fn until(&self, name: &str, mut condition: impl FnMut() -> bool) {
        let due = Instant::now() + LONG;
        while !condition() {
            assert!(
                Instant::now() < due,
                "timed out waiting for {name}\n{:#}\n{}",
                self.observed(),
                dump(self.root)
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

fn journal(root: &Path) -> serde_json::Value {
    std::fs::read(
        root.join("data")
            .join(open_ferry_update::journal::JOURNAL_FILE),
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    .unwrap_or_default()
}

// Requests continue across loading, hand-over and go. Every request must get
// the root handler's successful response; an I/O failure is also a failure.
fn complete_get(port: u16) -> (u16, Option<String>) {
    use std::io::{Read, Write};
    let wait = Duration::from_secs(5);
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = std::net::TcpStream::connect_timeout(&address, wait).unwrap();
    stream.set_read_timeout(Some(wait)).unwrap();
    stream.set_write_timeout(Some(wait)).unwrap();
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("the complete response body");
    let (head, body) = response.split_once("\r\n\r\n").expect("HTTP headers");
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .expect("the root response has a content length");
    assert_eq!(body.len(), length, "a truncated response: {response}");
    let status = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let marker = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("x-open-ferry-test-marker")
            .then(|| value.trim().to_owned())
    });
    (status, marker)
}

type Responses = Vec<(u16, Option<String>)>;

struct Requests {
    stop: Arc<AtomicBool>,
    new_seen: Arc<AtomicBool>,
    ready: Arc<AtomicUsize>,
    threads: Vec<std::thread::JoinHandle<Responses>>,
}
impl Requests {
    fn start(port: u16) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let new_seen = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicUsize::new(0));
        let threads = (0..4)
            .map(|_| {
                let done = stop.clone();
                let seen = new_seen.clone();
                let ready = ready.clone();
                std::thread::spawn(move || {
                    let mut answers = Vec::new();
                    while !done.load(Ordering::SeqCst) {
                        let answer = complete_get(port);
                        if answer.1.as_deref() == Some("99.0.0") {
                            seen.store(true, Ordering::SeqCst);
                        }
                        answers.push(answer);
                        if answers.len() == 1 {
                            ready.fetch_add(1, Ordering::SeqCst);
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    answers
                })
            })
            .collect();
        Self {
            stop,
            new_seen,
            ready,
            threads,
        }
    }
    fn finish(mut self) -> Responses {
        self.stop.store(true, Ordering::SeqCst);
        self.threads
            .drain(..)
            .flat_map(|thread| {
                let answers = thread.join().unwrap();
                assert!(!answers.is_empty(), "each request loop made requests");
                answers
            })
            .collect()
    }
}
impl Drop for Requests {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

async fn prepare() -> (tempfile::TempDir, releases::ReleaseServer, Sup, u16) {
    let dir = scratch();
    let root = dir.path();
    let data = data_dir::DataDir::at(root.join("data"));
    let seams = data.root().join(".blue-green-test");
    std::fs::create_dir_all(&seams).unwrap();
    let key = releases::TestKey::new();
    std::fs::write(seams.join("release-key"), key.public()).unwrap();
    std::fs::write(seams.join("update-now"), []).unwrap();
    std::fs::write(seams.join("auto.before_check.hold"), []).unwrap();
    let server = releases::ReleaseServer::start().await;
    let newer = "99.0.0";
    let mut binary = artifact_bytes(newer);
    binary.extend_from_slice(format!("\n--update-test-version {newer}--").as_bytes());
    let name = releases::archive_name(newer, open_ferry_update::TARGET);
    let archive = releases::release_archive(newer, open_ferry_update::TARGET, &binary);
    server.publish(&key, newer, &[(name, archive)]);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let sup = launch(root, port, server.base.as_str());
    let waits = Waits {
        root,
        port,
        server: &server,
    };
    waits
        .until("the original worker to serve HTTP", || {
            http_get(port, "/").is_some()
        })
        .await;
    let identity = open_ferry_supervisor::endpoint::Identity::current(
        open_ferry_supervisor::endpoint::ServiceKind::User,
    )
    .unwrap();
    let due = Instant::now() + LONG;
    loop {
        let status = open_ferry_supervisor::client::status(
            data.root(),
            &identity,
            Instant::now() + Duration::from_millis(750),
        )
        .await;
        if status
            .as_ref()
            .is_ok_and(|snapshot| snapshot.state == "idle")
        {
            break;
        }
        assert!(
            Instant::now() < due,
            "the supervisor never became idle: {status:?}\n{:#}\n{}",
            waits.observed(),
            dump(root)
        );
        tokio::time::sleep(POLL).await;
    }
    waits
        .until("the update lock to be released", || {
            data.try_lock().unwrap().is_some()
        })
        .await;
    (dir, server, sup, port)
}

fn can_update_here() -> bool {
    if privilege::privileged() {
        eprintln!("skipped: real CLI requires unprivileged scratch");
        return false;
    }
    if open_ferry_update::install::detect_container(std::path::Path::exists, |name| {
        std::env::var_os(name)
    }) {
        eprintln!("skipped: container installs do not update themselves");
        return false;
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_check_switches_the_real_supervised_worker_without_failed_requests() {
    if !can_update_here() {
        return;
    }
    let _permit = Permit::take();
    let (dir, server, _sup, port) = prepare().await;
    let root = dir.path();
    let waits = Waits {
        root,
        port,
        server: &server,
    };
    let requests = Requests::start(port);
    waits
        .until("all four request loops to start", || {
            requests.ready.load(Ordering::SeqCst) == 4
        })
        .await;
    waits
        .until("the automatic check to reach its barrier", || {
            root.join("data/.blue-green-test/auto.before_check.arrived")
                .exists()
        })
        .await;
    std::fs::write(
        root.join("data/.blue-green-test/auto.before_check.release"),
        [],
    )
    .unwrap();
    waits
        .until("99.0.0 to become active", || {
            journal(root)["active"]["version"] == "99.0.0"
        })
        .await;
    waits
        .until("a request to reach 99.0.0", || {
            requests.new_seen.load(Ordering::SeqCst)
        })
        .await;
    let answers = requests.finish();
    assert!(answers.len() > 1);
    assert!(
        answers.iter().all(|(status, _)| *status == 200),
        "{answers:?}"
    );
    assert!(
        answers
            .iter()
            .any(|(_, marker)| marker.as_deref() == Some(env!("CARGO_PKG_VERSION")))
    );
    assert!(
        answers
            .iter()
            .any(|(_, marker)| marker.as_deref() == Some("99.0.0"))
    );
    assert!(
        server.requests() >= 3,
        "the worker fetched the signed list and archive"
    );
    assert!(
        dump(root).contains("automatic switch accepted"),
        "{}",
        dump(root)
    );
}

fn update_request(port: u16, method: &str, suffix: &str) -> (u16, String) {
    try_update_request(port, method, suffix).unwrap()
}

fn try_update_request(port: u16, method: &str, suffix: &str) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let wait = Duration::from_secs(10);
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = std::net::TcpStream::connect_timeout(&address, wait)?;
    stream.set_read_timeout(Some(wait))?;
    stream.set_write_timeout(Some(wait))?;
    write!(
        stream,
        "{method} /open-ferry/api/v1/update{suffix} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer auto-test-password\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    let mut text = String::new();
    stream.read_to_string(&mut text)?;
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "HTTP status missing")
        })?;
    Ok((status, text))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_that_fails_its_watch_is_rolled_back_and_not_retried() {
    if !can_update_here() {
        return;
    }
    let _permit = Permit::take();
    let (dir, server, _sup, port) = prepare().await;
    let root = dir.path();
    let seams = root.join("data/.blue-green-test");
    let waits = Waits {
        root,
        port,
        server: &server,
    };
    waits
        .until("the automatic check to reach its barrier", || {
            seams.join("auto.before_check.arrived").exists()
        })
        .await;
    // Only the original worker needs an immediate check. New workers use
    // their normal first-check delay, leaving the explicit check below alone.
    std::fs::remove_file(seams.join("update-now")).unwrap();
    std::fs::write(seams.join("fault.exit-after"), "go@99.0.0").unwrap();
    std::fs::write(seams.join("auto.before_check.release"), []).unwrap();
    let data = data_dir::DataDir::at(root.join("data"));
    waits
        .until("the watch failure and completed rollback", || {
            let state = open_ferry_update::state::State::load(&data.state_file());
            let view = journal(root);
            let failed = state.has_failed("99.0.0")
                || view["outcomes"].as_array().is_some_and(|outcomes| {
                    outcomes
                        .iter()
                        .any(|outcome| outcome["mark_failed"]["version"] == "99.0.0")
                });
            failed
                && view["active"]["version"] == env!("CARGO_PKG_VERSION")
                && view["switch"].is_null()
        })
        .await;
    waits
        .until("the returned worker to serve HTTP", || {
            http_get(port, "/").is_some()
        })
        .await;
    waits
        .until("the dashboard to report checking=false", || {
            let (status, answer) = update_request(port, "GET", "");
            assert_eq!(status, 200, "{answer}");
            let (_, body) = answer.split_once("\r\n\r\n").unwrap();
            let status: serde_json::Value = serde_json::from_str(body).unwrap();
            status["checking"] == false
        })
        .await;
    // last_check has seconds precision. Leave the prior check's second so
    // its unchanged result cannot satisfy the explicit check's completion.
    let previous = open_ferry_update::state::State::load(&data.state_file()).last_check;
    waits
        .until("the prior last_check second to end", || {
            previous.as_deref()
                != Some(&chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        })
        .await;
    let before = server.requests();
    let (status, answer) = update_request(port, "POST", "/check");
    assert_eq!(status, 202, "{answer}\n{}", dump(root));
    assert!(
        answer.contains("started"),
        "dashboard starts the explicit check: {answer}"
    );
    waits
        .until("the explicit check to skip the failed release", || {
            let state = open_ferry_update::state::State::load(&data.state_file());
            state.last_check != previous
                && server.requests() >= before + 2
                && state.last_result.as_deref() == Some("skipped")
        })
        .await;
    let log = dump(root);
    assert_eq!(log.matches("automatic switch accepted").count(), 1, "{log}");
    let state = open_ferry_update::state::State::load(&data.state_file());
    let recorded = state.history.iter().any(|record| {
        record.to == "99.0.0"
            && record.result.as_deref() == Some("failed-after-switch")
            && record.returned_to.as_deref() == Some(env!("CARGO_PKG_VERSION"))
    });
    let view = journal(root);
    let pending = view["outcomes"].as_array().is_some_and(|outcomes| {
        outcomes.iter().any(|outcome| {
            outcome["record"]["to"] == "99.0.0"
                && outcome["record"]["result"] == "failed-after-switch"
                && outcome["record"]["returned_to"] == env!("CARGO_PKG_VERSION")
        })
    });
    assert!(
        recorded || pending,
        "the supervisor recorded the watch failure and return: {state:?}, {view}"
    );
}
