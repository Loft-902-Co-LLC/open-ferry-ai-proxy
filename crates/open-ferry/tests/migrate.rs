//! `open-ferry migrate` as a user runs it, and open-ferry started under
//! CLIProxyAPI's file names, as a drop-in switch leaves it. `migrate` is
//! run only with `-h` and bad usage, which it answers before it looks at
//! the system, so no test here reads the machine's processes, services or
//! scheduled tasks. The server is started only on a port on 127.0.0.1 that
//! nothing listened on, with its config, auth directory and logs in a
//! temporary directory, and only the process the test started is killed.
//! With blue-green-test, the supervised service form also serves an isolated
//! migration config and stops through its supervisor; no service is installed.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[path = "support/privilege.rs"]
mod privilege;
#[path = "../../open-ferry-supervisor/tests/under_test/timeouts.rs"]
mod timeouts;
use timeouts::LONG;

const OPEN_FERRY: &str = env!("CARGO_BIN_EXE_open-ferry");

/// `start` once a copied binary can run. While another test forks, its
/// child briefly holds the copy's write handle until it runs its own
/// program, and Linux refuses to run a file open for writing (ETXTBSY).
fn when_runnable<T>(mut start: impl FnMut() -> std::io::Result<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match start() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            result => return result.unwrap(),
        }
    }
}

/// Runs `binary` with `args` in `dir`, which has no `.env`.
fn run_as(binary: &Path, dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(dir)
        .env_remove("MANAGEMENT_PASSWORD")
        .env_remove("WRITABLE_PATH")
        .env_remove("writable_path")
        .env("LOCALAPPDATA", dir)
        .env("XDG_DATA_HOME", dir)
        .env("HOME", dir);
    when_runnable(|| command.output())
}

fn run(dir: &Path, args: &[&str]) -> Output {
    run_as(Path::new(OPEN_FERRY), dir, args)
}

fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A port on 127.0.0.1 that nothing listens on, while the socket lives.
fn closed_port() -> (tokio::net::TcpSocket, u16) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = socket.local_addr().unwrap().port();
    (socket, port)
}

/// open-ferry's binary under `name` in `dir`: a hard link, or a copy where
/// one can't be made.
fn renamed(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if std::fs::hard_link(OPEN_FERRY, &path).is_err() {
        std::fs::copy(OPEN_FERRY, &path).unwrap();
    }
    path
}

// Not upstream's: migrate's usage, and its errors in usage, exit as the
// server's flags do, before anything is looked at.
#[test]
fn migrate_answers_usage_first() {
    let dir = tempfile::tempdir().unwrap();
    let help = run(dir.path(), &["migrate", "-h"]);
    assert_eq!(code(&help), Some(0));
    let usage = stderr(&help);
    assert!(usage.contains("Usage:"), "{usage}");
    assert!(
        usage.contains("migrate -undo [-restore] [-yes] [-dry-run]"),
        "{usage}"
    );
    assert!(usage.contains("-dry-run"), "{usage}");

    for (args, message) in [
        (
            &["migrate", "-bogus"][..],
            "flag provided but not defined: -bogus",
        ),
        (&["migrate", "now"][..], "unexpected argument: now"),
        (&["migrate", "-restore"][..], "-restore goes with -undo"),
        (
            &["migrate", "-undo", "-json"][..],
            "-json doesn't go with -undo",
        ),
    ] {
        let output = run(dir.path(), args);
        assert_eq!(code(&output), Some(2), "{args:?}");
        let text = stderr(&output);
        assert!(text.contains(message), "{args:?}: {text}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

// Not upstream's: open-ferry under CLIProxyAPI's file names answers its
// usage and subcommands as it does under its own.
#[test]
fn behaves_the_same_under_another_name() {
    let dir = tempfile::tempdir().unwrap();
    let own = run(dir.path(), &["-h"]);
    for name in ["cli-proxy-api", "cliproxyapi"] {
        let binary = renamed(dir.path(), name);
        let help = run_as(&binary, dir.path(), &["-h"]);
        assert_eq!(code(&help), code(&own), "{name}");
        let text = stderr(&help);
        assert!(text.contains("Usage of"), "{name}: {text}");
        assert!(text.contains("-config"), "{name}: {text}");
        assert!(text.contains("-local-model"), "{name}: {text}");

        let migrate = run_as(&binary, dir.path(), &["migrate", "-h"]);
        assert_eq!(code(&migrate), Some(0), "{name}");
        assert!(stderr(&migrate).contains("migrate -undo"), "{name}");

        let bogus = run_as(&binary, dir.path(), &["migrate", "-bogus"]);
        assert_eq!(code(&bogus), Some(2), "{name}");
        assert!(
            stderr(&bogus).contains("flag provided but not defined: -bogus"),
            "{name}"
        );

        let unknown = run_as(&binary, dir.path(), &["-bogus"]);
        assert_eq!(code(&unknown), Some(2), "{name}");
        assert!(
            stderr(&unknown).contains("flag provided but not defined: -bogus"),
            "{name}"
        );
    }
}

/// `GET /` on 127.0.0.1:`port`: the response, or nothing yet.
fn get(port: u16, path: &str, key: Option<&str>) -> Option<String> {
    let mut stream =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let auth = key
        .map(|key| format!("Authorization: Bearer {key}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

/// Kills the server the test started when the test ends.
struct Started(std::process::Child);

impl Drop for Started {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Both a fixture key and the rejection of a different key identify our
/// server: a foreign keyless server accepts either and must not pass.
fn answers_as_ours(port: u16, key: &str) -> bool {
    get(port, "/", None).is_some_and(|root| {
        root.starts_with("HTTP/1.1 200") && root.contains("open-ferry-ai-proxy")
    }) && get(port, "/v1/models", Some(key))
        .is_some_and(|models| models.starts_with("HTTP/1.1 200"))
        && get(port, "/v1/models", Some(&format!("{key}-wrong")))
            .is_some_and(|denied| denied.starts_with("HTTP/1.1 401"))
}

/// Starts our renamed server, retrying only its own logged bind failure.
fn start_renamed(dir: &Path, first_port: Option<u16>, budget: Duration) -> (Started, u16) {
    let binary = renamed(dir, "cliproxyapi");
    let auth = dir.join("auths");
    std::fs::create_dir(&auth).unwrap();
    let key = format!("renamed-{}", dir.file_name().unwrap().to_string_lossy());
    let config = dir.join("config.yaml");
    let log_path = dir.join("server.log");
    let deadline = Instant::now() + budget;
    let mut next_port = first_port;
    let mut attempt = 0;
    'attempts: loop {
        assert!(
            Instant::now() < deadline,
            "no free port within the startup budget"
        );
        let port = next_port.take().unwrap_or_else(|| {
            let (socket, port) = closed_port();
            drop(socket);
            port
        });
        attempt += 1;
        std::fs::write(&config, format!(
            "host: 127.0.0.1\nport: {port}\nauth-dir: '{}'\napi-keys:\n  - '{key}'\nself-update:\n  mode: off\n", auth.display()
        )).unwrap();
        let log = std::fs::File::create(&log_path).unwrap();
        let mut command = Command::new(&binary);
        command
            .arg("-config")
            .arg(&config)
            .current_dir(dir)
            .env_remove("MANAGEMENT_PASSWORD")
            .env_remove("WRITABLE_PATH")
            .env_remove("writable_path")
            .env("LOCALAPPDATA", dir)
            .env("XDG_DATA_HOME", dir)
            .env("HOME", dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        let mut server = Started(when_runnable(|| command.spawn()));
        loop {
            let exit = server.0.try_wait().unwrap();
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            if let Some(status) = exit {
                let conflict = log.lines().any(|line| {
                    line.contains("failed to start HTTP server")
                        && ["(os error 98)", "(os error 48)", "(os error 10048)"]
                            .iter()
                            .any(|code| line.contains(code))
                });
                assert!(
                    conflict,
                    "the server exited ({status}) before it answered:\n{log}"
                );
                if attempt == 1 {
                    std::fs::write(dir.join("first-bind-conflict.log"), &log).unwrap();
                }
                continue 'attempts;
            }
            if answers_as_ours(port, &key) {
                return (server, port);
            }
            assert!(
                Instant::now() < deadline,
                "nothing authenticated on port {port}:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

// Not upstream's: open-ferry started as `cliproxyapi`, with CLIProxyAPI's
// command line, serves as open-ferry, which is what migrate's drop-in relies on.
#[test]
fn serves_under_cliproxyapis_name() {
    let dir = tempfile::tempdir().unwrap();
    let (_server, _port) = start_renamed(dir.path(), None, Duration::from_secs(30));
}

#[test]
fn a_bind_conflict_retries_on_a_different_port() {
    // The non-root Linux job exercises this control.
    if privilege::privileged() {
        eprintln!(
            "skipped migrate::a_bind_conflict_retries_on_a_different_port: requires an unprivileged account"
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // Reserve without listening: the first child must fail its bind, without
    // a probe waiting for an HTTP reply from this intentionally silent socket.
    let (held, occupied) = closed_port();
    let (_server, port) = start_renamed(dir.path(), Some(occupied), LONG);
    assert_ne!(port, occupied);
    let log = std::fs::read_to_string(dir.path().join("first-bind-conflict.log")).unwrap();
    assert!(log.contains("failed to start HTTP server"), "{log}");
    drop(held);
}

#[tokio::test]
async fn a_keyless_foreign_server_is_rejected() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = axum::Router::new()
        .route("/", axum::routing::get(|| async { "open-ferry-ai-proxy" }))
        .route("/v1/models", axum::routing::get(|| async { "{}" }));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let accepted = tokio::task::spawn_blocking(move || answers_as_ours(port, "ours"))
        .await
        .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap();
    assert!(
        !accepted,
        "a foreign keyless server passed the fixture identity check"
    );
}

#[cfg(feature = "blue-green-test")]
#[path = "../../open-ferry-supervisor/tests/under_test/mod.rs"]
mod under_test;

// Not upstream's: the service form migrate installs serves its preserved
// config through a real supervisor, and the uninstall stop ends that run.
#[cfg(feature = "blue-green-test")]
#[test]
fn serves_the_migrated_config_with_the_supervisor() {
    use open_ferry_supervisor::client::{self, StopOutcome};
    use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
    use under_test::{Permit, Sup, isolate, scratch, write_executable};

    if privilege::privileged() {
        eprintln!(
            "skipped serves_the_migrated_config_with_the_supervisor: requires an unprivileged account"
        );
        return;
    }
    let _permit = Permit::take();
    let dir = scratch();
    let root = dir.path();
    let working = root.join("cpa");
    let bin = root.join("bin");
    std::fs::create_dir(&working).unwrap();
    std::fs::create_dir(&bin).unwrap();
    let exe = bin.join(format!("open-ferry{}", std::env::consts::EXE_SUFFIX));
    write_executable(&exe, &std::fs::read(OPEN_FERRY).unwrap());
    let data = root.join("data");
    let config = working.join("config.yaml");
    let key = format!("migrated-{}", root.file_name().unwrap().to_string_lossy());
    let log_path = root.join("supervisor.log");
    let until = Instant::now() + LONG;
    for _ in 0..8 {
        let (socket, port) = closed_port();
        std::fs::write(&config, format!(
            "host: 127.0.0.1\nport: {port}\nauth-dir: '{}'\napi-keys:\n  - '{key}'\nself-update:\n  mode: off\n",
            working.join("auths").display()
        )).unwrap();
        let log = std::fs::File::create(&log_path).unwrap();
        let mut command = Command::new(&exe);
        #[cfg(windows)]
        command.args(["service", "run"]);
        command
            .arg("-supervise")
            .arg("-config")
            .arg(&config)
            .arg("-data-dir")
            .arg(&data);
        #[cfg(unix)]
        if let Some(endpoint) = under_test::endpoint_flag(root) {
            command.arg("-test-endpoint").arg(endpoint);
        }
        isolate(&mut command);
        command
            .current_dir(&working)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        drop(socket);
        let mut supervisor = Sup::adopt(when_runnable(|| command.spawn()), root);
        loop {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            assert!(supervisor.try_exit().is_none(), "supervisor exited:\n{log}");
            if answers_as_ours(port, &key) {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let identity = Identity::current(ServiceKind::User).unwrap();
                let stopped =
                    runtime.block_on(client::stop(&data, &identity, Instant::now() + LONG));
                assert_eq!(stopped, StopOutcome::Stopped, "{log}");
                while supervisor.try_exit().is_none() {
                    assert!(Instant::now() < until, "supervisor did not exit:\n{log}");
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(!answers_as_ours(port, &key));
                return;
            }
            let conflict = log.lines().any(|line| {
                line.contains("failed to bind the main listener")
                    && ["(os error 98)", "(os error 48)", "(os error 10048)"]
                        .iter()
                        .any(|code| line.contains(code))
            });
            assert!(Instant::now() < until, "supervisor did not answer:\n{log}");
            if conflict {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    panic!("no free port within the startup budget");
}
