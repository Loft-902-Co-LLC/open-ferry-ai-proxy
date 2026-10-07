//! Running a proxy under test: its config, its process, and what the OS says
//! about the process's CPU time and memory.
//!
//! Each run gets a fresh directory with its own config, auth directory and
//! home directory. The process starts with an environment that holds little
//! more than those directories and a dead loopback proxy for any request the
//! proxy would make beyond the fake upstream: CLIProxyAPI fetches a version
//! manifest from the internet at start, whatever its config says.

use std::error::Error;
use std::fs::{self, File};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// The client key both proxies are configured with.
pub const CLIENT_KEY: &str = "open-ferry-bench-client-key";

/// The model each provider of the config serves.
pub const OPENAI_MODEL: &str = "bench-gpt";
pub const CLAUDE_MODEL: &str = "bench-claude";

/// How long a proxy may take to answer `/v1/models` after it starts.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Attempts at `/v1/models` waiting at once, at most.
const MAX_ATTEMPTS: usize = 1024;

/// The config both proxies run with, in the v8 layout both read. Everything
/// that could reach beyond loopback or write more than logs is off: the
/// management API and its control panel (whose page CLIProxyAPI downloads),
/// CLIProxyAPI's mDNS announcements, request logs and usage statistics.
/// `-local-model` keeps CLIProxyAPI's model catalogs to the ones built in;
/// open-ferry never downloads one.
/// CLIProxyAPI's Claude request cloaking is off, as open-ferry has none, so
/// both send the client's request on as it came.
pub fn config(port: u16, upstream: SocketAddr, auth_dir: &Path) -> String {
    // A single-quoted YAML string holds a Windows path as it is.
    let auth_dir = auth_dir.display().to_string().replace('\'', "''");
    format!(
        "\
config-version: 8

server:
  host: \"127.0.0.1\"
  port: {port}
  discovery:
    enabled: false

management:
  allow-remote: false
  secret-key: \"\"
  disable-control-panel: true

access:
  api-keys:
    - \"{CLIENT_KEY}\"

routing:
  retry:
    request-retry: 0

upstream:
  claude:
    disable-claude-cloak-mode: true

api-keys:
  claude:
    - name: bench-claude
      base-url: \"http://{upstream}\"
      keys:
        - api-key: \"bench-upstream-key\"
      models:
        - name: \"{CLAUDE_MODEL}\"
          alias: \"{CLAUDE_MODEL}\"
  openai-compatibility:
    - name: bench
      base-url: \"http://{upstream}/v1\"
      keys:
        - api-key: \"bench-upstream-key\"
      models:
        - name: \"{OPENAI_MODEL}\"
          alias: \"{OPENAI_MODEL}\"

oauth:
  auth-dir: '{auth_dir}'

observability:
  logs:
    debug: false
    logging-to-file: false
    request-log: false
  usage:
    usage-statistics-enabled: false
"
    )
}

/// A proxy binary to benchmark.
pub struct Target {
    /// `open-ferry` or `CLIProxyAPI`.
    pub name: &'static str,
    pub binary: PathBuf,
    /// Where it came from, for the report.
    pub version: String,
}

/// What every run of a proxy shares.
pub struct Setup {
    pub port: u16,
    pub upstream: SocketAddr,
    /// The port of the dead proxy in the environment.
    pub dead_proxy: u16,
    /// Run directories go here.
    pub runs: PathBuf,
}

/// A running proxy, killed when dropped.
pub struct Running {
    child: Child,
    dir: PathBuf,
    started: Instant,
    stopped: bool,
}

impl Running {
    /// Writes a fresh run directory named `name` and starts `target` in it.
    pub fn start(target: &Target, setup: &Setup, name: &str) -> Result<Self, Box<dyn Error>> {
        if listening(setup.port) {
            return Err(format!(
                "something already listens on 127.0.0.1:{}; pick another port with --port",
                setup.port
            )
            .into());
        }
        let dir = setup.runs.join(name);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        let auth_dir = dir.join("auth");
        let home = dir.join("home");
        let tmp = dir.join("tmp");
        for path in [&auth_dir, &home, &tmp] {
            fs::create_dir_all(path)?;
        }
        let config_path = dir.join("config.yaml");
        fs::write(&config_path, config(setup.port, setup.upstream, &auth_dir))?;

        let mut command = Command::new(&target.binary);
        command
            .arg("-config")
            .arg(&config_path)
            .arg("-local-model")
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(File::create(dir.join("stdout.log"))?)
            .stderr(File::create(dir.join("stderr.log"))?);
        isolate_env(&mut command, &home, &tmp, setup.dead_proxy);
        let started = Instant::now();
        let child = command
            .spawn()
            .map_err(|err| format!("couldn't start {}: {err}", target.binary.display()))?;
        Ok(Self {
            child,
            dir,
            started,
            stopped: false,
        })
    }

    pub fn pid(&self) -> Pid {
        Pid::from_u32(self.child.id())
    }

    /// Waits for the first `200` from `GET /v1/models`, and returns how long
    /// after the start it came.
    pub async fn wait_ready(&mut self, port: u16) -> Result<Duration, Box<dyn Error>> {
        // A new attempt starts every millisecond, each on a connection of its
        // own. On Windows a connection to a port nobody listens on isn't
        // refused for seconds, and a timer of the async runtime can't wait
        // less than the system's clock tick (15.6 ms there): so attempts
        // overlap, and the first that gets an answer counts.
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let mut attempts = tokio::task::JoinSet::new();
        let mut last = String::from("no answer");
        while self.started.elapsed() < READY_TIMEOUT {
            if attempts.len() < MAX_ATTEMPTS {
                attempts.spawn(models_status(addr));
            }
            while let Some(done) = attempts.try_join_next() {
                match done {
                    Ok(Ok((200, answered))) => {
                        attempts.abort_all();
                        return Ok(answered.saturating_duration_since(self.started));
                    }
                    Ok(Ok((status, _))) => last = format!("HTTP {status}"),
                    Ok(Err(err)) => last = err.to_string(),
                    Err(_) => {}
                }
            }
            if let Some(status) = self.child.try_wait()? {
                return Err(format!(
                    "the proxy exited ({status}) before it answered; see {}",
                    self.dir.display()
                )
                .into());
            }
            let _ =
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(1))).await;
        }
        Err(format!(
            "the proxy didn't answer /v1/models within {READY_TIMEOUT:?} ({last}); see {}",
            self.dir.display()
        )
        .into())
    }

    /// Kills the proxy and removes its run directory.
    pub fn stop(mut self) -> Result<(), Box<dyn Error>> {
        self.kill();
        self.stopped = true;
        // The process may still hold its log files for a moment on Windows.
        let mut result = fs::remove_dir_all(&self.dir);
        for _ in 0..20 {
            if result.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            result = fs::remove_dir_all(&self.dir);
        }
        result.map_err(|err| format!("couldn't remove {}: {err}", self.dir.display()).into())
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.stopped {
            self.kill();
            eprintln!("logs of the stopped run are in {}", self.dir.display());
        }
    }
}

/// Sends `GET /v1/models` with the client key on a connection of its own,
/// and returns the answer's status and when the whole answer had come.
async fn models_status(addr: SocketAddr) -> std::io::Result<(u16, Instant)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let request = format!(
        "GET /v1/models HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {CLIENT_KEY}\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await?;
    let answered = Instant::now();
    // The status line: `HTTP/1.1 200 OK`.
    let status = answer
        .get(9..12)
        .and_then(|code| std::str::from_utf8(code).ok())
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, answered))
}

/// Whether something accepts connections on 127.0.0.1:`port`. Found by
/// connecting, never by binding.
fn listening(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Starts the process with only what it needs from the environment, its own
/// home and temporary directories, and the dead proxy for anything that
/// isn't on loopback.
fn isolate_env(command: &mut Command, home: &Path, tmp: &Path, dead_proxy: u16) {
    command.env_clear();
    #[cfg(windows)]
    {
        for name in [
            "SystemRoot",
            "windir",
            "SystemDrive",
            "NUMBER_OF_PROCESSORS",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Some(root) = std::env::var_os("SystemRoot") {
            let root = PathBuf::from(root);
            command.env("PATH", root.join("System32"));
        }
        command
            .env("USERPROFILE", home)
            .env("APPDATA", home.join("AppData").join("Roaming"))
            .env("LOCALAPPDATA", home.join("AppData").join("Local"))
            .env("TEMP", tmp)
            .env("TMP", tmp);
    }
    #[cfg(not(windows))]
    {
        command.env("PATH", "/usr/bin:/bin").env("TMPDIR", tmp);
    }
    command.env("HOME", home);
    let proxy = format!("http://127.0.0.1:{dead_proxy}");
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(name, &proxy);
    }
    for name in ["NO_PROXY", "no_proxy"] {
        command.env(name, "127.0.0.1,localhost,::1");
    }
}

/// A loopback port that refuses every connection: a socket bound to it but
/// not listening, held for as long as this lives.
pub struct DeadPort {
    _socket: tokio::net::TcpSocket,
    pub port: u16,
}

impl DeadPort {
    pub fn new() -> std::io::Result<Self> {
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let port = socket.local_addr()?.port();
        Ok(Self {
            _socket: socket,
            port,
        })
    }
}

/// A process's CPU time and resident memory, as the OS reports them.
pub struct Usage {
    system: System,
    pid: Pid,
}

impl Usage {
    pub fn new(pid: Pid) -> Self {
        Self {
            system: System::new(),
            pid,
        }
    }

    fn refresh(&mut self) -> Option<&sysinfo::Process> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        self.system.process(self.pid)
    }

    /// User and system CPU time so far, in milliseconds.
    pub fn cpu_ms(&mut self) -> Option<u64> {
        self.refresh().map(sysinfo::Process::accumulated_cpu_time)
    }

    /// Resident memory (the working set on Windows), in bytes.
    pub fn memory(&mut self) -> Option<u64> {
        self.refresh().map(sysinfo::Process::memory)
    }
}

/// What the run is doing when memory is sampled.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    /// Started, before any request.
    Idle = 0,
    /// Warming up or running the long conversation.
    Other = 1,
    /// Short requests under load.
    Load = 2,
}

/// Samples a process's memory every 50 ms on a thread of its own.
pub struct MemorySampler {
    stage: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Vec<(u8, u64)>>,
}

/// Memory over a run.
pub struct Memory {
    /// Median while idle after the start.
    pub idle: Option<u64>,
    /// Median under load.
    pub load: Option<u64>,
    /// Highest seen.
    pub peak: Option<u64>,
}

impl MemorySampler {
    pub fn start(pid: Pid) -> Self {
        let stage = Arc::new(AtomicU8::new(Stage::Idle as u8));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stage, stop) = (Arc::clone(&stage), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut usage = Usage::new(pid);
                let mut samples = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    if let Some(bytes) = usage.memory() {
                        samples.push((stage.load(Ordering::Relaxed), bytes));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                samples
            })
        };
        Self {
            stage,
            stop,
            thread,
        }
    }

    pub fn set(&self, stage: Stage) {
        self.stage.store(stage as u8, Ordering::Relaxed);
    }

    pub fn finish(self) -> Memory {
        self.stop.store(true, Ordering::Relaxed);
        let samples = self.thread.join().unwrap_or_default();
        let median = |stage: Stage| {
            let mut of: Vec<u64> = samples
                .iter()
                .filter(|(s, _)| *s == stage as u8)
                .map(|(_, bytes)| *bytes)
                .collect();
            of.sort_unstable();
            of.get(of.len() / 2).copied()
        };
        Memory {
            idle: median(Stage::Idle),
            load: median(Stage::Load),
            peak: samples.iter().map(|(_, bytes)| *bytes).max(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the bench config keeps both proxies on loopback and
    // points both providers at the fake upstream.
    #[test]
    fn config_stays_on_loopback() {
        let upstream = SocketAddr::from((Ipv4Addr::LOCALHOST, 41000));
        let text = config(18317, upstream, Path::new(r"C:\tmp\it's\auth"));
        assert!(text.contains("  host: \"127.0.0.1\"\n  port: 18317\n"));
        assert!(text.contains("base-url: \"http://127.0.0.1:41000\"\n"));
        assert!(text.contains("base-url: \"http://127.0.0.1:41000/v1\"\n"));
        assert!(text.contains("auth-dir: 'C:\\tmp\\it''s\\auth'\n"));
        assert!(text.contains("disable-control-panel: true"));
        assert!(!text.contains("0.0.0.0"));
    }

    // Not upstream's: the readiness probe sends the client key and reads the
    // answer's status.
    #[tokio::test]
    async fn probes_models() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });
        let (status, _) = models_status(addr).await.unwrap();
        assert_eq!(status, 200);
        let request = server.await.unwrap();
        assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
        assert!(request.contains(&format!("\r\nAuthorization: Bearer {CLIENT_KEY}\r\n")));
    }

    // Not upstream's: the dead proxy's port refuses connections.
    #[tokio::test]
    async fn dead_port_refuses() {
        let dead = DeadPort::new().unwrap();
        assert!(!listening(dead.port));
    }
}
