//! Supervised SCM entry and worker output without registering a service.
#![cfg(all(windows, feature = "blue-green-test"))]

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use open_ferry_supervisor::process::windows::{SpawnSpec, WorkerProcess};

fn scratch() -> tempfile::TempDir {
    let root = PathBuf::from(std::env::var_os("OFP_TEST_SCRATCH").expect("OFP_TEST_SCRATCH"));
    tempfile::tempdir_in(root).unwrap()
}

fn isolate(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if ["ANTHROPIC", "CLAUDE", "OPENAI", "CODEX"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        {
            command.env_remove(name);
        }
    }
}

fn wait(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("our child exceeded its watchdog");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_hand_run_system_supervisor_refuses_before_starting_any_worker() {
    let root = scratch();
    let config = root.path().join("config.yaml");
    fs::write(
        &config,
        "host: 127.0.0.1\nport: 0\nself-update:\n  mode: off\n",
    )
    .unwrap();
    let data = root.path().join("data");
    let output = root.path().join("output.log");
    let log = File::create(&output).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_open-ferry"));
    isolate(&mut command);
    let mut child = command
        .args(["service", "run", "-system", "-supervise", "-config"])
        .arg(&config)
        .arg("-data-dir")
        .arg(&data)
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    assert_eq!(wait(&mut child).code(), Some(1));
    let output = fs::read_to_string(output).unwrap();
    assert!(
        output.contains("a system supervisor runs only as the system service; sc start open-ferry"),
        "{output}"
    );
    assert!(!data.exists(), "startup touched the data directory");
    assert!(
        !root.path().join("service.log").exists(),
        "the service started"
    );
}

fn worker_line(log: Option<&Path>) {
    let exe = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let args = ["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(),
        "[Console]::Out.WriteLine('worker stdout reaches service.log'); [Console]::Error.WriteLine('worker stderr reaches service.log')".into()];
    let output = log.map(|path| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
    });
    let worker = WorkerProcess::spawn(&SpawnSpec {
        exe: &exe,
        args: &args,
        cwd: None,
        stdout: output.as_ref(),
        stderr: None,
    })
    .unwrap();
    let status = worker.wait_timeout(Duration::from_secs(20)).unwrap();
    if status.is_none() {
        worker.kill().unwrap();
        panic!("our output worker exceeded its watchdog");
    }
    assert!(status.unwrap().success());
}

#[test]
fn worker_output_reaches_the_redirected_service_log() {
    if std::env::var_os("OFP_SCM_OUTPUT_CHILD").is_some() {
        worker_line(None);
        return;
    }
    let root = scratch();
    let log_path = root.path().join("service.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    isolate(&mut command);
    let mut child = command
        .args([
            "--exact",
            "worker_output_reaches_the_redirected_service_log",
            "--test-threads=1",
        ])
        .env("OFP_SCM_OUTPUT_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    assert!(wait(&mut child).success());
    let text = fs::read_to_string(&log_path).unwrap();
    for line in [
        "worker stdout reaches service.log",
        "worker stderr reaches service.log",
    ] {
        assert!(text.contains(line), "{text}");
    }
    // Explicit log handles still follow the same restricted inheritance path.
    let explicit = root.path().join("explicit.log");
    worker_line(Some(&explicit));
    assert_eq!(fs::read_to_string(explicit).unwrap().lines().count(), 2);
}
