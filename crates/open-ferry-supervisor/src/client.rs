//! Local supervisor clients: detection and stopping for installers, and
//! status, switches and cancellation for supervised updates.
//!
//! [`detect`] decides whether the installed binary may be replaced. It runs
//! only under the caller's update lock, and answers [`Detection::Clear`] only
//! with the binary's install lock taken: a supervisor holds that lock for its
//! whole life, whichever data directory it uses, so one taken shows that none
//! runs from the binary. Never from a failure. [`stop`] takes no lock. Each
//! takes one deadline that bounds all it does, its file reads included.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use open_ferry_update::install_lock::InstallLock;
use serde::Deserialize;

use crate::endpoint::{EndpointError, Identity, Location, connect};
use crate::process::SupervisorLock;
use crate::protocol::{Channel, EndpointReply, EndpointRequest, VERSION};
use crate::timings::DETECT_TIMEOUT;

mod switching;
pub use switching::{EndpointSwitcher, SwitchRequest, cancel, status, switch};

/// The most of `supervisor.json` that is read.
const RECORD_LIMIT: u64 = 64 * 1024;

/// How often [`stop`] looks again at a supervisor that has not answered, and
/// at the lock of one that is stopping.
const STOP_POLL: Duration = Duration::from_millis(250);

/// What [`detect`] found.
#[derive(Debug)]
pub enum Detection {
    /// No supervisor runs from the binary: its install lock, which the
    /// caller keeps through the replacement and the receipt.
    Clear(InstallLock),
    /// A supervisor of the data directory runs: it answered, or it holds the
    /// data directory's lifetime lock while it starts.
    Supervised(Running),
    /// The install lock is held, and no supervisor of the data directory
    /// answered: another install, or a supervisor of another data directory,
    /// as the reason says. Worth trying again.
    Busy(String),
    /// Whether one runs can't be told, for the reason given: nothing may be
    /// replaced.
    Refuse(String),
}

/// What a supervisor says of itself, as far as it said anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Running {
    pub pid: Option<u32>,
    pub version: Option<String>,
}

/// How [`stop`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// The supervisor said it was stopping, and its lifetime lock came free.
    Stopped,
    /// No supervisor held the data directory's lifetime lock.
    NotRunning,
    /// The deadline came first; what was last seen.
    TimedOut(String),
    /// The stop couldn't be asked for, for the reason given.
    Refused(String),
}

/// Whether a supervisor runs from the installed binary `installed`, for a
/// replacement of it by a caller holding `data_dir`'s update lock, who keeps
/// both locks through the replacement and its receipt. Never waits on a
/// lock, and returns by `deadline`.
///
/// A supervisor of another data directory that is still waiting for its own
/// update lock holds neither lock this checks, and isn't seen. If the binary
/// is replaced meanwhile, that supervisor finds the installed file isn't its
/// own bytes (its own hash is of the image it was loaded from) and runs
/// degraded until it is restarted.
pub async fn detect(
    data_dir: &Path,
    identity: &Identity,
    installed: &Path,
    deadline: Instant,
) -> Detection {
    let install = match InstallLock::try_acquire(installed) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            // The probe only names a supervisor of this data directory. A
            // reply is never what clears: that is the lock.
            if let Some(running) = probe(data_dir, identity, deadline).await {
                return Detection::Supervised(running);
            }
            let lock = InstallLock::path_for(installed).unwrap_or_else(|_| installed.to_owned());
            return Detection::Busy(format!(
                "another open-ferry install, or a supervisor using another data directory, holds {}",
                lock.display()
            ));
        }
        Err(error) => {
            return Detection::Refuse(format!(
                "could not take the install lock of {}: {error}",
                installed.display()
            ));
        }
    };
    match SupervisorLock::try_acquire(data_dir) {
        Ok(lock) => {
            drop(lock);
            Detection::Clear(install)
        }
        // A supervisor of this data directory is in its startup wait, before
        // its endpoint and before its install lock.
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Detection::Supervised(Running::default())
        }
        Err(error) => Detection::Refuse(format!(
            "could not check whether a supervisor holds {}: {error}",
            data_dir.join("supervisor.lock").display()
        )),
    }
}

/// Asks the supervisor of `data_dir` to stop, and waits until `deadline`
/// for its lifetime lock to come free. Takes no lock of its own.
///
/// It targets the supervisor the data directory's own record names. Without
/// a readable record it falls back to the identity's location only where
/// that follows from the data directory (Windows' pipe). On Unix the
/// identity's location is the per-user socket, which says nothing of the data
/// directory, so with no record there is nothing to ask: the data directory's
/// lock alone decides (free: `NotRunning`; held: retried to the deadline, a
/// starting supervisor may not have written its record yet, then `TimedOut`).
/// Every step is bounded by the one absolute `deadline`, and nothing is sent
/// after it.
pub async fn stop(data_dir: &Path, identity: &Identity, deadline: Instant) -> StopOutcome {
    let mut last = "the supervisor was not asked yet".to_owned();
    loop {
        if time_left(deadline).is_zero() {
            return StopOutcome::TimedOut(last);
        }
        // The record read and the resolution can stall on the file system:
        // they run where the deadline can abandon them.
        let located = {
            let (dir, identity) = (data_dir.to_owned(), identity.clone());
            within(deadline, move || locate(&dir, &identity)).await
        };
        let Some(location) = located else {
            return StopOutcome::TimedOut(last);
        };
        if time_left(deadline).is_zero() {
            return StopOutcome::TimedOut(last);
        }
        let asked = match &location {
            Ok(location) => {
                let request = EndpointRequest::Stop { v: VERSION };
                match tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    exchange(location, identity, &request, deadline),
                )
                .await
                {
                    Ok(Exchange::Reply(EndpointReply::Stopping { v })) if v == VERSION => {
                        return wait_for_the_lock(data_dir, deadline).await;
                    }
                    Ok(Exchange::Reply(EndpointReply::Stopping { v })) => {
                        return StopOutcome::Refused(format!(
                            "the supervisor answered the stop in protocol version {v}, not {VERSION}"
                        ));
                    }
                    Ok(Exchange::Reply(EndpointReply::Error { reason, .. })) => {
                        return StopOutcome::Refused(format!(
                            "the supervisor refused the stop: {reason}"
                        ));
                    }
                    Ok(Exchange::Reply(other)) => {
                        return StopOutcome::Refused(format!(
                            "the supervisor answered the stop with {other:?}"
                        ));
                    }
                    Ok(Exchange::Hard(why)) => return StopOutcome::Refused(why),
                    Ok(Exchange::Unreachable(why)) => why,
                    Ok(Exchange::TimedOut) | Err(_) => {
                        "the supervisor's endpoint did not answer the stop in time".to_owned()
                    }
                }
            }
            Err(why) => format!("no endpoint location is known for this data directory: {why}"),
        };
        last = asked;
        match lock_is_free(data_dir, deadline).await {
            None => return StopOutcome::TimedOut(last),
            Some(Ok(true)) => return StopOutcome::NotRunning,
            Some(Ok(false)) => {
                last = format!(
                    "a supervisor holds the lock but its endpoint is not reachable: {last}"
                );
            }
            Some(Err(error)) => {
                return StopOutcome::Refused(format!(
                    "could not check whether a supervisor holds {}: {error}",
                    data_dir.join("supervisor.lock").display()
                ));
            }
        }
        tokio::time::sleep(STOP_POLL.min(time_left(deadline))).await;
    }
}

/// Polls the lifetime lock of a supervisor that said it is stopping.
async fn wait_for_the_lock(data_dir: &Path, deadline: Instant) -> StopOutcome {
    loop {
        match lock_is_free(data_dir, deadline).await {
            None => {
                return StopOutcome::TimedOut(
                    "the supervisor said it was stopping, but still holds its lock".to_owned(),
                );
            }
            Some(Ok(true)) => return StopOutcome::Stopped,
            Some(Ok(false)) => {}
            Some(Err(error)) => {
                return StopOutcome::Refused(format!(
                    "could not check whether the supervisor has stopped: {error}"
                ));
            }
        }
        let left = time_left(deadline);
        if left.is_zero() {
            return StopOutcome::TimedOut(
                "the supervisor said it was stopping, but still holds its lock".to_owned(),
            );
        }
        tokio::time::sleep(STOP_POLL.min(left)).await;
    }
}

/// Runs `work`, which may block on the file system, on a thread of its own,
/// and waits for it only until `deadline`: `None` when that came first, or no
/// thread could be started. A thread still stuck then is left to finish by
/// itself and its answer is dropped. It is a plain `std::thread`, not
/// `spawn_blocking`: a runtime's drop waits for its blocking tasks, and
/// callers drop their own right after they return.
///
/// When the answer and the deadline are ready together the answer is
/// returned: it is a true fact (the lock was free, the record was read), late
/// only by the runtime's own delay, and turning a known "the supervisor is
/// gone" into a timeout would make an uninstall refuse for nothing.
async fn within<T: Send + 'static>(
    deadline: Instant,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    if time_left(deadline).is_zero() {
        return None;
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("supervisor-client-io".to_owned())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .ok()?;
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), receiver)
        .await
        .ok()?
        .ok()
}

/// Whether the lifetime lock of `data_dir`'s supervisor is free, `None` when
/// `deadline` came first. The lock is taken and let go again inside the one
/// thread. A check abandoned at the deadline, stuck in the file system, holds
/// the lock (and on Windows its file handle) until that thread ends; another
/// check meanwhile finds it held and says a supervisor may be running, the safe
/// side and never `Clear`.
async fn lock_is_free(data_dir: &Path, deadline: Instant) -> Option<io::Result<bool>> {
    let dir = data_dir.to_owned();
    within(deadline, move || {
        #[cfg(test)]
        tests::stall(&tests::LOCK_STALLS, &dir);
        match SupervisorLock::try_acquire(&dir) {
            Ok(lock) => {
                drop(lock);
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error),
        }
    })
    .await
}

/// Where the supervisor of `data_dir` listens: its record's endpoint, else
/// (only where it follows from the data directory, that is on Windows) the
/// identity's own location. The error says why there is none.
fn locate(data_dir: &Path, identity: &Identity) -> Result<Location, String> {
    match read_record(data_dir) {
        Ok(record) => Ok(record.location),
        Err(why) if cfg!(windows) => {
            Location::resolve(identity, data_dir).map_err(|error| format!("{why}; {error}"))
        }
        Err(why) => Err(why),
    }
}

fn time_left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Asks the data directory's own supervisor for its status, within
/// [`DETECT_TIMEOUT`] of starting and the deadline, the record read included.
/// `None` when nothing valid answers, and on Unix when the data directory has
/// no readable record: then nothing names its supervisor's socket.
async fn probe(data_dir: &Path, identity: &Identity, deadline: Instant) -> Option<Running> {
    let deadline = deadline.min(Instant::now() + DETECT_TIMEOUT);
    if time_left(deadline).is_zero() {
        return None;
    }
    let record = {
        let dir = data_dir.to_owned();
        within(deadline, move || read_record(&dir).ok()).await?
    };
    let location = match &record {
        Some(record) => record.location.clone(),
        None if cfg!(windows) => Location::resolve(identity, data_dir).ok()?,
        None => return None,
    };
    if time_left(deadline).is_zero() {
        return None;
    }
    let request = EndpointRequest::Status { v: VERSION };
    let reply = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        exchange(&location, identity, &request, deadline),
    )
    .await
    .ok()?;
    let Exchange::Reply(EndpointReply::Status { v, snapshot }) = reply else {
        return None;
    };
    if v != VERSION || !snapshot.is_object() {
        return None;
    }
    let version = snapshot
        .get("supervisor_version")
        .and_then(serde_json::Value::as_str)
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
        .or_else(|| record.as_ref().map(|record| record.version.clone()));
    Some(Running {
        pid: record.as_ref().map(|record| record.pid),
        version,
    })
}

/// How one request to the endpoint went.
enum Exchange {
    Reply(EndpointReply),
    /// Nothing is serving, or what was serving went away: worth asking again.
    Unreachable(String),
    /// The other end can't be trusted or understood: not worth asking again.
    Hard(String),
    /// The deadline passed after the connect: nothing was sent.
    TimedOut,
}

/// Sends `request` to the endpoint and reads its reply. Once connected it
/// waits for the stream to be writable, so that the I/O driver has seen the
/// connection, then checks `deadline`, then sends: the timeout around this
/// call polls it before its timer, so without the check a connect and a
/// deadline that are ready together would send after the deadline.
///
/// This narrows the race and doesn't close it. A frame whose write is resumed
/// after the deadline (a partial write, a cooperative yield) is the same as a
/// request sent just before it, which the supervisor acts on after the caller
/// has given up; the caller's `TimedOut` allows for that.
async fn exchange(
    location: &Location,
    identity: &Identity,
    request: &EndpointRequest,
    deadline: Instant,
) -> Exchange {
    let stream = match connect(location, identity).await {
        Ok(stream) => stream,
        Err(EndpointError::PeerRejected(why)) => {
            return Exchange::Hard(format!("the endpoint's server was refused: {why}"));
        }
        Err(error) => return Exchange::Unreachable(error.to_string()),
    };
    if let Err(error) = stream.writable().await {
        return Exchange::Unreachable(format!("the endpoint is not writable: {error}"));
    }
    #[cfg(test)]
    tests::after_writable();
    if time_left(deadline).is_zero() {
        return Exchange::TimedOut;
    }
    let (reader, writer) = tokio::io::split(stream);
    let mut channel = Channel::new(reader, writer);
    if let Err(error) = channel.send(request).await {
        return Exchange::Unreachable(format!("the request could not be sent: {error}"));
    }
    match channel.receive::<EndpointReply>().await {
        Ok(Some(frame)) => Exchange::Reply(frame.message),
        Ok(None) => Exchange::Unreachable("the endpoint closed without answering".to_owned()),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            Exchange::Hard(format!("the endpoint's reply is malformed: {error}"))
        }
        Err(error) => Exchange::Unreachable(format!("the reply could not be read: {error}")),
    }
}

/// What a supervisor wrote to `supervisor.json`, as far as clients use it.
struct Record {
    pid: u32,
    version: String,
    location: Location,
}

#[derive(Deserialize)]
struct RawRecord {
    pid: u32,
    endpoint: String,
    version: String,
    // Present in every record; read to check the shape, not used.
    #[expect(dead_code, reason = "part of the record's shape")]
    protocol: u32,
}

/// Reads the data directory's `supervisor.json`: a regular file, not a link,
/// at most 64 KiB, in the shape startup writes. The error says what is wrong.
fn read_record(data_dir: &Path) -> Result<Record, String> {
    #[cfg(test)]
    tests::stall(&tests::READ_STALLS, data_dir);
    let path = data_dir.join("supervisor.json");
    let bytes = read_bounded(&path, RECORD_LIMIT)
        .map_err(|error| format!("{} can't be read: {error}", path.display()))?;
    let raw: RawRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not a supervisor record: {error}", path.display()))?;
    let location = Location::from_record(&raw.endpoint)
        .map_err(|error| format!("{} names no usable endpoint: {error}", path.display()))?;
    Ok(Record {
        pid: raw.pid,
        version: raw.version,
        location,
    })
}

/// The contents of the regular file `path`, opened without following a link,
/// refusing one longer than `limit`.
fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = open_regular(path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("it is larger than {limit} bytes"),
        ));
    }
    Ok(bytes)
}

fn open_regular(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK so that a FIFO in the record's place can't hold the open.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::other("it is a reparse point"));
        }
    }
    if !metadata.is_file() {
        return Err(io::Error::other("it is not a regular file"));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{Server, ServiceKind};
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    thread_local! {
        /// How long the client waits between its connect and its send, for the
        /// test that runs it (the exchange runs on the test's own thread).
        pub(super) static CONNECTED_DELAY: std::cell::Cell<Duration> =
            const { std::cell::Cell::new(Duration::ZERO) };
        /// Whether the exchange got as far as that point.
        static CONNECTED_REACHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Where a test holds the client: after the connect and the writable wait,
    /// right before the deadline check and the send.
    pub(super) fn after_writable() {
        CONNECTED_REACHED.with(|reached| reached.set(true));
        std::thread::sleep(CONNECTED_DELAY.with(std::cell::Cell::get));
    }

    /// How long a file-system step takes, by data directory, and whether it
    /// has been entered: the steps run on threads of their own, which a
    /// thread-local could not reach.
    type Stalls = std::sync::Mutex<Vec<(PathBuf, Duration, bool)>>;
    pub(super) static READ_STALLS: Stalls = std::sync::Mutex::new(Vec::new());
    pub(super) static LOCK_STALLS: Stalls = std::sync::Mutex::new(Vec::new());

    pub(super) fn stall(table: &Stalls, data_dir: &Path) {
        let delay = table
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(path, ..)| path == data_dir)
            .map(|entry| {
                entry.2 = true;
                entry.1
            });
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
    }

    /// A stall in `table` for `data_dir`, until dropped.
    struct Stalled(&'static Stalls, PathBuf);

    impl Stalled {
        fn new(table: &'static Stalls, data_dir: &Path, delay: Duration) -> Self {
            table
                .lock()
                .unwrap()
                .push((data_dir.to_owned(), delay, false));
            Self(table, data_dir.to_owned())
        }

        /// Whether the stalled step ran.
        fn entered(&self) -> bool {
            self.0
                .lock()
                .unwrap()
                .iter()
                .any(|(path, _, entered)| path == &self.1 && *entered)
        }
    }

    impl Drop for Stalled {
        fn drop(&mut self) {
            self.0.lock().unwrap().retain(|(path, ..)| path != &self.1);
        }
    }

    /// How far past its deadline a call may return: 2 s, for a CI runner that
    /// is descheduled for a while. The stalls are 5 s, so a call that waited
    /// one out takes at least that and misses this by 3 s.
    const MARGIN: Duration = Duration::from_secs(2);
    const STALL: Duration = Duration::from_secs(5);

    #[cfg(unix)]
    static NEXT: AtomicU32 = AtomicU32::new(0);

    fn identity() -> Identity {
        Identity::current(ServiceKind::User).unwrap()
    }

    /// A data directory and a binary directory, each of its own.
    struct Dirs {
        data: tempfile::TempDir,
        bin: tempfile::TempDir,
    }

    impl Dirs {
        fn new() -> Self {
            Self {
                data: tempfile::tempdir().unwrap(),
                bin: tempfile::tempdir().unwrap(),
            }
        }

        fn data(&self) -> &Path {
            self.data.path()
        }

        fn installed(&self) -> PathBuf {
            self.bin.path().join("open-ferry")
        }
    }

    fn soon(seconds: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds)
    }

    /// An endpoint location for `data`: the pipe it names on Windows, a
    /// socket of its own under `/tmp` (the one root the policy accepts) on Unix.
    fn location_for(data: &Path) -> Location {
        #[cfg(windows)]
        {
            Location::for_data_dir(data)
        }
        #[cfg(unix)]
        {
            let _ = data;
            let name = format!(
                "ofp-client-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            );
            let root = std::fs::canonicalize("/tmp").expect("canonical endpoint root");
            let location = Location::new(&root, name).unwrap();
            assert!(
                location.socket_path().as_os_str().len() <= 103,
                "SET-UP: canonical socket path exceeds 103 bytes: {}",
                location.socket_path().display()
            );
            let _ = std::fs::remove_dir_all(location.dir());
            location
        }
    }

    struct Endpoint {
        location: Location,
        task: tokio::task::JoinHandle<()>,
        _lock_dir: tempfile::TempDir,
    }

    impl Drop for Endpoint {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// Binds an endpoint at `location` and answers each connection with
    /// `serve`, which is given the request line.
    fn serve<F, Fut>(location: Location, serve: F) -> Endpoint
    where
        F: Fn(String, crate::endpoint::ServerStream) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let lock_dir = tempfile::tempdir().unwrap();
        let lock = SupervisorLock::try_acquire(lock_dir.path()).unwrap();
        let mut server = Server::bind(&location, &identity(), &lock).unwrap();
        let serve = std::sync::Arc::new(serve);
        let task = tokio::spawn(async move {
            let _lock = lock;
            loop {
                let Ok(connection) = server.accept().await else {
                    return;
                };
                let serve = serve.clone();
                tokio::spawn(async move {
                    let mut stream = connection.stream;
                    let mut line = Vec::new();
                    let mut byte = [0u8; 1];
                    while stream.read_exact(&mut byte).await.is_ok() {
                        if byte[0] == b'\n' {
                            break;
                        }
                        line.push(byte[0]);
                    }
                    serve(String::from_utf8_lossy(&line).into_owned(), stream).await;
                });
            }
        });
        Endpoint {
            location,
            task,
            _lock_dir: lock_dir,
        }
    }

    async fn reply(stream: &mut crate::endpoint::ServerStream, text: &str) {
        stream.write_all(text.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.flush().await.unwrap();
    }

    /// A supervisor that answers a status with `snapshot` and a stop with `Stopping`.
    fn answering(location: Location, snapshot: &'static str) -> Endpoint {
        serve(location, move |request, mut stream| async move {
            let text = if request.contains("\"stop\"") {
                r#"{"type":"stopping","v":1}"#.to_owned()
            } else {
                format!(r#"{{"type":"status","v":1,"snapshot":{snapshot}}}"#)
            };
            reply(&mut stream, &text).await;
        })
    }

    fn write_record(data: &Path, location: &Location, pid: u32) {
        let record = serde_json::json!({
            "pid": pid,
            "started": 1,
            "endpoint": location.to_record(),
            "version": "9.9.9",
            "protocol": 1,
        });
        std::fs::write(data.join("supervisor.json"), record.to_string()).unwrap();
    }

    fn assert_busy(detection: Detection) {
        assert!(matches!(detection, Detection::Busy(_)), "{detection:?}");
    }

    #[tokio::test]
    async fn with_both_locks_free_the_binary_is_clear_and_the_install_lock_is_returned() {
        let dirs = Dirs::new();
        let detection = detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await;
        let Detection::Clear(lock) = detection else {
            panic!("{detection:?}");
        };
        // The caller's lock is the one held, and the supervisor lock was let go.
        assert_eq!(
            InstallLock::try_acquire(&dirs.installed())
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(SupervisorLock::try_acquire(dirs.data()).unwrap());
        drop(lock);
    }

    #[tokio::test]
    async fn a_held_install_lock_with_no_supervisor_answering_is_busy_and_names_the_lock() {
        let dirs = Dirs::new();
        let held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let detection = detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await;
        let Detection::Busy(why) = detection else {
            panic!("{detection:?}");
        };
        assert!(why.contains(&held.path().display().to_string()), "{why}");
    }

    #[tokio::test]
    async fn a_held_install_lock_and_a_supervisor_that_answers_is_supervised_with_its_pid_and_version()
     {
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let endpoint = answering(
            location_for(dirs.data()),
            r#"{"state":"idle","supervisor_version":"1.2.3"}"#,
        );
        write_record(dirs.data(), &endpoint.location, 4242);
        let detection = detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await;
        let Detection::Supervised(running) = detection else {
            panic!("{detection:?}");
        };
        assert_eq!(running.pid, Some(4242));
        assert_eq!(running.version.as_deref(), Some("1.2.3"));
    }

    #[tokio::test]
    async fn without_a_record_the_probe_uses_the_identitys_own_location() {
        // Only where the location follows from the data directory.
        if !cfg!(windows) {
            return;
        }
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let _endpoint = answering(location_for(dirs.data()), r#"{"state":"idle"}"#);
        let detection = detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await;
        let Detection::Supervised(running) = detection else {
            panic!("{detection:?}");
        };
        assert_eq!(running, Running::default());
    }

    #[tokio::test]
    async fn a_held_install_lock_and_garbage_from_the_endpoint_is_busy() {
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let endpoint = serve(location_for(dirs.data()), |_, mut stream| async move {
            reply(&mut stream, "this is not json").await;
        });
        write_record(dirs.data(), &endpoint.location, 1);
        assert_busy(detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await);
    }

    #[tokio::test]
    async fn a_reply_of_another_kind_or_version_is_not_a_status() {
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let endpoint = serve(location_for(dirs.data()), |_, mut stream| async move {
            reply(&mut stream, r#"{"type":"status","v":2,"snapshot":{}}"#).await;
        });
        write_record(dirs.data(), &endpoint.location, 1);
        assert_busy(detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await);
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let endpoint = serve(location_for(dirs.data()), |_, mut stream| async move {
            reply(&mut stream, r#"{"type":"cancelled","v":1}"#).await;
        });
        write_record(dirs.data(), &endpoint.location, 1);
        assert_busy(detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await);
    }

    #[tokio::test]
    async fn a_held_install_lock_and_an_endpoint_that_never_answers_is_busy_within_the_deadline() {
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let endpoint = serve(location_for(dirs.data()), |_, stream| async move {
            let _stream = stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        write_record(dirs.data(), &endpoint.location, 1);
        let began = Instant::now();
        let detection = detect(
            dirs.data(),
            &identity(),
            &dirs.installed(),
            Instant::now() + Duration::from_millis(400),
        )
        .await;
        assert_busy(detection);
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "{:?}",
            began.elapsed()
        );
    }

    #[tokio::test]
    async fn a_deadline_that_has_passed_is_not_a_reason_to_probe_or_to_clear_from_a_failure() {
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let endpoint = answering(location_for(dirs.data()), r#"{"state":"idle"}"#);
        write_record(dirs.data(), &endpoint.location, 1);
        let detection = detect(dirs.data(), &identity(), &dirs.installed(), Instant::now()).await;
        assert_busy(detection);
    }

    #[tokio::test]
    async fn an_install_lock_that_cannot_be_taken_for_another_reason_is_a_refusal() {
        let dirs = Dirs::new();
        std::fs::create_dir(InstallLock::path_for(&dirs.installed()).unwrap()).unwrap();
        let detection = detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await;
        assert!(matches!(detection, Detection::Refuse(_)), "{detection:?}");
        let detection = detect(dirs.data(), &identity(), &dirs.bin_missing(), soon(5)).await;
        assert!(matches!(detection, Detection::Refuse(_)), "{detection:?}");
    }

    impl Dirs {
        /// A binary in a directory that does not exist.
        fn bin_missing(&self) -> PathBuf {
            self.bin.path().join("missing").join("open-ferry")
        }
    }

    #[tokio::test]
    async fn a_busy_supervisor_lock_with_the_install_lock_free_is_a_supervisor_in_its_startup_wait()
    {
        let dirs = Dirs::new();
        let _supervisor = SupervisorLock::try_acquire(dirs.data()).unwrap();
        let detection = detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await;
        let Detection::Supervised(running) = detection else {
            panic!("{detection:?}");
        };
        assert_eq!(running, Running::default());
        // The install lock taken on the way was let go again.
        drop(InstallLock::try_acquire(&dirs.installed()).unwrap());
    }

    #[tokio::test]
    async fn a_supervisor_lock_that_cannot_be_opened_is_a_refusal_and_lets_the_install_lock_go() {
        let dirs = Dirs::new();
        std::fs::create_dir(dirs.data().join("supervisor.lock")).unwrap();
        let detection = detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await;
        assert!(matches!(detection, Detection::Refuse(_)), "{detection:?}");
        drop(InstallLock::try_acquire(&dirs.installed()).unwrap());
    }

    #[test]
    fn a_missing_garbage_oversized_or_misshapen_record_is_an_error_and_a_good_one_is_read() {
        let dirs = Dirs::new();
        let record = dirs.data().join("supervisor.json");
        assert!(read_record(dirs.data()).is_err(), "missing");
        std::fs::write(&record, "{not json").unwrap();
        assert!(read_record(dirs.data()).is_err(), "garbage");
        std::fs::write(&record, "{}").unwrap();
        assert!(read_record(dirs.data()).is_err(), "empty object");
        let location = location_for(dirs.data());
        let good = serde_json::json!({
            "pid": 7, "started": 1, "endpoint": location.to_record(),
            "version": "1.0.0", "protocol": 1,
        })
        .to_string();
        std::fs::write(&record, &good).unwrap();
        let read = read_record(dirs.data()).unwrap();
        assert_eq!((read.pid, read.version.as_str()), (7, "1.0.0"));
        assert_eq!(read.location, location);
        let padded = format!("{good}{}", " ".repeat(RECORD_LIMIT as usize));
        std::fs::write(&record, padded).unwrap();
        assert!(read_record(dirs.data()).is_err(), "oversized");
        let wrong = serde_json::json!({
            "pid": 7, "started": 1, "endpoint": "/not/a/socket",
            "version": "1.0.0", "protocol": 1,
        })
        .to_string();
        std::fs::write(&record, wrong).unwrap();
        assert!(
            read_record(dirs.data()).is_err(),
            "an endpoint of the wrong shape"
        );
    }

    #[test]
    fn a_record_that_is_a_directory_is_not_read() {
        let dirs = Dirs::new();
        std::fs::create_dir(dirs.data().join("supervisor.json")).unwrap();
        assert!(read_record(dirs.data()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_record_that_is_a_link_is_not_followed() {
        let dirs = Dirs::new();
        let location = location_for(dirs.data());
        let elsewhere = dirs.data().join("elsewhere.json");
        let good = serde_json::json!({
            "pid": 7, "started": 1, "endpoint": location.to_record(),
            "version": "1.0.0", "protocol": 1,
        });
        std::fs::write(&elsewhere, good.to_string()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dirs.data().join("supervisor.json")).unwrap();
        assert!(read_record(dirs.data()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_record_that_is_a_fifo_does_not_hold_the_read() {
        let dirs = Dirs::new();
        let path =
            std::ffi::CString::new(dirs.data().join("supervisor.json").to_str().unwrap()).unwrap();
        // SAFETY: `path` is NUL-terminated.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(read_record(dirs.data()).is_err());
    }

    #[tokio::test]
    async fn a_bad_record_with_no_answering_endpoint_is_busy() {
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        std::fs::write(dirs.data().join("supervisor.json"), "{not json").unwrap();
        // Nothing answers, whether the probe falls back to the identity's own
        // location (Windows) or has no location at all (Unix): only `Busy`.
        assert_busy(detect(dirs.data(), &identity(), &dirs.installed(), soon(5)).await);
    }

    #[tokio::test]
    async fn a_supervisor_that_stops_when_asked_is_stopped_once_its_lock_is_free() {
        let dirs = Dirs::new();
        let lock = SupervisorLock::try_acquire(dirs.data()).unwrap();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(lock)));
        let hook = slot.clone();
        let endpoint = serve(location_for(dirs.data()), move |_, mut stream| {
            let hook = hook.clone();
            async move {
                reply(&mut stream, r#"{"type":"stopping","v":1}"#).await;
                // The lock goes a moment after the reply, as a stopping supervisor's does.
                tokio::time::sleep(Duration::from_millis(300)).await;
                hook.lock().unwrap().take();
            }
        });
        write_record(dirs.data(), &endpoint.location, 1);
        let outcome = stop(dirs.data(), &identity(), soon(10)).await;
        assert_eq!(outcome, StopOutcome::Stopped);
        assert!(slot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn nothing_serving_and_no_lock_held_is_not_running() {
        let dirs = Dirs::new();
        let outcome = stop(dirs.data(), &identity(), soon(10)).await;
        assert_eq!(outcome, StopOutcome::NotRunning);
    }

    #[tokio::test]
    async fn a_held_lock_with_no_endpoint_is_retried_until_the_deadline_and_then_timed_out() {
        let dirs = Dirs::new();
        let _supervisor = SupervisorLock::try_acquire(dirs.data()).unwrap();
        let began = Instant::now();
        // Room for at least one full pass (ask, then check the lock) on a
        // loaded machine; a 900 ms deadline could end inside the first check.
        let outcome = stop(
            dirs.data(),
            &identity(),
            Instant::now() + Duration::from_secs(3),
        )
        .await;
        let StopOutcome::TimedOut(seen) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(seen.contains("not reachable"), "{seen}");
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{:?}",
            began.elapsed()
        );
    }

    #[tokio::test]
    async fn a_supervisor_that_frees_its_lock_while_stop_retries_is_not_running() {
        let dirs = Dirs::new();
        let supervisor = SupervisorLock::try_acquire(dirs.data()).unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(supervisor);
        });
        let outcome = stop(dirs.data(), &identity(), soon(10)).await;
        release.await.unwrap();
        assert_eq!(outcome, StopOutcome::NotRunning);
    }

    #[tokio::test]
    async fn a_supervisor_that_never_answers_the_stop_ends_at_the_deadline_with_timed_out() {
        let dirs = Dirs::new();
        let _supervisor = SupervisorLock::try_acquire(dirs.data()).unwrap();
        let endpoint = serve(location_for(dirs.data()), |_, stream| async move {
            let _stream = stream;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        write_record(dirs.data(), &endpoint.location, 1);
        let began = Instant::now();
        let outcome = stop(
            dirs.data(),
            &identity(),
            Instant::now() + Duration::from_millis(600),
        )
        .await;
        assert!(matches!(outcome, StopOutcome::TimedOut(_)), "{outcome:?}");
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "{:?}",
            began.elapsed()
        );
    }

    #[tokio::test]
    async fn a_supervisor_that_says_stopping_but_keeps_its_lock_times_out() {
        let dirs = Dirs::new();
        let _supervisor = SupervisorLock::try_acquire(dirs.data()).unwrap();
        let endpoint = answering(location_for(dirs.data()), "{}");
        write_record(dirs.data(), &endpoint.location, 1);
        let outcome = stop(
            dirs.data(),
            &identity(),
            Instant::now() + Duration::from_millis(700),
        )
        .await;
        let StopOutcome::TimedOut(seen) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(seen.contains("stopping"), "{seen}");
    }

    #[tokio::test]
    async fn a_malformed_or_unexpected_reply_to_the_stop_is_a_refusal() {
        for text in [
            "garbage",
            r#"{"type":"status","v":1,"snapshot":{}}"#,
            r#"{"type":"stopping","v":2}"#,
            r#"{"type":"error","v":1,"reason":"unsupported protocol version"}"#,
        ] {
            let dirs = Dirs::new();
            let _supervisor = SupervisorLock::try_acquire(dirs.data()).unwrap();
            let endpoint = serve(location_for(dirs.data()), move |_, mut stream| async move {
                reply(&mut stream, text).await;
            });
            write_record(dirs.data(), &endpoint.location, 1);
            let outcome = stop(dirs.data(), &identity(), soon(5)).await;
            assert!(
                matches!(outcome, StopOutcome::Refused(_)),
                "{text}: {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_supervisor_lock_that_cannot_be_opened_makes_the_stop_a_refusal() {
        let dirs = Dirs::new();
        std::fs::create_dir(dirs.data().join("supervisor.lock")).unwrap();
        let outcome = stop(dirs.data(), &identity(), soon(5)).await;
        assert!(matches!(outcome, StopOutcome::Refused(_)), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_stop_whose_deadline_has_passed_asks_nothing() {
        let dirs = Dirs::new();
        let outcome = stop(dirs.data(), &identity(), Instant::now()).await;
        assert!(matches!(outcome, StopOutcome::TimedOut(_)), "{outcome:?}");
    }

    /// An endpoint that records whether a request reached it. A client that
    /// connects and goes away without sending is not a request: `serve` hands
    /// over the empty line it read.
    fn watched(
        dirs: &Dirs,
        text: &'static str,
    ) -> (Endpoint, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = asked.clone();
        let endpoint = serve(location_for(dirs.data()), move |request, mut stream| {
            let seen = seen.clone();
            async move {
                if request.is_empty() {
                    return;
                }
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
                reply(&mut stream, text).await;
            }
        });
        (endpoint, asked)
    }

    #[tokio::test]
    async fn a_stalled_record_read_ends_the_stop_at_the_deadline_with_no_stop_sent() {
        let dirs = Dirs::new();
        let _supervisor = SupervisorLock::try_acquire(dirs.data()).unwrap();
        let (endpoint, asked) = watched(&dirs, r#"{"type":"stopping","v":1}"#);
        write_record(dirs.data(), &endpoint.location, 1);
        let stall = Stalled::new(&READ_STALLS, dirs.data(), STALL);
        let began = Instant::now();
        let outcome = stop(dirs.data(), &identity(), began + Duration::from_millis(300)).await;
        assert!(began.elapsed() < Duration::from_millis(300) + MARGIN);
        assert!(stall.entered(), "the record read was never reached");
        assert!(matches!(outcome, StopOutcome::TimedOut(_)), "{outcome:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_stalled_record_read_ends_the_probe_at_the_deadline_with_no_status_request_sent() {
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let (endpoint, asked) = watched(&dirs, r#"{"type":"status","v":1,"snapshot":{}}"#);
        write_record(dirs.data(), &endpoint.location, 1);
        let stall = Stalled::new(&READ_STALLS, dirs.data(), STALL);
        let began = Instant::now();
        let detection = detect(
            dirs.data(),
            &identity(),
            &dirs.installed(),
            began + Duration::from_millis(300),
        )
        .await;
        assert!(began.elapsed() < Duration::from_millis(300) + MARGIN);
        assert!(stall.entered(), "the record read was never reached");
        assert_busy(detection);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_stalled_lock_check_ends_the_stop_at_the_deadline() {
        // No record and no supervisor: the lock check is all there is, and it
        // would say `NotRunning` if it were waited for.
        let dirs = Dirs::new();
        let stall = Stalled::new(&LOCK_STALLS, dirs.data(), STALL);
        let began = Instant::now();
        let outcome = stop(dirs.data(), &identity(), began + Duration::from_millis(300)).await;
        assert!(began.elapsed() < Duration::from_millis(300) + MARGIN);
        assert!(stall.entered(), "the lock check was never reached");
        assert!(matches!(outcome, StopOutcome::TimedOut(_)), "{outcome:?}");
        // The abandoned check takes the lock after its stall and lets it go
        // at once: nothing is held once it has ended.
        tokio::time::sleep(STALL + Duration::from_millis(300)).await;
        drop(SupervisorLock::try_acquire(dirs.data()).unwrap());
    }

    #[tokio::test]
    async fn a_deadline_that_passes_between_the_connect_and_the_send_sends_no_stop() {
        let dirs = Dirs::new();
        let _supervisor = SupervisorLock::try_acquire(dirs.data()).unwrap();
        let (endpoint, asked) = watched(&dirs, r#"{"type":"stopping","v":1}"#);
        write_record(dirs.data(), &endpoint.location, 1);
        // The connect is done in time; the deadline passes while the client
        // is held after it.
        CONNECTED_REACHED.with(|reached| reached.set(false));
        CONNECTED_DELAY.with(|delay| delay.set(Duration::from_millis(600)));
        let outcome = stop(
            dirs.data(),
            &identity(),
            Instant::now() + Duration::from_millis(300),
        )
        .await;
        CONNECTED_DELAY.with(|delay| delay.set(Duration::ZERO));
        assert!(
            CONNECTED_REACHED.with(std::cell::Cell::get),
            "the exchange never reached the connect-to-send point"
        );
        assert!(matches!(outcome, StopOutcome::TimedOut(_)), "{outcome:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_deadline_that_passes_between_the_connect_and_the_send_sends_no_status_request() {
        let dirs = Dirs::new();
        let _held = InstallLock::try_acquire(&dirs.installed()).unwrap();
        let (endpoint, asked) = watched(&dirs, r#"{"type":"status","v":1,"snapshot":{}}"#);
        write_record(dirs.data(), &endpoint.location, 1);
        CONNECTED_REACHED.with(|reached| reached.set(false));
        CONNECTED_DELAY.with(|delay| delay.set(Duration::from_millis(600)));
        let detection = detect(
            dirs.data(),
            &identity(),
            &dirs.installed(),
            Instant::now() + Duration::from_millis(300),
        )
        .await;
        CONNECTED_DELAY.with(|delay| delay.set(Duration::ZERO));
        assert!(
            CONNECTED_REACHED.with(std::cell::Cell::get),
            "the exchange never reached the connect-to-send point"
        );
        assert_busy(detection);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn without_a_record_neither_stop_nor_the_probe_reaches_any_socket() {
        // The identity's own location is the per-user socket of any data
        // directory: it must not be asked about this one.
        let dirs = Dirs::new();
        let _supervisor = SupervisorLock::try_acquire(dirs.data()).unwrap();
        let outcome = stop(
            dirs.data(),
            &identity(),
            Instant::now() + Duration::from_millis(600),
        )
        .await;
        let StopOutcome::TimedOut(seen) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(seen.contains("supervisor.json"), "{seen}");
        assert!(probe(dirs.data(), &identity(), soon(2)).await.is_none());
    }

    fn update_request() -> SwitchRequest {
        SwitchRequest {
            to: "2.0.0".into(),
            sha256: "ab".repeat(32),
            pin: "pin".into(),
            how: crate::protocol::How::Update,
            auto: false,
        }
    }

    #[tokio::test]
    async fn switch_client_yields_progress_and_done_and_checks_the_operation_id() {
        use open_ferry_update::{SwitchError, SwitchEvent};
        for wrong_id in [false, true] {
            let dirs = Dirs::new();
            let endpoint = serve(
                location_for(dirs.data()),
                move |request, mut stream| async move {
                    let request: EndpointRequest = serde_json::from_str(&request).unwrap();
                    assert!(matches!(
                        request,
                        EndpointRequest::Switch { auto: false, .. }
                    ));
                    reply(&mut stream, r#"{"type":"accepted","v":1,"id":"one"}"#).await;
                    reply(
                        &mut stream,
                        r#"{"type":"progress","v":1,"id":"one","phase":"loading"}"#,
                    )
                    .await;
                    let id = if wrong_id { "other" } else { "one" };
                    reply(&mut stream, &format!(r#"{{"type":"done","v":1,"id":"{id}","result":{{"result":"switched"}}}}"#)).await;
                },
            );
            write_record(dirs.data(), &endpoint.location, 1);
            let mut handle = switch(dirs.data(), &identity(), update_request(), soon(5))
                .await
                .unwrap();
            assert_eq!(handle.id, "one");
            assert_eq!(
                handle.next(soon(5)).await.unwrap(),
                Some(SwitchEvent::Progress {
                    phase: "loading".into()
                })
            );
            let done = handle.next(soon(5)).await;
            if wrong_id {
                assert!(matches!(done, Err(SwitchError::Protocol(_))));
            } else {
                assert!(matches!(done, Ok(Some(SwitchEvent::Done { .. }))));
            }
            assert_eq!(handle.next(soon(5)).await.unwrap(), None);
        }
    }

    #[tokio::test]
    async fn switch_client_reports_refusal_eof_and_bad_versions() {
        use open_ferry_update::SwitchError;
        for (first, last) in [
            (r#"{"type":"error","v":1,"reason":"busy"}"#, None),
            (
                r#"{"type":"unsupported","v":1,"message_type":"switch"}"#,
                None,
            ),
            (r#"{"type":"accepted","v":2,"id":"one"}"#, None),
            (r#"{"type":"accepted","v":1,"id":"one"}"#, None),
            (
                r#"{"type":"accepted","v":1,"id":"one"}"#,
                Some(r#"{"type":"error","v":1,"reason":"stopping"}"#),
            ),
        ] {
            let dirs = Dirs::new();
            let endpoint = serve(location_for(dirs.data()), move |_, mut stream| async move {
                reply(&mut stream, first).await;
                if let Some(last) = last {
                    reply(&mut stream, last).await;
                }
            });
            write_record(dirs.data(), &endpoint.location, 1);
            let outcome = switch(dirs.data(), &identity(), update_request(), soon(5)).await;
            if first.contains("busy") {
                assert!(matches!(outcome, Err(SwitchError::Refused(_))));
            } else if first.contains("unsupported") {
                assert!(
                    matches!(outcome, Err(SwitchError::Protocol(reason)) if reason.contains("does not support switch requests"))
                );
            } else if first.contains("\"v\":2") {
                assert!(
                    matches!(outcome, Err(SwitchError::Protocol(reason)) if reason.contains("protocol version 2, expected 1"))
                );
            } else {
                let error = outcome.unwrap().next(soon(5)).await.unwrap_err();
                if last.is_some() {
                    assert!(matches!(error, SwitchError::Ended(reason) if reason == "stopping"));
                } else {
                    assert!(matches!(error, SwitchError::Stopped), "{error}");
                }
            }
        }
    }

    #[tokio::test]
    async fn switch_client_progress_timeout_preserves_a_partial_frame() {
        use open_ferry_update::{SwitchError, SwitchEvent};
        let dirs = Dirs::new();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let sending = release.clone();
        let endpoint = serve(location_for(dirs.data()), move |_, mut stream| {
            let sending = sending.clone();
            async move {
                reply(&mut stream, r#"{"type":"accepted","v":1,"id":"one"}"#).await;
                stream
                    .write_all(br#"{"type":"done","v":1,"id":"one","result":"#)
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                sending.notified().await;
                reply(&mut stream, r#"{}}"#).await;
            }
        });
        write_record(dirs.data(), &endpoint.location, 1);
        let mut handle = switch(dirs.data(), &identity(), update_request(), soon(5))
            .await
            .unwrap();
        assert!(matches!(
            handle
                .next(Instant::now() + Duration::from_millis(50))
                .await,
            Err(SwitchError::TimedOut)
        ));
        release.notify_one();
        assert!(matches!(
            handle.next(soon(5)).await.unwrap(),
            Some(SwitchEvent::Done { .. })
        ));
    }

    #[tokio::test]
    async fn status_and_cancel_use_the_endpoint_without_touching_locks() {
        use open_ferry_update::CancelOutcome;
        for answer in ["cancelled", "too_late", "nothing"] {
            let dirs = Dirs::new();
            let _lifetime = SupervisorLock::try_acquire(dirs.data()).unwrap();
            let _install = InstallLock::try_acquire(&dirs.installed()).unwrap();
            let endpoint = serve(
                location_for(dirs.data()),
                move |request, mut stream| async move {
                    let request: EndpointRequest = serde_json::from_str(&request).unwrap();
                    let response = match request {
                        EndpointRequest::Status { .. } => EndpointReply::Status {
                            v: VERSION,
                            snapshot: serde_json::to_value(crate::snapshot::Snapshot::default())
                                .unwrap(),
                        },
                        EndpointRequest::Cancel { .. } => match answer {
                            "cancelled" => EndpointReply::Cancelled { v: VERSION },
                            "too_late" => EndpointReply::TooLate { v: VERSION },
                            _ => EndpointReply::Error {
                                v: VERSION,
                                reason: "nothing is switching".into(),
                            },
                        },
                        other => panic!("{other:?}"),
                    };
                    reply(&mut stream, &serde_json::to_string(&response).unwrap()).await;
                },
            );
            write_record(dirs.data(), &endpoint.location, 1);
            assert_eq!(
                status(dirs.data(), &identity(), soon(5)).await.unwrap(),
                crate::snapshot::Snapshot::default()
            );
            assert_eq!(
                cancel(dirs.data(), &identity(), soon(5)).await.unwrap(),
                match answer {
                    "cancelled" => CancelOutcome::Cancelled,
                    "too_late" => CancelOutcome::TooLate,
                    _ => CancelOutcome::Nothing,
                }
            );
        }
    }

    #[tokio::test]
    async fn switch_client_bounds_record_reads_and_never_sends_after_the_deadline() {
        use open_ferry_update::SwitchError;
        let dirs = Dirs::new();
        let (endpoint, asked) = watched(&dirs, r#"{"type":"accepted","v":1,"id":"one"}"#);
        write_record(dirs.data(), &endpoint.location, 1);
        let stall = Stalled::new(&READ_STALLS, dirs.data(), STALL);
        let began = Instant::now();
        assert!(matches!(
            switch(
                dirs.data(),
                &identity(),
                update_request(),
                began + Duration::from_millis(100)
            )
            .await,
            Err(SwitchError::Unreachable(why)) if why == "timed out before sending"
        ));
        assert!(began.elapsed() < Duration::from_millis(100) + MARGIN);
        assert!(stall.entered());
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
    }
    #[tokio::test]
    async fn request_classifies_pre_send_failures_and_a_mid_write_endpoint_failure() {
        use open_ferry_update::SwitchError;
        let dirs = Dirs::new();
        let location = location_for(dirs.data());
        write_record(dirs.data(), &location, 1);
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let result = switching::request(
            dirs.data(),
            &identity(),
            EndpointRequest::Cancel { v: VERSION },
            soon(5),
            |_| async {
                Ok((
                    tokio::io::empty(),
                    switching::tests::PartialWriteError(writes.clone()),
                ))
            },
        )
        .await;
        assert!(matches!(result, Err(SwitchError::Stopped)));
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 2);
        let result = switching::request(
            dirs.data(),
            &identity(),
            EndpointRequest::Cancel { v: VERSION },
            Instant::now() + Duration::from_millis(30),
            |_| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok((tokio::io::empty(), tokio::io::sink()))
            },
        )
        .await;
        assert!(
            matches!(result, Err(SwitchError::Unreachable(why)) if why == "timed out before sending")
        );
        let mut oversized = update_request();
        oversized.pin = "p".repeat(crate::protocol::MAX_FRAME);
        let result = switching::request(
            dirs.data(),
            &identity(),
            EndpointRequest::Switch {
                v: VERSION,
                to: oversized.to,
                sha256: oversized.sha256,
                pin: oversized.pin,
                how: oversized.how,
                auto: false,
                admitted: crate::protocol::Admission {
                    while_off: false,
                    mode_gen: 0,
                },
            },
            soon(5),
            |_| async { Ok((tokio::io::empty(), tokio::io::sink())) },
        )
        .await;
        assert!(matches!(result, Err(SwitchError::Unreachable(why)) if why.contains("encode")));
    }
}
