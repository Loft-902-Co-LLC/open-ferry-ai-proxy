//! Not upstream's: helpers shared by the Windows process tests. Every process
//! these start is held by a `WorkerProcess`, so a failed test that unwinds
//! closes the job and ends it by handle; nothing is found by name or listed.
#![cfg(windows)]
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use open_ferry_supervisor::process::windows::{SpawnSpec, WorkerProcess};

pub const LONG: Duration = Duration::from_secs(20);

pub fn probe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_windows_probe"))
}

pub fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

pub fn text(path: &Path) -> OsString {
    path.as_os_str().to_owned()
}

/// Starts the probe as a worker with its output in `log`.
pub fn spawn_probe(args: &[OsString], log: &File) -> WorkerProcess {
    let spec = SpawnSpec {
        exe: probe(),
        args,
        cwd: None,
        stdout: Some(log),
        stderr: Some(log),
    };
    WorkerProcess::spawn(&spec).expect("the probe starts")
}

pub fn wait_for(path: &Path) -> bool {
    let end = Instant::now() + LONG;
    while Instant::now() < end {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

pub fn wait_for_state(path: &Path, wanted: &str) -> bool {
    let end = Instant::now() + LONG;
    while Instant::now() < end {
        if std::fs::read_to_string(path).is_ok_and(|state| state == wanted) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// A file that is whole whenever it exists, like the probe's.
pub fn write_atomic(path: &Path, content: &str) {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, content).expect("write");
    std::fs::rename(&temporary, path).expect("rename");
}

/// Connects (the backlog takes the connection even if nobody accepts yet).
pub fn connect(address: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect_timeout(&address, LONG).expect("connect");
    stream
        .set_read_timeout(Some(LONG))
        .expect("set the read timeout");
    stream
}

/// What the probe answers: its process id.
pub fn answer(mut stream: TcpStream) -> String {
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read the answer");
    reply.trim().to_owned()
}
