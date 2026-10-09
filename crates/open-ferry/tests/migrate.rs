//! `open-ferry migrate` as a user runs it, and open-ferry started under
//! CLIProxyAPI's file names, as a drop-in switch leaves it. `migrate` is
//! run only with `-h` and bad usage, which it answers before it looks at
//! the system, so no test here reads the machine's processes, services or
//! scheduled tasks. The server is started only on a port on 127.0.0.1 that
//! nothing listened on, with its config, auth directory and logs in a
//! temporary directory, and only the process the test started is killed.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const OPEN_FERRY: &str = env!("CARGO_BIN_EXE_open-ferry");

/// Runs `binary` with `args` in `dir`, which has no `.env`.
fn run_as(binary: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new(binary)
        .args(args)
        .current_dir(dir)
        .env_remove("MANAGEMENT_PASSWORD")
        .env_remove("WRITABLE_PATH")
        .env_remove("writable_path")
        .output()
        .unwrap()
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
fn get_root(port: u16) -> Option<String> {
    let mut stream =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
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

// Not upstream's: open-ferry started as `cliproxyapi`, with CLIProxyAPI's
// command line, serves as open-ferry, which is what migrate's drop-in
// relies on.
#[test]
fn serves_under_cliproxyapis_name() {
    let dir = tempfile::tempdir().unwrap();
    let binary = renamed(dir.path(), "cliproxyapi");
    let auth = dir.path().join("auths");
    std::fs::create_dir(&auth).unwrap();
    let port = {
        let (_socket, port) = closed_port();
        port
    };
    let config = dir.path().join("config.yaml");
    std::fs::write(
        &config,
        format!(
            "host: 127.0.0.1\nport: {port}\nauth-dir: '{}'\napi-keys:\n  - 'test-client-key-for-the-renamed-binary'\n",
            auth.display()
        ),
    )
    .unwrap();
    let child = Command::new(&binary)
        .arg("-config")
        .arg(&config)
        .current_dir(dir.path())
        .env_remove("MANAGEMENT_PASSWORD")
        .env_remove("WRITABLE_PATH")
        .env_remove("writable_path")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = Started(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    let response = loop {
        if let Some(response) = get_root(port)
            && !response.is_empty()
        {
            break response;
        }
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "the server exited before it answered"
        );
        assert!(Instant::now() < deadline, "nothing answered on port {port}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("open-ferry-ai-proxy"), "{response}");
    drop(server);
}
